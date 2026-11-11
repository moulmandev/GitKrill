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
//! * Merges (`--rebase-merges`): the range is split into lines of history
//!   ([`Line`], see [`arrange`]): the branch's own line, the first parents
//!   from HEAD down to the base, and for each merge one line per parent
//!   after the first, the commits it brings in. Each line is planned like
//!   the branch's own: its commits are kept, reworded, dropped, folded into
//!   each other and reordered within it. The todo list replays each side
//!   from where it starts (`reset`), labels where it ends, and makes the
//!   merge again from there with its own message (`merge -C`). Commits and
//!   merges whose parents did not change are not rewritten at all (git
//!   fast-forwards over them). Dropping a merge drops what it brings in;
//!   see [`Plan::refusal`] and [`Plan::problems`] for what is not allowed.
//! * A merge squashed or fixed up into the commit before it is made as
//!   usual, then taken back off with its tree kept, and that commit is
//!   amended with it ([`FOLD`]): its changes, and those of the commits it
//!   brings in, become part of that commit, and the merged-in commits are
//!   no longer in the history. Git's todo list has no way to fold a merge.
//! * A base that came in through a merge, rather than lying on the
//!   branch's own line: the plan starts at the oldest merge of that line
//!   that has the base in its history. That merge's first parent, and the
//!   line below it, stay as they are; the todo list starts by resetting to
//!   it ([`load_range`]).
//! * Paths never appear in a command line. The staging folder's name
//!   travels in an environment variable and the shell snippets find git's
//!   folders themselves, so spaces, quotes and backslashes need no escaping.

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
/// Environment variable naming the staging folder for the sequence editor.
const STAGE_ENV: &str = "GITKRILL_REBASE_STAGE";

/// Run by git in place of an editor for the todo list (`$1`, in git's
/// `rebase-merge` folder). Copies the staging folder, which sits next to
/// that folder in the git directory, next to the todo list, installs the
/// prepared list, then removes the staging folder. Git appends the file
/// name to this text and runs it through `sh`, hence the function.
const SEQUENCE_EDITOR: &str = concat!(
    "f() { test -n \"$GITKRILL_REBASE_STAGE\" || return 1; s=\"${1%/*}/../$GITKRILL_REBASE_STAGE\"; ",
    "d=\"${1%/*}/gitkrill\"; cp -R \"$s\" \"$d\" && cp \"$d/todo\" \"$1\" || return 1; ",
    "rm -rf \"$s\"; return 0; }; f",
);

/// Todo line that remembers where HEAD is before the commit that gets
/// message `n` is applied.
fn mark_line(n: usize) -> String {
    format!("exec git rev-parse HEAD >\"$(git rev-parse --git-path {STATE_DIR}/head-{n})\"")
}

/// Todo line that gives the commit just built message number `n`.
///
/// It does nothing when HEAD has not moved since the matching
/// [`mark_line`]: the commit was skipped while resolving a conflict, and
/// amending would rewrite the message of whatever came before it.
fn amend_line(n: usize) -> String {
    format!(
        "exec d=\"$(git rev-parse --git-path {STATE_DIR})\" && read -r h <\"$d/head-{n}\" && \
         {{ test \"$h\" = \"$(git rev-parse HEAD)\" || \
         git commit -q --amend --allow-empty --cleanup=verbatim -F \"$d/msg-{n}\"; }}"
    )
}

/// Todo line that remembers where HEAD is before a merge that [`FOLD`]
/// folds into the commit before it.
const MERGE_MARK: &str = "exec git rev-parse HEAD >\"$(git rev-parse --git-path rebase-merge/gitkrill/merge-head)\"";

/// Todo line, after a merge squashed or fixed up into the commit before
/// it: takes the merge back off with `reset --soft`, which keeps its tree
/// staged, and amends that commit with it, keeping that commit's message,
/// author and parents. Hooks do not run, as for git's own fixups.
///
/// Only when HEAD is a merge whose first parent is where [`MERGE_MARK`]
/// left HEAD: a merge skipped while resolving a conflict, or left out by
/// git for having nothing left to merge, must not take the commit before it
/// away. When the line was cut short after the reset (a signature that
/// failed, say) and runs again, the tree is still staged on that commit and
/// only the amend is left to do.
const FOLD: &str = concat!(
    "exec d=\"$(git rev-parse --git-path rebase-merge/gitkrill)\" && read -r h <\"$d/merge-head\" && ",
    "if test -n \"$(git rev-parse -q --verify HEAD^2)\" && test \"$(git rev-parse HEAD^1)\" = \"$h\"; ",
    "then git reset -q --soft HEAD^1 && git commit -q --amend --no-edit --no-verify --allow-empty --cleanup=verbatim; ",
    "elif test \"$(git rev-parse HEAD)\" = \"$h\" && ! git diff --cached --quiet; ",
    "then git commit -q --amend --no-edit --no-verify --allow-empty --cleanup=verbatim; fi",
);

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

/// One line of history in a range: commits each of which is the first
/// parent of the next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// Indexes into [`Range::commits`], oldest first.
    pub commits: Vec<usize>,
    /// What the oldest of them starts from (its first parent); `None` for a
    /// commit without parents.
    pub start: Option<Oid>,
    /// For the commits a merge brings in: the merge (an index into
    /// [`Range::commits`]) and which of its parents (1 for the second) is
    /// the newest of them. `None` for the branch's own line.
    pub merge: Option<(usize, usize)>,
}

/// The commits an interactive rebase from `base` would replay.
#[derive(Clone, Debug)]
pub struct Range {
    pub base: Oid,
    pub base_summary: String,
    /// HEAD when the range was read; the plan is only valid while it holds.
    pub head: Head,
    /// Every commit the plan arranges, parents before children. Without
    /// merges, simply every commit after the base, oldest first.
    pub commits: Vec<RangeCommit>,
    /// The lines the commits fall into, each commit on exactly one (see
    /// [`arrange`]). `lines[0]` is the branch's own line, the first parents
    /// from HEAD down to the base (or, for a base that came in through a
    /// merge, down to that merge: see [`load_range`]); the others are what
    /// merges bring in.
    pub lines: Vec<Line>,
    /// The line each commit is on.
    pub line_of: Vec<usize>,
    /// The lines each merge brings in, by its parents after the first.
    pub sides: HashMap<usize, Vec<usize>>,
    index: HashMap<Oid, usize>,
}

impl Range {
    fn new(base: Oid, base_summary: String, head: Head, commits: Vec<RangeCommit>, lines: Vec<Line>, line_of: Vec<usize>) -> Range {
        let index = commits.iter().enumerate().map(|(i, c)| (c.oid, i)).collect();
        let mut sides: HashMap<usize, Vec<usize>> = HashMap::new();
        for (l, line) in lines.iter().enumerate() {
            if let Some((merge, _)) = line.merge {
                sides.entry(merge).or_default().push(l);
            }
        }
        Range { base, base_summary, head, commits, lines, line_of, sides, index }
    }

    pub fn has_merges(&self) -> bool {
        self.commits.iter().any(|c| c.is_merge)
    }

    pub fn index_of(&self, oid: Oid) -> Option<usize> {
        self.index.get(&oid).copied()
    }

    /// The line merge `commit` brings in through its parent `n`, if that
    /// parent leads to commits no line has yet.
    pub fn side(&self, commit: usize, n: usize) -> Option<usize> {
        self.sides.get(&commit)?.iter().copied().find(|l| self.lines[*l].merge == Some((commit, n)))
    }

    /// How many commits merge `commit` brings in: those of its sides and
    /// of the merges on them.
    pub fn brought_in(&self, commit: usize) -> usize {
        self.sides.get(&commit).into_iter().flatten().map(|l| self.size(*l)).sum()
    }

    /// How many commits `line` has, with those its merges bring in.
    pub fn size(&self, line: usize) -> usize {
        let commits = &self.lines[line].commits;
        commits.len() + commits.iter().map(|c| self.brought_in(*c)).sum::<usize>()
    }

    /// Whether the branch's own line starts below the base, the base
    /// having come in through a merge.
    pub fn starts_below_base(&self) -> bool {
        self.lines[0].start != Some(self.base)
    }
}

/// Splits the commits of a range into lines (see [`Line`]). `parents[i]`
/// are the parents of commit `i`, `index` finds a commit of the range by
/// its id, `main` is the branch's own line, oldest first, and `start` what
/// it starts from. Returns the lines and the line of each commit.
///
/// Each merge, oldest first along its line, gets a line for each of its
/// parents after the first: that parent and its first parents, as far as
/// they are in the range and on no line yet. The lines of one merge, and
/// those of the merges on them, are made before the next merge's. That is
/// the order the todo list replays them in, so every commit a line starts
/// from or merges is on a line made before it, or earlier on its own.
/// The result does not depend on the order of `parents`.
pub(crate) fn arrange(parents: &[&[Oid]], index: &HashMap<Oid, usize>, main: Vec<usize>, start: Option<Oid>) -> (Vec<Line>, Vec<usize>) {
    let mut line_of = vec![usize::MAX; parents.len()];
    for c in &main {
        line_of[*c] = 0;
    }
    let mut lines = vec![Line { commits: main, start, merge: None }];
    sides_of_line(0, parents, index, &mut lines, &mut line_of);
    (lines, line_of)
}

fn sides_of_line(line: usize, parents: &[&[Oid]], index: &HashMap<Oid, usize>, lines: &mut Vec<Line>, line_of: &mut [usize]) {
    let mut pos = 0;
    while pos < lines[line].commits.len() {
        let merge = lines[line].commits[pos];
        for (n, parent) in parents[merge].iter().enumerate().skip(1) {
            let id = lines.len();
            let mut chain = Vec::new();
            let mut at = Some(*parent);
            while let Some(i) = at.and_then(|oid| index.get(&oid).copied()).filter(|i| line_of[*i] == usize::MAX) {
                line_of[i] = id;
                chain.push(i);
                at = parents[i].first().copied();
            }
            if chain.is_empty() {
                continue;
            }
            chain.reverse();
            lines.push(Line { commits: chain, start: at, merge: Some((merge, n)) });
            sides_of_line(id, parents, index, lines, line_of);
        }
        pos += 1;
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
/// Fails when there is no such range (the commit is not in HEAD's history,
/// or the range is absurdly long).
///
/// A base on the branch's own line (its first parents from HEAD) starts
/// everything after it. A base that came in through a merge has no such
/// line below it: the plan then starts at the oldest merge of the branch's
/// line that has the base in its history, the line below that merge stays
/// as it is, and what the merge brings in is planned from the base on
/// (the rest of its side too, as far as the line below does not have it).
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
        let n = chain.len();
        let lines = vec![Line { commits: (0..n).collect(), start: Some(base), merge: None }];
        return Ok(Range::new(base, base_summary, head, chain, lines, vec![0; n]));
    }

    // Merges: everything `base..HEAD` contains (from where the branch's
    // line starts), parents before children.
    if !repo.graph_descendant_of(tip, base).map_err(err)? {
        return Err(not_ancestor());
    }
    let start = line_start(repo, tip, base)?;
    let walk = |sort: Sort| -> Result<git2::Revwalk<'_>, String> {
        let mut walk = repo.revwalk().map_err(err)?;
        walk.set_sorting(sort).map_err(err)?;
        walk.push(tip).map_err(err)?;
        walk.hide(base).map_err(err)?;
        if start != base {
            walk.hide(start).map_err(err)?;
        }
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
    let mut main = Vec::new();
    let mut next = Some(tip);
    while let Some(i) = next.and_then(|oid| index.get(&oid).copied()) {
        main.push(i);
        next = all[i].parents.first().copied();
    }
    main.reverse();
    let parents: Vec<&[Oid]> = all.iter().map(|c| c.parents.as_slice()).collect();
    let (lines, line_of) = arrange(&parents, &index, main, Some(start));
    Ok(Range::new(base, base_summary, head, all, lines, line_of))
}

/// Where the branch's own line starts for a rebase from `base`: the base
/// when the first parents from `tip` reach it, otherwise the first parent
/// of the oldest commit of that line with the base in its history (a merge
/// that brought it in).
fn line_start(repo: &Repository, tip: Oid, base: Oid) -> Result<Oid, String> {
    let mut at = tip;
    for _ in 0..=MAX_COMMITS {
        if at == base {
            return Ok(base);
        }
        let commit = repo.find_commit(at).map_err(err)?;
        // `tip` has the base in its history, so the line meets it, or a
        // merge with it on another side, before it ends.
        let first = commit.parent_id(0).map_err(err)?;
        // A commit with one parent has the base in its history through it.
        if commit.parent_count() > 1 && first != base && !repo.graph_descendant_of(first, base).map_err(err)? {
            return Ok(first);
        }
        at = first;
    }
    Err(too_many())
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
    /// The line it is on.
    pub line: usize,
    /// Position in the line's items of the commit the others fold into.
    pub leader: usize,
    /// Positions of the commits folded into it, oldest first.
    pub members: Vec<usize>,
}

/// What becomes of a line as a whole, by the merges that bring it in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fate {
    /// Its commits are replayed and merged as planned.
    Kept,
    /// The merge that brings it in (or one bringing that merge in) is
    /// dropped, and its commits with it.
    Dropped,
    /// The merge that brings it in (or one bringing that merge in) is
    /// folded into another commit: its commits are replayed for their
    /// changes, which end up in that commit, and leave the history.
    Folded,
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

    /// How many of the planned commits are left once the plan ran.
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

/// How the todo list gives commits their messages.
#[derive(Clone, Copy)]
enum Messages<'a> {
    /// The plan's own: rewords and combined messages, applied by amending
    /// once each group is complete.
    Plan,
    /// Git's, as `git rebase --autosquash` makes them: `squash` combines
    /// the messages, `fixup -C` takes the folded commit's (for these
    /// commits, by index), `fixup` keeps the first.
    Git(&'a HashSet<usize>),
}

/// Why the todo list names a commit of the range somewhere else than where
/// it is replayed.
#[derive(Clone, Copy)]
enum Refer {
    /// A merge's side starts from it.
    Starts,
    /// A merge has it as a parent.
    Merges,
}

/// A group set aside while the sides of a merge folded into it are
/// written.
struct Frame {
    /// Commits written while it was open.
    written: Vec<usize>,
    /// Labels HEAD as it was then.
    back: String,
    pending: Option<usize>,
    deferred: Vec<(usize, String)>,
    open: bool,
}

/// Writes a [`Script`], keeping track of what the merges need.
struct Todo<'a> {
    plan: &'a Plan,
    from: Messages<'a>,
    todo: String,
    messages: Vec<String>,
    /// Message number to apply once the current group is complete.
    pending: Option<usize>,
    /// Labels to set once the current group is complete, with the commit
    /// each one names.
    deferred: Vec<(usize, String)>,
    /// A group (a kept commit and what is folded into it) is being written.
    open: bool,
    /// Commits written since the current group began.
    group: Vec<usize>,
    /// Groups set aside, innermost last.
    frames: Vec<Frame>,
    /// Rewritten commits that something later starts from or merges, by
    /// index.
    labels: HashMap<usize, String>,
    /// Commits other lines start from or merges have as a parent (other
    /// than the side a merge brings in): each gets a label where it is
    /// replayed.
    needed: HashSet<usize>,
    /// Lines left out with a dropped merge.
    gone: HashSet<usize>,
    serial: usize,
    /// The plan names commits before they are replayed.
    problems: Vec<String>,
}

impl<'a> Todo<'a> {
    fn new(plan: &'a Plan, from: Messages<'a>) -> Self {
        Todo {
            plan,
            from,
            todo: String::new(),
            messages: Vec::new(),
            pending: None,
            deferred: Vec::new(),
            open: false,
            group: Vec::new(),
            frames: Vec::new(),
            labels: HashMap::new(),
            needed: needed(&plan.range),
            gone: HashSet::new(),
            serial: 0,
            problems: Vec::new(),
        }
    }

    fn line(&mut self, line: &str) {
        let _ = writeln!(self.todo, "{line}");
    }

    fn new_label(&mut self, kind: &str) -> String {
        self.serial += 1;
        format!("gitkrill-{kind}-{}", self.serial)
    }

    /// Applies the message of the group just written and sets the labels
    /// waiting for it.
    fn close_group(&mut self) {
        if let Some(n) = self.pending.take() {
            self.line(&amend_line(n));
        }
        for (commit, label) in std::mem::take(&mut self.deferred) {
            self.line(&format!("label {label}"));
            self.labels.insert(commit, label);
        }
        self.open = false;
        self.group.clear();
    }

    /// Labels what HEAD is once the current group is complete as the
    /// rewritten `commit`.
    fn label_after(&mut self, commit: usize) {
        let label = self.new_label("fork");
        if self.open {
            self.deferred.push((commit, label));
        } else {
            self.line(&format!("label {label}"));
            self.labels.insert(commit, label);
        }
    }

    /// Writes the items of `line`, merges with their sides.
    fn write_line(&mut self, line: usize) {
        let plan = self.plan;
        let range = &plan.range;
        let groups = plan.groups(line);
        let mut next_group = 0;
        // Messages of commits that leave the history with a folded merge
        // do not matter.
        let messages = plan.fate(line) == Fate::Kept;
        for item in &plan.lines[line] {
            let c = item.source;
            let commit = &range.commits[c];
            match item.action {
                Action::Pick | Action::Reword => {
                    self.close_group();
                    let group = &groups[next_group];
                    next_group += 1;
                    let parents = commit.is_merge.then(|| self.write_sides(c));
                    let message = match self.from {
                        Messages::Plan if messages => plan.new_message(group),
                        _ => None,
                    };
                    if let Some(message) = message {
                        self.messages.push(message);
                        let n = self.messages.len();
                        self.pending = Some(n);
                        self.line(&mark_line(n));
                    }
                    match parents {
                        Some(parents) => self.merge_line(commit, &parents),
                        None => self.line(&format!("pick {}", commit.oid)),
                    }
                    self.open = true;
                }
                Action::Squash | Action::Fixup if commit.is_merge => {
                    let parents = self.write_sides(c);
                    self.line(MERGE_MARK);
                    self.merge_line(commit, &parents);
                    self.line(FOLD);
                }
                // The combined message is applied afterwards in one go, so
                // squashes need no message handling of their own here.
                Action::Squash | Action::Fixup => {
                    let verb = match self.from {
                        Messages::Git(amend) if amend.contains(&c) => "fixup -C",
                        Messages::Git(_) if item.action == Action::Squash => "squash",
                        _ => "fixup",
                    };
                    self.line(&format!("{verb} {}", commit.oid));
                }
                // Listed rather than left out, so git's check for commits
                // missing from the list (rebase.missingCommitsCheck) is happy.
                Action::Drop => {
                    if commit.is_merge {
                        self.drop_sides(c);
                    }
                    self.line(&format!("drop {}", commit.oid));
                }
            }
            if self.open {
                self.group.push(c);
            }
            if self.needed.contains(&c) {
                self.label_after(c);
            }
        }
    }

    /// Replays the sides of the merge `merge` and comes back to where HEAD
    /// was. Returns how the merge names each of its parents after the
    /// first.
    fn write_sides(&mut self, merge: usize) -> Vec<String> {
        let plan = self.plan;
        let range = &plan.range;
        let commit = &range.commits[merge];
        let others = 1..commit.parents.len();
        if !others.clone().any(|n| range.side(merge, n).is_some()) {
            return others.map(|n| self.reference(commit.parents[n], merge, Refer::Merges)).collect();
        }
        let back = self.new_label("line");
        self.line(&format!("label {back}"));
        // A group the merge folds into stays open meanwhile.
        self.frames.push(Frame {
            written: std::mem::take(&mut self.group),
            back,
            pending: self.pending.take(),
            deferred: std::mem::take(&mut self.deferred),
            open: std::mem::replace(&mut self.open, false),
        });
        let mut names = Vec::new();
        for n in others {
            match range.side(merge, n) {
                Some(side) => {
                    let to = match range.lines[side].start {
                        Some(start) => self.reference(start, merge, Refer::Starts),
                        None => "[new root]".to_owned(),
                    };
                    self.line(&format!("reset {to}"));
                    self.write_line(side);
                    self.close_group();
                    let end = self.new_label("side");
                    self.line(&format!("label {end}"));
                    names.push(end);
                }
                None => names.push(self.reference(commit.parents[n], merge, Refer::Merges)),
            }
        }
        let frame = self.frames.pop().expect("pushed above");
        self.group = frame.written;
        self.pending = frame.pending;
        self.deferred = frame.deferred;
        self.open = frame.open;
        self.line(&format!("reset {}", frame.back));
        names
    }

    /// How the todo list names the rewritten `oid` where `by` needs it.
    fn reference(&mut self, oid: Oid, by: usize, why: Refer) -> String {
        let plan = self.plan;
        let range = &plan.range;
        // Outside the range: not rewritten.
        let Some(commit) = range.index_of(oid) else { return oid.to_string() };
        // Written while a group that is set aside was open: HEAD as it was.
        if let Some(frame) = self.frames.iter().rev().find(|frame| frame.written.contains(&commit)) {
            return frame.back.clone();
        }
        // In the group being written (a merge of it folded into it): HEAD.
        if self.group.contains(&commit) {
            let here = self.new_label("here");
            self.line(&format!("label {here}"));
            return here;
        }
        if let Some(label) = self.labels.get(&commit) {
            return label.clone();
        }
        let line = range.line_of[commit];
        if self.gone.contains(&line) {
            // Dropped with its merge: what builds on it builds on what is
            // below it.
            return match range.lines[line].start {
                Some(start) => self.reference(start, by, why),
                None => {
                    self.problems.push(format!(
                        "{} merges commits that are dropped with the merge that brings them in. Keep \
                         that merge, or drop this one too.",
                        describe(&range.commits[by])
                    ));
                    oid.to_string()
                }
            };
        }
        self.order_problem(commit, by, why);
        oid.to_string()
    }

    /// `by` needs `commit`, which the plan replays after it.
    fn order_problem(&mut self, commit: usize, by: usize, why: Refer) {
        let plan = self.plan;
        let range = &plan.range;
        // From a commit's own line up to the branch's, with the commit that
        // stands for it on each: itself, then the merges bringing it in.
        let chain = |c: usize| {
            let mut out = vec![(range.line_of[c], c)];
            let mut line = range.line_of[c];
            while let Some((merge, _)) = range.lines[line].merge {
                line = range.line_of[merge];
                out.push((line, merge));
            }
            out
        };
        let (of_commit, of_by) = (chain(commit), chain(by));
        let common = of_commit.iter().find_map(|(line, c)| {
            of_by.iter().find(|(other, _)| other == line).map(|(_, b)| (*c, *b))
        });
        let name = |c: usize| describe(&range.commits[c]);
        let what = match why {
            Refer::Starts => "merges commits that branch off from",
            Refer::Merges => "merges",
        };
        let text = match common {
            Some((c, b)) if c == commit && b == by => {
                format!("{} {what} {}, so it has to stay above that commit.", name(by), name(commit))
            }
            Some((c, b)) if b == by && c != b => format!(
                "{} {what} {}, which {} brings in, so it has to stay above that merge.",
                name(by),
                name(commit),
                name(c)
            ),
            Some((c, b)) if c == commit && c != b => format!(
                "{}, which {} brings in, {what} {}, so {} has to stay above that commit.",
                name(by),
                name(b),
                name(commit),
                short(range.commits[b].oid)
            ),
            Some((c, b)) if c != b => format!(
                "{}, which {} brings in, {what} {}, which {} brings in, so {} has to stay above {}.",
                name(by),
                name(b),
                name(commit),
                name(c),
                short(range.commits[b].oid),
                short(range.commits[c].oid)
            ),
            _ => format!("{} {what} {}, which the plan replays after it.", name(by), name(commit)),
        };
        if !self.problems.contains(&text) {
            self.problems.push(text);
        }
    }

    /// Leaves out the sides of a dropped merge, and those of the merges on
    /// them.
    fn drop_sides(&mut self, merge: usize) {
        let plan = self.plan;
        let range = &plan.range;
        for side in range.sides.get(&merge).into_iter().flatten() {
            self.gone.insert(*side);
            for c in &range.lines[*side].commits {
                if range.commits[*c].is_merge {
                    self.drop_sides(*c);
                }
            }
        }
    }

    /// Makes `commit` again as a merge of HEAD and `parents`, with its own
    /// message.
    fn merge_line(&mut self, commit: &RangeCommit, parents: &[String]) {
        let mut line = format!("merge -C {}", commit.oid);
        for parent in parents {
            line.push(' ');
            line.push_str(parent);
        }
        // Git shows the rest of the line when the merge stops on conflicts.
        let summary: String = commit.summary.chars().filter(|c| !c.is_control()).collect();
        if !summary.is_empty() {
            line.push_str(" # ");
            line.push_str(&summary);
        }
        self.line(&line);
    }

    fn finish(mut self) -> (Script, Vec<String>) {
        self.close_group();
        // What dropped merges brought in is listed too (see `Action::Drop`).
        let mut gone: Vec<usize> = self.gone.iter().copied().collect();
        gone.sort_unstable();
        for line in gone {
            for item in &self.plan.lines[line] {
                let _ = writeln!(self.todo, "drop {}", self.plan.range.commits[item.source].oid);
            }
        }
        (Script { todo: self.todo, messages: self.messages }, self.problems)
    }
}

/// Commits other lines start from, and those merges have as a parent other
/// than through a side of their own: each gets a label where it is
/// replayed.
fn needed(range: &Range) -> HashSet<usize> {
    let parents: Vec<&[Oid]> = range.commits.iter().map(|c| c.parents.as_slice()).collect();
    needed_in(&range.lines, &parents, &range.index)
}

/// [`needed`] for lines from [`arrange`] (with what it was given).
pub(crate) fn needed_in(lines: &[Line], parents: &[&[Oid]], index: &HashMap<Oid, usize>) -> HashSet<usize> {
    let mut needed = HashSet::new();
    for line in &lines[1..] {
        if let Some(start) = line.start.and_then(|oid| index.get(&oid).copied()) {
            needed.insert(start);
        }
    }
    let sides: HashSet<(usize, usize)> = lines.iter().filter_map(|line| line.merge).collect();
    for (m, of_merge) in parents.iter().enumerate().filter(|(_, p)| p.len() > 1) {
        for (n, parent) in of_merge.iter().enumerate().skip(1) {
            if !sides.contains(&(m, n))
                && let Some(p) = index.get(parent).copied()
            {
                needed.insert(p);
            }
        }
    }
    needed
}

fn describe(commit: &RangeCommit) -> String {
    format!("{} \"{}\"", short(commit.oid), commit.summary)
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub range: Arc<Range>,
    /// For each line of the range, what happens to its commits, oldest
    /// first, in the order the todo list replays them: a permutation of the
    /// line's commits.
    pub lines: Vec<Vec<Item>>,
}

impl Plan {
    /// Every commit picked, in its original order.
    pub fn new(range: Arc<Range>) -> Self {
        let lines = range
            .lines
            .iter()
            .map(|line| {
                line.commits.iter().map(|source| Item { source: *source, action: Action::Pick, reword: None, combined: None }).collect()
            })
            .collect();
        Plan { range, lines }
    }

    pub fn commit(&self, line: usize, pos: usize) -> &RangeCommit {
        &self.range.commits[self.lines[line][pos].source]
    }

    /// Where the commit with range index `source` is in the plan: its line
    /// and position.
    pub fn find(&self, source: usize) -> Option<(usize, usize)> {
        let at = |line: usize| self.lines[line].iter().position(|item| item.source == source).map(|pos| (line, pos));
        at(*self.range.line_of.get(source)?).or_else(|| (0..self.lines.len()).find_map(at))
    }

    /// The kept commits of `line` with what is folded into each, oldest
    /// first. A squash or fixup with nothing before it belongs to no group;
    /// see [`Plan::problems`].
    pub fn groups(&self, line: usize) -> Vec<Group> {
        let mut groups: Vec<Group> = Vec::new();
        for (pos, item) in self.lines[line].iter().enumerate() {
            match item.action {
                Action::Pick | Action::Reword => groups.push(Group { line, leader: pos, members: Vec::new() }),
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

    /// What becomes of `line` as a whole, by the merges that bring it in.
    pub fn fate(&self, line: usize) -> Fate {
        let mut fate = Fate::Kept;
        let mut at = line;
        while let Some((merge, _)) = self.range.lines.get(at).and_then(|l| l.merge) {
            let Some((merge_line, pos)) = self.find(merge) else { break };
            match self.lines[merge_line][pos].action {
                Action::Drop => return Fate::Dropped,
                Action::Squash | Action::Fixup => fate = Fate::Folded,
                Action::Pick | Action::Reword => {}
            }
            at = merge_line;
        }
        fate
    }

    /// How many commits end up in the one built from `group`: those of the
    /// group, and those merges folded into it bring in and keep.
    pub fn folded_in(&self, group: &Group) -> usize {
        let items = &self.lines[group.line];
        let brought = group.members.iter().map(|pos| self.kept_under(items[*pos].source)).sum::<usize>();
        group.members.len() + 1 + brought
    }

    /// How many commits the sides of the merge `commit` keep, those of
    /// the merges on them included (none for a dropped merge).
    fn kept_under(&self, commit: usize) -> usize {
        let sides = self.range.sides.get(&commit).into_iter().flatten();
        sides
            .flat_map(|side| &self.lines[*side])
            .filter(|item| item.action != Action::Drop)
            .map(|item| 1 + self.kept_under(item.source))
            .sum()
    }

    /// Whether the group has a squash, which is what makes git ask for a
    /// combined message (fixups alone keep the first message).
    pub fn has_squash(&self, group: &Group) -> bool {
        group.members.iter().any(|pos| self.lines[group.line][*pos].action == Action::Squash)
    }

    /// The message the group's first commit has on its own: the reworded
    /// text if it is being reworded, its original message otherwise.
    fn own_message(&self, group: &Group) -> String {
        let item = &self.lines[group.line][group.leader];
        match (&item.reword, item.action) {
            (Some(text), Action::Reword) => normalize(text),
            _ => normalize(&self.commit(group.line, group.leader).message),
        }
    }

    /// What git proposes for a squash: the first message followed by the
    /// message of every squashed (not fixup) commit, a blank line between.
    pub fn default_combined(&self, group: &Group) -> String {
        let mut text = self.own_message(group).trim_end().to_owned();
        for pos in &group.members {
            if self.lines[group.line][*pos].action == Action::Squash {
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(self.commit(group.line, *pos).message.trim());
            }
        }
        normalize(&text)
    }

    /// The message the commit built from `group` ends up with.
    pub fn message(&self, group: &Group) -> String {
        if self.has_squash(group) {
            match &self.lines[group.line][group.leader].combined {
                Some(text) => normalize(text),
                None => self.default_combined(group),
            }
        } else {
            self.own_message(group)
        }
    }

    /// The message to write onto the group's commit, or `None` when picking
    /// the first commit (and fixing the rest up into it) already yields it.
    fn new_message(&self, group: &Group) -> Option<String> {
        let message = self.message(group);
        (message != normalize(&self.commit(group.line, group.leader).message)).then_some(message)
    }

    pub fn summary(&self) -> Summary {
        let mut summary = Summary::default();
        for (line, items) in self.lines.iter().enumerate() {
            let dropped = items.iter().filter(|i| i.action == Action::Drop).count();
            match self.fate(line) {
                Fate::Dropped => {
                    summary.dropped += items.len();
                    continue;
                }
                // Part of the commit the merge is folded into, already
                // counted there.
                Fate::Folded => {
                    summary.squashed += items.len() - dropped;
                    summary.dropped += dropped;
                    continue;
                }
                Fate::Kept => {}
            }
            for group in self.groups(line) {
                if !group.members.is_empty() {
                    summary.squashed += group.members.len() + 1;
                    summary.squashed_into += 1;
                } else if self.new_message(&group).is_some() {
                    summary.reworded += 1;
                } else {
                    summary.kept += 1;
                }
            }
            summary.dropped += dropped;
            // Where dropped commits sit makes no difference to the result.
            // The commits of a line are in the range's order.
            let kept: Vec<usize> = items.iter().filter(|i| i.action != Action::Drop).map(|i| i.source).collect();
            summary.reordered |= kept.windows(2).any(|pair| pair[0] > pair[1]);
        }
        summary
    }

    /// Everything that makes the plan itself impossible to run, in plain
    /// language. Empty when it can start (given a clean working tree; see
    /// [`tree_blocker`]).
    pub fn problems(&self) -> Vec<String> {
        self.check(Messages::Plan)
    }

    fn check(&self, from: Messages<'_>) -> Vec<String> {
        let mut problems = Vec::new();
        if self.range.commits.is_empty() {
            problems.push("There are no commits after this one to rebase.".to_owned());
            return problems;
        }
        if self.lines[0].iter().all(|item| item.action == Action::Drop) {
            problems.push(
                "Every commit is dropped, so nothing would be left to rebase. To throw all of \
                 them away, reset the branch to the base commit instead."
                    .to_owned(),
            );
            return problems;
        }

        for (line, items) in self.lines.iter().enumerate() {
            if self.fate(line) == Fate::Dropped {
                continue;
            }
            let merge = self.range.lines[line].merge.map(|(merge, _)| &self.range.commits[merge]);
            match items.iter().position(|item| item.action != Action::Drop) {
                Some(pos) if items[pos].action.folds() => {
                    let commit = self.commit(line, pos);
                    let action = items[pos].action.label().to_lowercase();
                    problems.push(match merge {
                        None => format!(
                            "{} is the oldest commit that is kept, so there is no older commit to \
                             {action} it into. Pick it, or move it above the commit it should be folded into.",
                            describe(commit),
                        ),
                        Some(merge) => format!(
                            "{} is the oldest commit kept of those {} brings in, so there is no older \
                             commit there to {action} it into. Pick it, or move it above the commit it \
                             should be folded into.",
                            describe(commit),
                            describe(merge),
                        ),
                    });
                }
                // Every commit of a side dropped: the merge would merge
                // nothing (git leaves such a merge out).
                None if !items.is_empty() => {
                    if let Some(merge) = merge {
                        let text = format!(
                            "Every commit {} brings in is dropped, so there would be nothing left to merge. \
                             Drop the merge too, or keep one of them.",
                            describe(merge)
                        );
                        if !problems.contains(&text) {
                            problems.push(text);
                        }
                    }
                }
                _ => {}
            }
            for item in items {
                if let Some(reason) = self.refusal(line, item.action) {
                    let commit = &self.range.commits[item.source];
                    problems.push(format!("{}: {reason}", describe(commit)));
                }
            }
        }
        problems.extend(self.write(from).1);

        if matches!(from, Messages::Plan) {
            for line in (0..self.lines.len()).filter(|line| self.fate(*line) == Fate::Kept) {
                for group in self.groups(line) {
                    // Only a message the plan writes: a commit that already
                    // has an empty one may keep it.
                    if self.new_message(&group).is_some_and(|message| message.is_empty()) {
                        problems.push(format!(
                            "The new message for {} is empty. Type a message, or pick the commit \
                             to keep its original one.",
                            describe(self.commit(line, group.leader)),
                        ));
                    }
                }
            }
        }
        problems
    }

    /// Why a commit of `line` cannot be given `action`, or `None` when it
    /// can. Commits that leave the history with a folded merge keep no
    /// message of their own, so they cannot be reworded.
    pub fn refusal(&self, line: usize, action: Action) -> Option<&'static str> {
        (action == Action::Reword && self.fate(line) == Fate::Folded).then_some(MESSAGE_LOST)
    }

    /// How many merges the plan makes again, because something below them
    /// changes: a merge that needed conflicts resolved stops for them
    /// again. (Merges whose parents stay the same are kept as they are.)
    pub fn remade_merges(&self) -> usize {
        let mut same = HashMap::new();
        let mut remade = 0;
        self.unchanged(0, true, &mut same, &mut remade);
        remade
    }

    /// Whether `line` comes out as it went in, given whether what it starts
    /// from does; notes the same for each commit in `same` and counts the
    /// merges made again in `remade`.
    fn unchanged(&self, line: usize, start_same: bool, same: &mut HashMap<usize, bool>, remade: &mut usize) -> bool {
        let range = &self.range;
        let original = &range.lines[line].commits;
        let mut ok = start_same;
        let mut leader = None;
        for (pos, item) in self.lines[line].iter().enumerate() {
            let c = item.source;
            let commit = &range.commits[c];
            let was = |same: &HashMap<usize, bool>, oid: Oid| {
                range.index_of(oid).is_none_or(|i| same.get(&i).copied().unwrap_or(false))
            };
            let mut parents_same = ok;
            if commit.is_merge && item.action != Action::Drop {
                for n in 1..commit.parents.len() {
                    parents_same &= match range.side(c, n) {
                        Some(side) => {
                            let start = range.lines[side].start.is_none_or(|start| was(same, start));
                            self.unchanged(side, start, same, remade)
                        }
                        None => was(same, commit.parents[n]),
                    };
                }
                if !parents_same {
                    *remade += 1;
                }
            }
            let own = original.get(pos) == Some(&c)
                && match item.action {
                    Action::Pick => true,
                    Action::Reword => {
                        item.reword.as_ref().is_none_or(|text| normalize(text) == normalize(&commit.message))
                    }
                    _ => false,
                };
            match item.action {
                Action::Pick | Action::Reword => leader = Some(c),
                // The commit folded into changes too.
                Action::Squash | Action::Fixup => {
                    if let Some(leader) = leader {
                        same.insert(leader, false);
                    }
                }
                Action::Drop => {}
            }
            ok = parents_same && own;
            same.insert(c, ok);
        }
        ok
    }

    /// The todo list and message files that carry out the plan.
    pub fn script(&self) -> Script {
        self.write(Messages::Plan).0
    }

    /// The todo list, and how it names commits before replaying them.
    fn write(&self, from: Messages<'_>) -> (Script, Vec<String>) {
        let mut out = Todo::new(self, from);
        if self.range.starts_below_base()
            && let Some(start) = self.range.lines[0].start
        {
            // The base came in through a merge: the line below the plan
            // stays as it is.
            out.line(&format!("reset {start}"));
        }
        out.write_line(0);
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

        write_stage(repo, &self.script())?;
        let name = head.branch.clone().unwrap_or_else(|| "HEAD".to_owned());
        let mut cmd = GitCmd::empty(LABEL)
            .undoable(UndoKind::HeadMove)
            .done(self.summary().headline(&name, self.range.commits.len()));
        let range = &self.range;
        cmd.steps.push(rebase_step(range.base, range.has_merges(), range.starts_below_base()));
        // Never replay a plan onto a branch that moved after it was checked.
        cmd.expect_head = Some(head);
        Ok(cmd)
    }
}

const MESSAGE_LOST: &str = "its message would not be kept: the merge that brings it in is folded into \
                            another commit, and this commit leaves the history with it.";

const DIRTY: &str = "There are uncommitted changes to tracked files. Commit or stash them before \
                     rebasing: the rebase rewrites the files in the working tree.";

fn stage_dir(repo: &Repository) -> PathBuf {
    repo.path().join(STAGE_DIR)
}

/// Writes the todo list and messages where the sequence editor of
/// [`rebase_step`] picks them up.
fn write_stage(repo: &Repository, script: &Script) -> Result<(), String> {
    let stage = stage_dir(repo);
    let _ = std::fs::remove_dir_all(&stage);
    let write = |name: String, text: &str| {
        std::fs::write(stage.join(&name), text)
            .map_err(|e| format!("Could not write {}: {e}", stage.join(&name).display()))
    };
    std::fs::create_dir_all(&stage).map_err(|e| format!("Could not create {}: {e}", stage.display()))?;
    write("todo".to_owned(), &script.todo)?;
    for (i, message) in script.messages.iter().enumerate() {
        write(format!("msg-{}", i + 1), message)?;
    }
    Ok(())
}

/// The `git rebase -i` from `base` that runs the staged todo list.
/// `merges`: the range has merges; `below`: the branch's line starts below
/// the base ([`Range::starts_below_base`]).
fn rebase_step(base: Oid, merges: bool, below: bool) -> Step {
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
    let mut args: Vec<String> = ["-c", "advice.mergeConflict=false"].map(str::to_owned).to_vec();
    // From a base that came in through a merge, git's own list has the
    // line below the plan too, which the todo list leaves alone.
    if below {
        args.extend(["-c", "rebase.missingCommitsCheck=ignore"].map(str::to_owned));
    }
    args.extend(["rebase", "-i", "--quiet", "--reschedule-failed-exec"].map(str::to_owned));
    // The todo list makes the merges again either way; this tells git
    // the rebase is one that keeps them.
    if merges {
        args.push("--rebase-merges".to_owned());
    }
    args.push(base.to_string());
    Step::new(args, None).env("GIT_SEQUENCE_EDITOR", SEQUENCE_EDITOR).env(STAGE_ENV, STAGE_DIR)
}

/// How autosquash folds one commit into another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FoldHow {
    /// `fixup!`: keeps the other commit's message.
    Fixup,
    /// `squash!`: adds this commit's message to the other's.
    Squash,
    /// `amend!`: replaces the other commit's message with this one's.
    Amend,
}

/// A commit autosquash moves after another and folds into it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fold {
    pub commit: Oid,
    pub into: Oid,
    pub how: FoldHow,
}

/// Stages the todo list of an autosquash over merges: the range from
/// `base` with every commit picked, but each of `folds` (oldest first)
/// moved after the commit it is folded into (after the ones folded into it
/// before), on whatever line that commit is, merged side or the branch's
/// own. They are folded with git's own `fixup`, `squash` and `fixup -C`, so
/// the messages come out as `git rebase --autosquash` makes them.
/// [`folds_step`] then runs it.
///
/// Touches the disk: it runs on the operator thread, as a step of the
/// autosquash command (`git::fixup::autosquash`).
pub fn stage_folds(repo: &Repository, base: Oid, folds: &[Fold]) -> Result<(), String> {
    let range = Arc::new(load_range(repo, base)?);
    let mut plan = Plan::new(range.clone());
    let needed = needed(&range);
    let mut amend = HashSet::new();
    // The last commit folded into each one so far.
    let mut tail: HashMap<usize, usize> = HashMap::new();
    let gone = |oid: Oid| format!("{} is no longer among the commits after {}.", short(oid), short(base));
    for fold in folds {
        let commit = range.index_of(fold.commit).ok_or_else(|| gone(fold.commit))?;
        let into = range.index_of(fold.into).ok_or_else(|| gone(fold.into))?;
        let (line, pos) = plan.find(commit).ok_or_else(|| gone(fold.commit))?;
        let after = tail.get(&into).copied().unwrap_or(into);
        let (to_line, _) = plan.find(after).ok_or_else(|| gone(fold.into))?;
        // Another line starts from it there, or a merge merges it.
        if to_line != line && needed.contains(&commit) {
            return Err(format!(
                "{} cannot move to the merged branch {} is on: other commits start from it.",
                short(fold.commit),
                short(fold.into)
            ));
        }
        let mut item = plan.lines[line].remove(pos);
        item.action = if fold.how == FoldHow::Squash { Action::Squash } else { Action::Fixup };
        if fold.how == FoldHow::Amend {
            amend.insert(commit);
        }
        let (to_line, to_pos) = plan.find(after).ok_or_else(|| gone(fold.into))?;
        plan.lines[to_line].insert(to_pos + 1, item);
        tail.insert(into, commit);
    }
    if let Some(problem) = plan.check(Messages::Git(&amend)).into_iter().next() {
        return Err(problem);
    }
    write_stage(repo, &plan.write(Messages::Git(&amend)).0)
}

/// The step that runs what [`stage_folds`] staged for a range from `base`
/// (a commit of the branch's own line).
pub fn folds_step(base: Oid) -> Step {
    rebase_step(base, true, false)
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
        let n = messages.len();
        let lines = vec![Line { commits: (0..n).collect(), start: Some(oid(0)), merge: None }];
        let head = Head { branch: Some("main".to_owned()), oid: Some(oid(99)), detached: false };
        Arc::new(Range::new(oid(0), "base".to_owned(), head, commits, lines, vec![0; n]))
    }

    fn plan(messages: &[&str], actions: &[Action]) -> Plan {
        let mut plan = Plan::new(range(messages));
        for (item, action) in plan.lines[0].iter_mut().zip(actions) {
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
        assert!(mark_line(3).contains(&format!("{STATE_DIR}/head-3")));
        assert!(amend_line(3).contains("/head-3\"") && amend_line(3).contains("/msg-3\""));
        assert!(MERGE_MARK.contains(&format!("{STATE_DIR}/merge-head")));
        assert!(FOLD.contains(&format!("--git-path {STATE_DIR})")) && FOLD.contains("/merge-head\""));
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
        plan.lines[0][0].reword = Some("a\n".to_owned());
        assert!(plan.summary().is_noop());

        plan.lines[0][0].reword = Some("new a\n\nbody".to_owned());
        assert_eq!(plan.summary(), Summary { kept: 1, reworded: 1, ..Default::default() });
        let script = plan.script();
        assert_eq!(script.messages, ["new a\n\nbody\n"]);
        assert_eq!(
            script.todo,
            format!("{}\npick {}\n{}\npick {}\n", mark_line(1), oid(1), amend_line(1), oid(2))
        );
    }

    #[test]
    fn a_squash_group_proposes_the_messages_git_would_combine() {
        let mut plan = plan(&["a\n\nbody a", "b", "c", "d"], &[Pick, Squash, Fixup, Squash]);
        let groups = plan.groups(0);
        assert_eq!(groups, [Group { line: 0, leader: 0, members: vec![1, 2, 3] }]);
        // The fixup's message is left out, as git does.
        assert_eq!(plan.default_combined(&groups[0]), "a\n\nbody a\n\nb\n\nd\n");
        assert_eq!(plan.summary(), Summary { squashed: 4, squashed_into: 1, ..Default::default() });

        plan.lines[0][0].combined = Some("all four".to_owned());
        let script = plan.script();
        assert_eq!(script.messages, ["all four\n"]);
        assert_eq!(
            script.todo,
            format!(
                "{}\npick {}\nfixup {}\nfixup {}\nfixup {}\n{}\n",
                mark_line(1),
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
        plan.lines[0][0].reword = Some("better a".to_owned());
        let group = &plan.groups(0)[0];
        assert_eq!(plan.default_combined(group), "better a\n\nb\n");
        // With a fixup instead, the reworded text is the whole message.
        plan.lines[0][1].action = Fixup;
        assert_eq!(plan.message(&plan.groups(0)[0]), "better a\n");
    }

    #[test]
    fn the_summary_reads_like_a_sentence() {
        let mut plan = plan(
            &["a", "b", "c", "d", "e", "f", "g"],
            &[Pick, Pick, Reword, Pick, Squash, Drop, Pick],
        );
        plan.lines[0][2].reword = Some("C".to_owned());
        assert_eq!(plan.summary().text(), "3 commits kept, 1 reworded, 2 squashed into 1, 1 dropped");
        assert_eq!(plan.summary().changes(), "1 reworded, 2 squashed into 1, 1 dropped");
        assert_eq!(plan.summary().result(), 5);

        plan.lines[0].swap(0, 1);
        assert!(plan.summary().reordered);
        assert!(plan.summary().text().ends_with(", order changed"));
    }

    #[test]
    fn moving_a_dropped_commit_is_not_a_reorder() {
        let mut plan = plan(&["a", "b", "c"], &[Pick, Drop, Pick]);
        plan.lines[0].swap(1, 2);
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
        empty.lines[0][1].reword = Some(" \n ".to_owned());
        assert!(empty.problems()[0].contains("is empty"));

        let mut empty_group = plan(&["a", "b"], &[Pick, Squash]);
        empty_group.lines[0][0].combined = Some(String::new());
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

        // HEAD itself as the base: an empty range, which the plan refuses.
        let head = fx.snapshot().head.oid.unwrap();
        let empty = load(&fx, head);
        assert!(empty.lines[0].is_empty());
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
        plan.lines[0][1].action = Action::Reword;
        plan.lines[0][1].reword = Some(typed.to_owned());
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
        plan.lines[0][1].action = Action::Squash;
        plan.lines[0][3].action = Action::Fixup;
        let groups = plan.groups(0);
        assert_eq!(
            plan.default_combined(&groups[0]),
            "add one\n\nbody of one\n\nadd two\n\nbody of two\n"
        );
        plan.lines[0][0].combined = Some("One and two together\n\n- \"quoted\"\n- ünïcödé".to_owned());
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
        plan.lines[0][1].action = Action::Squash;
        fx.run(prepare(&fx, &plan).unwrap());
        assert_eq!(log(&fx, base), ["add one\n\nbody of one\n\nadd two\n\nbody of two"]);
        assert_eq!(tree(&fx, "HEAD"), ["base.txt", "one.txt", "two.txt"]);
    }

    #[test]
    fn drop_removes_the_commit_and_its_changes() {
        let fx = Fixture::new("rebase-drop");
        let base = repo_with(&fx, &["one", "two", "three"]);
        let mut plan = load(&fx, base);
        plan.lines[0][1].action = Action::Drop;

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
        plan.lines[0].rotate_left(1); // two, three, one
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
        let items: [Item; 6] = std::mem::take(&mut plan.lines[0]).try_into().unwrap();
        let [mut one, mut two, mut three, four, mut five, mut six] = items;
        one.action = Action::Drop;
        two.action = Action::Squash;
        three.action = Action::Fixup;
        five.action = Action::Reword;
        five.reword = Some("Five, reworded".to_owned());
        six.combined = Some("Six with two".to_owned());
        plan.lines[0] = vec![four, three, five, six, two, one];
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
        plan.lines[0].swap(0, 1);
        plan.lines[0][1].action = Action::Reword;
        plan.lines[0][1].reword = Some("ONE after the conflict".to_owned());
        plan.lines[0][2].action = Action::Reword;
        plan.lines[0][2].reword = Some("THREE at the end\n\nwith a body".to_owned());

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
        plan.lines[0].swap(0, 1);
        plan.lines[0][2].action = Action::Reword;
        plan.lines[0][2].reword = Some("never applied".to_owned());

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
        plan.lines[0].swap(0, 1);
        plan.lines[0][1].action = Action::Reword;
        plan.lines[0][1].reword = Some("must not land on another commit".to_owned());

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
        plan.lines[0][0].action = Action::Reword;
        plan.lines[0][0].reword = Some("wip one".to_owned());

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

    /// Summaries of a line's commits, in plan order.
    fn summaries(plan: &Plan, line: usize) -> Vec<&str> {
        plan.lines[line].iter().map(|item| plan.range.commits[item.source].summary.as_str()).collect()
    }

    /// Where the commit with this summary is in the plan.
    fn at(plan: &Plan, summary: &str) -> (usize, usize) {
        let source = plan.range.commits.iter().position(|c| c.summary == summary).expect("in the range");
        plan.find(source).unwrap()
    }

    fn set(plan: &mut Plan, summary: &str, action: Action) {
        let (line, pos) = at(plan, summary);
        plan.lines[line][pos].action = action;
    }

    fn reword(plan: &mut Plan, summary: &str, text: &str) {
        let (line, pos) = at(plan, summary);
        plan.lines[line][pos].action = Action::Reword;
        plan.lines[line][pos].reword = Some(text.to_owned());
    }

    #[test]
    fn a_merge_is_a_row_and_what_it_brings_in_is_a_line_of_its_own() {
        let fx = Fixture::new("rebase-merge-range");
        let base = with_merge(&fx);
        let plan = load(&fx, base);
        let range = &plan.range;
        assert_eq!(summaries(&plan, 0), ["add one", "main work", "Merge side", "after the merge"]);
        assert_eq!(summaries(&plan, 1), ["side work"]);
        assert!(range.has_merges() && !range.starts_below_base());
        let (_, merge) = at(&plan, "Merge side");
        let merge = plan.lines[0][merge].source;
        let one = plan.lines[0][0].source;
        // "side work" branched off from "add one".
        assert_eq!(range.lines[1].merge, Some((merge, 1)));
        assert_eq!(range.lines[1].start, Some(range.commits[one].oid));
        assert_eq!(range.sides[&merge], [1]);
        assert_eq!(range.side(merge, 1), Some(1));
        assert_eq!(range.brought_in(merge), 1);
        assert!(plan.problems().is_empty());
        // Merges fold like any commit now; only commits that leave the
        // history with a folded merge cannot be reworded.
        assert_eq!(plan.refusal(0, Action::Squash), None);
        assert_eq!(plan.refusal(1, Action::Reword), None);
        assert_eq!(plan.fate(1), Fate::Kept);
    }

    /// base - one - main work - Merge side - after the merge, where the side
    /// branched off from "one" with three commits, each adding a file.
    fn with_side(fx: &Fixture) -> Oid {
        let base = repo_with(fx, &["one"]);
        git(&fx.dir, &["checkout", "-q", "-b", "side"]);
        for name in ["s1", "s2", "s3"] {
            fx.write(&format!("{name}.txt"), &format!("{name}\n"));
            fx.commit_all(&format!("side {name}"));
        }
        git(&fx.dir, &["checkout", "-q", "main"]);
        fx.write("main.txt", "main\n");
        fx.commit_all("main work");
        git(&fx.dir, &["merge", "-q", "--no-ff", "-m", "Merge side", "side"]);
        fx.write("after.txt", "after\n");
        fx.commit_all("after the merge");
        git(&fx.dir, &["branch", "-q", "-D", "side"]);
        base
    }

    #[test]
    fn the_commits_a_merge_brings_in_are_planned_one_by_one() {
        let fx = Fixture::new("rebase-side-plan");
        let base = with_side(&fx);
        let tip = fx.snapshot().head.oid.unwrap();
        let (one, main) = (rev(&fx, "HEAD~2~1"), rev(&fx, "HEAD~2"));
        let mut plan = load(&fx, base);
        assert_eq!(summaries(&plan, 1), ["side s1", "side s2", "side s3"]);
        // s3 fixed up into s1, s2 reworded, all within the side.
        let s3 = plan.lines[1].remove(2);
        plan.lines[1].insert(1, s3);
        set(&mut plan, "side s3", Action::Fixup);
        reword(&mut plan, "side s2", "side two, reworded");
        assert!(plan.problems().is_empty(), "{:?}", plan.problems());
        assert_eq!(plan.summary().text(), "4 commits kept, 1 reworded, 2 squashed into 1, order changed");
        assert_eq!(plan.remade_merges(), 1);

        let done = fx.run(prepare(&fx, &plan).unwrap());
        assert_eq!(
            shape(&fx, base),
            sorted([
                "after the merge <- Merge side",
                "Merge side <- main work + side two, reworded",
                "side two, reworded <- side s1",
                "side s1 <- add one",
                "main work <- add one",
                "add one <- base",
            ])
        );
        // The fixup's file is in the commit it was folded into.
        assert_eq!(tree(&fx, "HEAD~1^2~1"), ["base.txt", "one.txt", "s1.txt", "s3.txt"]);
        assert_eq!(tree(&fx, "HEAD"), ["after.txt", "base.txt", "main.txt", "one.txt", "s1.txt", "s2.txt", "s3.txt"]);
        // The line below the side is not rewritten.
        assert_eq!((rev(&fx, "HEAD~2~1"), rev(&fx, "HEAD~2")), (one, main));
        assert_no_leftovers(&fx);

        let entry = done.undo.expect("undoable");
        let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry));
        undone.result.expect("undo works");
        assert_eq!(undone.snap.head.oid, Some(tip));
    }

    #[test]
    fn a_side_that_conflicts_on_the_way_carries_on_after_continue() {
        let fx = Fixture::new("rebase-side-conflict");
        let base = repo_with(&fx, &["one"]);
        git(&fx.dir, &["checkout", "-q", "-b", "side"]);
        for n in ["1", "2", "3"] {
            fx.write("f.txt", &format!("{n}\n"));
            fx.write(&format!("s{n}.txt"), "s\n");
            fx.commit_all(&format!("side {n}"));
        }
        git(&fx.dir, &["checkout", "-q", "main"]);
        fx.write("main.txt", "main\n");
        fx.commit_all("main work");
        git(&fx.dir, &["merge", "-q", "--no-ff", "-m", "Merge side", "side"]);
        let tip = fx.snapshot().head.oid.unwrap();

        // Without "side 2", "side 3" no longer applies to what "side 1" left.
        let mut plan = load(&fx, base);
        set(&mut plan, "side 2", Action::Drop);
        reword(&mut plan, "side 3", "side 3, after the conflict");
        assert_eq!(plan.summary().text(), "4 commits kept, 1 reworded, 1 dropped");
        let stopped = fx.run_as(prepare(&fx, &plan).unwrap(), Origin::User);
        let message = stopped.result.expect_err("side 3 conflicts");
        assert!(message.contains("CONFLICT"), "{message}");
        assert_eq!(stopped.snap.state, RepoState::Rebase);
        assert!(stopped.undo.is_none());

        resolve(&fx, "3\n");
        let done = fx.run(ops::continue_op(RepoState::Rebase).unwrap());
        assert_eq!(done.snap.state, RepoState::Clean);
        assert_eq!(
            shape(&fx, base),
            sorted([
                "Merge side <- main work + side 3, after the conflict",
                "side 3, after the conflict <- side 1",
                "side 1 <- add one",
                "main work <- add one",
                "add one <- base",
            ])
        );
        assert_eq!(fx.read("f.txt"), "3\n");
        assert!(!fx.dir.join("s2.txt").exists());
        assert_no_leftovers(&fx);

        // Undone in one step, conflict and all.
        let entry = done.undo.expect("the finished plan is undoable");
        let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry));
        undone.result.expect("undo works");
        assert_eq!(undone.snap.head.oid, Some(tip));
        assert_eq!(fx.read("s2.txt"), "s\n");
    }

    #[test]
    fn dropping_every_commit_of_a_side_asks_to_drop_the_merge_too() {
        let fx = Fixture::new("rebase-side-empty");
        let base = with_side(&fx);
        let mut plan = load(&fx, base);
        for item in &mut plan.lines[1] {
            item.action = Action::Drop;
        }
        let problems = plan.problems();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].starts_with("Every commit") && problems[0].contains("\"Merge side\" brings in"), "{problems:?}");
        // Dropping the merge drops its side with it, whatever its rows say.
        set(&mut plan, "Merge side", Action::Drop);
        assert!(plan.problems().is_empty());
        assert_eq!(plan.fate(1), Fate::Dropped);
        assert_eq!(plan.summary().text(), "3 commits kept, 4 dropped");
        for item in &mut plan.lines[1] {
            item.action = Action::Reword;
        }
        assert!(plan.problems().is_empty(), "the side's own actions no longer count");
    }

    #[test]
    fn rewording_above_a_merge_leaves_the_merge_alone() {
        let fx = Fixture::new("rebase-merge-above");
        let base = with_merge(&fx);
        let merge = rev(&fx, "HEAD~1");
        let mut plan = load(&fx, base);
        plan.lines[0][3].action = Action::Reword;
        plan.lines[0][3].reword = Some("after, reworded".to_owned());
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
        plan.lines[0][0].action = Action::Reword;
        plan.lines[0][0].reword = Some("one, reworded".to_owned());
        plan.lines[0][2].action = Action::Reword;
        plan.lines[0][2].reword = Some("Merge side, reworded".to_owned());
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
        plan.lines[0][2].action = Action::Drop;
        assert_eq!(plan.summary().text(), "3 commits kept, 2 dropped");
        let script = plan.script();
        let side = plan.commit(1, 0).oid;
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
        plan.lines[0][3].action = Action::Fixup;
        plan.lines[0].swap(0, 1);
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
        let run = [plan.commit(0, 0).oid, plan.commit(0, 1).oid];
        assert_eq!(crate::git::multi::squash_plan(&mut plan, &run), Some((0, 0)));
        plan.lines[0][0].combined = Some("one and main".to_owned());
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
    fn a_merge_cannot_go_below_where_its_side_began() {
        let fx = Fixture::new("rebase-merge-limits");
        let base = with_merge(&fx);
        let mut plan = load(&fx, base);
        // Merge side below "add one", which side work branched off from.
        let merge = plan.lines[0].remove(2);
        plan.lines[0].insert(0, merge);
        let problems = plan.problems();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("\"Merge side\" merges commits that branch off from"), "{problems:?}");
        assert!(problems[0].contains("\"add one\""), "{problems:?}");
        assert!(prepare(&fx, &plan).is_err());
        // Dropped, it no longer needs that commit.
        plan.lines[0][0].action = Action::Drop;
        assert!(plan.problems().is_empty());
    }

    #[test]
    fn a_merge_squashed_into_the_commit_before_it_is_no_longer_a_merge() {
        let fx = Fixture::new("rebase-merge-squash");
        let base = with_side(&fx);
        let tip = fx.snapshot().head.oid.unwrap();
        let side: Vec<String> = (1..=3).map(|n| rev(&fx, &format!("HEAD~1^2~{}", 3 - n))).collect();
        let mut plan = load(&fx, base);
        set(&mut plan, "Merge side", Action::Squash);
        assert!(plan.problems().is_empty(), "{:?}", plan.problems());
        assert_eq!(plan.fate(1), Fate::Folded);
        // What leaves the history with the merge keeps no message.
        assert!(plan.refusal(1, Action::Reword).is_some());
        assert_eq!(plan.summary().text(), "2 commits kept, 5 squashed into 1");
        let group = plan.groups(0).into_iter().find(|g| !g.members.is_empty()).unwrap();
        assert_eq!(plan.folded_in(&group), 5);
        assert_eq!(plan.default_combined(&group), "main work\n\nMerge side\n");
        plan.lines[0][group.leader].combined = Some("Main work with the side".to_owned());

        let done = fx.run(prepare(&fx, &plan).unwrap());
        assert_eq!(
            shape(&fx, base),
            sorted(["after the merge <- Main work with the side", "Main work with the side <- add one", "add one <- base"])
        );
        // Its changes, and the side's, are in the commit it was folded into.
        assert_eq!(tree(&fx, "HEAD~1"), ["base.txt", "main.txt", "one.txt", "s1.txt", "s2.txt", "s3.txt"]);
        assert_eq!(git(&fx.dir, &["log", "-1", "--format=%an %B", "HEAD~1"]), "Test User Main work with the side");
        // The merged-in commits are no longer in the history.
        let history = git(&fx.dir, &["rev-list", "HEAD"]);
        assert!(side.iter().all(|sha| !history.contains(sha.as_str())), "{history}");
        assert_no_leftovers(&fx);

        let entry = done.undo.expect("undoable");
        let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry));
        undone.result.expect("undo works");
        assert_eq!(undone.snap.head.oid, Some(tip));
    }

    #[test]
    fn a_merge_folds_into_the_commit_its_side_branched_off_from() {
        let fx = Fixture::new("rebase-merge-fixup-fork");
        let base = repo_with(&fx, &["one"]);
        git(&fx.dir, &["checkout", "-q", "-b", "side"]);
        fx.write("side.txt", "side\n");
        fx.commit_all("side work");
        git(&fx.dir, &["checkout", "-q", "main"]);
        git(&fx.dir, &["merge", "-q", "--no-ff", "-m", "Merge side", "side"]);
        fx.write("after.txt", "after\n");
        fx.commit_all("after the merge");
        let mut plan = load(&fx, base);
        assert_eq!(summaries(&plan, 0), ["add one", "Merge side", "after the merge"]);
        set(&mut plan, "Merge side", Action::Fixup);
        // The side is replayed onto "add one" as the group has it so far.
        let todo = plan.script().todo;
        assert!(todo.contains(MERGE_MARK) && todo.contains(FOLD), "{todo}");
        fx.run(prepare(&fx, &plan).unwrap());
        assert_eq!(shape(&fx, base), sorted(["after the merge <- add one", "add one <- base"]));
        assert_eq!(git(&fx.dir, &["log", "-1", "--format=%B", "HEAD~1"]), "add one\n\nbody of one");
        assert_eq!(tree(&fx, "HEAD~1"), ["base.txt", "one.txt", "side.txt"]);
        assert_no_leftovers(&fx);
    }

    /// base, then "main work" and "side work" both setting f.txt, merged by
    /// hand into "Merge side", then "after". With `early`, "early" (its own
    /// file) comes before "main work". Returns the base.
    fn merged_by_hand(fx: &Fixture, early: bool) -> Oid {
        fx.write("f.txt", "base\n");
        fx.commit_all("base");
        let base = fx.snapshot().head.oid.unwrap();
        git(&fx.dir, &["checkout", "-q", "-b", "side"]);
        fx.write("f.txt", "side\n");
        fx.commit_all("side work");
        git(&fx.dir, &["checkout", "-q", "main"]);
        if early {
            fx.write("e.txt", "e\n");
            fx.commit_all("early");
        }
        fx.write("f.txt", "main\n");
        fx.commit_all("main work");
        let _ = std::process::Command::new("git").current_dir(&fx.dir).args(["merge", "-q", "side"]).output();
        fx.write("f.txt", "resolved by hand\n");
        git(&fx.dir, &["add", "f.txt"]);
        git(&fx.dir, &["commit", "-q", "-m", "Merge side"]);
        fx.write("g.txt", "g\n");
        fx.commit_all("after");
        base
    }

    #[test]
    fn a_merge_made_again_stops_on_its_conflicts_and_carries_on() {
        let fx = Fixture::new("rebase-merge-conflict");
        let base = merged_by_hand(&fx, false);
        let tip = fx.snapshot().head.oid.unwrap();

        let mut plan = load(&fx, base);
        plan.lines[0][0].action = Action::Reword;
        plan.lines[0][0].reword = Some("main work, reworded".to_owned());
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
    fn a_folded_merge_that_conflicts_is_folded_once_continued() {
        let fx = Fixture::new("rebase-merge-fold-conflict");
        let base = merged_by_hand(&fx, true);
        let tip = fx.snapshot().head.oid.unwrap();
        // The merge is made again only for a change below the commit it is
        // folded into, which is applied before the merge.
        let mut plan = load(&fx, base);
        reword(&mut plan, "early", "early, reworded");
        set(&mut plan, "Merge side", Action::Fixup);
        let stopped = fx.run_as(prepare(&fx, &plan).unwrap(), Origin::User);
        assert!(stopped.result.expect_err("the merge conflicts again").contains("CONFLICT"));

        resolve(&fx, "resolved again\n");
        let done = fx.run(ops::continue_op(RepoState::Rebase).unwrap());
        assert_eq!(done.snap.state, RepoState::Clean);
        assert_eq!(
            shape(&fx, base),
            sorted(["after <- main work", "main work <- early, reworded", "early, reworded <- base"])
        );
        assert_eq!(git(&fx.dir, &["show", "HEAD~1:f.txt"]), "resolved again");
        assert_no_leftovers(&fx);

        let entry = done.undo.expect("undoable once finished");
        let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry));
        undone.result.expect("undo works");
        assert_eq!(undone.snap.head.oid, Some(tip));
    }

    #[test]
    fn a_folded_merge_skipped_on_its_conflict_leaves_the_commit_before_it_alone() {
        let fx = Fixture::new("rebase-merge-fold-skip");
        let base = merged_by_hand(&fx, true);
        let mut plan = load(&fx, base);
        reword(&mut plan, "early", "early, reworded");
        reword(&mut plan, "main work", "main work, reworded");
        set(&mut plan, "Merge side", Action::Fixup);
        assert!(fx.run_as(prepare(&fx, &plan).unwrap(), Origin::User).result.is_err());
        let done = fx.run(GitCmd::new("Skip", &["rebase", "--skip"]));
        assert_eq!(done.snap.state, RepoState::Clean);
        // Not folded, and "main work" still there with its new message.
        assert_eq!(
            shape(&fx, base),
            sorted(["after <- main work, reworded", "main work, reworded <- early, reworded", "early, reworded <- base"])
        );
        assert_eq!(fx.read("f.txt"), "main\n");
        assert_no_leftovers(&fx);
    }

    #[test]
    fn a_base_that_came_in_through_a_merge_plans_from_that_merge() {
        let fx = Fixture::new("rebase-merge-base");
        let base = with_side(&fx);
        let tip = fx.snapshot().head.oid.unwrap();
        let (s1, main) = (rev(&fx, "HEAD~1^2~2"), rev(&fx, "HEAD~2"));
        let s1_oid = Oid::from_str(&s1).unwrap();

        let mut plan = load(&fx, s1_oid);
        let range = &plan.range;
        assert!(range.starts_below_base());
        assert_eq!(range.lines[0].start, Some(Oid::from_str(&main).unwrap()));
        assert_eq!(summaries(&plan, 0), ["Merge side", "after the merge"]);
        assert_eq!(summaries(&plan, 1), ["side s2", "side s3"]);
        assert_eq!(plan.range.lines[1].start, Some(s1_oid));
        reword(&mut plan, "side s2", "side two, reworded");
        let script = plan.script();
        assert!(script.todo.starts_with(&format!("reset {main}\n")), "{}", script.todo);
        let cmd = prepare(&fx, &plan).unwrap();
        assert!(cmd.steps[0].args.iter().any(|a| a == "rebase.missingCommitsCheck=ignore"));
        let done = fx.run(cmd);
        assert_eq!(
            shape(&fx, base),
            sorted([
                "after the merge <- Merge side",
                "Merge side <- main work + side s3",
                "side s3 <- side two, reworded",
                "side two, reworded <- side s1",
                "side s1 <- add one",
                "main work <- add one",
                "add one <- base",
            ])
        );
        // What the plan does not reach is not rewritten.
        assert_eq!((rev(&fx, "HEAD~2"), rev(&fx, "HEAD~1^2~2")), (main, s1));
        assert_no_leftovers(&fx);
        let entry = done.undo.expect("undoable");
        let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry));
        undone.result.expect("undo works");
        assert_eq!(undone.snap.head.oid, Some(tip));

        // A commit that is not in HEAD's history at all.
        git(&fx.dir, &["checkout", "-q", "--orphan", "elsewhere"]);
        fx.commit_all("unrelated root");
        let unrelated = fx.job(move |loader| load_range(loader.repo(), s1_oid)).unwrap_err();
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
        assert_eq!(summaries(&plan, 0), ["a", "M1", "b", "M2"]);
        // M1 brings in s1; M2 the rest, which builds on s1.
        assert_eq!(summaries(&plan, 1), ["s1"]);
        assert_eq!(summaries(&plan, 2), ["side takes main", "s3"]);
        assert_eq!(plan.range.lines[2].start, Some(plan.commit(1, 0).oid));
        let m1 = plan.lines[0][1].source;
        let m2 = plan.lines[0][3].source;
        assert_eq!((plan.range.brought_in(m1), plan.range.brought_in(m2)), (1, 2));

        // Moved below M1, M2's side would start from a commit not replayed yet.
        let mut moved = plan.clone();
        moved.lines[0].swap(1, 3);
        let problems = moved.problems();
        assert!(
            problems.iter().any(|p| p.contains("\"M2\" merges commits that branch off from")
                && p.contains("which") && p.contains("\"M1\" brings in, so it has to stay above that merge")),
            "{problems:?}"
        );
        // "side takes main", which M2 brings in, merges "a".
        let mut below = plan.clone();
        let m2_item = below.lines[0].remove(3);
        below.lines[0].insert(0, m2_item);
        assert!(
            below.problems().iter().any(|p| p.contains("\"side takes main\", which") && p.contains("merges ")
                && p.contains("\"a\", so ")),
            "{:?}",
            below.problems()
        );

        plan.lines[0][0].action = Action::Reword;
        plan.lines[0][0].reword = Some("A".to_owned());
        reword(&mut plan, "s3", "S3");
        assert_eq!(plan.remade_merges(), 3);
        fx.run(prepare(&fx, &plan).unwrap());
        assert_eq!(
            shape(&fx, base),
            sorted([
                "M2 <- b + S3",
                "S3 <- side takes main",
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
    fn a_side_of_unrelated_history_is_replayed_from_a_new_root() {
        let fx = Fixture::new("rebase-merge-root");
        let base = repo_with(&fx, &["one"]);
        git(&fx.dir, &["checkout", "-q", "--orphan", "other"]);
        git(&fx.dir, &["rm", "-q", "-r", "-f", "."]);
        fx.write("o.txt", "o\n");
        fx.commit_all("other root");
        fx.write("o2.txt", "o2\n");
        fx.commit_all("other two");
        git(&fx.dir, &["checkout", "-q", "main"]);
        git(&fx.dir, &["merge", "-q", "--no-ff", "--allow-unrelated-histories", "-m", "Merge other", "other"]);
        let mut plan = load(&fx, base);
        assert_eq!(summaries(&plan, 1), ["other root", "other two"]);
        assert_eq!(plan.range.lines[1].start, None);
        reword(&mut plan, "other root", "Other root, reworded");
        assert!(plan.script().todo.contains("reset [new root]\n"));
        fx.run(prepare(&fx, &plan).unwrap());
        let out = git(&fx.dir, &["log", "--format=%s|%p", "HEAD^2"]);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "{out}");
        assert!(lines[0].starts_with("other two|") && lines[1] == "Other root, reworded|", "{out}");
        assert_eq!(tree(&fx, "HEAD"), ["base.txt", "o.txt", "o2.txt", "one.txt"]);
    }

    #[test]
    fn a_dirty_tree_or_a_moved_branch_stops_the_plan_before_git_runs() {
        let fx = Fixture::new("rebase-dirty");
        let base = repo_with(&fx, &["one", "two"]);
        let tip = fx.snapshot().head.oid.unwrap();
        let mut plan = load(&fx, base);
        plan.lines[0][1].action = Action::Drop;

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
            plan.lines[0][2].action = Action::Drop;
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
        plan.lines[0][0].action = Action::Reword;
        plan.lines[0][0].reword = Some("two, from a worktree".to_owned());
        plan.lines[0][1].action = Action::Squash;
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
        plan.lines[0][0].action = Action::Reword;
        plan.lines[0][0].reword = Some("it's \"one\"".to_owned());
        plan.lines[0][1].action = Action::Squash;
        fx.run(prepare(&fx, &plan).unwrap());
        assert_eq!(log(&fx, base), ["it's \"one\"\n\nadd two\n\nbody of two"]);
        assert_eq!(fx.read("left alone.txt"), "untracked\n");
        assert_no_leftovers(&fx);
    }
}
