//! Fixup commits, and autosquash to fold them in.
//!
//! `git commit --fixup=<commit>` records the staged changes as a commit
//! titled `fixup! <summary of that commit>`; `--squash=<commit>` as
//! `squash! <summary>`, whose own message is added to that commit's when it
//! is folded in. Autosquash (`git rebase -i --autosquash`) moves each of them
//! after the commit it names and folds it in (and `amend!` commits, which
//! replace its message).
//!
//! [`plan`] finds, from the loaded history, the fixup commits of the
//! checked-out branch whose commit is on that branch too, and where a rebase
//! folding them all in has to start. It matches them the way git does, so
//! git folds exactly those. [`autosquash`] runs the rebase without an
//! editor: `GIT_SEQUENCE_EDITOR` (`true`, set for every git command by
//! `git::cli::command`, the same as `-c sequence.editor=:`) accepts the todo
//! list git arranged as it is, and `GIT_EDITOR` keeps the combined message of
//! each squash.

use std::collections::HashMap;

use super::internal::Internal;
use super::ops::{GitCmd, Step, UndoKind};
use super::{Head, History, Oid, RowKind, short};

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
    /// What the rebase starts from: the parent of the oldest commit
    /// something is folded into; `None` when that is the root commit.
    pub base: Option<Oid>,
    /// The fixup, squash and amend commits folded in, newest first.
    pub fixups: Vec<Oid>,
    /// How many commits the rebase rewrites, fixups included.
    pub rewritten: usize,
    /// HEAD when planned.
    pub head: Oid,
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

/// Plans an autosquash of HEAD's branch, going by the loaded history.
///
/// The branch is HEAD's first-parent line back to the first merge commit,
/// the first commit any of `stops` reaches (the tips of the other branches:
/// what they share is never rewritten), the end of the loaded history or
/// [`MAX_COMMITS`]. On it, each fixup is matched to the commit it names as
/// git does: an older commit with that exact summary (the oldest), else one
/// whose SHA starts with it, else one whose summary starts with it. `None`
/// when no fixup on the branch names a commit on it.
pub fn plan(history: &History, head: Oid, stops: &[Oid]) -> Option<Autosquash> {
    let row_of = |oid: &Oid| history.index_of.get(oid).map(|row| *row as usize);
    // Rows of the branch, newest first.
    let mut chain: Vec<usize> = Vec::new();
    let mut row = row_of(&head)?;
    loop {
        let commit = &history.commits[row];
        if commit.kind != RowKind::Commit || commit.parents.len() > 1 || chain.len() >= MAX_COMMITS {
            break;
        }
        chain.push(row);
        match commit.parents.first().and_then(row_of) {
            Some(parent) => row = parent,
            None => break,
        }
    }
    let bottom = *chain.last()?;

    // What the other branches reach. Rows are ordered children first, so
    // one pass down from the highest of their tips to the bottom of the
    // branch marks it all.
    let tips: Vec<usize> = stops.iter().filter_map(row_of).filter(|row| *row <= bottom).collect();
    if let Some(&top) = tips.iter().min() {
        let mut reached = vec![false; bottom - top + 1];
        for row in &tips {
            reached[row - top] = true;
        }
        for row in top..=bottom {
            if !reached[row - top] {
                continue;
            }
            for parent in history.commits[row].parents.iter().filter_map(row_of) {
                if parent <= bottom {
                    reached[parent - top] = true;
                }
            }
        }
        if let Some(cut) = chain.iter().position(|row| *row >= top && reached[row - top]) {
            chain.truncate(cut);
        }
    }

    // Oldest first, as git lists the commits it rebases.
    let todo: Vec<usize> = chain.iter().rev().copied().collect();
    let summary = |i: usize| history.commits[todo[i]].summary.as_str();
    let mut by_summary: HashMap<&str, usize> = HashMap::new();
    // (fixup, what it is folded into), as positions in `todo`.
    let mut folds: Vec<(usize, usize)> = Vec::new();
    for i in 0..todo.len() {
        let found = fixup_target(summary(i)).and_then(|text| {
            by_summary
                .get(text)
                .copied()
                .or_else(|| {
                    let hex = text.len() >= 4 && text.bytes().all(|b| b.is_ascii_hexdigit());
                    let text = text.to_ascii_lowercase();
                    (0..i).find(|&j| hex && history.commits[todo[j]].oid.to_string().starts_with(&text))
                })
                .or_else(|| (0..i).find(|&j| summary(j).starts_with(text)))
        });
        match found {
            Some(target) => folds.push((i, target)),
            None => {
                by_summary.entry(summary(i)).or_insert(i);
            }
        }
    }

    let oldest = folds.iter().map(|(_, target)| *target).min()?;
    Some(Autosquash {
        base: history.commits[todo[oldest]].parents.first().copied(),
        fixups: folds.iter().rev().map(|(fixup, _)| history.commits[todo[*fixup]].oid).collect(),
        rewritten: todo.len() - oldest,
        head,
    })
}

/// Folds the planned fixups in with `git rebase -i --autosquash`, from
/// HEAD as it was planned (`head`). Undone like a rebase; one that stops on
/// a conflict is a rebase in progress, finished or aborted from the banner.
pub fn autosquash(plan: &Autosquash, head: &Head) -> GitCmd {
    let base = plan.base.map(|base| base.to_string());
    let mut args = vec!["-c", "advice.mergeConflict=false", "rebase", "-i", "--autosquash", "--quiet"];
    args.push(base.as_deref().unwrap_or("--root"));
    let mut cmd = GitCmd::new(AUTOSQUASH_LABEL, &args)
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
    fn a_merge_commit_ends_the_branch() {
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
        fx.run(fixup(sha(&fx, "HEAD~1")));
        let snap = fx.snapshot();
        assert_eq!(plan(&snap.history, snap.head.oid.unwrap(), &[]), None, "its commit is past the merge");

        // A fixup of a commit after the merge folds onto the merge.
        fx.write("b.txt", "b\n");
        fx.commit_all("Add b");
        fx.write("b.txt", "b2\n");
        git(&fx.dir, &["add", "-A"]);
        fx.run(fixup(sha(&fx, "HEAD")));
        let snap = fx.snapshot();
        let planned = plan(&snap.history, snap.head.oid.unwrap(), &[]).unwrap();
        assert_eq!(planned.base, Some(sha(&fx, "HEAD~2")));
        fx.run(autosquash(&planned, &snap.head));
        assert_eq!(subjects(&fx)[..3], ["Add b", "fixup! Add a", "Merge side"]);
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
        let planned = plan(&snap.history, snap.head.oid.unwrap(), &[]).unwrap();
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
