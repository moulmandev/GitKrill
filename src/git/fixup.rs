//! Fixup commits, and autosquash to fold them in.
//!
//! `git commit --fixup=<commit>` records the staged changes as a commit
//! titled `fixup! <summary of that commit>`; `--squash=<commit>` as
//! `squash! <summary>`, whose own message is added to that commit's when it
//! is folded in. Autosquash (`git rebase -i --autosquash`) moves each of them
//! after the commit it names and folds it in (and `amend!` commits, which
//! replace its message).
//!
//! [`survey`] finds, from the loaded history, the fixup commits of the
//! checked-out branch (those of the branches merged into it included), the
//! commit each one names, and where a rebase folding them in has to start;
//! it says why it leaves the others (another branch has their commit, say).
//! It matches them the way git does. [`autosquash`] runs the rebase without
//! an editor. On a branch without merges after that start it is
//! `git rebase -i --autosquash`: `GIT_SEQUENCE_EDITOR` (`true`, set for every
//! git command by `git::cli::command`, the same as `-c sequence.editor=:`)
//! accepts the todo list git arranged as it is, and `GIT_EDITOR` keeps the
//! combined message of each squash. With merges, the rebase keeps them
//! (`--rebase-merges`) with a todo list of its own (`rebase::stage_folds`),
//! so that where a fixup goes does not depend on the order git happens to
//! list the merged branches in: each fixup right after its commit, inside
//! the merged branch if that is where the commit is, folded with git's own
//! `fixup`, `squash` and `fixup -C`, so the messages come out the same.

use std::collections::HashMap;

use git2::Repository;

use super::internal::Internal;
use super::ops::{GitCmd, Step, UndoKind};
use super::rebase::{self, Fold, FoldHow};
use super::{CommitInfo, Head, History, Oid, RowKind, short};

/// Label prefix of fixup commits. Unlike other commits they do not take the
/// commit panel's message, so they leave it where it is.
pub const FIXUP_LABEL: &str = "Fixup ";
/// Label of the autosquash rebase.
pub const AUTOSQUASH_LABEL: &str = "Autosquash";

/// How far back from HEAD [`plan`] looks for fixups and what they fix.
pub const MAX_COMMITS: usize = 1000;

/// Records the staged changes as a fixup of `target`. Undone like a commit:
/// the changes are staged again.
pub fn fixup(target: Oid) -> GitCmd {
    GitCmd::new(format!("{FIXUP_LABEL}{}", short(target)), &["commit", &format!("--fixup={target}")])
        .undoable(UndoKind::Commit)
        .done(format!("Committed a fixup of {}", short(target)))
}

/// Records the staged changes as a `squash!` commit for `target`. `message`
/// (from the commit panel, may be empty) is added to that commit's message
/// when it is folded in.
pub fn squash_into(target: Oid, message: &str) -> GitCmd {
    let mut args = vec!["commit".to_owned(), format!("--squash={target}")];
    let message = message.trim();
    let stdin = if message.is_empty() {
        args.push("--no-edit".to_owned());
        None
    } else {
        args.extend(["-F".to_owned(), "-".to_owned()]);
        Some(message.as_bytes().to_vec())
    };
    let mut cmd = GitCmd::empty(format!("Squash into {}", short(target)));
    cmd.steps.push(Step::new(args, stdin));
    cmd.undoable(UndoKind::Commit).done(format!("Committed a squash into {}", short(target)))
}

const PREFIXES: [&str; 3] = ["fixup! ", "squash! ", "amend! "];

fn strip_prefix(subject: &str) -> Option<&str> {
    PREFIXES.iter().find_map(|prefix| subject.strip_prefix(prefix))
}

/// What a `fixup!`, `squash!` or `amend!` subject names (past repeated
/// prefixes: `fixup! fixup! Add parser` names `Add parser`, as in git);
/// `None` for any other subject.
pub fn fixup_target(subject: &str) -> Option<&str> {
    let mut rest = strip_prefix(subject)?;
    loop {
        let trimmed = rest.trim_start();
        match strip_prefix(trimmed) {
            Some(next) => rest = next,
            None => return Some(trimmed),
        }
    }
}

pub fn is_fixup(subject: &str) -> bool {
    fixup_target(subject).is_some()
}

/// A rebase that folds the fixup commits of the checked-out branch in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Autosquash {
    /// What the rebase starts from: a commit of the branch's own line
    /// (its first parents), below the oldest commit something is folded
    /// into, or below the merge that brought that commit in; `None` when
    /// that is the root commit.
    pub base: Option<Oid>,
    /// The fixup, squash and amend commits folded in, newest first.
    pub fixups: Vec<Oid>,
    /// How many commits the rebase rewrites, fixups included.
    pub rewritten: usize,
    /// HEAD when planned.
    pub head: Oid,
    /// Each one folded and what into, oldest first.
    pub folds: Vec<Fold>,
    /// The branch has merges after the base: the rebase keeps them, with a
    /// todo list of its own (`rebase::stage_folds`).
    pub merges: bool,
}

/// What autosquash makes of the fixup commits of the checked-out branch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Survey {
    /// The rebase folding those that can be folded; `None` when none can.
    pub plan: Option<Autosquash>,
    /// The fixup commits it leaves as they are, newest first, each with
    /// the reason.
    pub left: Vec<(Oid, String)>,
}

impl Autosquash {
    /// "2 fixup commits": what is folded.
    pub fn what(&self) -> String {
        match self.fixups.len() {
            1 => "1 fixup commit".to_owned(),
            n => format!("{n} fixup commits"),
        }
    }

    /// "rewrites 5 commits from abc1234": what it costs.
    pub fn cost(&self) -> String {
        let n = self.rewritten;
        let commits = if n == 1 { "commit" } else { "commits" };
        match self.base {
            Some(base) => format!("rewrites {n} {commits} after {}", short(base)),
            // From the root: the whole branch, which nothing else shares.
            None => format!("rewrites all {n} {commits}"),
        }
    }
}

const NAMES_NOTHING: &str = "No earlier commit of this branch has the summary or SHA it names.";
const SHARED: &str = "The commit it names is on another branch too, and autosquash never rewrites \
                      what other branches have.";
const STARTS_OTHERS: &str = "Other commits start from it, so it cannot move to the merged branch the \
                             commit it names is on.";
const ROOT: &str = "The commit it names is the first commit of the repository, which autosquash can \
                    only fold into on a branch without merges.";

/// What autosquash does with HEAD's branch, going by the loaded history.
///
/// The branch is what HEAD reaches, as far as the history is loaded and up
/// to [`MAX_COMMITS`] commits, merged-in branches included. Each fixup
/// (not a merge) is matched to the commit it names among those before it,
/// as git does: one with that exact summary (the oldest), else one whose
/// SHA starts with it, else one whose summary starts with it, looking at
/// the commits no other branch has first. What any of `stops` (the tips of
/// the other branches) reaches is never rewritten, so a fixup of such a
/// commit is left. A fixup moves to wherever its commit is, into the branch
/// a merge brought in if that is where, except when other commits start
/// from it there (a branch merged later): those are left too.
pub fn survey(history: &History, head: Oid, stops: &[Oid]) -> Survey {
    let row_of = |oid: &Oid| history.index_of.get(oid).map(|row| *row as usize);
    let Some(head_row) = row_of(&head) else { return Survey::default() };
    let rows = &history.commits;

    // What HEAD reaches (bit 1) and what the other branches reach (bit 2).
    // Rows are ordered children first, so one pass down from the highest
    // of the tips marks it all.
    let tips: Vec<usize> = stops.iter().filter_map(row_of).collect();
    let top = tips.iter().copied().chain([head_row]).min().unwrap_or(head_row);
    let mut reach = vec![0u8; rows.len() - top];
    reach[head_row - top] |= 1;
    for tip in &tips {
        reach[tip - top] |= 2;
    }
    // HEAD's commits, children first.
    let mut window: Vec<usize> = Vec::new();
    for row in top..rows.len() {
        let flags = reach[row - top];
        if flags == 0 || rows[row].kind != RowKind::Commit {
            continue;
        }
        if flags & 1 != 0 {
            if window.len() >= MAX_COMMITS {
                break;
            }
            window.push(row);
        }
        for parent in rows[row].parents.iter().filter_map(row_of) {
            reach[parent - top] |= flags;
        }
    }

    // Parents first from here on, as git lists the commits it rebases.
    let order: Vec<usize> = window.iter().rev().copied().collect();
    let n = order.len();
    let commit = |i: usize| &rows[order[i]];
    let index: HashMap<Oid, usize> = order.iter().enumerate().map(|(i, row)| (rows[*row].oid, i)).collect();
    let parents: Vec<&[Oid]> = order.iter().map(|row| rows[*row].parents.as_slice()).collect();
    let shared: Vec<bool> = order.iter().map(|row| reach[row - top] & 2 != 0).collect();
    // The branch's own line, and how the rest falls into lines, as the
    // interactive rebase arranges them.
    let mut main = Vec::new();
    let mut at = Some(head);
    while let Some(i) = at.and_then(|oid| index.get(&oid).copied()) {
        main.push(i);
        at = parents[i].first().copied();
    }
    main.reverse();
    let on_main: HashMap<usize, usize> = main.iter().enumerate().map(|(pos, c)| (*c, pos)).collect();
    let (lines, line_of) = rebase::arrange(&parents, &index, main.clone(), at);
    let needed = rebase::needed_in(&lines, &parents, &index);

    let mut folded = vec![false; n];
    // (fixup, what it is folded into, how), as indexes into `order`.
    let mut folds: Vec<(usize, usize, FoldHow)> = Vec::new();
    let mut left: Vec<(usize, &str)> = Vec::new();
    for i in 0..n {
        let summary = commit(i).summary.as_str();
        let Some(text) = fixup_target(summary).filter(|_| parents[i].len() < 2) else { continue };
        let how = if summary.starts_with("squash! ") {
            FoldHow::Squash
        } else if summary.starts_with("amend! ") {
            FoldHow::Amend
        } else {
            FoldHow::Fixup
        };
        // The commits before it, oldest first.
        let mut before = Vec::new();
        let mut seen = vec![false; n];
        let mut stack: Vec<usize> = parents[i].iter().filter_map(|p| index.get(p).copied()).collect();
        while let Some(j) = stack.pop() {
            if !std::mem::replace(&mut seen[j], true) {
                before.push(j);
                stack.extend(parents[j].iter().filter_map(|p| index.get(p).copied()));
            }
        }
        before.sort_unstable();
        let hex = text.len() >= 4 && text.bytes().all(|b| b.is_ascii_hexdigit());
        let lower = text.to_ascii_lowercase();
        let find = |own: bool| {
            let candidates = || before.iter().copied().filter(|j| !own || !shared[*j]);
            candidates()
                .find(|j| !folded[*j] && commit(*j).summary == text)
                .or_else(|| candidates().find(|j| hex && commit(*j).oid.to_string().starts_with(&lower)))
                .or_else(|| candidates().find(|j| commit(*j).summary.starts_with(text)))
        };
        let own = find(true);
        let elsewhere = own.is_none() && find(false).is_some();
        match own {
            Some(target) if needed.contains(&i) && line_of[i] != line_of[target] => left.push((i, STARTS_OTHERS)),
            Some(target) => {
                folded[i] = true;
                folds.push((i, target, how));
            }
            None if elsewhere => left.push((i, SHARED)),
            None => left.push((i, NAMES_NOTHING)),
        }
    }

    // The commit of the branch's own line that brings each target in:
    // itself, or the merge that brought it in.
    let bringer = |mut c: usize| {
        while let Some((merge, _)) = lines[line_of[c]].merge {
            c = merge;
        }
        on_main[&c]
    };
    let (base, merges) = loop {
        let Some(bottom) = folds.iter().map(|(_, target, _)| bringer(*target)).min() else {
            return Survey { plan: None, left: newest_first(&order, &left, rows) };
        };
        let base = parents[main[bottom]].first().copied();
        let merges = main[bottom..].iter().any(|c| parents[*c].len() > 1);
        if base.is_some() || !merges {
            break (base, merges);
        }
        // From the root, only git's own autosquash can fold, which does
        // not keep merges.
        let (root, kept): (Vec<_>, Vec<_>) = folds.into_iter().partition(|(_, target, _)| bringer(*target) == bottom);
        left.extend(root.into_iter().map(|(fixup, _, _)| (fixup, ROOT)));
        folds = kept;
    };

    // What changes: the targets and everything built on them.
    let mut changed = vec![false; n];
    for (_, target, _) in &folds {
        changed[*target] = true;
    }
    for i in 0..n {
        if !changed[i] && parents[i].iter().any(|p| index.get(p).is_some_and(|j| changed[*j])) {
            changed[i] = true;
        }
    }
    let mut fixups: Vec<usize> = folds.iter().map(|(fixup, _, _)| *fixup).collect();
    fixups.sort_unstable_by(|a, b| b.cmp(a));
    let plan = Autosquash {
        base,
        fixups: fixups.iter().map(|i| commit(*i).oid).collect(),
        rewritten: changed.iter().filter(|c| **c).count(),
        head,
        folds: folds
            .iter()
            .map(|(fixup, target, how)| Fold { commit: commit(*fixup).oid, into: commit(*target).oid, how: *how })
            .collect(),
        merges,
    };
    Survey { plan: Some(plan), left: newest_first(&order, &left, rows) }
}

/// The fixups left, newest first, with their reasons.
fn newest_first(order: &[usize], left: &[(usize, &str)], rows: &[CommitInfo]) -> Vec<(Oid, String)> {
    let mut left = left.to_vec();
    left.sort_unstable_by_key(|(i, _)| std::cmp::Reverse(*i));
    left.into_iter().map(|(i, why)| (rows[order[i]].oid, why.to_owned())).collect()
}

/// Folds the planned fixups in from HEAD as it was planned (`head`): with
/// `git rebase -i --autosquash` on a branch without merges after the base,
/// otherwise with a todo list of its own that keeps the merges and puts
/// each fixup after its commit, wherever that is (`rebase::stage_folds`).
/// Undone like a rebase; one that stops on a conflict is a rebase in
/// progress, finished or aborted from the banner.
pub fn autosquash(plan: &Autosquash, head: &Head) -> GitCmd {
    let mut cmd = match plan.base.filter(|_| plan.merges) {
        Some(base) => {
            let folds = plan.folds.clone();
            let mut cmd = GitCmd::empty(AUTOSQUASH_LABEL);
            cmd.steps.push(Step::internal(Internal::new("arrange the fixups", move |workdir| {
                let repo = Repository::open(workdir).map_err(|e| e.message().to_owned())?;
                rebase::stage_folds(&repo, base, &folds).map(|()| String::new())
            })));
            cmd.steps.push(rebase::folds_step(base));
            cmd
        }
        None => {
            let base = plan.base.map(|base| base.to_string());
            let mut args = vec!["-c", "advice.mergeConflict=false", "rebase", "-i", "--autosquash", "--quiet"];
            args.push(base.as_deref().unwrap_or("--root"));
            GitCmd::new(AUTOSQUASH_LABEL, &args)
        }
    }
    .undoable(UndoKind::HeadMove)
    .done(format!("Autosquashed {}", plan.what()));
    // Git's last words are the summary of the last squash it committed;
    // the toast says what happened instead.
    let into = if plan.fixups.len() == 1 { "its commit" } else { "their commits" };
    let n = plan.rewritten;
    let commits = if n == 1 { "1 commit".to_owned() } else { format!("{n} commits") };
    let rewritten = match plan.base {
        Some(base) => format!("{commits} rewritten after {}", short(base)),
        None => format!("all {commits} rewritten"),
    };
    let result = format!("Folded {} into {into}; {rewritten}.", plan.what());
    cmd.steps.push(Step::internal(Internal::new("say what was folded", move |_| Ok(result.clone()))));
    // Never fold onto a branch that moved since the plan was made.
    cmd.expect_head = Some(head.clone());
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::RepoState;
    use crate::tests::{Fixture, git};
    use crate::worker::Origin;

    #[test]
    fn fixup_subjects_name_their_commit() {
        assert_eq!(fixup_target("fixup! Add parser"), Some("Add parser"));
        assert_eq!(fixup_target("squash! fixup!  Add parser"), Some("Add parser"));
        assert_eq!(fixup_target("amend! 1a2b3c4"), Some("1a2b3c4"));
        assert_eq!(fixup_target("Add parser"), None);
        assert_eq!(fixup_target("fixup!Add parser"), None);
        assert!(is_fixup("squash! x") && !is_fixup("Fix: squash! x"));
    }

    fn sha(fx: &Fixture, rev: &str) -> Oid {
        Oid::from_str(&git(&fx.dir, &["rev-parse", rev])).unwrap()
    }

    fn subjects(fx: &Fixture) -> Vec<String> {
        git(&fx.dir, &["log", "--format=%s"]).lines().map(str::to_owned).collect()
    }

    /// Three commits on a branch made off main, then fixes for two of them
    /// staged and committed the way the menu does.
    fn branch_with_fixups(name: &str) -> Fixture {
        let fx = Fixture::new(name);
        fx.write("base.txt", "base\n");
        fx.commit_all("Base");
        git(&fx.dir, &["checkout", "-q", "-b", "feature"]);
        fx.write("parser file.rs", "fn parse() {}\n");
        fx.commit_all("Add parser");
        fx.write("lexer-ü.rs", "fn lex() {}\n");
        fx.commit_all("Add lexer");
        fx.write("docs.md", "docs\n");
        fx.commit_all("Write docs");

        fx.write("parser file.rs", "fn parse() { todo!() }\n");
        git(&fx.dir, &["add", "-A"]);
        let parser = sha(&fx, "HEAD~2");
        let done = fx.run(fixup(parser));
        assert!(done.undo.is_some(), "a fixup is undoable");
        fx.write("lexer-ü.rs", "fn lex() { todo!() }\n");
        git(&fx.dir, &["add", "-A"]);
        fx.run(squash_into(sha(&fx, "HEAD~2"), "Lexing handles unicode too."));
        fx
    }

    fn stops(snap: &crate::git::RepoSnapshot) -> Vec<Oid> {
        snap.branches.iter().filter(|b| !b.is_head).filter_map(|b| b.oid).collect()
    }

    fn plan(history: &History, head: Oid, stops: &[Oid]) -> Option<Autosquash> {
        survey(history, head, stops).plan
    }

    /// Each commit HEAD reaches as "subject <- parent + parent" (by
    /// subject), sorted.
    fn shape(fx: &Fixture) -> Vec<String> {
        let out = git(&fx.dir, &["log", "--format=%s|%p"]);
        let mut lines: Vec<String> = out
            .lines()
            .map(|line| {
                let (subject, parents) = line.split_once('|').unwrap();
                let parents: Vec<String> = parents
                    .split(' ')
                    .filter(|p| !p.is_empty())
                    .map(|p| git(&fx.dir, &["log", "-1", "--format=%s", p]))
                    .collect();
                format!("{subject} <- {}", parents.join(" + "))
            })
            .collect();
        lines.sort();
        lines
    }

    #[test]
    fn fixups_are_planned_and_folded_into_their_commits() {
        let fx = branch_with_fixups("fixup-autosquash");
        assert_eq!(subjects(&fx), ["squash! Add lexer", "fixup! Add parser", "Write docs", "Add lexer", "Add parser", "Base"]);
        let snap = fx.snapshot();
        let head = snap.head.oid.unwrap();
        let planned = plan(&snap.history, head, &stops(&snap)).expect("two fixups to fold");
        assert_eq!(planned.base, Some(sha(&fx, "main")));
        assert_eq!(planned.fixups, [sha(&fx, "HEAD"), sha(&fx, "HEAD~1")]);
        assert_eq!(planned.rewritten, 5);
        assert_eq!(planned.what(), "2 fixup commits");

        let done = fx.run(autosquash(&planned, &snap.head));
        assert_eq!(subjects(&fx), ["Write docs", "Add lexer", "Add parser", "Base"]);
        assert_eq!(git(&fx.dir, &["log", "-1", "--format=%B", "HEAD~1"]), "Add lexer\n\nLexing handles unicode too.");
        assert_eq!(git(&fx.dir, &["show", "HEAD~2:parser file.rs"]), "fn parse() { todo!() }");
        let said = format!("Folded 2 fixup commits into their commits; 5 commits rewritten after {}.", short(planned.base.unwrap()));
        assert!(done.result.as_ref().unwrap().ends_with(&said), "{:?}", done.result);
        assert!(plan(&done.snap.history, done.snap.head.oid.unwrap(), &stops(&done.snap)).is_none(), "nothing left to fold");

        // Undo puts the branch back as it was, fixups and all.
        let entry = done.undo.expect("autosquash is undoable");
        fx.run_as(entry.undo.clone(), Origin::Undo(entry));
        assert_eq!(git(&fx.dir, &["rev-parse", "HEAD"]), head.to_string());
    }

    #[test]
    fn commits_other_branches_have_are_never_rewritten() {
        let fx = branch_with_fixups("fixup-shared");
        // main now has the parser too: its fixup can no longer be folded,
        // the squash into the lexer still can.
        git(&fx.dir, &["branch", "-f", "main", "feature~4"]);
        let snap = fx.snapshot();
        let planned = plan(&snap.history, snap.head.oid.unwrap(), &stops(&snap)).unwrap();
        assert_eq!(planned.fixups, [sha(&fx, "HEAD")]);
        assert_eq!(planned.base, Some(sha(&fx, "main")));
        assert_eq!(planned.rewritten, 4);

        // With main past both, nothing is left.
        git(&fx.dir, &["branch", "-f", "main", "feature~3"]);
        let snap = fx.snapshot();
        assert_eq!(plan(&snap.history, snap.head.oid.unwrap(), &stops(&snap)), None);
    }

    #[test]
    fn the_first_commit_is_left_on_a_branch_with_merges() {
        let fx = Fixture::new("fixup-merge");
        fx.write("a.txt", "a\n");
        fx.commit_all("Add a");
        git(&fx.dir, &["checkout", "-q", "-b", "side"]);
        fx.write("side.txt", "side\n");
        fx.commit_all("Side work");
        git(&fx.dir, &["checkout", "-q", "main"]);
        git(&fx.dir, &["merge", "-q", "--no-ff", "-m", "Merge side", "side"]);
        git(&fx.dir, &["branch", "-D", "side"]);
        fx.write("a.txt", "a2\n");
        git(&fx.dir, &["add", "-A"]);
        // "Add a", the merge's first parent, is the root commit.
        fx.run(fixup(sha(&fx, "HEAD~1")));
        let snap = fx.snapshot();
        let found = survey(&snap.history, snap.head.oid.unwrap(), &[]);
        assert_eq!(found.plan, None);
        assert_eq!(found.left, [(sha(&fx, "HEAD"), ROOT.to_owned())]);

        // A fixup of a commit after the merge folds onto the merge, with
        // git's own autosquash.
        fx.write("b.txt", "b\n");
        fx.commit_all("Add b");
        fx.write("b.txt", "b2\n");
        git(&fx.dir, &["add", "-A"]);
        fx.run(fixup(sha(&fx, "HEAD")));
        let snap = fx.snapshot();
        let planned = plan(&snap.history, snap.head.oid.unwrap(), &[]).unwrap();
        assert_eq!(planned.base, Some(sha(&fx, "HEAD~2")));
        assert!(!planned.merges);
        fx.run(autosquash(&planned, &snap.head));
        assert_eq!(subjects(&fx)[..3], ["Add b", "fixup! Add a", "Merge side"]);
    }

    /// Base, then "Add a" on main; "side" from there with "Side one" and a
    /// fixup of "Add a"; "Main work" on main, the merge of side, then a
    /// squash into "Side one" made on main. The side branch is deleted.
    fn fixups_across_a_merge(fx: &Fixture) {
        fx.write("base.txt", "base\n");
        fx.commit_all("Base");
        fx.write("a.txt", "a\n");
        fx.commit_all("Add a");
        git(&fx.dir, &["checkout", "-q", "-b", "side"]);
        fx.write("s.txt", "one\n");
        fx.commit_all("Side one");
        fx.write("a.txt", "a, fixed on the side\n");
        git(&fx.dir, &["add", "-A"]);
        fx.run(fixup(sha(fx, "main")));
        git(&fx.dir, &["checkout", "-q", "main"]);
        fx.write("m.txt", "main\n");
        fx.commit_all("Main work");
        git(&fx.dir, &["merge", "-q", "--no-ff", "-m", "Merge side", "side"]);
        git(&fx.dir, &["branch", "-q", "-D", "side"]);
        fx.write("s.txt", "one, fixed on main\n");
        git(&fx.dir, &["add", "-A"]);
        fx.run(squash_into(sha(fx, "HEAD^2~1"), "Side one handles more."));
    }

    #[test]
    fn fixups_on_both_sides_of_a_merge_fold_into_their_commits() {
        let fx = Fixture::new("fixup-across");
        fixups_across_a_merge(&fx);
        let snap = fx.snapshot();
        let head = snap.head.oid.unwrap();
        let (on_main, on_side) = (sha(&fx, "HEAD"), sha(&fx, "HEAD~1^2"));
        let found = survey(&snap.history, head, &stops(&snap));
        assert!(found.left.is_empty(), "{:?}", found.left);
        let planned = found.plan.expect("both fold");
        assert_eq!(planned.fixups, [on_main, on_side]);
        assert_eq!(planned.base, Some(sha(&fx, "HEAD~4")));
        assert!(planned.merges);
        assert_eq!(planned.rewritten, 6);
        assert_eq!(
            planned.folds,
            [
                Fold { commit: on_side, into: sha(&fx, "HEAD~3"), how: FoldHow::Fixup },
                Fold { commit: on_main, into: sha(&fx, "HEAD~1^2~1"), how: FoldHow::Squash },
            ]
        );

        let done = fx.run(autosquash(&planned, &snap.head));
        let said = format!("Folded 2 fixup commits into their commits; 6 commits rewritten after {}.", short(planned.base.unwrap()));
        assert!(done.result.as_ref().unwrap().ends_with(&said), "{:?}", done.result);
        // Each fix went where its commit is, and the merge was made again.
        assert_eq!(
            shape(&fx),
            ["Add a <- Base", "Base <- ", "Main work <- Add a", "Merge side <- Main work + Side one", "Side one <- Add a"]
        );
        assert_eq!(git(&fx.dir, &["show", "HEAD~1:a.txt"]), "a, fixed on the side");
        assert_eq!(git(&fx.dir, &["show", "HEAD^2:s.txt"]), "one, fixed on main");
        // git's own squash message: the squash commit's text after its subject.
        assert_eq!(git(&fx.dir, &["log", "-1", "--format=%B", "HEAD^2"]), "Side one\n\nSide one handles more.");
        assert_eq!(fx.read("a.txt"), "a, fixed on the side\n");
        assert!(plan(&done.snap.history, done.snap.head.oid.unwrap(), &[]).is_none(), "nothing left to fold");

        let entry = done.undo.expect("autosquash is undoable");
        fx.run_as(entry.undo.clone(), Origin::Undo(entry));
        assert_eq!(sha(&fx, "HEAD"), head);
    }

    #[test]
    fn a_fold_into_a_merged_branch_that_conflicts_carries_on_after_continue() {
        let fx = Fixture::new("fixup-across-conflict");
        fx.write("base.txt", "base\n");
        fx.commit_all("Base");
        git(&fx.dir, &["checkout", "-q", "-b", "side"]);
        fx.write("s.txt", "1\n");
        fx.commit_all("Side one");
        fx.write("s.txt", "2\n");
        fx.commit_all("Side two");
        git(&fx.dir, &["checkout", "-q", "main"]);
        fx.write("m.txt", "main\n");
        fx.commit_all("Main work");
        git(&fx.dir, &["merge", "-q", "--no-ff", "-m", "Merge side", "side"]);
        git(&fx.dir, &["branch", "-q", "-D", "side"]);
        // Changes the line "Side two" wrote: next to "Side one" it conflicts.
        fx.write("s.txt", "3\n");
        git(&fx.dir, &["add", "-A"]);
        fx.run(fixup(sha(&fx, "HEAD^2~1")));
        let snap = fx.snapshot();
        let head = snap.head.oid.unwrap();
        let planned = plan(&snap.history, head, &[]).unwrap();
        assert!(planned.merges);

        let stopped = fx.run_as(autosquash(&planned, &snap.head), Origin::User);
        let message = stopped.result.expect_err("the fixup conflicts");
        assert!(message.contains("CONFLICT") || message.contains("could not apply"), "{message}");
        assert_eq!(stopped.snap.state, RepoState::Rebase);
        assert!(stopped.undo.is_none());
        fx.write("s.txt", "1, fixed\n");
        git(&fx.dir, &["add", "s.txt"]);
        // "Side two" then conflicts with the resolution in turn.
        let next = fx.run_as(crate::git::ops::continue_op(RepoState::Rebase).unwrap(), Origin::User);
        if next.snap.state == RepoState::Rebase {
            fx.write("s.txt", "2\n");
            git(&fx.dir, &["add", "s.txt"]);
            let done = fx.run(crate::git::ops::continue_op(RepoState::Rebase).unwrap());
            assert_eq!(done.snap.state, RepoState::Clean);
            check_finished(&fx, done.undo, head);
        } else {
            next.result.expect("finished");
            check_finished(&fx, next.undo, head);
        }
    }

    /// After the conflict above: "Side one" has the fix, the fixup is gone,
    /// and undo puts the branch back where it was.
    fn check_finished(fx: &Fixture, undo: Option<crate::git::ops::UndoEntry>, head: Oid) {
        assert_eq!(subjects(fx), ["Merge side", "Side two", "Side one", "Main work", "Base"]);
        assert_eq!(git(&fx.dir, &["show", "HEAD^2~1:s.txt"]), "1, fixed");
        assert_eq!(fx.read("s.txt"), "2\n");
        let entry = undo.expect("undoable once finished");
        fx.run_as(entry.undo.clone(), Origin::Undo(entry));
        assert_eq!(sha(fx, "HEAD"), head);
    }

    #[test]
    fn fixups_that_cannot_be_folded_say_why() {
        let fx = Fixture::new("fixup-left");
        fixups_across_a_merge(&fx);
        // The merged branch is back: what it has stays as it is, the fixup
        // made on it included.
        git(&fx.dir, &["branch", "side", "HEAD~1^2"]);
        let snap = fx.snapshot();
        let found = survey(&snap.history, snap.head.oid.unwrap(), &stops(&snap));
        assert_eq!(found.plan, None);
        assert_eq!(found.left, [(sha(&fx, "HEAD"), SHARED.to_owned()), (sha(&fx, "HEAD~1^2"), SHARED.to_owned())]);
        git(&fx.dir, &["branch", "-D", "side"]);

        // A branch merged later starts from a fixup whose commit is on
        // another line: it cannot move there.
        git(&fx.dir, &["reset", "-q", "--hard", "HEAD~1"]);
        fx.write("s.txt", "one, fixed\n");
        git(&fx.dir, &["add", "-A"]);
        fx.run(fixup(sha(&fx, "HEAD^2~1")));
        git(&fx.dir, &["checkout", "-q", "-b", "later"]);
        fx.write("l.txt", "later\n");
        fx.commit_all("Later work");
        git(&fx.dir, &["checkout", "-q", "main"]);
        fx.write("n.txt", "next\n");
        fx.commit_all("Next");
        git(&fx.dir, &["merge", "-q", "--no-ff", "-m", "Merge later", "later"]);
        git(&fx.dir, &["branch", "-q", "-D", "later"]);
        let snap = fx.snapshot();
        let found = survey(&snap.history, snap.head.oid.unwrap(), &[]);
        let fork = sha(&fx, "HEAD~2");
        assert!(found.left.contains(&(fork, STARTS_OTHERS.to_owned())), "{:?}", found.left);
        // The fixup made on the side still folds.
        let planned = found.plan.expect("the side's fixup folds");
        assert_eq!(planned.fixups, [sha(&fx, "HEAD~3^2")]);
    }

    #[test]
    fn the_root_commit_can_be_fixed_up() {
        let fx = Fixture::new("fixup-root");
        fx.write("a.txt", "a\n");
        fx.commit_all("First");
        fx.write("b.txt", "b\n");
        fx.commit_all("Second");
        fx.write("a.txt", "a, fixed\n");
        git(&fx.dir, &["add", "-A"]);
        fx.run(fixup(sha(&fx, "HEAD~1")));
        let snap = fx.snapshot();
        let planned = plan(&snap.history, snap.head.oid.unwrap(), &[]).unwrap();
        assert_eq!(planned.base, None);
        assert_eq!(planned.cost(), "rewrites all 3 commits");
        fx.run(autosquash(&planned, &snap.head));
        assert_eq!(subjects(&fx), ["Second", "First"]);
        assert_eq!(git(&fx.dir, &["show", "HEAD~1:a.txt"]), "a, fixed");
    }

    #[test]
    fn fixups_name_commits_by_sha_and_by_the_start_of_their_summary() {
        let fx = Fixture::new("fixup-names");
        fx.write("a.txt", "a\n");
        fx.commit_all("First");
        fx.write("b.txt", "b\n");
        fx.commit_all("Add the second file");
        fx.write("c.txt", "c\n");
        fx.commit_all("Third");
        let first = sha(&fx, "HEAD~2");
        fx.write("a.txt", "a2\n");
        git(&fx.dir, &["add", "-A"]);
        git(&fx.dir, &["commit", "-q", "-m", &format!("fixup! {}", &first.to_string()[..8])]);
        fx.write("b.txt", "b2\n");
        git(&fx.dir, &["add", "-A"]);
        git(&fx.dir, &["commit", "-q", "-m", "fixup! Add the"]);
        // Names nothing on the branch: stays as it is.
        fx.write("d.txt", "d\n");
        git(&fx.dir, &["add", "-A"]);
        git(&fx.dir, &["commit", "-q", "-m", "fixup! Something long gone"]);
        let snap = fx.snapshot();
        let found = survey(&snap.history, snap.head.oid.unwrap(), &[]);
        assert_eq!(found.left, [(sha(&fx, "HEAD"), NAMES_NOTHING.to_owned())]);
        let planned = found.plan.unwrap();
        assert_eq!(planned.fixups, [sha(&fx, "HEAD~1"), sha(&fx, "HEAD~2")]);
        assert_eq!(planned.base, None);
        fx.run(autosquash(&planned, &snap.head));
        assert_eq!(subjects(&fx), ["fixup! Something long gone", "Third", "Add the second file", "First"]);
    }

    #[test]
    fn a_fixup_needs_staged_changes() {
        let fx = Fixture::new("fixup-nothing");
        fx.write("a.txt", "a\n");
        fx.commit_all("First");
        fx.write("a.txt", "unstaged\n");
        let failed = fx.run_as(fixup(sha(&fx, "HEAD")), Origin::User);
        assert!(failed.result.is_err(), "{:?}", failed.result);

        // Undoing a fixup stages its changes again.
        git(&fx.dir, &["add", "-A"]);
        let done = fx.run(fixup(sha(&fx, "HEAD")));
        let entry = done.undo.unwrap();
        let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry));
        assert_eq!(subjects(&fx), ["First"]);
        assert_eq!(undone.snap.status.staged.len(), 1);
    }

    #[test]
    fn a_conflicting_fold_stops_as_a_rebase_in_progress() {
        let fx = Fixture::new("fixup-conflict");
        fx.write("a.txt", "one\n");
        fx.commit_all("First");
        fx.write("a.txt", "two\n");
        fx.commit_all("Second");
        // Fixes First's line as it was, after Second changed it: moving it
        // next to First conflicts.
        fx.write("a.txt", "three\n");
        git(&fx.dir, &["add", "-A"]);
        fx.run(fixup(sha(&fx, "HEAD~1")));
        let snap = fx.snapshot();
        let planned = plan(&snap.history, snap.head.oid.unwrap(), &[]).unwrap();
        let stopped = fx.run_as(autosquash(&planned, &snap.head), Origin::User);
        let message = stopped.result.expect_err("stops on the conflict");
        assert!(message.contains("CONFLICT") || message.contains("could not apply"), "{message}");
        assert_eq!(stopped.snap.state, RepoState::Rebase);
        fx.run(crate::git::ops::abort(RepoState::Rebase).unwrap());
        assert_eq!(git(&fx.dir, &["rev-parse", "HEAD"]), snap.head.oid.unwrap().to_string());
    }

    #[test]
    fn a_branch_that_moved_is_not_folded() {
        let fx = branch_with_fixups("fixup-moved");
        let snap = fx.snapshot();
        let planned = plan(&snap.history, snap.head.oid.unwrap(), &stops(&snap)).unwrap();
        fx.write("late.txt", "late\n");
        fx.commit_all("Late commit");
        let refused = fx.run_as(autosquash(&planned, &snap.head), Origin::User);
        assert!(refused.result.is_err());
        assert_eq!(subjects(&fx)[0], "Late commit");
    }
}
