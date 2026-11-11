//! Merging a branch into the checked-out one in the way the user picks:
//! git's default (a fast-forward when possible, a merge commit otherwise),
//! always a merge commit, a fast-forward only, or a squash merge, which
//! stages what the branch adds as one change and leaves the commit to the
//! user.
//!
//! A squash merge leaves no `MERGE_HEAD`, so neither git nor libgit2 sees an
//! operation in progress. What marks one is `SQUASH_MSG` in the git
//! directory: the message git prepared for the commit that finishes it,
//! listing the commits squashed. The snapshot reads it
//! ([`read_squash_message`]); the commit panel starts its message from it
//! ([`prepared_message`]) and offers to abort. Git deletes the file when
//! that commit is made and on `git reset`, which is how a squash merge is
//! aborted or undone ([`UndoKind::Staged`]).

use super::internal::Internal;
use super::ops::{GitCmd, Step, UndoKind};
use super::repo::lossy;

/// How `git merge` brings a branch in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeMode {
    /// Git's own choice (and `merge.ff` if set): a fast-forward when the
    /// branch is ahead, a merge commit otherwise.
    Default,
    /// Always a merge commit (`--no-ff`).
    NoFastForward,
    /// Only a fast-forward; refuses otherwise (`--ff-only`).
    FastForwardOnly,
    /// Stage the result without committing (`--squash`).
    Squash,
}

impl MergeMode {
    /// The modes offered besides [`MergeMode::Default`], in menu order.
    pub const OPTIONS: [MergeMode; 3] = [MergeMode::NoFastForward, MergeMode::FastForwardOnly, MergeMode::Squash];

    /// The git option, if any.
    pub fn flag(self) -> Option<&'static str> {
        match self {
            MergeMode::Default => None,
            MergeMode::NoFastForward => Some("--no-ff"),
            MergeMode::FastForwardOnly => Some("--ff-only"),
            MergeMode::Squash => Some("--squash"),
        }
    }

    /// The entry in a "Merge options" submenu.
    pub fn title(self) -> &'static str {
        match self {
            MergeMode::Default => "Merge (fast-forward when possible)",
            MergeMode::NoFastForward => "Create a merge commit",
            MergeMode::FastForwardOnly => "Fast-forward only",
            MergeMode::Squash => "Squash into one change",
        }
    }

    /// What the mode does with these branches, for a tooltip.
    pub fn explain(self, source: &str, into: &str) -> String {
        match self {
            MergeMode::Default => {
                format!("Moves {into} forward to {source} when it can, and makes a merge commit otherwise.")
            }
            MergeMode::NoFastForward => format!(
                "Records a merge commit even when {into} could simply move forward to {source} (git merge --no-ff)."
            ),
            MergeMode::FastForwardOnly => format!(
                "Moves {into} forward to {source}, and changes nothing when {into} has commits that {source} \
                 lacks (git merge --ff-only)."
            ),
            MergeMode::Squash => format!(
                "Stages everything {source} adds as one change, with its message ready in the commit panel. \
                 Nothing is committed until you commit it (git merge --squash)."
            ),
        }
    }

    /// The whole action as a palette entry or a drop menu shows it.
    pub fn action(self, source: &str, into: &str) -> String {
        match self {
            MergeMode::Default => format!("Merge {source} into {into}"),
            MergeMode::NoFastForward => format!("Merge {source} into {into} with a merge commit"),
            MergeMode::FastForwardOnly => format!("Merge {source} into {into} (fast-forward only)"),
            MergeMode::Squash => format!("Squash merge {source} into {into}"),
        }
    }
}

/// Label of the command merging `source` in this mode: what shows while it
/// runs, in its toasts and in the undo menu.
pub fn label(source: &str, mode: MergeMode) -> String {
    match mode {
        MergeMode::Default | MergeMode::NoFastForward => format!("Merge {source}"),
        MergeMode::FastForwardOnly => format!("Merge {source} (fast-forward only)"),
        MergeMode::Squash => format!("Squash merge {source}"),
    }
}

/// The `git merge` arguments for `source` in this mode.
fn args(source: &str, mode: MergeMode) -> Vec<&str> {
    let mut args = vec!["merge"];
    // A merge commit takes git's prepared message; the editor is never
    // involved (`GIT_MERGE_AUTOEDIT=no` as well).
    if matches!(mode, MergeMode::Default | MergeMode::NoFastForward) {
        args.push("--no-edit");
    }
    args.extend(mode.flag());
    args.push(source);
    args
}

/// Merges `source` into the checked-out branch `into`. Undone like any
/// merge, by moving the branch back; a squash merge, which moves nothing,
/// by putting the index and the files back as HEAD has them.
pub fn merge(source: &str, into: &str, mode: MergeMode) -> GitCmd {
    let done = match mode {
        MergeMode::Default => format!("Merged {source} into {into}"),
        MergeMode::NoFastForward => format!("Merged {source} into {into} with a merge commit"),
        MergeMode::FastForwardOnly => format!("Fast-forwarded {into} to {source}"),
        MergeMode::Squash => format!("Squashed {source} into staged changes"),
    };
    let undo = if mode == MergeMode::Squash { UndoKind::Staged } else { UndoKind::HeadMove };
    let cmd = GitCmd::new(label(source, mode), &args(source, mode)).undoable(undo).done(done);
    squash_guard(cmd, mode)
}

/// Said when a squash merge would start over staged changes.
pub const STAGED: &str = "Commit or unstage the staged changes first: a squash merge stages its own \
                          changes, and undoing it unstages everything staged.";

/// For a squash merge: refuse to start over staged changes, which git
/// accepts when the branch is ahead and which would then end up in the
/// squashed commit (and be put back by its undo); and say what Continue
/// does if it stops on conflicts.
fn squash_guard(mut cmd: GitCmd, mode: MergeMode) -> GitCmd {
    if mode != MergeMode::Squash {
        return cmd;
    }
    let check = Internal::new("check that nothing is staged", |workdir| {
        let args = ["diff", "--cached", "--name-only"].map(str::to_owned);
        let staged = super::cli::run(workdir, &args, Default::default())?;
        if staged.trim().is_empty() { Ok(String::new()) } else { Err(STAGED.to_owned()) }
    });
    cmd.steps.insert(0, Step::internal(check));
    cmd.after_conflict("Continue then commits the result with the message in the commit panel.")
}

/// Checks out `into`, then merges `source` into it: for a branch dropped on
/// one that is not checked out. Like the plain checkout-and-merge of a drop,
/// it is one command and not undoable as a whole.
pub fn checkout_and_merge(source: &str, into: &str, mode: MergeMode) -> GitCmd {
    let what = match mode {
        MergeMode::Squash => "squash merge",
        _ => "merge",
    };
    let cmd = GitCmd::new(format!("Checkout {into} and {what} {source}"), &["checkout", into])
        .step(&args(source, mode))
        .done(format!("Checked out {into} and {what}d {source}"));
    squash_guard(cmd, mode)
}

/// Undoes a squash merge that is staged, conflicted or not: the index and
/// the files it changed go back to HEAD, other local changes stay, and git
/// forgets the prepared message (`git reset --merge`, which is what
/// `git merge --abort` runs for a real merge).
pub fn abort_squash() -> GitCmd {
    GitCmd::new("Abort squash merge", &["reset", "--merge"]).done("Aborted the squash merge")
}

// ---- the prepared message ----------------------------------------------------

/// The message git prepared for a squash merge waiting to be committed, if
/// one is. Called with every snapshot: one file that is usually absent.
pub fn read_squash_message(repo: &git2::Repository) -> Option<String> {
    std::fs::read(repo.path().join("SQUASH_MSG")).ok().map(|bytes| lossy(&bytes))
}

/// One commit listed in `SQUASH_MSG`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Squashed {
    pub sha: String,
    pub summary: String,
    pub body: String,
}

/// The commits `SQUASH_MSG` lists, newest first as git writes them:
///
/// ```text
/// Squashed commit of the following:
///
/// commit 3f2a...
/// Author: A U Thor <author@example.com>
/// Date:   ...
///
///     Summary line
///
///     Body
/// ```
pub fn squashed_commits(text: &str) -> Vec<Squashed> {
    let mut commits: Vec<(String, Vec<&str>)> = Vec::new();
    let mut in_message = false;
    for line in text.lines() {
        if let Some(sha) = line.strip_prefix("commit ") {
            commits.push((sha.trim().to_owned(), Vec::new()));
            in_message = false;
            continue;
        }
        let Some((_, message)) = commits.last_mut() else { continue };
        if !in_message {
            // The headers (Author, Date, Merge) end at the first blank line.
            in_message = line.trim().is_empty();
            continue;
        }
        match line.strip_prefix("    ") {
            Some(text) => message.push(text),
            None if line.trim().is_empty() => message.push(""),
            // Not git's layout after all (a conflict note, say): stop here.
            None => in_message = false,
        }
    }
    commits
        .into_iter()
        .filter_map(|(sha, lines)| {
            let text = lines.join("\n");
            let text = text.trim();
            if text.is_empty() {
                return None;
            }
            let (summary, body) = text.split_once('\n').unwrap_or((text, ""));
            Some(Squashed { sha, summary: summary.trim().to_owned(), body: body.trim().to_owned() })
        })
        .collect()
}

/// The summary and description the commit panel starts from for a squash
/// merge. One commit keeps its own message. Several get a summary naming
/// the branches when they are known (`names`: the branch squashed and the
/// one it went into) and a description listing their summaries, oldest
/// first.
pub fn prepared_message(text: &str, names: Option<(&str, &str)>) -> (String, String) {
    let commits = squashed_commits(text);
    match commits.as_slice() {
        [] => (String::new(), String::new()),
        [one] => (one.summary.clone(), one.body.clone()),
        many => {
            let summary = match names {
                Some((source, into)) => format!("Squash merge {source} into {into}"),
                None => format!("Squash {} commits", many.len()),
            };
            let list: Vec<String> = many.iter().rev().map(|c| format!("* {}", c.summary)).collect();
            (summary, list.join("\n"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{Fixture, git};
    use crate::worker::Origin;

    #[test]
    fn modes_become_git_merge_options() {
        let steps = |mode| merge("feature/x", "main", mode).steps.last().unwrap().args.clone();
        assert_eq!(steps(MergeMode::Default), ["merge", "--no-edit", "feature/x"]);
        assert_eq!(steps(MergeMode::NoFastForward), ["merge", "--no-edit", "--no-ff", "feature/x"]);
        assert_eq!(steps(MergeMode::FastForwardOnly), ["merge", "--ff-only", "feature/x"]);
        assert_eq!(steps(MergeMode::Squash), ["merge", "--squash", "feature/x"]);
        let drop = checkout_and_merge("origin/x", "main", MergeMode::Squash);
        assert_eq!(drop.label, "Checkout main and squash merge origin/x");
        assert!(drop.steps[0].internal.is_some(), "the index is checked before the checkout");
        assert_eq!(drop.steps[1].args, ["checkout", "main"]);
        assert_eq!(drop.steps[2].args, ["merge", "--squash", "origin/x"]);
        assert!(matches!(merge("x", "main", MergeMode::Squash).undo, UndoKind::Staged));
        assert!(matches!(merge("x", "main", MergeMode::FastForwardOnly).undo, UndoKind::HeadMove));
    }

    #[test]
    fn the_squash_message_lists_the_commits() {
        let text = "Squashed commit of the following:\n\n\
                    commit 1111111111111111111111111111111111111111\n\
                    Author: A <a@example.com>\n\
                    Date:   Wed Nov 11 14:16:11 2026 +0100\n\n    \
                    Add c\n    \n    \
                    Because c matters.\n\n\
                    commit 2222222222222222222222222222222222222222\n\
                    Merge: 1234567 89abcde\n\
                    Author: B <b@example.com>\n\
                    Date:   Wed Nov 11 14:16:11 2026 +0100\n\n    \
                    Add b\n";
        let commits = squashed_commits(text);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].summary, "Add c");
        assert_eq!(commits[0].body, "Because c matters.");
        assert_eq!(commits[1].summary, "Add b");
        assert_eq!(commits[1].sha, "2222222222222222222222222222222222222222");

        assert_eq!(
            prepared_message(text, Some(("feature", "main"))),
            ("Squash merge feature into main".to_owned(), "* Add b\n* Add c".to_owned())
        );
        assert_eq!(prepared_message(text, None).0, "Squash 2 commits");
        // One commit keeps its message as it was.
        let one = &text[..text.find("commit 2222").unwrap()];
        assert_eq!(prepared_message(one, None), ("Add c".to_owned(), "Because c matters.".to_owned()));
        assert_eq!(prepared_message("", None), (String::new(), String::new()));
    }

    fn diverged(name: &str) -> Fixture {
        let fx = Fixture::new(name);
        fx.write("base.txt", "base\n");
        fx.commit_all("base");
        git(&fx.dir, &["checkout", "-q", "-b", "feature"]);
        fx.write("feature one.txt", "one\n");
        fx.commit_all("Add feature one");
        fx.write("fé two.txt", "two\n");
        fx.commit_all("Add feature two");
        git(&fx.dir, &["checkout", "-q", "main"]);
        fx
    }

    #[test]
    fn a_merge_commit_even_when_a_fast_forward_would_do() {
        let fx = diverged("merge-no-ff");
        let before = git(&fx.dir, &["rev-parse", "HEAD"]);
        let done = fx.run(merge("feature", "main", MergeMode::NoFastForward));
        assert_eq!(git(&fx.dir, &["rev-list", "--count", "--merges", "HEAD"]), "1");
        assert_eq!(git(&fx.dir, &["log", "-1", "--format=%s"]), "Merge branch 'feature'");
        // Undo moves main back where it was.
        let entry = done.undo.expect("a merge is undoable");
        fx.run_as(entry.undo.clone(), Origin::Undo(entry.clone()));
        assert_eq!(git(&fx.dir, &["rev-parse", "HEAD"]), before);
    }

    #[test]
    fn fast_forward_only_refuses_a_diverged_branch() {
        let fx = diverged("merge-ff-only");
        fx.write("main.txt", "main\n");
        fx.commit_all("main moves on");
        let before = git(&fx.dir, &["rev-parse", "HEAD"]);
        let failed = fx.run_as(merge("feature", "main", MergeMode::FastForwardOnly), Origin::User);
        assert!(failed.result.is_err(), "{:?}", failed.result);
        assert!(failed.undo.is_none());
        assert_eq!(git(&fx.dir, &["rev-parse", "HEAD"]), before);

        // Once main has nothing of its own, it moves forward.
        git(&fx.dir, &["reset", "-q", "--hard", "HEAD~1"]);
        let done = fx.run(merge("feature", "main", MergeMode::FastForwardOnly));
        assert_eq!(git(&fx.dir, &["rev-parse", "HEAD"]), git(&fx.dir, &["rev-parse", "feature"]));
        assert!(done.undo.is_some());
    }

    #[test]
    fn a_squash_merge_stages_the_branch_and_undoes_cleanly() {
        let fx = diverged("merge-squash");
        fx.write("main.txt", "main\n");
        fx.commit_all("main moves on");
        // A local change elsewhere is left alone throughout.
        fx.write("base.txt", "edited\n");
        let head = git(&fx.dir, &["rev-parse", "HEAD"]);

        let done = fx.run(merge("feature", "main", MergeMode::Squash));
        assert_eq!(git(&fx.dir, &["rev-parse", "HEAD"]), head, "nothing is committed");
        let staged: Vec<&str> = done.snap.status.staged.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(staged, ["feature one.txt", "fé two.txt"]);
        let message = done.snap.squash_message.clone().expect("the snapshot sees the squash");
        assert_eq!(
            prepared_message(&message, Some(("feature", "main"))).1,
            "* Add feature one\n* Add feature two"
        );

        let entry = done.undo.expect("a squash merge is undoable");
        assert_eq!(entry.label, "Squash merge feature");
        let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry.clone()));
        assert!(undone.result.is_ok(), "{:?}", undone.result);
        assert!(undone.snap.status.staged.is_empty());
        assert!(undone.snap.squash_message.is_none());
        assert!(!fx.dir.join("feature one.txt").exists());
        assert_eq!(fx.read("base.txt"), "edited\n");

        // Redo stages it again, and committing finishes it.
        let redone = fx.run_as(entry.redo.clone(), Origin::Redo(entry));
        assert!(redone.result.is_ok(), "{:?}", redone.result);
        assert_eq!(redone.snap.status.staged.len(), 2);
        let committed = fx.run(crate::git::ops::commit("Squash merge feature into main", false));
        assert!(committed.snap.squash_message.is_none(), "git forgets the message once committed");
        assert_eq!(git(&fx.dir, &["rev-list", "--count", "--merges", "HEAD"]), "0");
    }

    #[test]
    fn a_conflicting_squash_merge_is_aborted_with_the_local_changes_kept() {
        let fx = diverged("merge-squash-conflict");
        fx.write("feature one.txt", "main's own\n");
        fx.commit_all("main adds the same file");
        fx.write("base.txt", "edited\n");
        let failed = fx.run_as(merge("feature", "main", MergeMode::Squash), Origin::User);
        let message = failed.result.expect_err("the squash stops on the conflict");
        assert!(message.contains("CONFLICT"), "{message}");
        assert!(failed.snap.squash_message.is_some());
        assert!(failed.snap.status.unstaged.iter().any(|f| f.kind == crate::git::ChangeKind::Conflicted));

        let aborted = fx.run(abort_squash());
        assert!(aborted.snap.squash_message.is_none());
        assert!(aborted.snap.status.staged.is_empty());
        assert_eq!(fx.read("feature one.txt"), "main's own\n");
        assert_eq!(fx.read("base.txt"), "edited\n");
    }

    /// Git itself takes a fast-forward squash over staged changes (feature
    /// is ahead of main here); the guard step does not.
    #[test]
    fn a_squash_merge_refuses_staged_changes() {
        let fx = diverged("merge-squash-staged");
        fx.write("base.txt", "staged\n");
        git(&fx.dir, &["add", "base.txt"]);
        let failed = fx.run_as(merge("feature", "main", MergeMode::Squash), Origin::User);
        assert_eq!(failed.result.unwrap_err(), STAGED);
        assert!(failed.undo.is_none());
        assert!(failed.snap.squash_message.is_none());
        let staged: Vec<&str> = failed.snap.status.staged.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(staged, ["base.txt"]);
    }
}
