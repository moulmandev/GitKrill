//! End-to-end tests of the backend: real repositories in a temp directory,
//! driven through the same worker threads the UI uses.
//!
//! [`Fixture`] is available to tests in other modules as `crate::tests::Fixture`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use crate::git::ops::{self, GitCmd, ResetMode, UndoEntry};
use crate::git::{
    ChangeKind, DiffSource, DiffTarget, FileChange, FileDiff, RepoSnapshot, RowKind,
};
use crate::worker::{Origin, RepoHandle, Response};

pub(crate) const TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct Fixture {
    pub(crate) dir: PathBuf,
    pub(crate) handle: RepoHandle,
}

pub(crate) struct Done {
    pub(crate) result: Result<String, String>,
    pub(crate) undo: Option<UndoEntry>,
    pub(crate) snap: Arc<RepoSnapshot>,
}

/// Runs plain git for test setup, panicking on failure.
pub(crate) fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

impl Fixture {
    pub(crate) fn new(name: &str) -> Self {
        let dir = std::env::temp_dir()
            .join("gitkrill-tests")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["config", "user.name", "Test User"]);
        git(&dir, &["config", "user.email", "test@example.com"]);
        git(&dir, &["config", "commit.gpgsign", "false"]);
        git(&dir, &["config", "core.autocrlf", "false"]);

        let handle = RepoHandle::spawn(dir.clone(), eframe::egui::Context::default(), 1000);
        Fixture { dir, handle }
    }

    pub(crate) fn write(&self, path: &str, content: &str) {
        let full = self.dir.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
    }

    pub(crate) fn read(&self, path: &str) -> String {
        std::fs::read_to_string(self.dir.join(path)).unwrap()
    }

    pub(crate) fn commit_all(&self, message: &str) {
        git(&self.dir, &["add", "-A"]);
        git(&self.dir, &["commit", "-q", "-m", message]);
    }

    pub(crate) fn snapshot(&self) -> Arc<RepoSnapshot> {
        // Drop anything stale, then wait for the answer to our own request.
        while self.handle.rx.try_recv().is_ok() {}
        self.handle.refresh();
        loop {
            match self.handle.rx.recv_timeout(TIMEOUT).expect("snapshot arrives") {
                Response::Snapshot(snap) => return snap,
                Response::LoadError(e) => panic!("load error: {e}"),
                _ => {}
            }
        }
    }

    pub(crate) fn run_as(&self, cmd: GitCmd, origin: Origin) -> Done {
        while self.handle.rx.try_recv().is_ok() {}
        self.handle.run(cmd, origin);
        let mut finished = None;
        loop {
            match self.handle.rx.recv_timeout(TIMEOUT).expect("operation finishes") {
                Response::OpDone { result, undo, .. } => finished = Some((result, undo)),
                // The operator requests a refresh after every command.
                Response::Snapshot(snap) => {
                    if let Some((result, undo)) = finished.take() {
                        return Done { result, undo, snap };
                    }
                }
                Response::LoadError(e) => panic!("load error: {e}"),
                _ => {}
            }
        }
    }

    pub(crate) fn run(&self, cmd: GitCmd) -> Done {
        let done = self.run_as(cmd, Origin::User);
        if let Err(e) = &done.result {
            panic!("command failed: {e}");
        }
        done
    }

    pub(crate) fn diff(&self, source: DiffSource, path: &str) -> Arc<FileDiff> {
        let target = DiffTarget { source, path: path.to_owned(), old_path: None };
        self.handle.diff(target.clone());
        loop {
            match self.handle.rx.recv_timeout(TIMEOUT).expect("diff arrives") {
                Response::Diff(t, result) if t == target => return result.expect("diff ok"),
                _ => {}
            }
        }
    }
}

impl Fixture {
    /// Runs a job on the reader thread and waits for its result.
    pub(crate) fn job<T: std::any::Any + Send>(
        &self,
        job: impl FnOnce(&crate::git::repo::Loader) -> T + Send + 'static,
    ) -> T {
        self.handle.job("test-job", job);
        loop {
            match self.handle.rx.recv_timeout(TIMEOUT).expect("job finishes") {
                Response::Job { key, result } if key == "test-job" => {
                    return match result.downcast::<T>() {
                        Ok(value) => *value,
                        Err(_) => panic!("the job returned a different type"),
                    };
                }
                _ => {}
            }
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn paths(files: &[FileChange]) -> Vec<(&str, ChangeKind)> {
    files.iter().map(|f| (f.path.as_str(), f.kind)).collect()
}

fn s(path: &str) -> Vec<String> {
    vec![path.to_owned()]
}

#[test]
fn stage_unstage_commit_and_undo_on_a_new_repository() {
    let fx = Fixture::new("first-commit");
    fx.write("a.txt", "one\n");

    let snap = fx.snapshot();
    assert!(snap.head.oid.is_none());
    assert_eq!(snap.head.branch.as_deref(), Some("main"));
    assert_eq!(paths(&snap.status.unstaged), [("a.txt", ChangeKind::Untracked)]);
    // Only the WIP row exists before the first commit.
    assert_eq!(snap.history.commits.len(), 1);
    assert_eq!(snap.history.commits[0].kind, RowKind::Wip);

    let snap = fx.run(ops::stage(&s("a.txt"))).snap;
    assert_eq!(paths(&snap.status.staged), [("a.txt", ChangeKind::Added)]);
    assert!(snap.status.unstaged.is_empty());

    let snap = fx.run(ops::unstage(&s("a.txt"), true)).snap;
    assert!(snap.status.staged.is_empty());
    assert_eq!(paths(&snap.status.unstaged), [("a.txt", ChangeKind::Untracked)]);

    fx.run(ops::stage_all());
    let done = fx.run(ops::commit("first\n\nwith a body", false));
    assert!(done.snap.status.is_clean());
    assert_eq!(done.snap.history.commits.len(), 1);
    assert_eq!(done.snap.history.commits[0].summary, "first");
    assert_eq!(done.snap.history.commits[0].body_preview, "with a body");
    assert_eq!(done.snap.head_message, "first\n\nwith a body");

    // Undoing the very first commit leaves the branch unborn, changes staged.
    let entry = done.undo.expect("commit is undoable");
    let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry.clone()));
    undone.result.expect("undo works");
    assert!(undone.snap.head.oid.is_none());
    assert_eq!(paths(&undone.snap.status.staged), [("a.txt", ChangeKind::Added)]);
}

#[test]
fn commit_undo_and_redo_restore_the_exact_commit() {
    let fx = Fixture::new("undo-redo");
    fx.write("a.txt", "one\n");
    fx.commit_all("base");
    let base = fx.snapshot().head.oid.unwrap();

    fx.write("a.txt", "two\n");
    let done = fx.run(ops::commit_everything("second"));
    let second = done.snap.head.oid.unwrap();
    assert_ne!(base, second);
    let entry = done.undo.expect("undoable");

    let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry.clone()));
    undone.result.unwrap();
    assert_eq!(undone.snap.head.oid, Some(base));
    assert_eq!(paths(&undone.snap.status.staged), [("a.txt", ChangeKind::Modified)]);

    let redone = fx.run_as(entry.redo.clone(), Origin::Redo(entry));
    redone.result.unwrap();
    assert_eq!(redone.snap.head.oid, Some(second));
    assert!(redone.snap.status.is_clean());
}

#[test]
fn undo_refuses_to_run_once_the_repository_has_moved_on() {
    let fx = Fixture::new("stale-undo");
    fx.write("a.txt", "one\n");
    fx.commit_all("base");

    fx.write("a.txt", "two\n");
    let entry = fx.run(ops::commit_everything("second")).undo.unwrap();

    // Someone commits from a terminal in the meantime.
    fx.write("a.txt", "three\n");
    fx.commit_all("third");
    let third = fx.snapshot().head.oid;

    let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry));
    assert!(undone.result.is_err());
    assert_eq!(undone.snap.head.oid, third);
}

#[test]
fn hunks_can_be_staged_unstaged_and_discarded_individually() {
    let fx = Fixture::new("hunks");
    let original: String = (1..=30).map(|i| format!("line {i}\n")).collect();
    fx.write("f.txt", &original);
    fx.commit_all("base");

    let edited = original.replace("line 2\n", "line two\n").replace("line 28\n", "line twenty-eight\n");
    fx.write("f.txt", &edited);

    let diff = fx.diff(DiffSource::Unstaged, "f.txt");
    assert_eq!(diff.hunks.len(), 2);
    assert_eq!((diff.additions, diff.deletions), (2, 2));
    assert_eq!(diff.kind, Some(ChangeKind::Modified));

    // Stage only the first hunk.
    let snap = fx.run(ops::apply_patch("Stage hunk", diff.hunk_patch(0).unwrap(), true, false)).snap;
    assert_eq!(paths(&snap.status.staged), [("f.txt", ChangeKind::Modified)]);
    assert_eq!(paths(&snap.status.unstaged), [("f.txt", ChangeKind::Modified)]);

    let staged = fx.diff(DiffSource::Staged, "f.txt");
    assert_eq!(staged.hunks.len(), 1);
    assert!(staged.hunks[0].lines.iter().any(|l| l.origin == '+' && l.text == "line two"));
    let unstaged = fx.diff(DiffSource::Unstaged, "f.txt");
    assert_eq!(unstaged.hunks.len(), 1);
    assert!(unstaged.hunks[0].lines.iter().any(|l| l.origin == '+' && l.text == "line twenty-eight"));

    // Unstage it again.
    let snap = fx.run(ops::apply_patch("Unstage hunk", staged.hunk_patch(0).unwrap(), true, true)).snap;
    assert!(snap.status.staged.is_empty());

    // Discard the second hunk from the working tree; the first survives.
    let diff = fx.diff(DiffSource::Unstaged, "f.txt");
    fx.run(ops::apply_patch("Discard hunk", diff.hunk_patch(1).unwrap(), false, true));
    assert_eq!(fx.read("f.txt"), original.replace("line 2\n", "line two\n"));
}

#[test]
fn hunk_staging_handles_a_missing_trailing_newline() {
    let fx = Fixture::new("no-eol");
    fx.write("f.txt", "alpha\nbeta");
    fx.commit_all("base");
    fx.write("f.txt", "alpha\ngamma");

    let diff = fx.diff(DiffSource::Unstaged, "f.txt");
    assert_eq!(diff.hunks.len(), 1);
    let snap = fx.run(ops::apply_patch("Stage hunk", diff.hunk_patch(0).unwrap(), true, false)).snap;
    // Everything is staged, byte for byte.
    assert!(snap.status.unstaged.is_empty());
    assert_eq!(git(&fx.dir, &["show", ":f.txt"]), "alpha\ngamma");
}

#[test]
fn crlf_files_round_trip_through_hunk_staging() {
    let fx = Fixture::new("crlf");
    fx.write("f.txt", "one\r\ntwo\r\nthree\r\n");
    fx.commit_all("base");
    fx.write("f.txt", "one\r\nTWO\r\nthree\r\n");

    let diff = fx.diff(DiffSource::Unstaged, "f.txt");
    // Line endings never leak into what is displayed.
    assert!(diff.hunks[0].lines.iter().all(|l| !l.text.contains('\r')));
    let snap = fx.run(ops::apply_patch("Stage hunk", diff.hunk_patch(0).unwrap(), true, false)).snap;
    assert!(snap.status.unstaged.is_empty());
    assert_eq!(paths(&snap.status.staged), [("f.txt", ChangeKind::Modified)]);
}

#[test]
fn awkward_paths_survive_staging_and_discarding() {
    let fx = Fixture::new("paths");
    fx.write("keep.txt", "keep\n");
    fx.commit_all("base");

    let odd = "dir with space/naïve [draft] *.txt";
    // '*' is not a legal file name on Windows.
    let odd = if cfg!(windows) { odd.replace(" *", "") } else { odd.to_owned() };
    fx.write(&odd, "hello\n");
    fx.write("keep.txt", "changed\n");

    let snap = fx.run(ops::stage(&s(&odd))).snap;
    assert_eq!(paths(&snap.status.staged), [(odd.as_str(), ChangeKind::Added)]);
    // A literal path must not act as a glob and drag other files along.
    assert_eq!(paths(&snap.status.unstaged), [("keep.txt", ChangeKind::Modified)]);

    let snap = fx.run(ops::unstage(&s(&odd), false)).snap;
    assert!(snap.status.staged.is_empty());

    let untracked = snap.status.unstaged.iter().find(|f| f.path == odd).unwrap().clone();
    assert_eq!(untracked.kind, ChangeKind::Untracked);
    let modified = snap.status.unstaged.iter().find(|f| f.path == "keep.txt").unwrap().clone();
    let snap = fx.run(ops::discard(&[untracked, modified])).snap;
    assert!(snap.status.is_clean());
    assert_eq!(fx.read("keep.txt"), "keep\n");
    assert!(!fx.dir.join(&odd).exists());
}

#[test]
fn staged_renames_are_reported_with_their_old_path() {
    let fx = Fixture::new("rename");
    fx.write("old.txt", "some content that is long enough to be matched\nline two\nline three\n");
    fx.commit_all("base");
    git(&fx.dir, &["mv", "old.txt", "new.txt"]);

    let snap = fx.snapshot();
    assert_eq!(snap.status.staged.len(), 1);
    let change = &snap.status.staged[0];
    assert_eq!(change.kind, ChangeKind::Renamed);
    assert_eq!(change.path, "new.txt");
    assert_eq!(change.old_path.as_deref(), Some("old.txt"));
}

#[test]
fn branches_can_be_created_deleted_and_undone() {
    let fx = Fixture::new("branches");
    fx.write("a.txt", "one\n");
    fx.commit_all("base");
    let base = fx.snapshot().head.oid.unwrap();

    let done = fx.run(ops::create_branch("feature/x", None, true));
    assert_eq!(done.snap.head.branch.as_deref(), Some("feature/x"));
    let entry = done.undo.expect("undoable");

    let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry.clone()));
    undone.result.unwrap();
    assert_eq!(undone.snap.head.branch.as_deref(), Some("main"));
    assert!(undone.snap.branches.iter().all(|b| b.name != "feature/x"));

    let redone = fx.run_as(entry.redo.clone(), Origin::Redo(entry));
    redone.result.unwrap();
    assert_eq!(redone.snap.head.branch.as_deref(), Some("feature/x"));

    fx.run(ops::checkout("main"));
    let done = fx.run(ops::delete_branch("feature/x", true));
    assert!(done.snap.branches.iter().all(|b| b.name != "feature/x"));
    let entry = done.undo.expect("delete is undoable");
    let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry));
    undone.result.unwrap();
    let restored = undone.snap.branches.iter().find(|b| b.name == "feature/x").expect("restored");
    assert_eq!(restored.oid, Some(base));
}

#[test]
fn merge_checkout_and_reset_move_head_and_can_be_undone() {
    let fx = Fixture::new("merge");
    fx.write("a.txt", "one\n");
    fx.commit_all("base");
    let base = fx.snapshot().head.oid.unwrap();

    fx.run(ops::create_branch("feature", None, true));
    fx.write("b.txt", "feature work\n");
    let tip = fx.run(ops::commit_everything("feature work")).snap.head.oid.unwrap();

    let done = fx.run(ops::checkout("main"));
    assert_eq!(done.snap.head.oid, Some(base));
    assert!(done.undo.is_some());

    let done = fx.run(ops::merge("feature"));
    assert_eq!(done.snap.head.oid, Some(tip));
    assert_eq!(done.snap.head.branch.as_deref(), Some("main"));
    let entry = done.undo.expect("merge is undoable");
    let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry));
    undone.result.unwrap();
    assert_eq!(undone.snap.head.oid, Some(base));
    assert!(!fx.dir.join("b.txt").exists());

    // Soft reset keeps the changes staged; undo puts HEAD back.
    fx.run(ops::merge("feature"));
    let done = fx.run(ops::reset(&base.to_string(), ResetMode::Soft));
    assert_eq!(done.snap.head.oid, Some(base));
    assert_eq!(paths(&done.snap.status.staged), [("b.txt", ChangeKind::Added)]);
    let entry = done.undo.unwrap();
    let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry));
    undone.result.unwrap();
    assert_eq!(undone.snap.head.oid, Some(tip));
    assert!(undone.snap.status.is_clean());
}

#[test]
fn stashes_show_up_in_the_graph_and_pop_back() {
    let fx = Fixture::new("stash");
    fx.write("a.txt", "one\n");
    fx.commit_all("base");
    let base = fx.snapshot().head.oid.unwrap();

    fx.write("a.txt", "changed\n");
    fx.write("untracked.txt", "new\n");
    let snap = fx.run(ops::stash_push("work in progress")).snap;
    assert!(snap.status.is_clean());
    assert_eq!(snap.stashes.len(), 1);
    assert!(snap.stashes[0].message.contains("work in progress"));

    // The stash sits directly above the commit it was made on, with only
    // that commit as its parent.
    let commits = &snap.history.commits;
    assert_eq!(commits.len(), 2);
    assert_eq!(commits[0].kind, RowKind::Stash);
    assert_eq!(commits[0].parents, vec![base]);
    assert_eq!(commits[1].oid, base);

    let snap = fx.run(ops::stash_pop(0)).snap;
    assert!(snap.stashes.is_empty());
    assert_eq!(fx.read("a.txt"), "changed\n");
    assert!(fx.dir.join("untracked.txt").exists());
    assert_eq!(snap.history.commits[0].kind, RowKind::Wip);
}

#[test]
fn commit_details_and_commit_diffs_describe_the_change() {
    let fx = Fixture::new("details");
    fx.write("a.txt", "one\n");
    fx.write("gone.txt", "bye\n");
    fx.commit_all("base");
    fx.write("a.txt", "one\ntwo\n");
    fx.write("sub/new.txt", "fresh\n");
    std::fs::remove_file(fx.dir.join("gone.txt")).unwrap();
    fx.commit_all("second\n\nbody text");
    let oid = fx.snapshot().head.oid.unwrap();

    fx.handle.details(oid, false);
    let details = loop {
        match fx.handle.rx.recv_timeout(TIMEOUT).unwrap() {
            Response::Details(o, result) if o == oid => break result.unwrap(),
            _ => {}
        }
    };
    assert_eq!(details.summary, "second");
    assert_eq!(details.body, "body text");
    assert_eq!(details.author, "Test User");
    assert_eq!(details.parents.len(), 1);
    assert_eq!(
        paths(&details.files),
        [
            ("a.txt", ChangeKind::Modified),
            ("gone.txt", ChangeKind::Deleted),
            ("sub/new.txt", ChangeKind::Added),
        ]
    );

    let diff = fx.diff(DiffSource::Commit(oid), "a.txt");
    assert_eq!((diff.additions, diff.deletions), (1, 0));
    let added = diff.hunks[0].lines.iter().find(|l| l.origin == '+').unwrap();
    assert_eq!((added.text.as_str(), added.new_no), ("two", Some(2)));
}

#[test]
fn tags_and_labels_attach_to_the_right_commits() {
    let fx = Fixture::new("labels");
    fx.write("a.txt", "one\n");
    fx.commit_all("base");
    let base = fx.snapshot().head.oid.unwrap();

    fx.run(ops::create_tag("v1", &base.to_string(), ""));
    fx.run(ops::create_tag("v1-annotated", &base.to_string(), "release notes"));
    let snap = fx.run(ops::create_branch("side", None, false)).snap;

    let names: Vec<&str> = snap.refs_by_oid[&base].iter().map(|l| l.name.as_str()).collect();
    // HEAD's branch first, then other branches, then tags.
    assert_eq!(names, ["main", "side", "v1", "v1-annotated"]);
    assert!(snap.refs_by_oid[&base][0].is_head);

    let snap = fx.run(ops::delete_tag("v1")).snap;
    assert_eq!(snap.tags.len(), 1);
}

#[test]
fn history_is_newest_first_with_children_before_parents() {
    let fx = Fixture::new("order");
    fx.write("a.txt", "0\n");
    fx.commit_all("c0");
    // Two branches whose commits interleave in time.
    git(&fx.dir, &["checkout", "-q", "-b", "side"]);
    for (i, branch) in [(1, "main"), (2, "side"), (3, "main"), (4, "side"), (5, "main")] {
        git(&fx.dir, &["checkout", "-q", branch]);
        fx.write(&format!("{branch}.txt"), &format!("{i}\n"));
        git(&fx.dir, &["add", "-A"]);
        let date = format!("2026-02-0{i}T00:00:00");
        let out = Command::new("git")
            .current_dir(&fx.dir)
            .args(["commit", "-q", "-m", &format!("c{i}")])
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .output()
            .unwrap();
        assert!(out.status.success());
    }

    let snap = fx.snapshot();
    let order: Vec<&str> = snap.history.commits.iter().map(|c| c.summary.as_str()).collect();
    assert_eq!(order, ["c5", "c4", "c3", "c2", "c1", "c0"]);

    let position = |oid| snap.row_of(oid).unwrap();
    for (row, commit) in snap.history.commits.iter().enumerate() {
        for parent in &commit.parents {
            assert!(position(*parent) > row, "{} is above its parent", commit.summary);
        }
    }
}

#[test]
fn history_loads_in_pages() {
    let fx = Fixture::new("pages");
    for i in 0..12 {
        fx.write("a.txt", &format!("{i}\n"));
        fx.commit_all(&format!("c{i}"));
    }

    fx.handle.set_limit(5);
    let snap = fx.snapshot();
    assert_eq!(snap.history.commits.len(), 5);
    assert!(snap.history.has_more);
    assert_eq!(snap.history.commits[0].summary, "c11");

    fx.handle.set_limit(50);
    let snap = fx.snapshot();
    assert_eq!(snap.history.commits.len(), 12);
    assert!(!snap.history.has_more);
}

#[test]
fn jobs_run_on_the_reader_thread_with_repository_access() {
    let fx = Fixture::new("jobs");
    fx.write("a.txt", "one\n");
    fx.commit_all("base");

    let summary = fx.job(|loader| {
        let head = loader.repo().head().unwrap().peel_to_commit().unwrap();
        String::from_utf8_lossy(head.summary_bytes().unwrap_or_default()).into_owned()
    });
    assert_eq!(summary, "base");
}

#[test]
fn hidden_branches_folders_and_tags_leave_the_graph() {
    use crate::git::hidden::{self, HEADS, Hidden, TAGS};

    let fx = Fixture::new("hidden");
    fx.write("a.txt", "base\n");
    fx.commit_all("base");
    for name in ["t3/one", "t3/two", "feature"] {
        git(&fx.dir, &["checkout", "-q", "-b", name, "main"]);
        fx.write("a.txt", &format!("{name}\n"));
        fx.commit_all(&format!("on {name}"));
    }
    git(&fx.dir, &["checkout", "-q", "main"]);
    git(&fx.dir, &["tag", "v1", "feature"]);

    let summaries = |snap: &RepoSnapshot| -> Vec<String> {
        let mut found: Vec<String> = snap.history.commits.iter().map(|c| c.summary.clone()).collect();
        found.sort();
        found
    };
    let labelled = |snap: &RepoSnapshot, name: &str| {
        snap.refs_by_oid.values().flatten().any(|label| label.name == name)
    };
    assert_eq!(summaries(&fx.snapshot()), ["base", "on feature", "on t3/one", "on t3/two"]);

    // The folder: its commits and labels go, the branches stay listed.
    let mut rules = Hidden::default();
    rules.set(&hidden::folder(HEADS, "t3"), true);
    fx.handle.set_hidden(rules.clone());
    let snap = fx.snapshot();
    assert_eq!(summaries(&snap), ["base", "on feature"]);
    assert!(!labelled(&snap, "t3/one") && labelled(&snap, "feature"));
    assert_eq!(snap.branches.len(), 4);

    // One branch shown again inside the hidden folder.
    rules.set(&hidden::local("t3/two"), false);
    fx.handle.set_hidden(rules.clone());
    assert_eq!(summaries(&fx.snapshot()), ["base", "on feature", "on t3/two"]);

    // A commit reached by a hidden branch and a shown tag stays, unlabelled
    // by the branch; hiding the tags too takes it away.
    rules.set(&hidden::local("feature"), true);
    fx.handle.set_hidden(rules.clone());
    let snap = fx.snapshot();
    assert!(summaries(&snap).contains(&"on feature".to_owned()));
    assert!(!labelled(&snap, "feature") && labelled(&snap, "v1"));
    rules.set(TAGS, true);
    fx.handle.set_hidden(rules.clone());
    assert_eq!(summaries(&fx.snapshot()), ["base", "on t3/two"]);

    // The checked out branch is always shown, even in a hidden folder.
    git(&fx.dir, &["checkout", "-q", "t3/one"]);
    let snap = fx.snapshot();
    assert!(summaries(&snap).contains(&"on t3/one".to_owned()));
    assert!(labelled(&snap, "t3/one"));
}
