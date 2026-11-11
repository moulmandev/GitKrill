//! Undo for an operation that stopped on conflicts and was finished later.
//!
//! A rebase, merge, cherry-pick or revert that stops on conflicts ends its
//! command with an error, so the operation thread cannot build its undo
//! entry then. [`Carry`] keeps what the entry needs (the command, with the
//! refs it moves noted before it ran, and HEAD before it) while git waits for
//! the user, and builds the entry once a command that carries the operation
//! on (Continue, Skip, or the commit that concludes a merge) leaves the
//! repository clean: undo then puts everything back as it was before the
//! operation started, however many stops it made on the way.
//!
//! The operation is recognised by what git wrote down when it began (the
//! branch and commit a rebase started from, the commit being merged, ...).
//! If that changed or went away between two of GitKrill's commands, because
//! it was aborted or finished in a terminal, the entry is dropped: an undo
//! must never be offered for something other than what it would undo.

use std::path::Path;

use git2::Repository;

use super::ops::{GitCmd, UndoEntry, UndoKind, build_undo};
use super::repo::read_head;
use super::{Head, Oid};

/// What an undo entry needs from before the operation, kept while it waits.
struct Held {
    /// The command as it ran, refs of [`UndoKind::Refs`] noted before.
    cmd: GitCmd,
    before: Head,
    deleted_at: Option<Oid>,
    /// [`marker`] of the operation while it waits.
    marker: String,
}

/// Owned by the operation thread; one per repository.
#[derive(Default)]
pub struct Carry {
    held: Option<Held>,
}

impl Carry {
    /// Called before a command from the user runs. Drops what is held when
    /// the operation it belongs to is no longer the one in progress, and
    /// says whether this command may carry it on.
    pub fn start(&mut self, workdir: &Path) -> bool {
        let Some(held) = &self.held else { return false };
        let now = Repository::open(workdir).ok().and_then(|repo| marker(&repo));
        if now.as_deref() == Some(held.marker.as_str()) {
            return true;
        }
        self.held = None;
        false
    }

    /// Called after a command from the user ran. `before` and `deleted_at`
    /// are what the operation thread noted for the command's own undo
    /// (`before` is `None` when it has none); `continued` is what
    /// [`Carry::start`] said.
    ///
    /// Returns the undo entry of a held operation that this command
    /// finished, which takes the place of the command's own.
    pub fn end(
        &mut self,
        workdir: &Path,
        cmd: &GitCmd,
        succeeded: bool,
        before: Option<&Head>,
        deleted_at: Option<Oid>,
        continued: bool,
    ) -> Option<UndoEntry> {
        let repo = Repository::open(workdir).ok()?;
        let now = marker(&repo);

        if continued && let Some(held) = self.held.take() {
            match &now {
                // Still waiting (a file was staged, or the next commit
                // stopped as well).
                Some(marker) if *marker == held.marker => {
                    self.held = Some(held);
                    return None;
                }
                None if succeeded && carries_on(cmd) => return finish(&repo, held),
                // Aborted, or replaced by something else.
                _ => {}
            }
        }

        // The command stopped half way, with its own undo still to build.
        if !succeeded
            && let (Some(marker), Some(before)) = (now, before)
            && !matches!(cmd.undo, UndoKind::None)
        {
            self.held = Some(Held { cmd: cmd.clone(), before: before.clone(), deleted_at, marker });
        }
        None
    }

    /// Whether an entry is waiting for its operation to finish.
    #[cfg(test)]
    pub fn is_holding(&self) -> bool {
        self.held.is_some()
    }
}

/// The entry for a held operation that has just finished.
fn finish(repo: &Repository, mut held: Held) -> Option<UndoEntry> {
    if let UndoKind::Refs(moves) = &mut held.cmd.undo {
        for m in moves {
            m.after = repo.refname_to_id(&m.name).ok();
        }
    }
    build_undo(&held.cmd, &held.before, &read_head(repo), held.deleted_at)
}

/// Whether `cmd` carries a stopped operation on, rather than abandoning it:
/// `--continue`, `--skip`, or a commit (which concludes a merge or a single
/// cherry-pick). `--abort`, `--quit` and resets do not.
fn carries_on(cmd: &GitCmd) -> bool {
    cmd.steps.iter().any(|step| {
        let args = &step.args;
        // Past `-c name=value` pairs to the subcommand.
        let mut at = 0;
        while args.get(at).is_some_and(|arg| arg == "-c") {
            at += 2;
        }
        args.get(at).is_some_and(|sub| sub == "commit")
            || args[at.min(args.len())..].iter().any(|arg| arg == "--continue" || arg == "--skip")
    })
}

/// What identifies the operation in progress, or `None` when there is none.
///
/// Each is written by git when the operation begins and stays the same until
/// it ends: the branch and commit a rebase started from, the commit a
/// sequence of cherry-picks or reverts started on, the commit being merged
/// or picked.
pub fn marker(repo: &Repository) -> Option<String> {
    let dir = repo.path();
    let read = |name: &str| {
        std::fs::read_to_string(dir.join(name)).ok().map(|text| text.trim().to_owned()).filter(|t| !t.is_empty())
    };
    for state in ["rebase-merge", "rebase-apply"] {
        if let Some(orig) = read(&format!("{state}/orig-head")) {
            let name = read(&format!("{state}/head-name")).unwrap_or_default();
            return Some(format!("rebase {name} {orig}"));
        }
    }
    if let Some(head) = read("sequencer/head") {
        return Some(format!("sequence {head}"));
    }
    ["MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD"]
        .into_iter()
        .find_map(|name| read(name).map(|oid| format!("{name} {oid}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::RepoState;
    use crate::git::ops::{self, RefMove};
    use crate::tests::{Fixture, git};
    use crate::worker::Origin;

    /// main and topic both change `f.txt` after `base`, so merging or
    /// rebasing one onto the other stops on a conflict.
    fn clash(name: &str) -> Fixture {
        let fx = Fixture::new(name);
        fx.write("f.txt", "base\n");
        fx.write("other file.txt", "other\n");
        fx.commit_all("base");
        git(&fx.dir, &["checkout", "-q", "-b", "topic"]);
        fx.write("f.txt", "topic\n");
        fx.write("tópico.txt", "topic\n");
        fx.commit_all("topic one");
        fx.write("topic two.txt", "two\n");
        fx.commit_all("topic two");
        git(&fx.dir, &["checkout", "-q", "main"]);
        fx.write("f.txt", "main\n");
        fx.commit_all("main work");
        fx
    }

    fn rev(fx: &Fixture, name: &str) -> Oid {
        Oid::from_str(&git(&fx.dir, &["rev-parse", name])).unwrap()
    }

    fn resolve(fx: &Fixture) {
        fx.write("f.txt", "resolved\n");
        git(&fx.dir, &["add", "f.txt"]);
    }

    fn continue_rebase() -> GitCmd {
        ops::continue_op(RepoState::Rebase).unwrap()
    }

    #[test]
    fn a_rebase_continued_after_conflicts_can_be_undone_and_redone() {
        let fx = clash("carry-rebase");
        git(&fx.dir, &["checkout", "-q", "topic"]);
        let tip = rev(&fx, "topic");

        let stopped = fx.run_as(ops::rebase("main"), Origin::User);
        assert!(stopped.result.unwrap_err().contains("CONFLICT"));
        assert!(stopped.undo.is_none());
        // Staging the resolution keeps the entry waiting.
        let staged = fx.run(ops::stage(&["f.txt".to_owned()]));
        assert!(staged.undo.is_none());
        fx.write("f.txt", "resolved\n");
        fx.run(ops::stage(&["f.txt".to_owned()]));

        let done = fx.run(continue_rebase());
        assert_eq!(done.snap.state, RepoState::Clean);
        let rebased = done.snap.head.oid.unwrap();
        assert_ne!(rebased, tip);
        let entry = done.undo.expect("the finished rebase is undoable");
        assert_eq!(entry.label, "Rebase onto main");

        let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry.clone()));
        undone.result.expect("undo works");
        assert_eq!(undone.snap.head.oid, Some(tip));
        assert_eq!(undone.snap.head.branch.as_deref(), Some("topic"));
        assert_eq!(fx.read("f.txt"), "topic\n");
        assert!(undone.snap.status.is_clean());

        let redone = fx.run_as(entry.redo.clone(), Origin::Redo(entry));
        redone.result.expect("redo works");
        assert_eq!(redone.snap.head.oid, Some(rebased));
    }

    #[test]
    fn a_rebase_that_stops_twice_is_undone_to_where_it_started() {
        let fx = clash("carry-rebase-twice");
        git(&fx.dir, &["checkout", "-q", "topic"]);
        // The second topic commit now clashes with main as well.
        fx.write("f.txt", "topic again\n");
        fx.commit_all("topic three");
        let tip = rev(&fx, "topic");

        assert!(fx.run_as(ops::rebase("main"), Origin::User).result.is_err());
        resolve(&fx);
        let again = fx.run_as(continue_rebase(), Origin::User);
        assert!(again.result.is_err(), "the next commit stops too");
        assert!(again.undo.is_none());
        resolve(&fx);
        let done = fx.run(continue_rebase());
        let entry = done.undo.expect("undoable once finished");
        let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry));
        undone.result.expect("undo works");
        assert_eq!(undone.snap.head.oid, Some(tip));
    }

    #[test]
    fn skipping_the_last_commit_finishes_the_rebase_too() {
        let fx = clash("carry-skip");
        git(&fx.dir, &["checkout", "-q", "topic"]);
        let tip = rev(&fx, "topic");
        // Only the first topic commit clashes; skip it.
        assert!(fx.run_as(ops::rebase("main"), Origin::User).result.is_err());
        let done = fx.run(GitCmd::new("Skip", &["rebase", "--skip"]));
        assert_eq!(done.snap.state, RepoState::Clean);
        let entry = done.undo.expect("undoable");
        let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry));
        undone.result.expect("undo works");
        assert_eq!(undone.snap.head.oid, Some(tip));
    }

    #[test]
    fn an_aborted_or_abandoned_operation_leaves_nothing_to_undo() {
        let fx = clash("carry-abort");
        git(&fx.dir, &["checkout", "-q", "topic"]);
        assert!(fx.run_as(ops::rebase("main"), Origin::User).result.is_err());
        let aborted = fx.run(ops::abort(RepoState::Rebase).unwrap());
        assert!(aborted.undo.is_none());
        assert_eq!(aborted.snap.state, RepoState::Clean);

        // Aborted in a terminal, then a commit made in the app: that commit
        // gets its own undo, not the rebase's.
        assert!(fx.run_as(ops::rebase("main"), Origin::User).result.is_err());
        git(&fx.dir, &["rebase", "--abort"]);
        fx.write("new file.txt", "new\n");
        let commit = fx.run(ops::commit_everything("after the abort"));
        assert_eq!(commit.undo.expect("the commit is undoable").label, "Commit");

        // Finished in a terminal: nothing is offered for it either.
        assert!(fx.run_as(ops::rebase("main"), Origin::User).result.is_err());
        resolve(&fx);
        git(&fx.dir, &["-c", "core.editor=true", "rebase", "--continue"]);
        let refreshed = fx.run(ops::stage_all());
        assert!(refreshed.undo.is_none());
    }

    #[test]
    fn a_merge_concluded_by_a_commit_is_undone_as_the_merge() {
        let fx = clash("carry-merge");
        let main = rev(&fx, "main");
        let stopped = fx.run_as(ops::merge("topic"), Origin::User);
        assert!(stopped.result.unwrap_err().contains("CONFLICT"));
        resolve(&fx);
        let done = fx.run(ops::commit("Merge topic, by hand", false));
        let merge = done.snap.head.oid.unwrap();
        let entry = done.undo.expect("the merge is undoable");
        assert_eq!(entry.label, "Merge topic");
        let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry.clone()));
        undone.result.expect("undo works");
        assert_eq!(undone.snap.head.oid, Some(main));
        assert!(undone.snap.status.is_clean());
        let redone = fx.run_as(entry.redo.clone(), Origin::Redo(entry));
        redone.result.expect("redo works");
        assert_eq!(redone.snap.head.oid, Some(merge));
    }

    #[test]
    fn a_checkout_and_merge_that_stopped_is_undone_on_both_branches() {
        let fx = clash("carry-refs");
        git(&fx.dir, &["checkout", "-q", "topic"]);
        let (main, topic) = (rev(&fx, "main"), rev(&fx, "topic"));
        // What dragging topic onto main and choosing Merge runs.
        let cmd = GitCmd::new("Checkout main and merge topic", &["checkout", "main"])
            .step(&["merge", "--no-edit", "topic"])
            .undoable(UndoKind::Refs(vec![RefMove::new("refs/heads/main")]));
        assert!(fx.run_as(cmd, Origin::User).result.is_err());
        resolve(&fx);
        let done = fx.run(GitCmd::new("Continue merge", &["merge", "--continue"]));
        assert_eq!(done.snap.head.branch.as_deref(), Some("main"));
        let entry = done.undo.expect("undoable");
        let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry));
        undone.result.expect("undo works");
        assert_eq!(undone.snap.head.branch.as_deref(), Some("topic"));
        assert_eq!(rev(&fx, "main"), main);
        assert_eq!(rev(&fx, "topic"), topic);
    }

    #[test]
    fn the_marker_names_the_operation_in_progress() {
        let fx = clash("carry-marker");
        let repo = || Repository::open(&fx.dir).unwrap();
        assert_eq!(marker(&repo()), None);
        let _ = std::process::Command::new("git").current_dir(&fx.dir).args(["merge", "topic"]).output();
        let merge = marker(&repo()).unwrap();
        assert!(merge.starts_with("MERGE_HEAD "), "{merge}");
        git(&fx.dir, &["merge", "--abort"]);
        git(&fx.dir, &["checkout", "-q", "topic"]);
        let _ = std::process::Command::new("git").current_dir(&fx.dir).args(["rebase", "main"]).output();
        let rebase = marker(&repo()).unwrap();
        assert!(rebase.starts_with("rebase refs/heads/topic "), "{rebase}");

        let mut carry = Carry::default();
        assert!(!carry.start(&fx.dir));
        assert!(!carry.is_holding());
    }

    #[test]
    fn commands_that_carry_an_operation_on() {
        assert!(carries_on(&continue_rebase()));
        assert!(carries_on(&GitCmd::new("Skip", &["rebase", "--skip"])));
        assert!(carries_on(&ops::commit("m", false)));
        assert!(carries_on(&GitCmd::new("x", &["-c", "core.editor=true", "commit"])));
        assert!(!carries_on(&ops::abort(RepoState::Merge).unwrap()));
        assert!(!carries_on(&ops::discard_all()));
        assert!(!carries_on(&GitCmd::new("x", &["-c"])));
    }
}
