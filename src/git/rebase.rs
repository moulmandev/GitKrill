//! Interactive rebase plans.
//!
//! The UI loads a [`Range`] (the commits after a base, up to HEAD), lets the
//! user edit a [`Plan`] over it, and turns the plan into one `git rebase -i`
//! with [`Plan::prepare`].
//!
//! How a plan reaches git:
//!
//! * The todo list is written to a staging folder inside the git directory
//!   and handed over through `GIT_SEQUENCE_EDITOR`, which copies the folder
//!   into git's own `rebase-merge` state directory. Git deletes that
//!   directory when the rebase finishes or is aborted, so nothing is left
//!   behind however the rebase ends.
//! * The todo list only uses `pick`, `fixup`, `drop` and `exec`, plus
//!   `label`, `reset` and `merge` when the range has merges. A new message
//!   (reword, or the message of a squash group) is applied by an
//!   `exec git commit --amend -F <file>` line after the commits it belongs
//!   to. Everything the plan needs is therefore in the todo list itself: a
//!   rebase that stops on a conflict carries on correctly with a plain
//!   `git rebase --continue`, from this app or from a terminal, and no
//!   editor is ever involved, so messages arrive byte for byte.
//! * Merges (`--rebase-merges`): the plan lists the branch's own line of
//!   history, the first parents from HEAD down to the base, merge commits
//!   included. The commits a merge brought in from its other side are not
//!   planned one by one; they are replayed as they are, onto whatever they
//!   branched off from, and the merge is made again with its own message
//!   (`merge -C`). Commits and merges whose parents did not change are not
//!   rewritten at all. A merge can be kept, reworded or dropped (with what
//!   it brought in), and commits can be folded into it, but it cannot be
//!   folded into another commit; see [`Plan::refusal`].
//! * Paths never appear in a command line. The staging folder travels in an
//!   environment variable and the `exec` lines ask git where its state
//!   directory is, so spaces, quotes and backslashes need no escaping.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use git2::{Repository, RepositoryState, Sort, StatusOptions};

use super::ops::{GitCmd, Step, UndoKind};
use super::repo::{lossy, read_head};
use super::{ChangeKind, Head, Oid, RepoSnapshot, RepoState, short};

/// Label of the command, as shown while it runs and in the undo menu.
pub const LABEL: &str = "Interactive rebase";

/// Longest range offered for planning. Nobody arranges more commits than
/// this by hand, and it keeps a slip of the mouse on an ancient commit from
/// loading half the history.
pub const MAX_COMMITS: usize = 2000;

/// Folder in the git directory where a plan waits for `git rebase` to start.
const STAGE_DIR: &str = "gitkrill-rebase";
/// The same folder once it has been copied into git's rebase state.
const STATE_DIR: &str = "rebase-merge/gitkrill";
/// Environment variable carrying the staging folder to the sequence editor.
const STAGE_ENV: &str = "GITKRILL_REBASE_STAGE";

/// Run by git in place of an editor for the todo list (`$1`). Copies the
/// staging folder next to the todo list, installs the prepared list, then
/// removes the staging folder. Git appends the file name to this text and
/// runs it through `sh`, hence the function.
const SEQUENCE_EDITOR: &str = concat!(
    "f() { test -n \"$GITKRILL_REBASE_STAGE\" || return 1; d=\"${1%/*}/gitkrill\"; ",
    "cp -R \"$GITKRILL_REBASE_STAGE\" \"$d\" && cp \"$d/todo\" \"$1\" || return 1; ",
    "rm -rf \"$GITKRILL_REBASE_STAGE\"; return 0; }; f",
);

/// Todo line that remembers where HEAD is before a commit is applied.
const MARK: &str = "exec git rev-parse HEAD >\"$(git rev-parse --git-path rebase-merge/gitkrill/head)\"";

/// Todo line that gives the commit just built message number `n`.
///
/// It does nothing when HEAD has not moved since the matching [`MARK`]: the
/// commit was skipped while resolving a conflict, and amending would rewrite
/// the message of whatever came before it.
fn amend_line(n: usize) -> String {
    format!(
        "exec d=\"$(git rev-parse --git-path {STATE_DIR})\" && read -r h <\"$d/head\" && \
         {{ test \"$h\" = \"$(git rev-parse HEAD)\" || \
         git commit -q --amend --allow-empty --cleanup=verbatim -F \"$d/msg-{n}\"; }}"
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Keep the commit as it is.
    Pick,
    /// Keep the commit, change its message.
    Reword,
    /// Fold into the previous kept commit and combine the messages.
    Squash,
    /// Fold into the previous kept commit and discard this message.
    Fixup,
    /// Remove the commit.
    Drop,
}

impl Action {
    pub const ALL: [Action; 5] =
        [Action::Pick, Action::Reword, Action::Squash, Action::Fixup, Action::Drop];

    pub fn label(self) -> &'static str {
        match self {
            Action::Pick => "Pick",
            Action::Reword => "Reword",
            Action::Squash => "Squash",
            Action::Fixup => "Fixup",
            Action::Drop => "Drop",
        }
    }

    /// One line for the action menu. Worded for the plan list, which shows
    /// the newest commit on top, so the older commit is the one "below".
    pub fn explain(self) -> &'static str {
        match self {
            Action::Pick => "Keep the commit as it is",
            Action::Reword => "Keep the commit, change its message",
            Action::Squash => "Fold into the older commit below and combine the messages",
            Action::Fixup => "Fold into the older commit below and discard this message",
            Action::Drop => "Remove the commit and its changes",
        }
    }

    /// Squash and fixup: the commit disappears into an older one.
    pub fn folds(self) -> bool {
        matches!(self, Action::Squash | Action::Fixup)
    }
}

/// One commit of the range being rebased.
#[derive(Clone, Debug)]
pub struct RangeCommit {
    pub oid: Oid,
    pub summary: String,
    /// The full message, without trailing whitespace.
    pub message: String,
    pub author: String,
    pub email: String,
    pub is_merge: bool,
    /// First parent first, as git records them.
    pub parents: Vec<Oid>,
}

/// The commits an interactive rebase from `base` would replay.
#[derive(Clone, Debug)]
pub struct Range {
    pub base: Oid,
    pub base_summary: String,
    /// HEAD when the range was read; the plan is only valid while it holds.
    pub head: Head,
    /// The commits the plan arranges, oldest first, which is the order git
    /// applies them in: the first parents from HEAD down to the base, merge
    /// commits included. Without merges, simply every commit after the base.
    pub commits: Vec<RangeCommit>,
    /// The commits merges in `commits` brought in from their other side,
    /// oldest first in an order git can replay them in. They are not
    /// planned one by one: see [`Side`].
    pub merged: Vec<RangeCommit>,
    /// What each merge in `commits` brings in, by its index there.
    pub sides: HashMap<usize, Side>,
    /// Why this range cannot be rebased interactively, whatever the plan.
    pub blocker: Option<String>,
    /// A later base from which the range would be acceptable.
    pub suggested_base: Option<Oid>,
}

/// The other side of one merge of the plan.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Side {
    /// Indexes into [`Range::merged`] of every commit the merge's other
    /// parents lead to, oldest first. Kept merges replay them as they are.
    pub reach: Vec<usize>,
    /// Indexes into [`Range::commits`] of the commits those branch off from
    /// (or that the merge has as another parent): the merge has to come
    /// after them.
    pub forks: Vec<usize>,
    /// How many of `reach` no older merge of the line brings in already:
    /// what the merge's row says it brings in.
    pub brings: usize,
}

impl Range {
    pub fn has_merges(&self) -> bool {
        self.commits.iter().any(|c| c.is_merge)
    }
}

fn err(e: git2::Error) -> String {
    e.message().to_owned()
}

fn range_commit(commit: &git2::Commit<'_>) -> RangeCommit {
    let author = commit.author();
    RangeCommit {
        oid: commit.id(),
        summary: commit.summary_bytes().map(lossy).unwrap_or_default(),
        message: lossy(commit.message_bytes()).trim_end().to_owned(),
        author: lossy(author.name_bytes()),
        email: lossy(author.email_bytes()),
        is_merge: commit.parent_count() > 1,
        parents: commit.parent_ids().collect(),
    }
}

fn too_many() -> String {
    format!(
        "There are more than {MAX_COMMITS} commits after this one. Start the interactive rebase \
         from a more recent commit."
    )
}

/// Reads the commits after `base` up to HEAD.
///
/// Fails when there is no such range at all (the commit is not in HEAD's
/// history, or the range is absurdly long). A range that exists but cannot
/// be rebased interactively from this base, because the base came in through
/// a merge rather than lying on the branch's own line, is returned with
/// [`Range::blocker`] set so the UI can show what is in the way.
pub fn load_range(repo: &Repository, base: Oid) -> Result<Range, String> {
    // A plan that was prepared but never started (git refused to begin)
    // leaves its staging folder behind; nothing can still be using it.
    if !in_progress(repo) {
        let _ = std::fs::remove_dir_all(stage_dir(repo));
    }

    let head = read_head(repo);
    let tip = head.oid.ok_or("This branch has no commits yet.")?;
    let base_commit = repo
        .find_commit(base)
        .map_err(|_| format!("Commit {} no longer exists in this repository.", short(base)))?;
    let base_summary = base_commit.summary_bytes().map(lossy).unwrap_or_default();
    let name = head.branch.clone().unwrap_or_else(|| "HEAD".to_owned());
    let not_ancestor = || {
        format!(
            "{} is not part of the history of {name}, so there is nothing after it to rebase.",
            short(base)
        )
    };
    let range = |commits, merged, sides, blocker, suggested_base| Range {
        base,
        base_summary: base_summary.clone(),
        head: head.clone(),
        commits,
        merged,
        sides,
        blocker,
        suggested_base,
    };

    // A range without merges is the first-parent chain from HEAD down to
    // the base, which costs one object read per commit.
    let mut chain = Vec::new();
    let mut has_merge = false;
    let mut at = tip;
    while at != base {
        let commit = repo.find_commit(at).map_err(err)?;
        if commit.parent_count() > 1 {
            has_merge = true;
            break;
        }
        if chain.len() >= MAX_COMMITS {
            return Err(too_many());
        }
        chain.push(range_commit(&commit));
        // Reached the root without meeting the base.
        at = commit.parent_id(0).map_err(|_| not_ancestor())?;
    }
    if !has_merge {
        chain.reverse();
        return Ok(range(chain, Vec::new(), HashMap::new(), None, None));
    }

    // Merges: everything `base..HEAD` contains, parents before children.
    if !repo.graph_descendant_of(tip, base).map_err(err)? {
        return Err(not_ancestor());
    }
    let walk = |sort: Sort| -> Result<git2::Revwalk<'_>, String> {
        let mut walk = repo.revwalk().map_err(err)?;
        walk.set_sorting(sort).map_err(err)?;
        walk.push(tip).map_err(err)?;
        walk.hide(base).map_err(err)?;
        Ok(walk)
    };
    // Unsorted walks stream, so counting first keeps a huge range cheap.
    if walk(Sort::NONE)?.take(MAX_COMMITS + 1).count() > MAX_COMMITS {
        return Err(too_many());
    }
    let mut all = Vec::new();
    for oid in walk(Sort::TOPOLOGICAL | Sort::REVERSE)? {
        let commit = repo.find_commit(oid.map_err(err)?).map_err(err)?;
        all.push(range_commit(&commit));
    }
    let index: HashMap<Oid, usize> = all.iter().enumerate().map(|(i, c)| (c.oid, i)).collect();

    // The branch's own line: first parents from HEAD while in the range.
    let mut line = Vec::new();
    let mut next = Some(tip);
    while let Some(i) = next.and_then(|oid| index.get(&oid).copied()) {
        line.push(i);
        next = all[i].parents.first().copied();
    }
    if next != Some(base) {
        // The base came in through a merge: what follows it is not one
        // line the plan could list. Where the line leaves the range is the
        // nearest base that works, since it comes before this one.
        let blocker = format!(
            "{} is not on the line of {name} itself (the first parents from its tip) but on a              branch merged into it, so the commits after it do not form one line to plan. Start              from a commit on {name}'s own line instead.",
            short(base)
        );
        return Ok(range(all, Vec::new(), HashMap::new(), Some(blocker), next));
    }
    line.reverse();
    Ok(split(all, &line, range))
}

/// Splits `all` (the range, parents first) into the line the plan lists
/// (`line`, indexes into `all`, oldest first) and what its merges bring in.
fn split(
    all: Vec<RangeCommit>,
    line: &[usize],
    range: impl FnOnce(Vec<RangeCommit>, Vec<RangeCommit>, HashMap<usize, Side>, Option<String>, Option<Oid>) -> Range,
) -> Range {
    let on_line: HashMap<Oid, usize> = line.iter().enumerate().map(|(pos, i)| (all[*i].oid, pos)).collect();
    let commits: Vec<RangeCommit> = line.iter().map(|i| all[*i].clone()).collect();
    let merged: Vec<RangeCommit> = all.into_iter().filter(|c| !on_line.contains_key(&c.oid)).collect();
    let in_merged: HashMap<Oid, usize> = merged.iter().enumerate().map(|(i, c)| (c.oid, i)).collect();

    let mut sides = HashMap::new();
    let mut claimed: HashSet<usize> = HashSet::new();
    for (pos, commit) in commits.iter().enumerate().filter(|(_, c)| c.is_merge) {
        let mut reach = HashSet::new();
        let mut forks = HashSet::new();
        let mut stack: Vec<Oid> = commit.parents[1..].to_vec();
        while let Some(oid) = stack.pop() {
            if let Some(&i) = in_merged.get(&oid) {
                if reach.insert(i) {
                    stack.extend(merged[i].parents.iter().copied());
                }
            } else if let Some(&p) = on_line.get(&oid) {
                forks.insert(p);
            }
        }
        let mut reach: Vec<usize> = reach.into_iter().collect();
        reach.sort_unstable();
        let mut forks: Vec<usize> = forks.into_iter().collect();
        forks.sort_unstable();
        let brings = reach.iter().filter(|i| claimed.insert(**i)).count();
        sides.insert(pos, Side { reach, forks, brings });
    }
    range(commits, merged, sides, None, None)
}

/// What happens to one commit of the range.
#[derive(Clone, Debug)]
pub struct Item {
    /// Index of the commit in [`Range::commits`].
    pub source: usize,
    pub action: Action,
    /// The replacement message for a reword, once the user has typed one.
    pub reword: Option<String>,
    /// The message typed for the squash group this commit leads. `None`
    /// means "what git would propose": the messages one after the other.
    pub combined: Option<String>,
}

/// A commit that stays, plus the squash / fixup commits folded into it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    /// Position in [`Plan::items`] of the commit the others fold into.
    pub leader: usize,
    /// Positions of the commits folded into it, oldest first.
    pub members: Vec<usize>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// Commits that come out the way they went in.
    pub kept: usize,
    /// Commits that only get a new message.
    pub reworded: usize,
    /// Commits taking part in a squash or fixup, the surviving ones included.
    pub squashed: usize,
    /// Commits those are squashed into.
    pub squashed_into: usize,
    pub dropped: usize,
    pub reordered: bool,
}

fn commits(n: usize) -> String {
    if n == 1 { "1 commit".to_owned() } else { format!("{n} commits") }
}

impl Summary {
    /// The plan would leave the branch exactly as it is.
    pub fn is_noop(&self) -> bool {
        self.reworded == 0 && self.squashed == 0 && self.dropped == 0 && !self.reordered
    }

    fn parts(&self, with_kept: bool) -> Vec<String> {
        let mut parts = Vec::new();
        if with_kept && self.kept > 0 {
            parts.push(format!("{} kept", commits(self.kept)));
        }
        if self.reworded > 0 {
            parts.push(format!("{} reworded", self.reworded));
        }
        if self.squashed > 0 {
            parts.push(format!("{} squashed into {}", self.squashed, self.squashed_into));
        }
        if self.dropped > 0 {
            parts.push(format!("{} dropped", self.dropped));
        }
        if self.reordered {
            parts.push("order changed".to_owned());
        }
        parts
    }

    /// "3 commits kept, 1 reworded, 2 squashed into 1, 1 dropped".
    pub fn text(&self) -> String {
        self.parts(true).join(", ")
    }

    /// The same without the untouched commits, for the success message.
    pub fn changes(&self) -> String {
        self.parts(false).join(", ")
    }

    /// Title of the success message: what happened, as far as it fits the
    /// one line a toast has room for.
    pub fn headline(&self, branch: &str, total: usize) -> String {
        let full = format!("Rebased {branch}: {}", self.changes());
        if full.chars().count() <= 48 {
            full
        } else {
            format!("Rebased {branch}: {} rewritten as {}", commits(total), self.result())
        }
    }

    /// How many commits the branch has after the base once the plan ran.
    pub fn result(&self) -> usize {
        self.kept + self.reworded + self.squashed_into
    }
}

/// A commit message the way it is stored: Unix line endings, no blank lines
/// before the subject, exactly one newline at the end. The text in between is
/// left exactly as typed.
pub fn normalize(text: &str) -> String {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines = text.lines().skip_while(|line| line.trim().is_empty()).collect::<Vec<_>>().join("\n");
    lines.truncate(lines.trim_end().len());
    if !lines.is_empty() {
        lines.push('\n');
    }
    lines
}

/// The todo list for `git rebase -i` and the message files it refers to.
#[derive(Debug, PartialEq, Eq)]
pub struct Script {
    pub todo: String,
    /// `messages[n - 1]` is the content of `msg-n`.
    pub messages: Vec<String>,
}

/// Writes a [`Script`], keeping track of what the merges need.
struct Todo<'a> {
    range: &'a Range,
    todo: String,
    messages: Vec<String>,
    /// Message number to apply once the current group is complete.
    pending: Option<usize>,
    /// Labels to set once the current group is complete.
    deferred: Vec<String>,
    /// A group (a kept commit and what is folded into it) is being written.
    open: bool,
    /// Rewritten commits that something later starts from or merges, by
    /// the original commit.
    labels: HashMap<Oid, String>,
    /// Merged-in commits already replayed.
    done: HashSet<Oid>,
    /// Every commit of the range that has each commit as a parent, with
    /// which parent it is.
    children: HashMap<Oid, Vec<(Oid, usize)>>,
    serial: usize,
}

impl<'a> Todo<'a> {
    fn new(range: &'a Range) -> Self {
        let mut children: HashMap<Oid, Vec<(Oid, usize)>> = HashMap::new();
        if range.has_merges() {
            for commit in range.commits.iter().chain(&range.merged) {
                for (n, parent) in commit.parents.iter().enumerate() {
                    children.entry(*parent).or_default().push((commit.oid, n));
                }
            }
        }
        Todo {
            range,
            todo: String::new(),
            messages: Vec::new(),
            pending: None,
            deferred: Vec::new(),
            open: false,
            labels: HashMap::new(),
            done: HashSet::new(),
            children,
            serial: 0,
        }
    }

    fn line(&mut self, line: &str) {
        let _ = writeln!(self.todo, "{line}");
    }

    /// Applies the message of the group just written and sets the labels
    /// waiting for it.
    fn close_group(&mut self) {
        if let Some(n) = self.pending.take() {
            let _ = writeln!(self.todo, "{}", amend_line(n));
        }
        for label in std::mem::take(&mut self.deferred) {
            self.line(&format!("label {label}"));
        }
        self.open = false;
    }

    fn new_label(&mut self, kind: &str) -> String {
        self.serial += 1;
        format!("gitkrill-{kind}-{}", self.serial)
    }

    /// Labels what HEAD is once the current group is complete as the
    /// rewritten `oid`.
    fn label_line(&mut self, oid: Oid) {
        let label = self.new_label("fork");
        self.labels.insert(oid, label.clone());
        if self.open {
            self.deferred.push(label);
        } else {
            self.line(&format!("label {label}"));
        }
    }

    /// How the todo list names the rewritten `oid`: its label, or the commit
    /// itself when it is not rewritten (it is outside the range).
    fn name(&self, oid: Oid) -> String {
        self.labels.get(&oid).cloned().unwrap_or_else(|| oid.to_string())
    }

    /// Replays what the merge at `source` brings in that is not replayed
    /// yet, then comes back to the line.
    fn side_of(&mut self, source: usize) {
        let range = self.range;
        let Some(side) = range.sides.get(&source) else { return };
        let todo: Vec<&RangeCommit> =
            side.reach.iter().map(|i| &range.merged[*i]).filter(|c| !self.done.contains(&c.oid)).collect();
        if todo.is_empty() {
            return;
        }
        let back = self.new_label("line");
        self.line(&format!("label {back}"));
        let mut head: Option<Oid> = None;
        for (k, commit) in todo.iter().enumerate() {
            if let Some(&first) = commit.parents.first()
                && head != Some(first)
            {
                let line = format!("reset {}", self.name(first));
                self.line(&line);
            }
            if commit.is_merge {
                self.merge(commit);
            } else {
                self.line(&format!("pick {}", commit.oid));
            }
            self.done.insert(commit.oid);
            head = Some(commit.oid);
            // Labelled when anything but the next commit, as its first
            // parent, builds on it.
            let next = todo.get(k + 1).map(|c| c.oid);
            let needed = self.children.get(&commit.oid).is_some_and(|children| {
                children.iter().any(|(child, n)| !(Some(*child) == next && *n == 0))
            });
            if needed {
                let label = self.new_label("side");
                self.line(&format!("label {label}"));
                self.labels.insert(commit.oid, label);
            }
        }
        self.line(&format!("reset {back}"));
    }

    /// Makes `commit` again as a merge of HEAD and its other parents, with
    /// its own message.
    fn merge(&mut self, commit: &RangeCommit) {
        let mut line = format!("merge -C {}", commit.oid);
        for parent in &commit.parents[1..] {
            line.push(' ');
            line.push_str(&self.name(*parent));
        }
        // Git shows the rest of the line when the merge stops on conflicts.
        let summary: String = commit.summary.chars().filter(|c| !c.is_control()).collect();
        if !summary.is_empty() {
            line.push_str(" # ");
            line.push_str(&summary);
        }
        self.line(&line);
    }

    fn finish(mut self) -> Script {
        if let Some(n) = self.pending.take() {
            let _ = writeln!(self.todo, "{}", amend_line(n));
        }
        // What dropped merges brought in is listed too (see `drop` above).
        for commit in &self.range.merged {
            if !self.done.contains(&commit.oid) {
                let _ = writeln!(self.todo, "drop {}", commit.oid);
            }
        }
        Script { todo: self.todo, messages: self.messages }
    }
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub range: Arc<Range>,
    /// Oldest first, like the todo list: a permutation of the range.
    pub items: Vec<Item>,
}

impl Plan {
    /// Every commit picked, in its original order.
    pub fn new(range: Arc<Range>) -> Self {
        let items = (0..range.commits.len())
            .map(|source| Item { source, action: Action::Pick, reword: None, combined: None })
            .collect();
        Plan { range, items }
    }

    pub fn commit(&self, pos: usize) -> &RangeCommit {
        &self.range.commits[self.items[pos].source]
    }

    /// The kept commits with what is folded into each, oldest first.
    /// A squash or fixup with nothing before it belongs to no group; see
    /// [`Plan::problems`].
    pub fn groups(&self) -> Vec<Group> {
        let mut groups: Vec<Group> = Vec::new();
        for (pos, item) in self.items.iter().enumerate() {
            match item.action {
                Action::Pick | Action::Reword => groups.push(Group { leader: pos, members: Vec::new() }),
                Action::Squash | Action::Fixup => {
                    if let Some(group) = groups.last_mut() {
                        group.members.push(pos);
                    }
                }
                Action::Drop => {}
            }
        }
        groups
    }

    /// Whether the group has a squash, which is what makes git ask for a
    /// combined message (fixups alone keep the first message).
    pub fn has_squash(&self, group: &Group) -> bool {
        group.members.iter().any(|pos| self.items[*pos].action == Action::Squash)
    }

    /// The message the group's first commit has on its own: the reworded
    /// text if it is being reworded, its original message otherwise.
    fn own_message(&self, leader: usize) -> String {
        let item = &self.items[leader];
        match (&item.reword, item.action) {
            (Some(text), Action::Reword) => normalize(text),
            _ => normalize(&self.commit(leader).message),
        }
    }

    /// What git proposes for a squash: the first message followed by the
    /// message of every squashed (not fixup) commit, a blank line between.
    pub fn default_combined(&self, group: &Group) -> String {
        let mut text = self.own_message(group.leader).trim_end().to_owned();
        for pos in &group.members {
            if self.items[*pos].action == Action::Squash {
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(self.commit(*pos).message.trim());
            }
        }
        normalize(&text)
    }

    /// The message the commit built from `group` ends up with.
    pub fn message(&self, group: &Group) -> String {
        if self.has_squash(group) {
            match &self.items[group.leader].combined {
                Some(text) => normalize(text),
                None => self.default_combined(group),
            }
        } else {
            self.own_message(group.leader)
        }
    }

    /// The message to write onto the group's commit, or `None` when picking
    /// the first commit (and fixing the rest up into it) already yields it.
    fn new_message(&self, group: &Group) -> Option<String> {
        let message = self.message(group);
        (message != normalize(&self.commit(group.leader).message)).then_some(message)
    }

    pub fn summary(&self) -> Summary {
        let mut summary = Summary::default();
        for group in self.groups() {
            if !group.members.is_empty() {
                summary.squashed += group.members.len() + 1;
                summary.squashed_into += 1;
            } else if self.new_message(&group).is_some() {
                summary.reworded += 1;
            } else {
                summary.kept += 1;
            }
        }
        summary.dropped = self.items.iter().filter(|i| i.action == Action::Drop).count();
        // Where dropped commits sit makes no difference to the result.
        let kept: Vec<usize> =
            self.items.iter().filter(|i| i.action != Action::Drop).map(|i| i.source).collect();
        summary.reordered = kept.windows(2).any(|pair| pair[0] > pair[1]);
        summary
    }

    /// Everything that makes the plan itself impossible to run, in plain
    /// language. Empty when it can start (given a clean working tree; see
    /// [`tree_blocker`]).
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if let Some(blocker) = &self.range.blocker {
            problems.push(blocker.clone());
            return problems;
        }
        if self.items.is_empty() {
            problems.push("There are no commits after this one to rebase.".to_owned());
            return problems;
        }

        let first_kept = self.items.iter().position(|item| item.action != Action::Drop);
        match first_kept {
            None => problems.push(
                "Every commit is dropped, so nothing would be left to rebase. To throw all of \
                 them away, reset the branch to the base commit instead."
                    .to_owned(),
            ),
            Some(pos) if self.items[pos].action.folds() => {
                let commit = self.commit(pos);
                problems.push(format!(
                    "{} \"{}\" is the oldest commit that is kept, so there is no older commit to \
                     {} it into. Pick it, or move it above the commit it should be folded into.",
                    short(commit.oid),
                    commit.summary,
                    self.items[pos].action.label().to_lowercase(),
                ));
            }
            Some(_) => {}
        }

        for (pos, item) in self.items.iter().enumerate() {
            if let Some(reason) = self.refusal(pos, item.action) {
                let commit = self.commit(pos);
                problems.push(format!("{} \"{}\": {reason}", short(commit.oid), commit.summary));
            }
        }
        problems.extend(self.order_problems());

        for group in self.groups() {
            // Only a message the plan writes: a commit that already has an
            // empty one may keep it.
            if self.new_message(&group).is_some_and(|message| message.is_empty()) {
                let commit = self.commit(group.leader);
                problems.push(format!(
                    "The new message for {} \"{}\" is empty. Type a message, or pick the commit \
                     to keep its original one.",
                    short(commit.oid),
                    commit.summary,
                ));
            }
        }
        problems
    }

    /// Why the commit at `pos` cannot be given `action`, or `None` when it
    /// can. Only merges refuse anything: git makes them again as merges, so
    /// they cannot be folded into an older commit.
    pub fn refusal(&self, pos: usize, action: Action) -> Option<&'static str> {
        (self.commit(pos).is_merge && action.folds()).then_some(MERGE_FOLDS)
    }

    /// Merges moved below a commit their other side branches off from.
    fn order_problems(&self) -> Vec<String> {
        let at: HashMap<usize, usize> = self.items.iter().enumerate().map(|(pos, item)| (item.source, pos)).collect();
        let mut problems = Vec::new();
        for (pos, item) in self.items.iter().enumerate() {
            let Some(side) = self.range.sides.get(&item.source) else { continue };
            if item.action == Action::Drop {
                continue;
            }
            for fork in &side.forks {
                if at.get(fork).is_some_and(|fork_pos| *fork_pos > pos) {
                    let (merge, fork) = (self.commit(pos), &self.range.commits[*fork]);
                    problems.push(format!(
                        "{} \"{}\" merges commits that branch off from {} \"{}\", so it has to stay \
                         above that commit.",
                        short(merge.oid),
                        merge.summary,
                        short(fork.oid),
                        fork.summary,
                    ));
                }
            }
        }
        problems
    }

    /// How many kept merges the plan makes again, because something below
    /// them changes: a merge that needed conflicts resolved stops for them
    /// again. (Merges whose parents stay the same are kept as they are.)
    pub fn remade_merges(&self) -> usize {
        let changed = |pos: usize, item: &Item| {
            item.source != pos
                || match item.action {
                    Action::Pick => false,
                    Action::Reword => item.reword.as_ref().is_some_and(|text| {
                        normalize(text) != normalize(&self.range.commits[item.source].message)
                    }),
                    _ => true,
                }
        };
        let Some(first) = self.items.iter().enumerate().position(|(pos, item)| changed(pos, item)) else {
            return 0;
        };
        self.items
            .iter()
            .skip(first + 1)
            .filter(|item| item.action != Action::Drop && self.range.commits[item.source].is_merge)
            .count()
    }

    /// The todo list and message files that carry out the plan.
    pub fn script(&self) -> Script {
        let groups = self.groups();
        let mut out = Todo::new(&self.range);
        // Commits of the line that kept merges branch off from: each gets a
        // label once its group is complete, for the merged side to start
        // from.
        let forks: HashSet<usize> = self
            .items
            .iter()
            .filter(|item| item.action != Action::Drop)
            .filter_map(|item| self.range.sides.get(&item.source))
            .flat_map(|side| side.forks.iter().copied())
            .collect();

        let mut next_group = 0;
        for (pos, item) in self.items.iter().enumerate() {
            let commit = self.commit(pos);
            let sha = commit.oid;
            match item.action {
                Action::Pick | Action::Reword => {
                    out.close_group();
                    let group = &groups[next_group];
                    next_group += 1;
                    if commit.is_merge {
                        out.side_of(item.source);
                    }
                    if let Some(message) = self.new_message(group) {
                        out.messages.push(message);
                        out.pending = Some(out.messages.len());
                        out.line(MARK);
                    }
                    if commit.is_merge {
                        out.merge(commit);
                    } else {
                        out.line(&format!("pick {sha}"));
                    }
                    out.open = true;
                }
                // The combined message is applied afterwards in one go, so
                // squashes need no message handling of their own here.
                Action::Squash | Action::Fixup => out.line(&format!("fixup {sha}")),
                // Listed rather than left out, so git's check for commits
                // missing from the list (rebase.missingCommitsCheck) is happy.
                Action::Drop => out.line(&format!("drop {sha}")),
            }
            if forks.contains(&item.source) {
                out.label_line(sha);
            }
        }
        out.finish()
    }

    /// Checks the plan against the repository as it is right now, writes the
    /// todo list and messages to the staging folder and returns the command
    /// that runs the rebase.
    ///
    /// Touches the disk: call it from a reader-thread job, not from UI code.
    pub fn prepare(&self, repo: &Repository) -> Result<GitCmd, String> {
        if let Some(problem) = self.problems().into_iter().next() {
            return Err(problem);
        }
        if in_progress(repo) {
            return Err("Another operation is in progress. Finish or abort it first.".to_owned());
        }
        let head = read_head(repo);
        if head != self.range.head {
            return Err("The branch has moved since this plan was made. Reload the commits and \
                        plan again."
                .to_owned());
        }
        let mut options = StatusOptions::new();
        options.include_untracked(false).include_ignored(false).exclude_submodules(true);
        if !repo.statuses(Some(&mut options)).map_err(err)?.is_empty() {
            return Err(DIRTY.to_owned());
        }

        let script = self.script();
        let stage = stage_dir(repo);
        let _ = std::fs::remove_dir_all(&stage);
        let write = |name: String, text: &str| {
            std::fs::write(stage.join(&name), text)
                .map_err(|e| format!("Could not write {}: {e}", stage.join(&name).display()))
        };
        std::fs::create_dir_all(&stage)
            .map_err(|e| format!("Could not create {}: {e}", stage.display()))?;
        write("todo".to_owned(), &script.todo)?;
        for (i, message) in script.messages.iter().enumerate() {
            write(format!("msg-{}", i + 1), message)?;
        }

        let name = head.branch.clone().unwrap_or_else(|| "HEAD".to_owned());
        // --reschedule-failed-exec: when a message cannot be applied (a
        // commit-msg hook rejects it, say) the rebase stops, and continuing
        // tries that line again instead of silently moving past it. Git
        // remembers the option for the rest of the rebase.
        //
        // --quiet and no advice: toasts show the last line git printed, and
        // without these that is a progress counter for every step run
        // together on one line, or a paragraph about terminal commands,
        // rather than the conflict or the hook's complaint. The "Rebase in
        // progress" banner offers those commands as buttons.
        let mut args: Vec<String> =
            ["-c", "advice.mergeConflict=false", "rebase", "-i", "--quiet", "--reschedule-failed-exec"]
                .map(str::to_owned)
                .to_vec();
        // The todo list makes the merges again either way; this tells git
        // the rebase is one that keeps them.
        if self.range.has_merges() {
            args.push("--rebase-merges".to_owned());
        }
        args.push(self.range.base.to_string());
        let step = Step::new(args, None)
            .env("GIT_SEQUENCE_EDITOR", SEQUENCE_EDITOR)
            // Forward slashes: the value is read by git's POSIX shell tools.
            .env(STAGE_ENV, stage.to_string_lossy().replace('\\', "/"));

        let mut cmd = GitCmd::empty(LABEL)
            .undoable(UndoKind::HeadMove)
            .done(self.summary().headline(&name, self.items.len()));
        cmd.steps.push(step);
        // Never replay a plan onto a branch that moved after it was checked.
        cmd.expect_head = Some(head);
        Ok(cmd)
    }
}

const MERGE_FOLDS: &str = "a merge commit cannot be squashed or fixed up into another commit, because \
                           the rebase makes it again as a merge. Keep, reword or drop it instead.";

const DIRTY: &str = "There are uncommitted changes to tracked files. Commit or stash them before \
                     rebasing: the rebase rewrites the files in the working tree.";

fn stage_dir(repo: &Repository) -> PathBuf {
    repo.path().join(STAGE_DIR)
}

fn in_progress(repo: &Repository) -> bool {
    repo.state() != RepositoryState::Clean
}

/// Why the working tree, as last seen, does not allow a rebase to start.
///
/// Untracked files are fine (git leaves them alone); changes to tracked
/// files are refused rather than stashed automatically, because re-applying
/// them onto rewritten commits can conflict long after the user stopped
/// thinking about them.
pub fn tree_blocker(snap: &RepoSnapshot) -> Option<String> {
    if snap.state != RepoState::Clean {
        let what = snap.state.label().unwrap_or("Another operation is in progress");
        return Some(format!("{what}. Finish or abort it before starting an interactive rebase."));
    }
    let tracked = |files: &[super::FileChange]| {
        files.iter().any(|f| {
            f.kind != ChangeKind::Untracked && !snap.submodules.iter().any(|s| s.path == f.path)
        })
    };
    (tracked(&snap.status.staged) || tracked(&snap.status.unstaged)).then(|| DIRTY.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::ops;
    use crate::tests::{Fixture, git};
    use crate::worker::Origin;

    // ---- planning, without a repository ------------------------------------

    fn oid(n: u8) -> Oid {
        Oid::from_bytes(&[n; 20]).unwrap()
    }

    fn range(messages: &[&str]) -> Arc<Range> {
        let commits = messages
            .iter()
            .enumerate()
            .map(|(i, message)| RangeCommit {
                oid: oid(i as u8 + 1),
                summary: message.lines().next().unwrap_or_default().to_owned(),
                message: (*message).to_owned(),
                author: "A".to_owned(),
                email: "a@example.com".to_owned(),
                is_merge: false,
                parents: vec![oid(i as u8)],
            })
            .collect();
        Arc::new(Range {
            base: oid(0),
            base_summary: "base".to_owned(),
            head: Head { branch: Some("main".to_owned()), oid: Some(oid(99)), detached: false },
            commits,
            merged: Vec::new(),
            sides: HashMap::new(),
            blocker: None,
            suggested_base: None,
        })
    }

    fn plan(messages: &[&str], actions: &[Action]) -> Plan {
        let mut plan = Plan::new(range(messages));
        for (item, action) in plan.items.iter_mut().zip(actions) {
            item.action = *action;
        }
        plan
    }

    use Action::{Drop, Fixup, Pick, Reword, Squash};

    #[test]
    fn messages_are_normalised_only_at_the_edges() {
        assert_eq!(normalize("\n \nSubject  \r\n\r\n  indented\n\n\n# kept\n\n  \n"), "Subject  \n\n  indented\n\n\n# kept\n");
        assert_eq!(normalize("  \n\n"), "");
        assert_eq!(normalize("one"), "one\n");
    }

    #[test]
    fn the_shell_snippets_agree_on_names() {
        assert!(SEQUENCE_EDITOR.contains(&format!("\"${STAGE_ENV}\"")));
        assert!(MARK.contains(&format!("{STATE_DIR}/head")));
        assert!(STATE_DIR.ends_with("/gitkrill") && SEQUENCE_EDITOR.contains("/gitkrill\""));
    }

    #[test]
    fn an_untouched_plan_is_a_no_op_made_of_picks() {
        let plan = plan(&["a", "b"], &[]);
        assert!(plan.summary().is_noop());
        assert!(plan.problems().is_empty());
        let script = plan.script();
        assert_eq!(script.todo, format!("pick {}\npick {}\n", oid(1), oid(2)));
        assert!(script.messages.is_empty());
    }

    #[test]
    fn a_reword_marks_head_then_amends_with_the_typed_message() {
        let mut plan = plan(&["a", "b"], &[Reword, Pick]);
        // Reword chosen but nothing typed yet: still a no-op.
        assert!(plan.summary().is_noop());
        plan.items[0].reword = Some("a\n".to_owned());
        assert!(plan.summary().is_noop());

        plan.items[0].reword = Some("new a\n\nbody".to_owned());
        assert_eq!(plan.summary(), Summary { kept: 1, reworded: 1, ..Default::default() });
        let script = plan.script();
        assert_eq!(script.messages, ["new a\n\nbody\n"]);
        assert_eq!(
            script.todo,
            format!("{MARK}\npick {}\n{}\npick {}\n", oid(1), amend_line(1), oid(2))
        );
    }

    #[test]
    fn a_squash_group_proposes_the_messages_git_would_combine() {
        let mut plan = plan(&["a\n\nbody a", "b", "c", "d"], &[Pick, Squash, Fixup, Squash]);
        let groups = plan.groups();
        assert_eq!(groups, [Group { leader: 0, members: vec![1, 2, 3] }]);
        // The fixup's message is left out, as git does.
        assert_eq!(plan.default_combined(&groups[0]), "a\n\nbody a\n\nb\n\nd\n");
        assert_eq!(plan.summary(), Summary { squashed: 4, squashed_into: 1, ..Default::default() });

        plan.items[0].combined = Some("all four".to_owned());
        let script = plan.script();
        assert_eq!(script.messages, ["all four\n"]);
        assert_eq!(
            script.todo,
            format!(
                "{MARK}\npick {}\nfixup {}\nfixup {}\nfixup {}\n{}\n",
                oid(1),
                oid(2),
                oid(3),
                oid(4),
                amend_line(1)
            )
        );
    }

    #[test]
    fn fixups_alone_need_no_message_handling() {
        let plan = plan(&["a", "b", "c"], &[Pick, Fixup, Drop]);
        assert_eq!(plan.summary(), Summary { squashed: 2, squashed_into: 1, dropped: 1, ..Default::default() });
        assert_eq!(plan.summary().text(), "2 squashed into 1, 1 dropped");
        assert_eq!(plan.summary().result(), 1);
        let script = plan.script();
        assert_eq!(script.todo, format!("pick {}\nfixup {}\ndrop {}\n", oid(1), oid(2), oid(3)));
        assert!(script.messages.is_empty());
    }

    #[test]
    fn a_reworded_leader_feeds_the_combined_message() {
        let mut plan = plan(&["a", "b"], &[Reword, Squash]);
        plan.items[0].reword = Some("better a".to_owned());
        let group = &plan.groups()[0];
        assert_eq!(plan.default_combined(group), "better a\n\nb\n");
        // With a fixup instead, the reworded text is the whole message.
        plan.items[1].action = Fixup;
        assert_eq!(plan.message(&plan.groups()[0]), "better a\n");
    }

    #[test]
    fn the_summary_reads_like_a_sentence() {
        let mut plan = plan(
            &["a", "b", "c", "d", "e", "f", "g"],
            &[Pick, Pick, Reword, Pick, Squash, Drop, Pick],
        );
        plan.items[2].reword = Some("C".to_owned());
        assert_eq!(plan.summary().text(), "3 commits kept, 1 reworded, 2 squashed into 1, 1 dropped");
        assert_eq!(plan.summary().changes(), "1 reworded, 2 squashed into 1, 1 dropped");
        assert_eq!(plan.summary().result(), 5);

        plan.items.swap(0, 1);
        assert!(plan.summary().reordered);
        assert!(plan.summary().text().ends_with(", order changed"));
    }

    #[test]
    fn moving_a_dropped_commit_is_not_a_reorder() {
        let mut plan = plan(&["a", "b", "c"], &[Pick, Drop, Pick]);
        plan.items.swap(1, 2);
        assert!(!plan.summary().reordered);
    }

    #[test]
    fn impossible_plans_are_explained() {
        let all_dropped = plan(&["a", "b"], &[Drop, Drop]);
        assert!(all_dropped.problems()[0].contains("Every commit is dropped"));

        let orphan = plan(&["a", "b", "c"], &[Drop, Squash, Pick]);
        let problems = orphan.problems();
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("oldest commit that is kept"), "{problems:?}");
        assert!(problems[0].contains("squash it into"), "{problems:?}");

        let mut empty = plan(&["a", "b"], &[Pick, Reword]);
        empty.items[1].reword = Some(" \n ".to_owned());
        assert!(empty.problems()[0].contains("is empty"));

        let mut empty_group = plan(&["a", "b"], &[Pick, Squash]);
        empty_group.items[0].combined = Some(String::new());
        assert!(empty_group.problems()[0].contains("is empty"));

        // A commit whose message was empty to begin with can still be picked.
        let odd = plan(&["", "b"], &[Pick, Pick]);
        assert!(odd.problems().is_empty());

        let none = Plan::new(range(&[]));
        assert!(none.problems()[0].contains("no commits"));
    }

    // ---- against real repositories -----------------------------------------

    /// A repository with a base commit and one commit per entry of `files`
    /// on top, each adding `<name>.txt`. Returns the base.
    fn repo_with(fx: &Fixture, names: &[&str]) -> Oid {
        fx.write("base.txt", "base\n");
        fx.commit_all("base");
        let base = fx.snapshot().head.oid.unwrap();
        for name in names {
            fx.write(&format!("{name}.txt"), &format!("{name}\n"));
            fx.commit_all(&format!("add {name}\n\nbody of {name}"));
        }
        base
    }

    fn load(fx: &Fixture, base: Oid) -> Plan {
        let range = fx.job(move |loader| load_range(loader.repo(), base)).expect("range loads");
        Plan::new(Arc::new(range))
    }

    fn prepare(fx: &Fixture, plan: &Plan) -> Result<GitCmd, String> {
        let plan = plan.clone();
        fx.job(move |loader| plan.prepare(loader.repo()))
    }

    /// Full messages from HEAD back to (not including) `base`, newest first.
    fn log(fx: &Fixture, base: Oid) -> Vec<String> {
        let out = git(&fx.dir, &["log", "-z", "--format=%B", &format!("{base}..HEAD")]);
        out.split('\0').map(|m| m.trim_end().to_owned()).filter(|m| !m.is_empty()).collect()
    }

    /// Files in the tree of `rev`.
    fn tree(fx: &Fixture, rev: &str) -> Vec<String> {
        git(&fx.dir, &["ls-tree", "-r", "--name-only", rev]).lines().map(str::to_owned).collect()
    }

    fn assert_no_leftovers(fx: &Fixture) {
        let gitdir = fx.dir.join(".git");
        assert!(!gitdir.join(STAGE_DIR).exists(), "staging folder left behind");
        assert!(!gitdir.join("rebase-merge").exists(), "rebase state left behind");
    }

    #[test]
    fn the_range_is_listed_oldest_first_with_full_messages() {
        let fx = Fixture::new("rebase-range");
        let base = repo_with(&fx, &["one", "two", "three"]);
        let plan = load(&fx, base);
        let range = &plan.range;
        assert_eq!(range.base_summary, "base");
        assert_eq!(range.head.branch.as_deref(), Some("main"));
        let summaries: Vec<&str> = range.commits.iter().map(|c| c.summary.as_str()).collect();
        assert_eq!(summaries, ["add one", "add two", "add three"]);
        assert_eq!(range.commits[0].message, "add one\n\nbody of one");
        assert_eq!(range.commits[0].author, "Test User");
        assert!(range.blocker.is_none());

        // HEAD itself as the base: an empty range, which the plan refuses.
        let head = fx.snapshot().head.oid.unwrap();
        let empty = load(&fx, head);
        assert!(empty.items.is_empty());
        assert!(prepare(&fx, &empty).unwrap_err().contains("no commits"));
    }

    #[test]
    fn reword_writes_the_message_exactly_as_typed() {
        let fx = Fixture::new("rebase-reword");
        let base = repo_with(&fx, &["one", "two", "three"]);
        let tip = fx.snapshot().head.oid.unwrap();
        let first = git(&fx.dir, &["rev-parse", "HEAD~2"]);

        let typed = "Reword \"two\" – naïve 'quotes' $HOME `tick` \\n %s\n\n\
                     # not a comment\n\tindented \"body\"\n\n\nline after two blank lines\n日本語";
        let mut plan = load(&fx, base);
        plan.items[1].action = Action::Reword;
        plan.items[1].reword = Some(typed.to_owned());
        assert_eq!(plan.summary().text(), "2 commits kept, 1 reworded");

        let done = fx.run(prepare(&fx, &plan).unwrap());
        // Nothing for the success toast to trip over (see `prepare`).
        assert_eq!(done.result.as_deref(), Ok(""));
        assert_ne!(done.snap.head.oid, Some(tip));
        assert_eq!(done.snap.head.branch.as_deref(), Some("main"));
        assert_eq!(done.snap.state, RepoState::Clean);
        assert_eq!(log(&fx, base), ["add three\n\nbody of three", typed, "add one\n\nbody of one"]);
        // Byte for byte, with exactly one newline at the end.
        let raw = git(&fx.dir, &["cat-file", "commit", "HEAD~1"]);
        assert!(raw.ends_with(typed), "{raw}");
        // Commits before the first change are not rewritten at all.
        assert_eq!(git(&fx.dir, &["rev-parse", "HEAD~2"]), first);
        // The trees are what they were.
        assert_eq!(tree(&fx, "HEAD"), ["base.txt", "one.txt", "three.txt", "two.txt"]);
        assert_eq!(tree(&fx, "HEAD~1"), ["base.txt", "one.txt", "two.txt"]);
        assert_eq!(git(&fx.dir, &["log", "-1", "--format=%an", "HEAD~1"]), "Test User");
        assert_no_leftovers(&fx);
    }

    #[test]
    fn squash_uses_the_edited_message_and_fixup_keeps_the_first() {
        let fx = Fixture::new("rebase-squash");
        let base = repo_with(&fx, &["one", "two", "three", "four", "five"]);

        let mut plan = load(&fx, base);
        // one <- two (squash), three <- four (fixup), five untouched.
        plan.items[1].action = Action::Squash;
        plan.items[3].action = Action::Fixup;
        let groups = plan.groups();
        assert_eq!(
            plan.default_combined(&groups[0]),
            "add one\n\nbody of one\n\nadd two\n\nbody of two\n"
        );
        plan.items[0].combined = Some("One and two together\n\n- \"quoted\"\n- ünïcödé".to_owned());
        assert_eq!(plan.summary().text(), "1 commit kept, 4 squashed into 2");

        fx.run(prepare(&fx, &plan).unwrap());
        assert_eq!(
            log(&fx, base),
            [
                "add five\n\nbody of five",
                "add three\n\nbody of three",
                "One and two together\n\n- \"quoted\"\n- ünïcödé",
            ]
        );
        assert_eq!(tree(&fx, "HEAD~2"), ["base.txt", "one.txt", "two.txt"]);
        assert_eq!(tree(&fx, "HEAD~1"), ["base.txt", "four.txt", "one.txt", "three.txt", "two.txt"]);
        assert_eq!(fx.read("five.txt"), "five\n");
        assert_no_leftovers(&fx);
    }

    #[test]
    fn a_squash_left_alone_gets_the_combined_default() {
        let fx = Fixture::new("rebase-squash-default");
        let base = repo_with(&fx, &["one", "two"]);
        let mut plan = load(&fx, base);
        plan.items[1].action = Action::Squash;
        fx.run(prepare(&fx, &plan).unwrap());
        assert_eq!(log(&fx, base), ["add one\n\nbody of one\n\nadd two\n\nbody of two"]);
        assert_eq!(tree(&fx, "HEAD"), ["base.txt", "one.txt", "two.txt"]);
    }

    #[test]
    fn drop_removes_the_commit_and_its_changes() {
        let fx = Fixture::new("rebase-drop");
        let base = repo_with(&fx, &["one", "two", "three"]);
        let mut plan = load(&fx, base);
        plan.items[1].action = Action::Drop;

        let done = fx.run(prepare(&fx, &plan).unwrap());
        assert_eq!(log(&fx, base), ["add three\n\nbody of three", "add one\n\nbody of one"]);
        assert_eq!(tree(&fx, "HEAD"), ["base.txt", "one.txt", "three.txt"]);
        assert!(!fx.dir.join("two.txt").exists());
        assert!(done.snap.status.is_clean());
    }

    #[test]
    fn reorder_replays_the_commits_in_the_new_order() {
        let fx = Fixture::new("rebase-reorder");
        let base = repo_with(&fx, &["one", "two", "three"]);
        let mut plan = load(&fx, base);
        plan.items.rotate_left(1); // two, three, one
        assert_eq!(plan.summary().text(), "3 commits kept, order changed");

        fx.run(prepare(&fx, &plan).unwrap());
        assert_eq!(
            log(&fx, base),
            ["add one\n\nbody of one", "add three\n\nbody of three", "add two\n\nbody of two"]
        );
        assert_eq!(tree(&fx, "HEAD~2"), ["base.txt", "two.txt"]);
        assert_eq!(tree(&fx, "HEAD~1"), ["base.txt", "three.txt", "two.txt"]);
        assert_eq!(tree(&fx, "HEAD"), ["base.txt", "one.txt", "three.txt", "two.txt"]);
    }

    #[test]
    fn a_combined_plan_can_be_undone_and_redone() {
        let fx = Fixture::new("rebase-combined");
        let base = repo_with(&fx, &["one", "two", "three", "four", "five", "six"]);
        let tip = fx.snapshot().head.oid.unwrap();

        // Newest first after the rebase: six+two (squash, edited), five
        // (reworded), four with three fixed up into it... built as:
        //   four, three(fixup), five(reword), six, two(squash), one(drop)
        let mut plan = load(&fx, base);
        let items: [Item; 6] = std::mem::take(&mut plan.items).try_into().unwrap();
        let [mut one, mut two, mut three, four, mut five, mut six] = items;
        one.action = Action::Drop;
        two.action = Action::Squash;
        three.action = Action::Fixup;
        five.action = Action::Reword;
        five.reword = Some("Five, reworded".to_owned());
        six.combined = Some("Six with two".to_owned());
        plan.items = vec![four, three, five, six, two, one];
        assert_eq!(plan.summary().text(), "1 reworded, 4 squashed into 2, 1 dropped, order changed");
        assert_eq!(plan.summary().result(), 3);

        let cmd = prepare(&fx, &plan).unwrap();
        // Too much for one line of a toast, so it is boiled down.
        assert_eq!(cmd.success.as_deref(), Some("Rebased main: 6 commits rewritten as 3"));
        assert_eq!(
            Summary { reworded: 1, dropped: 2, ..Default::default() }.headline("main", 5),
            "Rebased main: 1 reworded, 2 dropped"
        );
        let done = fx.run(cmd);
        let rebased = done.snap.head.oid.unwrap();
        assert_eq!(log(&fx, base), ["Six with two", "Five, reworded", "add four\n\nbody of four"]);
        assert_eq!(tree(&fx, "HEAD~2"), ["base.txt", "four.txt", "three.txt"]);
        assert_eq!(tree(&fx, "HEAD~1"), ["base.txt", "five.txt", "four.txt", "three.txt"]);
        assert_eq!(
            tree(&fx, "HEAD"),
            ["base.txt", "five.txt", "four.txt", "six.txt", "three.txt", "two.txt"]
        );
        assert!(!fx.dir.join("one.txt").exists());
        assert_no_leftovers(&fx);

        // The toolbar's Undo puts the branch back on the original commits.
        let entry = done.undo.expect("the rebase is undoable");
        assert_eq!(entry.label, LABEL);
        let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry.clone()));
        undone.result.expect("undo works");
        assert_eq!(undone.snap.head.oid, Some(tip));
        assert_eq!(undone.snap.head.branch.as_deref(), Some("main"));
        assert!(undone.snap.status.is_clean());
        assert_eq!(fx.read("one.txt"), "one\n");
        assert_eq!(log(&fx, base).len(), 6);

        let redone = fx.run_as(entry.redo.clone(), Origin::Redo(entry));
        redone.result.expect("redo works");
        assert_eq!(redone.snap.head.oid, Some(rebased));
    }

    /// Three commits that each rewrite the same line, so any reorder conflicts.
    fn conflicting(fx: &Fixture) -> Oid {
        fx.write("f.txt", "base\n");
        fx.write("other.txt", "other\n");
        fx.commit_all("base");
        let base = fx.snapshot().head.oid.unwrap();
        for name in ["one", "two", "three"] {
            fx.write("f.txt", &format!("{name}\n"));
            fx.write(&format!("{name}.txt"), &format!("{name}\n"));
            fx.commit_all(&format!("set {name}"));
        }
        base
    }

    fn resolve(fx: &Fixture, content: &str) {
        fx.write("f.txt", content);
        git(&fx.dir, &["add", "f.txt"]);
    }

    #[test]
    fn a_plan_that_conflicts_carries_on_after_continue() {
        let fx = Fixture::new("rebase-conflict");
        let base = conflicting(&fx);
        let tip = fx.snapshot().head.oid.unwrap();

        // two, one (reworded), three (reworded): "two" no longer applies
        // cleanly, and neither does what follows it.
        let mut plan = load(&fx, base);
        plan.items.swap(0, 1);
        plan.items[1].action = Action::Reword;
        plan.items[1].reword = Some("ONE after the conflict".to_owned());
        plan.items[2].action = Action::Reword;
        plan.items[2].reword = Some("THREE at the end\n\nwith a body".to_owned());

        let stopped = fx.run_as(prepare(&fx, &plan).unwrap(), Origin::User);
        let message = stopped.result.expect_err("the rebase stops on the conflict");
        assert!(message.contains("CONFLICT"), "{message}");
        assert!(!message.contains("Rebasing ("), "{message}");
        assert_eq!(stopped.snap.state, RepoState::Rebase);
        assert!(stopped.undo.is_none());
        assert!(stopped.snap.status.unstaged.iter().any(|f| f.kind == ChangeKind::Conflicted));
        // The plan now lives in git's own state folder.
        assert!(!fx.dir.join(".git").join(STAGE_DIR).exists());
        assert!(fx.dir.join(".git/rebase-merge/gitkrill/msg-2").exists());

        // Exactly what the banner's Continue button runs, as often as needed.
        let continue_cmd = || ops::continue_op(RepoState::Rebase).unwrap();
        resolve(&fx, "two\n");
        let again = fx.run_as(continue_cmd(), Origin::User);
        assert!(again.result.is_err(), "the next commit conflicts too");
        assert_eq!(again.snap.state, RepoState::Rebase);
        resolve(&fx, "one\n");
        let third = fx.run_as(continue_cmd(), Origin::User);
        assert!(third.result.is_err());
        resolve(&fx, "three\n");
        let done = fx.run(continue_cmd());

        assert_eq!(done.snap.state, RepoState::Clean);
        assert_eq!(done.snap.head.branch.as_deref(), Some("main"));
        assert_ne!(done.snap.head.oid, Some(tip));
        assert_eq!(
            log(&fx, base),
            ["THREE at the end\n\nwith a body", "ONE after the conflict", "set two"]
        );
        assert_eq!(fx.read("f.txt"), "three\n");
        assert_eq!(git(&fx.dir, &["show", "HEAD~1:f.txt"]), "one");
        assert_eq!(git(&fx.dir, &["show", "HEAD~2:f.txt"]), "two");
        assert_eq!(tree(&fx, "HEAD~2"), ["f.txt", "other.txt", "two.txt"]);
        assert_eq!(tree(&fx, "HEAD"), ["f.txt", "one.txt", "other.txt", "three.txt", "two.txt"]);
        assert!(done.snap.status.is_clean());
        assert_no_leftovers(&fx);

        // Finished after three stops, it is still undone in one step.
        let entry = done.undo.expect("the finished plan is undoable");
        assert_eq!(entry.label, LABEL);
        let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry));
        undone.result.expect("undo works");
        assert_eq!(undone.snap.head.oid, Some(tip));
        assert_eq!(fx.read("f.txt"), "three\n");
        assert_eq!(log(&fx, base), ["set three", "set two", "set one"]);
    }

    #[test]
    fn aborting_a_stopped_plan_restores_everything() {
        let fx = Fixture::new("rebase-abort");
        let base = conflicting(&fx);
        let tip = fx.snapshot().head.oid.unwrap();
        let mut plan = load(&fx, base);
        plan.items.swap(0, 1);
        plan.items[2].action = Action::Reword;
        plan.items[2].reword = Some("never applied".to_owned());

        let stopped = fx.run_as(prepare(&fx, &plan).unwrap(), Origin::User);
        assert!(stopped.result.is_err());
        let done = fx.run(ops::abort(RepoState::Rebase).unwrap());
        assert_eq!(done.snap.head.oid, Some(tip));
        assert_eq!(done.snap.state, RepoState::Clean);
        assert!(done.snap.status.is_clean());
        assert_no_leftovers(&fx);
    }

    #[test]
    fn a_skipped_commit_does_not_give_its_message_to_another() {
        let fx = Fixture::new("rebase-skip");
        let base = conflicting(&fx);
        // two, one (reworded), three: skip "one" when it conflicts.
        let mut plan = load(&fx, base);
        plan.items.swap(0, 1);
        plan.items[1].action = Action::Reword;
        plan.items[1].reword = Some("must not land on another commit".to_owned());

        assert!(fx.run_as(prepare(&fx, &plan).unwrap(), Origin::User).result.is_err());
        resolve(&fx, "two\n");
        assert!(fx.run_as(ops::continue_op(RepoState::Rebase).unwrap(), Origin::User).result.is_err());
        // "one" is in conflict now; give up on it, as `git rebase --skip` in
        // a terminal would. "three" then applies cleanly on top of "two".
        let done = fx.run(GitCmd::new("Skip", &["rebase", "--skip"]));
        assert_eq!(done.snap.state, RepoState::Clean);

        assert_eq!(log(&fx, base), ["set three", "set two"]);
        assert_eq!(fx.read("f.txt"), "three\n");
        assert_no_leftovers(&fx);
    }

    #[test]
    fn hooks_run_and_a_rejected_message_is_retried_not_lost() {
        let fx = Fixture::new("rebase-hook");
        let base = repo_with(&fx, &["one", "two"]);
        let tip = fx.snapshot().head.oid.unwrap();
        // Rejects any message mentioning "wip" until the marker file exists.
        fx.write(
            ".git/hooks/commit-msg",
            "#!/bin/sh\nif grep -q wip \"$1\" && ! test -f .git/allow-wip; then\n  echo 'no wip commits' >&2\n  exit 1\nfi\n",
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let hook = fx.dir.join(".git/hooks/commit-msg");
            std::fs::set_permissions(hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let mut plan = load(&fx, base);
        plan.items[0].action = Action::Reword;
        plan.items[0].reword = Some("wip one".to_owned());

        let stopped = fx.run_as(prepare(&fx, &plan).unwrap(), Origin::User);
        let message = stopped.result.expect_err("the hook rejects the message");
        assert!(message.contains("no wip commits"), "{message}");
        assert_eq!(stopped.snap.state, RepoState::Rebase);

        // Continuing does not silently drop the reword: it is tried again.
        let again = fx.run_as(ops::continue_op(RepoState::Rebase).unwrap(), Origin::User);
        assert!(again.result.is_err());
        assert_eq!(again.snap.state, RepoState::Rebase);

        fx.write(".git/allow-wip", "");
        let done = fx.run(ops::continue_op(RepoState::Rebase).unwrap());
        assert_eq!(done.snap.state, RepoState::Clean);
        assert_ne!(done.snap.head.oid, Some(tip));
        assert_eq!(log(&fx, base), ["add two\n\nbody of two", "wip one"]);
        assert_no_leftovers(&fx);
    }

    /// base - one - main work - Merge side - after the merge, where "side
    /// work" branched off from "one" and was merged. Returns the base.
    fn with_merge(fx: &Fixture) -> Oid {
        let base = repo_with(fx, &["one"]);
        git(&fx.dir, &["checkout", "-q", "-b", "side"]);
        fx.write("side file.txt", "side\n");
        fx.commit_all("side work");
        git(&fx.dir, &["checkout", "-q", "main"]);
        fx.write("main.txt", "main\n");
        fx.commit_all("main work");
        git(&fx.dir, &["merge", "-q", "--no-ff", "-m", "Merge side\n\nwith a body", "side"]);
        fx.write("after.txt", "after\n");
        fx.commit_all("after the merge");
        base
    }

    fn rev(fx: &Fixture, name: &str) -> String {
        git(&fx.dir, &["rev-parse", name])
    }

    /// Each commit from HEAD down to `base` as "subject <- parent + parent"
    /// (by subject), sorted.
    fn shape(fx: &Fixture, base: Oid) -> Vec<String> {
        let out = git(&fx.dir, &["log", "--topo-order", "--format=%s|%p", &format!("{base}..HEAD")]);
        let name = |sha: &str| {
            if sha.is_empty() {
                return String::new();
            }
            git(&fx.dir, &["log", "-1", "--format=%s", sha])
        };
        let mut lines: Vec<String> = out
            .lines()
            .map(|line| {
                let (subject, parents) = line.split_once('|').unwrap();
                let parents: Vec<String> = parents.split(' ').map(name).collect();
                format!("{subject} <- {}", parents.join(" + "))
            })
            .collect();
        lines.sort();
        lines
    }

    fn sorted<const N: usize>(lines: [&str; N]) -> Vec<String> {
        let mut lines: Vec<String> = lines.iter().map(|l| (*l).to_owned()).collect();
        lines.sort();
        lines
    }

    #[test]
    fn a_merge_is_a_row_and_what_it_brought_in_is_kept_with_it() {
        let fx = Fixture::new("rebase-merge-range");
        let base = with_merge(&fx);
        let plan = load(&fx, base);
        let range = &plan.range;
        assert!(range.blocker.is_none());
        let line: Vec<&str> = range.commits.iter().map(|c| c.summary.as_str()).collect();
        assert_eq!(line, ["add one", "main work", "Merge side", "after the merge"]);
        assert!(range.commits[2].is_merge && range.has_merges());
        let merged: Vec<&str> = range.merged.iter().map(|c| c.summary.as_str()).collect();
        assert_eq!(merged, ["side work"]);
        // "side work" branched off from "add one", so the merge must stay above it.
        assert_eq!(range.sides[&2], Side { reach: vec![0], forks: vec![0], brings: 1 });
        assert!(plan.problems().is_empty());
        assert_eq!(plan.refusal(2, Action::Squash), Some(MERGE_FOLDS));
        assert_eq!(plan.refusal(2, Action::Reword), None);
        assert_eq!(plan.refusal(3, Action::Squash), None);
    }

    #[test]
    fn rewording_above_a_merge_leaves_the_merge_alone() {
        let fx = Fixture::new("rebase-merge-above");
        let base = with_merge(&fx);
        let merge = rev(&fx, "HEAD~1");
        let mut plan = load(&fx, base);
        plan.items[3].action = Action::Reword;
        plan.items[3].reword = Some("after, reworded".to_owned());
        assert_eq!(plan.remade_merges(), 0);
        let cmd = prepare(&fx, &plan).unwrap();
        assert!(cmd.steps[0].args.iter().any(|a| a == "--rebase-merges"));
        fx.run(cmd);
        // The merge and everything under it were not rewritten at all.
        assert_eq!(rev(&fx, "HEAD~1"), merge);
        assert_eq!(git(&fx.dir, &["log", "-1", "--format=%s"]), "after, reworded");
        assert_no_leftovers(&fx);
        assert!(git(&fx.dir, &["for-each-ref", "refs/rewritten"]).is_empty());
    }

    #[test]
    fn rewording_below_a_merge_replays_its_side_and_merges_again() {
        let fx = Fixture::new("rebase-merge-below");
        let base = with_merge(&fx);
        let tip = fx.snapshot().head.oid.unwrap();
        let mut plan = load(&fx, base);
        plan.items[0].action = Action::Reword;
        plan.items[0].reword = Some("one, reworded".to_owned());
        plan.items[2].action = Action::Reword;
        plan.items[2].reword = Some("Merge side, reworded".to_owned());
        assert_eq!(plan.remade_merges(), 1);
        let done = fx.run(prepare(&fx, &plan).unwrap());
        assert_eq!(
            shape(&fx, base),
            sorted([
                "after the merge <- Merge side, reworded",
                "Merge side, reworded <- main work + side work",
                "side work <- one, reworded",
                "main work <- one, reworded",
                "one, reworded <- base",
            ])
        );
        assert_eq!(tree(&fx, "HEAD"), ["after.txt", "base.txt", "main.txt", "one.txt", "side file.txt"]);
        assert_no_leftovers(&fx);
        assert!(git(&fx.dir, &["for-each-ref", "refs/rewritten"]).is_empty());
        // The side branch itself still points at the old commits.
        assert_eq!(git(&fx.dir, &["log", "-1", "--format=%s", "side~1"]), "add one");

        let entry = done.undo.expect("undoable");
        let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry));
        undone.result.expect("undo works");
        assert_eq!(undone.snap.head.oid, Some(tip));
    }

    #[test]
    fn dropping_a_merge_drops_what_it_brought_in() {
        let fx = Fixture::new("rebase-merge-drop");
        let base = with_merge(&fx);
        let mut plan = load(&fx, base);
        plan.items[2].action = Action::Drop;
        assert_eq!(plan.summary().text(), "3 commits kept, 1 dropped");
        let script = plan.script();
        let side = plan.range.merged[0].oid;
        assert!(script.todo.ends_with(&format!("drop {side}\n")), "{}", script.todo);
        fx.run(prepare(&fx, &plan).unwrap());
        assert_eq!(shape(&fx, base), sorted(["after the merge <- main work", "main work <- add one", "add one <- base"]));
        assert!(!fx.dir.join("side file.txt").exists());
        assert_no_leftovers(&fx);
    }

    #[test]
    fn commits_fold_into_a_merge_and_move_around_it() {
        let fx = Fixture::new("rebase-merge-fold");
        let base = with_merge(&fx);
        let mut plan = load(&fx, base);
        // "after the merge" folded into the merge; "main work" moved below "one".
        plan.items[3].action = Action::Fixup;
        plan.items.swap(0, 1);
        assert!(plan.problems().is_empty(), "{:?}", plan.problems());
        fx.run(prepare(&fx, &plan).unwrap());
        assert_eq!(
            shape(&fx, base),
            sorted([
                "Merge side <- add one + side work",
                "side work <- add one",
                "add one <- main work",
                "main work <- base",
            ])
        );
        assert_eq!(git(&fx.dir, &["log", "-1", "--format=%B"]), "Merge side\n\nwith a body");
        assert_eq!(tree(&fx, "HEAD"), ["after.txt", "base.txt", "main.txt", "one.txt", "side file.txt"]);
        assert_no_leftovers(&fx);
    }

    #[test]
    fn squashing_where_a_merged_branch_began_moves_it_onto_the_result() {
        let fx = Fixture::new("rebase-merge-squash-fork");
        let base = with_merge(&fx);
        // "main work" squashed into "add one", which "side work" started from.
        let mut plan = load(&fx, base);
        let run = [plan.range.commits[0].oid, plan.range.commits[1].oid];
        assert_eq!(crate::git::multi::squash_plan(&mut plan, &run), Some(0));
        plan.items[0].combined = Some("one and main".to_owned());
        let script = plan.script();
        // The label for the side to start from comes after the new message.
        let amend = script.todo.find("--amend").unwrap();
        let label = script.todo.find("label gitkrill-fork").unwrap();
        assert!(label > amend, "{}", script.todo);
        fx.run(prepare(&fx, &plan).unwrap());
        assert_eq!(
            shape(&fx, base),
            sorted([
                "after the merge <- Merge side",
                "Merge side <- one and main + side work",
                "side work <- one and main",
                "one and main <- base",
            ])
        );
        assert_no_leftovers(&fx);
    }

    #[test]
    fn a_merge_cannot_go_below_where_its_side_began_or_fold_away() {
        let fx = Fixture::new("rebase-merge-limits");
        let base = with_merge(&fx);
        let mut plan = load(&fx, base);
        // Merge side below "add one", which side work branched off from.
        let merge = plan.items.remove(2);
        plan.items.insert(0, merge);
        let problems = plan.problems();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("\"Merge side\" merges commits that branch off from"), "{problems:?}");
        assert!(problems[0].contains("\"add one\""), "{problems:?}");
        assert!(prepare(&fx, &plan).is_err());
        // Dropped, it no longer needs that commit.
        plan.items[0].action = Action::Drop;
        assert!(plan.problems().is_empty());

        let mut plan = load(&fx, base);
        plan.items[2].action = Action::Squash;
        assert!(plan.problems()[0].contains("cannot be squashed or fixed up"), "{:?}", plan.problems());
    }

    #[test]
    fn a_merge_made_again_stops_on_its_conflicts_and_carries_on() {
        let fx = Fixture::new("rebase-merge-conflict");
        fx.write("f.txt", "base\n");
        fx.commit_all("base");
        let base = fx.snapshot().head.oid.unwrap();
        git(&fx.dir, &["checkout", "-q", "-b", "side"]);
        fx.write("f.txt", "side\n");
        fx.commit_all("side work");
        git(&fx.dir, &["checkout", "-q", "main"]);
        fx.write("f.txt", "main\n");
        fx.commit_all("main work");
        let _ = std::process::Command::new("git").current_dir(&fx.dir).args(["merge", "-q", "side"]).output();
        fx.write("f.txt", "resolved by hand\n");
        git(&fx.dir, &["add", "f.txt"]);
        git(&fx.dir, &["commit", "-q", "-m", "Merge side"]);
        fx.write("g.txt", "g\n");
        fx.commit_all("after");
        let tip = fx.snapshot().head.oid.unwrap();

        let mut plan = load(&fx, base);
        plan.items[0].action = Action::Reword;
        plan.items[0].reword = Some("main work, reworded".to_owned());
        let stopped = fx.run_as(prepare(&fx, &plan).unwrap(), Origin::User);
        let message = stopped.result.expect_err("the merge conflicts again");
        assert!(message.contains("CONFLICT"), "{message}");
        assert_eq!(stopped.snap.state, RepoState::Rebase);

        resolve(&fx, "resolved again\n");
        let done = fx.run(ops::continue_op(RepoState::Rebase).unwrap());
        assert_eq!(done.snap.state, RepoState::Clean);
        assert_eq!(
            shape(&fx, base),
            sorted(["after <- Merge side", "Merge side <- main work, reworded + side work", "main work, reworded <- base", "side work <- base"])
        );
        assert_eq!(fx.read("f.txt"), "resolved again\n");
        assert_no_leftovers(&fx);

        let entry = done.undo.expect("undoable once finished");
        let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry));
        undone.result.expect("undo works");
        assert_eq!(undone.snap.head.oid, Some(tip));
        assert_eq!(fx.read("f.txt"), "resolved by hand\n");
    }

    #[test]
    fn a_base_that_came_in_through_a_merge_is_refused_with_a_way_out() {
        let fx = Fixture::new("rebase-merge-base");
        with_merge(&fx);
        let side = Oid::from_str(&rev(&fx, "side")).unwrap();
        let one = Oid::from_str(&rev(&fx, "side~1")).unwrap();
        let tip = fx.snapshot().head.oid.unwrap();

        let plan = load(&fx, side);
        let blocker = plan.range.blocker.as_deref().expect("not on the line");
        assert!(blocker.contains("not on the line of main itself"), "{blocker}");
        assert_eq!(plan.range.suggested_base, Some(one));
        assert_eq!(plan.problems(), [blocker.to_owned()]);
        assert_eq!(prepare(&fx, &plan).unwrap_err(), blocker);
        assert_eq!(fx.snapshot().head.oid, Some(tip));
        // From the suggestion, the merge is planned as usual.
        assert!(load(&fx, one).range.blocker.is_none());

        // A commit that is not in HEAD's history at all.
        git(&fx.dir, &["checkout", "-q", "--orphan", "elsewhere"]);
        fx.commit_all("unrelated root");
        let unrelated = fx.job(move |loader| load_range(loader.repo(), side)).unwrap_err();
        assert!(unrelated.contains("not part of the history of elsewhere"), "{unrelated}");
    }

    #[test]
    fn merges_on_the_merged_side_and_merged_twice_are_replayed_too() {
        // main:  base - a - M1(side at s1) - b - M2(side at s3)
        // side:  base - s1 - M(main at a) - s3
        let fx = Fixture::new("rebase-merge-twice");
        fx.write("base.txt", "base\n");
        fx.commit_all("base");
        let base = fx.snapshot().head.oid.unwrap();
        git(&fx.dir, &["checkout", "-q", "-b", "side"]);
        fx.write("s1.txt", "s1\n");
        fx.commit_all("s1");
        git(&fx.dir, &["checkout", "-q", "main"]);
        fx.write("a.txt", "a\n");
        fx.commit_all("a");
        git(&fx.dir, &["checkout", "-q", "side"]);
        git(&fx.dir, &["merge", "-q", "--no-ff", "-m", "side takes main", "main"]);
        fx.write("s3.txt", "s3\n");
        fx.commit_all("s3");
        git(&fx.dir, &["checkout", "-q", "main"]);
        git(&fx.dir, &["merge", "-q", "--no-ff", "-m", "M1", "side~2"]);
        fx.write("b.txt", "b\n");
        fx.commit_all("b");
        git(&fx.dir, &["merge", "-q", "--no-ff", "-m", "M2", "side"]);

        let mut plan = load(&fx, base);
        let line: Vec<&str> = plan.range.commits.iter().map(|c| c.summary.as_str()).collect();
        assert_eq!(line, ["a", "M1", "b", "M2"]);
        assert_eq!(plan.range.sides[&1].brings, 1);
        assert_eq!(plan.range.sides[&3].brings, 2);
        // "side takes main" merged "a": M2 has to stay above it as well.
        assert_eq!(plan.range.sides[&3].forks, [0]);

        plan.items[0].action = Action::Reword;
        plan.items[0].reword = Some("A".to_owned());
        fx.run(prepare(&fx, &plan).unwrap());
        assert_eq!(
            shape(&fx, base),
            sorted([
                "M2 <- b + s3",
                "s3 <- side takes main",
                "side takes main <- s1 + A",
                "b <- M1",
                "M1 <- A + s1",
                "A <- base",
                "s1 <- base",
            ])
        );
        assert_eq!(tree(&fx, "HEAD"), ["a.txt", "b.txt", "base.txt", "s1.txt", "s3.txt"]);
        assert_no_leftovers(&fx);
    }

    #[test]
    fn a_dirty_tree_or_a_moved_branch_stops_the_plan_before_git_runs() {
        let fx = Fixture::new("rebase-dirty");
        let base = repo_with(&fx, &["one", "two"]);
        let tip = fx.snapshot().head.oid.unwrap();
        let mut plan = load(&fx, base);
        plan.items[1].action = Action::Drop;

        // Untracked files do not matter; tracked changes do.
        fx.write("untracked.txt", "new\n");
        let snap = fx.snapshot();
        assert_eq!(tree_blocker(&snap), None);
        fx.write("one.txt", "edited\n");
        let snap = fx.snapshot();
        assert!(tree_blocker(&snap).unwrap().contains("uncommitted changes"));
        assert!(prepare(&fx, &plan).unwrap_err().contains("uncommitted changes"));
        assert_eq!(fx.snapshot().head.oid, Some(tip));
        assert!(!fx.dir.join(".git").join(STAGE_DIR).exists());

        // A commit made elsewhere after the plan was loaded.
        fx.commit_all("meanwhile");
        assert!(prepare(&fx, &plan).unwrap_err().contains("has moved"));

        // ... or between preparing the plan and running it.
        let plan = {
            let mut plan = load(&fx, base);
            plan.items[2].action = Action::Drop;
            plan
        };
        let cmd = prepare(&fx, &plan).unwrap();
        fx.write("late.txt", "late\n");
        fx.commit_all("just before the rebase ran");
        let late = fx.snapshot().head.oid;
        let refused = fx.run_as(cmd, Origin::User);
        assert!(refused.result.unwrap_err().contains("moved on"));
        assert_eq!(refused.snap.head.oid, late);
        // The plan that never started is cleared away the next time a
        // range is loaded.
        assert!(fx.dir.join(".git").join(STAGE_DIR).exists());
        load(&fx, base);
        assert_no_leftovers(&fx);
    }

    #[test]
    fn a_linked_worktree_keeps_the_plan_in_its_own_git_directory() {
        let main = Fixture::new("rebase-worktree-main");
        repo_with(&main, &["one"]);
        // A second working tree, whose git directory lives inside the first
        // repository's `.git/worktrees`, not under its own folder.
        let dir = main.dir.with_file_name(format!("rebase worktree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        git(&main.dir, &["worktree", "add", "-q", "-b", "side", dir.to_str().unwrap()]);
        let handle = crate::worker::RepoHandle::spawn(dir.clone(), eframe::egui::Context::default(), 1000);
        let fx = Fixture { dir, handle };
        let base = fx.snapshot().head.oid.unwrap();
        fx.write("two.txt", "two\n");
        fx.commit_all("add two");
        fx.write("three.txt", "three\n");
        fx.commit_all("add three");

        let mut plan = load(&fx, base);
        plan.items[0].action = Action::Reword;
        plan.items[0].reword = Some("two, from a worktree".to_owned());
        plan.items[1].action = Action::Squash;
        let done = fx.run(prepare(&fx, &plan).unwrap());
        assert_eq!(done.snap.head.branch.as_deref(), Some("side"));
        assert_eq!(log(&fx, base), ["two, from a worktree\n\nadd three"]);
        assert_eq!(tree(&fx, "HEAD"), ["base.txt", "one.txt", "three.txt", "two.txt"]);

        let gitdir = PathBuf::from(git(&fx.dir, &["rev-parse", "--absolute-git-dir"]));
        assert!(gitdir.parent().is_some_and(|p| p.ends_with("worktrees")), "{}", gitdir.display());
        assert!(!gitdir.join(STAGE_DIR).exists());
        assert!(!gitdir.join("rebase-merge").exists());
        // The first working tree was not involved.
        assert_eq!(main.snapshot().head.oid, Some(base));
    }

    #[test]
    fn awkward_repository_paths_and_untracked_files_do_not_matter() {
        // The fixture's folder name ends up inside the staging path.
        let fx = Fixture::new("rebase-päth with 'quotes' & (spaces)");
        let base = repo_with(&fx, &["one", "two"]);
        fx.write("left alone.txt", "untracked\n");
        let mut plan = load(&fx, base);
        plan.items[0].action = Action::Reword;
        plan.items[0].reword = Some("it's \"one\"".to_owned());
        plan.items[1].action = Action::Squash;
        fx.run(prepare(&fx, &plan).unwrap());
        assert_eq!(log(&fx, base), ["it's \"one\"\n\nadd two\n\nbody of two"]);
        assert_eq!(fx.read("left alone.txt"), "untracked\n");
        assert_no_leftovers(&fx);
    }
}
