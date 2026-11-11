//! Background threads for one open repository.
//!
//! * The **reader** owns the libgit2 handle and answers refresh / diff /
//!   commit-detail requests. Nothing on the UI thread ever touches the disk.
//! * The **operator** runs mutating `git` commands one at a time, then asks
//!   the reader for a fresh snapshot.
//! * A file watcher flags the repository as dirty when something relevant
//!   changes on disk, so edits made in an editor or terminal show up live.

use std::any::Any;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use eframe::egui;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::git::hidden::Hidden;
use crate::git::ops::{GitCmd, UndoEntry, UndoKind, build_undo};
use crate::git::repo::{Loader, branch_oid, read_head};
use crate::git::{CommitDetails, DiffTarget, FileDiff, Oid, RepoSnapshot, cli};

/// Arbitrary read-only work on the reader thread; see [`RepoHandle::job`].
type Job = Box<dyn FnOnce(&Loader) -> Box<dyn Any + Send> + Send>;

enum Request {
    Refresh,
    Job { key: String, job: Job },
    Diff(DiffTarget, crate::git::diff::Mode),
    Details { oid: Oid, is_stash: bool },
}

/// Why a command was run; undo/redo carry their entry so it can be moved to
/// the opposite stack once the command succeeds.
pub enum Origin {
    User,
    Undo(UndoEntry),
    Redo(UndoEntry),
}

// A handful of these exist at a time and each is consumed within a frame, so
// boxing the larger variant would only add an allocation.
#[allow(clippy::large_enum_variant)]
pub enum Response {
    Snapshot(Arc<RepoSnapshot>),
    LoadError(String),
    Diff(DiffTarget, Result<Arc<FileDiff>, String>),
    Details(Oid, Result<Arc<CommitDetails>, String>),
    /// The value returned by a [`RepoHandle::job`], under the key it was given.
    Job { key: String, result: Box<dyn Any + Send> },
    OpDone {
        cmd: GitCmd,
        result: Result<String, String>,
        undo: Option<UndoEntry>,
        origin: Origin,
    },
}

pub struct RepoHandle {
    req_tx: Sender<Request>,
    op_tx: Sender<(GitCmd, Origin)>,
    pub rx: Receiver<Response>,
    limit: Arc<AtomicUsize>,
    /// Refs left out of the graph; read by every refresh.
    hidden: Arc<Mutex<Hidden>>,
    /// Set by the file watcher; cleared by the UI when it schedules a refresh.
    pub fs_dirty: Arc<AtomicBool>,
    _watcher: Option<RecommendedWatcher>,
}

impl RepoHandle {
    pub fn spawn(workdir: PathBuf, ctx: egui::Context, limit: usize) -> Self {
        let (req_tx, req_rx) = channel::<Request>();
        let (op_tx, op_rx) = channel::<(GitCmd, Origin)>();
        let (res_tx, rx) = channel::<Response>();
        let limit = Arc::new(AtomicUsize::new(limit));
        let hidden = Arc::new(Mutex::new(Hidden::default()));
        let fs_dirty = Arc::new(AtomicBool::new(false));

        {
            let workdir = workdir.clone();
            let ctx = ctx.clone();
            let res_tx = res_tx.clone();
            let limit = limit.clone();
            let hidden = hidden.clone();
            thread::Builder::new()
                .name("git-reader".into())
                .spawn(move || reader(&workdir, req_rx, res_tx, &limit, &hidden, &ctx))
                .expect("spawn reader thread");
        }
        {
            let workdir = workdir.clone();
            let ctx = ctx.clone();
            let req_tx = req_tx.clone();
            thread::Builder::new()
                .name("git-operator".into())
                .spawn(move || operator(&workdir, op_rx, res_tx, req_tx, &ctx))
                .expect("spawn operator thread");
        }

        let watcher = start_watcher(&workdir, fs_dirty.clone(), ctx);
        RepoHandle { req_tx, op_tx, rx, limit, hidden, fs_dirty, _watcher: watcher }
    }

    pub fn refresh(&self) {
        let _ = self.req_tx.send(Request::Refresh);
    }

    pub fn set_limit(&self, limit: usize) {
        self.limit.store(limit, Ordering::Relaxed);
    }

    /// Which refs the graph leaves out, from the next refresh on.
    pub fn set_hidden(&self, hidden: Hidden) {
        *self.hidden.lock().unwrap_or_else(|e| e.into_inner()) = hidden;
    }

    /// Loads a diff as the diff view shows it now: in the mode set in
    /// Preferences > Editor, which the answer carries (`FileDiff::mode`).
    pub fn diff(&self, target: DiffTarget) {
        self.diff_with(target, crate::git::diff::mode());
    }

    pub fn diff_with(&self, target: DiffTarget, mode: crate::git::diff::Mode) {
        let _ = self.req_tx.send(Request::Diff(target, mode));
    }

    pub fn details(&self, oid: Oid, is_stash: bool) {
        let _ = self.req_tx.send(Request::Details { oid, is_stash });
    }

    /// Runs `job` on the reader thread, with access to the libgit2 handle, and
    /// delivers its return value as [`Response::Job`] under `key`.
    ///
    /// This is how features read from the repository without blocking the UI
    /// and without each needing its own request and response types. Jobs run
    /// in the order they were submitted.
    pub fn job<T: Any + Send>(
        &self,
        key: impl Into<String>,
        job: impl FnOnce(&Loader) -> T + Send + 'static,
    ) {
        let job: Job = Box::new(move |loader| Box::new(job(loader)));
        let _ = self.req_tx.send(Request::Job { key: key.into(), job });
    }

    pub fn run(&self, cmd: GitCmd, origin: Origin) {
        let _ = self.op_tx.send((cmd, origin));
    }
}

fn reader(
    workdir: &Path,
    req_rx: Receiver<Request>,
    res_tx: Sender<Response>,
    limit: &AtomicUsize,
    hidden: &Mutex<Hidden>,
    ctx: &egui::Context,
) {
    let mut loader = match Loader::open(workdir) {
        Ok(loader) => loader,
        Err(e) => {
            let _ = res_tx.send(Response::LoadError(e));
            ctx.request_repaint();
            return;
        }
    };

    while let Ok(first) = req_rx.recv() {
        // Collapse whatever queued up while we were busy: only the newest
        // diff / details request matters and one refresh covers them all.
        let mut refresh = false;
        let mut diff = None;
        let mut details = None;
        let mut jobs = Vec::new();
        let mut next = Some(first);
        while let Some(req) = next {
            match req {
                Request::Refresh => refresh = true,
                Request::Diff(target, mode) => diff = Some((target, mode)),
                Request::Details { oid, is_stash } => details = Some((oid, is_stash)),
                Request::Job { key, job } => jobs.push((key, job)),
            }
            next = req_rx.try_recv().ok();
        }

        // Cheap, user-visible answers first.
        if let Some((target, mode)) = diff {
            let result = loader.diff_with(&target, mode).map(Arc::new);
            let _ = res_tx.send(Response::Diff(target, result));
            ctx.request_repaint();
        }
        if let Some((oid, is_stash)) = details {
            let result = loader.commit_details(oid, is_stash).map(Arc::new);
            let _ = res_tx.send(Response::Details(oid, result));
            ctx.request_repaint();
        }
        for (key, job) in jobs {
            let result = job(&loader);
            let _ = res_tx.send(Response::Job { key, result });
            ctx.request_repaint();
        }
        if refresh {
            let hidden = hidden.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let response = match loader.snapshot(limit.load(Ordering::Relaxed), &hidden) {
                Ok(snapshot) => Response::Snapshot(Arc::new(snapshot)),
                Err(e) => Response::LoadError(e),
            };
            if res_tx.send(response).is_err() {
                return;
            }
            ctx.request_repaint();
        }
    }
}

fn operator(
    workdir: &Path,
    op_rx: Receiver<(GitCmd, Origin)>,
    res_tx: Sender<Response>,
    req_tx: Sender<Request>,
    ctx: &egui::Context,
) {
    while let Ok((mut cmd, origin)) = op_rx.recv() {
        let wants_undo = matches!(origin, Origin::User) && !matches!(cmd.undo, UndoKind::None);

        let (before, deleted_at) = if wants_undo {
            match git2::Repository::open(workdir) {
                Ok(repo) => {
                    let deleted_at = match &cmd.undo {
                        UndoKind::BranchDelete(name) => branch_oid(&repo, name),
                        _ => None,
                    };
                    note_refs(&repo, &mut cmd.undo, false);
                    crate::git::internal::note_files(&mut cmd.undo, false);
                    (Some(read_head(&repo)), deleted_at)
                }
                Err(_) => (None, None),
            }
        } else {
            (None, None)
        };

        let mut output = String::new();
        let mut result = Ok(());
        if let Some(expected) = &cmd.expect_head {
            let current = git2::Repository::open(workdir).ok().map(|repo| read_head(&repo));
            if current.as_ref() != Some(expected) {
                result = Err("The repository has moved on since then, so this step can no \
                              longer be replayed safely."
                    .to_owned());
            }
        }
        for step in &cmd.steps {
            if result.is_err() {
                break;
            }
            let ran = match &step.internal {
                Some(internal) => internal.run(workdir),
                None => cli::run(
                    workdir,
                    &step.args,
                    cli::Options {
                        stdin: step.stdin.as_deref(),
                        literal_paths: step.literal_paths,
                        background: cmd.background,
                        env: &step.env,
                    },
                ),
            };
            match ran {
                Ok(text) => {
                    if !text.is_empty() {
                        if !output.is_empty() {
                            output.push('\n');
                        }
                        output.push_str(&text);
                    }
                }
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }

        let undo = match (&result, before) {
            (Ok(()), Some(before)) => git2::Repository::open(workdir).ok().and_then(|repo| {
                note_refs(&repo, &mut cmd.undo, true);
                crate::git::internal::note_files(&mut cmd.undo, true);
                build_undo(&cmd, &before, &read_head(&repo), deleted_at)
            }),
            _ => None,
        };

        // Preferences > General > Keep submodules up to date. It runs while
        // the operation still shows as busy; only a failure is reported, on
        // its own, since the operation itself did what it was asked.
        let submodules = result.as_ref().ok().and_then(|()| crate::prefs::general::run_submodule_update(workdir, &cmd));

        let done = Response::OpDone { cmd, result: result.map(|()| output), undo, origin };
        if res_tx.send(done).is_err() {
            return;
        }
        if let Some((update, e)) = submodules {
            let _ = res_tx.send(Response::OpDone { cmd: update, result: Err(e), undo: None, origin: Origin::User });
        }
        let _ = req_tx.send(Request::Refresh);
        ctx.request_repaint();
    }
}

/// Records where the refs of an [`UndoKind::Refs`] command point, before
/// (`after` false) or after it ran.
fn note_refs(repo: &git2::Repository, undo: &mut UndoKind, after: bool) {
    if let UndoKind::Refs(moves) = undo {
        for m in moves {
            let oid = repo.refname_to_id(&m.name).ok();
            if after { m.after = oid } else { m.before = oid }
        }
    }
}

fn start_watcher(
    workdir: &Path,
    dirty: Arc<AtomicBool>,
    ctx: egui::Context,
) -> Option<RecommendedWatcher> {
    // The watcher gets its own handle purely to answer "is this path ignored?".
    let repo = git2::Repository::open(workdir).ok()?;
    let gitdir = repo.path().to_path_buf();
    let root = workdir.to_path_buf();
    // Worktrees and submodules keep their git directory outside the tree.
    let external_gitdir = (!workdir.join(".git").is_dir()).then(|| gitdir.clone());

    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else { return };
        if matches!(event.kind, EventKind::Access(_)) {
            return;
        }
        if event.paths.iter().any(|p| is_relevant(p, &root, &gitdir, &repo))
            && !dirty.swap(true, Ordering::Relaxed)
        {
            ctx.request_repaint();
        }
    })
    .ok()?;

    watcher.watch(workdir, RecursiveMode::Recursive).ok()?;
    if let Some(gitdir) = external_gitdir {
        let _ = watcher.watch(&gitdir, RecursiveMode::Recursive);
    }
    Some(watcher)
}

/// Filters file events down to the ones that can change what we display.
fn is_relevant(path: &Path, root: &Path, gitdir: &Path, repo: &git2::Repository) -> bool {
    let in_git = path
        .strip_prefix(root)
        .ok()
        .and_then(|rel| rel.strip_prefix(".git").ok())
        .or_else(|| path.strip_prefix(gitdir).ok());

    if let Some(rel) = in_git {
        if path.extension().is_some_and(|e| e == "lock") {
            return false;
        }
        return match rel.components().next().and_then(|c| c.as_os_str().to_str()) {
            // Object writes always come with a ref or index update.
            Some("objects" | "hooks" | "lfs" | "info") => false,
            // Only the stash lives purely in the reflog.
            Some("logs") => rel.ends_with("stash"),
            _ => true,
        };
    }

    match path.strip_prefix(root) {
        // Build output and other ignored files must not trigger refreshes.
        Ok(rel) => !repo.is_path_ignored(rel).unwrap_or(false),
        Err(_) => true,
    }
}
