//! Application state shared by the UI modules.

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::git::ops::{GitCmd, UndoEntry, UndoKind};
use crate::git::{
    CommitDetails, DiffSource, DiffTarget, FileDiff, Oid, RepoSnapshot, RowKind,
};
use crate::worker::{Origin, RepoHandle, Response};

/// Commits loaded up front unless Preferences > Commit Graph says otherwise;
/// more are pulled in as the graph is scrolled.
pub const INITIAL_LIMIT: usize = 3000;
/// Minimum gap between refreshes triggered by file system activity.
const FS_DEBOUNCE: Duration = Duration::from_millis(350);
/// How soon after opening a repository the first automatic fetch runs.
const FIRST_AUTO_FETCH: Duration = Duration::from_secs(8);

/// An operation that is queued or running.
pub struct BusyOp {
    pub label: String,
    /// Routine and background work is not worth a progress toast.
    pub quiet: bool,
    pub since: Instant,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Selection {
    None,
    Wip,
    Commit(Oid),
}

/// A full-size view that takes the place of the graph. At most one is open;
/// Escape or its own close button dismisses it.
pub enum Overlay {
    /// Resolving one conflicted file.
    Conflict(crate::ui::conflict::ConflictView),
    /// History or blame of one file.
    History(crate::ui::history::HistoryView),
    /// Planning an interactive rebase.
    Rebase(crate::ui::rebase::RebaseView),
}

pub struct DiffView {
    pub target: DiffTarget,
    /// `None` while the first result is still loading.
    pub data: Option<Result<Arc<FileDiff>, String>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Success,
    Error,
}

pub struct Toast {
    pub kind: ToastKind,
    pub title: String,
    pub body: String,
    pub born: Instant,
    /// A button that opens a page: its label and the address. Only for
    /// addresses built from validated parts (`hosting::RepoId`), since it is
    /// handed to the operating system.
    pub link: Option<(String, String)>,
}

pub enum Dialog {
    CreateBranch { at: Option<String>, at_label: String, name: String, checkout: bool },
    RenameBranch { old: String, name: String },
    CreateTag { at: String, at_label: String, name: String, message: String },
    Stash { message: String },
    Confirm { title: String, body: String, button: String, danger: bool, cmd: GitCmd },
    PushRejected,
    Clone { url: String, dest: String },
    About,
    /// A newer release to update to; see `ui::update`.
    Update(std::sync::Arc<crate::update::Release>),
    /// What runs on a self-hosted server (`crate::ui::hosting::connect_body`).
    ConnectServer { host: String, path: String, choice: Option<crate::hosting::ServerKind> },
    /// "Create pull request"; see `ui::new_pull`.
    NewPull(Box<crate::ui::new_pull::NewPullForm>),
    /// Gitflow: start a feature, release or hotfix; see `ui::gitflow`.
    GitflowStart(crate::ui::gitflow::StartForm),
    /// Gitflow: finish one; see `ui::gitflow`.
    GitflowFinish(Box<crate::ui::gitflow::FinishForm>),
}

impl Dialog {
    pub fn confirm(title: &str, body: impl Into<String>, button: &str, cmd: GitCmd) -> Self {
        Dialog::Confirm {
            title: title.to_owned(),
            body: body.into(),
            button: button.to_owned(),
            danger: true,
            cmd,
        }
    }
}

/// Things only the application shell can do.
pub enum AppAction {
    OpenDialog,
    /// Opens the repository in a new tab, or brings forward the tab that
    /// already shows it.
    Open(PathBuf),
    /// Shows the repository in the current tab, in place of the one there,
    /// unless another tab has it already. Falls back to a new tab while the
    /// current one has work that would be lost (a commit message being
    /// written, a conflict being resolved, an operation running).
    Switch(PathBuf),
    /// Asks for a folder and adds it as a workspace (`crate::workspace`),
    /// then opens the list it was asked from to show what is found.
    AddWorkspace(crate::workspace::Opens),
    CloseTab(usize),
    Activate(usize),
    Clone { url: String, dest: PathBuf },
    InitDialog,
    /// Opens the preferences page at a section (or moves it there).
    OpenPreferences(crate::ui::prefs::Section),
    ClosePreferences,
    Quit,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Settings {
    pub tabs: Vec<PathBuf>,
    pub active: usize,
    pub recent: Vec<PathBuf>,
    pub tree_view: bool,
    pub show_author: bool,
    pub show_date: bool,
    pub show_sha: bool,
    pub show_sidebar: bool,
    /// Minutes between automatic fetches; 0 turns them off.
    pub auto_fetch_minutes: u32,
    pub refs_width: f32,
    /// `None` sizes the graph column to fit the lanes.
    pub graph_width: Option<f32>,
    /// Show author pictures from Gravatar and GitHub; off means initials
    /// only and no network requests for avatars.
    pub fetch_avatars: bool,
    /// Show pull requests, issues and CI runs from the hosting service
    /// (GitHub, GitLab) in the sidebar. Off means no request is ever made.
    pub hosting_integration: bool,
    /// Dark, light, or whatever the operating system uses.
    pub theme: crate::theme::Choice,
    /// Self-hosted servers and what the user said runs on each (GitHub
    /// Enterprise, GitLab, or neither). Other than github.com and
    /// gitlab.com, only servers listed here are ever contacted.
    #[serde(default)]
    pub hosting_servers: crate::hosting::Servers,
    /// Look for a newer release now and then (official builds only).
    pub check_updates: bool,
    /// A version the user chose to skip: not offered again by itself.
    pub skipped_update: Option<String>,
    /// Folders of repositories to pick from; see `crate::workspace`.
    pub workspaces: Vec<crate::workspace::Workspace>,

    // File > Preferences (`crate::prefs`): one struct per area, each in
    // its own module.
    pub general: crate::prefs::general::General,
    pub tools: crate::prefs::tools::Tools,
    pub identity: crate::prefs::identity::Identity,
    pub graph: crate::prefs::graph::GraphPrefs,
    pub appearance: crate::prefs::appearance::Appearance,
    pub editor: crate::prefs::editor::Editor,
    /// Repository-specific preferences, by working directory.
    pub repos: std::collections::BTreeMap<PathBuf, crate::prefs::RepoPrefs>,
    /// Hosts whose sign-in GitKrill does not use (Preferences > Integrations
    /// > Disconnect); see `crate::hosting::Disconnected`.
    pub hosting_disconnected: crate::hosting::Disconnected,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            tabs: Vec::new(),
            active: 0,
            recent: Vec::new(),
            tree_view: false,
            show_author: false,
            show_date: false,
            show_sha: false,
            show_sidebar: true,
            auto_fetch_minutes: 5,
            refs_width: 170.0,
            graph_width: None,
            fetch_avatars: true,
            hosting_integration: true,
            theme: crate::theme::Choice::Dark,
            check_updates: true,
            skipped_update: None,
            hosting_servers: Default::default(),
            workspaces: Vec::new(),
            general: Default::default(),
            tools: Default::default(),
            identity: Default::default(),
            graph: Default::default(),
            appearance: Default::default(),
            editor: Default::default(),
            repos: Default::default(),
            hosting_disconnected: Default::default(),
        }
    }
}

impl Settings {
    fn file() -> Option<PathBuf> {
        // Lets tests and scripted runs keep their state out of the real config.
        if let Some(dir) = std::env::var_os("GITKRILL_CONFIG_DIR") {
            return Some(PathBuf::from(dir).join("settings.json"));
        }
        Some(dirs::config_dir()?.join("GitKrill").join("settings.json"))
    }

    /// Where settings lived while the app was still called RustGit.
    fn legacy_file() -> Option<PathBuf> {
        if std::env::var_os("GITKRILL_CONFIG_DIR").is_some() {
            return None;
        }
        Some(dirs::config_dir()?.join("RustGit").join("settings.json"))
    }

    pub fn load() -> Self {
        // The old location is only read, never written or removed: the first
        // save puts the settings in the new place.
        [Self::file(), Self::legacy_file()]
            .into_iter()
            .flatten()
            .find_map(|path| std::fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(path) = Self::file() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }

    pub fn remember(&mut self, path: &Path) {
        self.recent.retain(|p| p != path);
        self.recent.insert(0, path.to_path_buf());
        self.recent.truncate(15);
    }
}

pub struct Shared {
    pub settings: Settings,
    pub avatars: crate::avatars::Avatars,
    pub dialog: Option<Dialog>,
    /// True on the first frame a dialog is shown, to focus its first field.
    pub dialog_fresh: bool,
    pub toasts: Vec<Toast>,
    pub actions: Vec<AppAction>,
    pub updater: crate::update::Updater,
    /// Workspace scans and repository summaries; see `crate::workspace`.
    pub workspaces: crate::workspace::Workspaces,
    /// The preferences page, while it is open (`crate::ui::prefs`).
    pub prefs: Option<Box<crate::ui::prefs::PrefsView>>,
}

impl Shared {
    pub fn show(&mut self, dialog: Dialog) {
        self.dialog = Some(dialog);
        self.dialog_fresh = true;
    }

    pub fn toast(&mut self, kind: ToastKind, title: impl Into<String>, body: impl Into<String>) {
        self.push_toast(Toast { kind, title: title.into(), body: body.into(), born: Instant::now(), link: None });
    }

    /// A toast with a button that opens `url` in the browser. `url` must be
    /// built by `hosting::RepoId`, never taken from a server's answer.
    pub fn toast_link(
        &mut self,
        kind: ToastKind,
        title: impl Into<String>,
        body: impl Into<String>,
        label: impl Into<String>,
        url: String,
    ) {
        let link = Some((label.into(), url));
        self.push_toast(Toast { kind, title: title.into(), body: body.into(), born: Instant::now(), link });
    }

    fn push_toast(&mut self, toast: Toast) {
        self.toasts.push(toast);
        if self.toasts.len() > 6 {
            self.toasts.remove(0);
        }
    }
}

pub struct RepoTab {
    pub path: PathBuf,
    pub name: String,
    handle: Option<RepoHandle>,
    pub snap: Option<Arc<RepoSnapshot>>,
    pub error: Option<String>,

    pub selection: Selection,
    pub details: Option<Result<Arc<CommitDetails>, String>>,
    details_for: Option<Oid>,
    pub diff: Option<DiffView>,
    pub overlay: Option<Overlay>,
    /// Finished [`RepoTab::spawn_job`] results, by key, until collected.
    results: HashMap<String, Box<dyn Any + Send>>,
    /// Jobs submitted and not yet answered.
    jobs_pending: usize,
    /// How recent operations ended: (label, when, succeeded). Short and
    /// bounded; see [`RepoTab::take_outcome`].
    outcomes: Vec<(String, Instant, bool)>,
    /// Pull requests, issues and CI runs from the hosting service.
    pub hosting: crate::hosting::HostingState,

    pub summary: String,
    pub description: String,
    pub amend: bool,

    pub undo: Vec<UndoEntry>,
    pub redo: Vec<UndoEntry>,
    /// Operations that are queued or running.
    pub busy: Vec<BusyOp>,

    /// Row the graph should bring into view on the next frame.
    pub scroll_to: Option<usize>,
    pub limit: usize,
    /// The "initial commits" preference `limit` started from; see
    /// [`RepoTab::set_initial_limit`].
    initial_limit: usize,
    /// The branch the pointer is on, for the graph to highlight what it
    /// contains (set by the left panel and the graph's labels).
    pub ref_hover: crate::ui::graph::reach::RefHover,

    pub sidebar_filter: String,
    pub search: String,
    pub search_open: bool,
    pub search_focus: bool,
    /// Rows matching `search`, ascending.
    pub search_hits: Vec<u32>,
    /// The query and history the hits were computed for.
    pub search_stamp: (String, usize),
    /// Collapsed sidebar sections, branch folders and file tree folders.
    pub collapsed: HashSet<String>,

    last_fs_refresh: Instant,
    /// When the next automatic fetch is due.
    next_auto_fetch: Instant,
}

impl RepoTab {
    pub fn new(path: PathBuf) -> Self {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        RepoTab {
            path,
            name,
            handle: None,
            snap: None,
            error: None,
            selection: Selection::None,
            details: None,
            details_for: None,
            diff: None,
            overlay: None,
            results: HashMap::new(),
            jobs_pending: 0,
            outcomes: Vec::new(),
            hosting: Default::default(),
            summary: String::new(),
            description: String::new(),
            amend: false,
            undo: Vec::new(),
            redo: Vec::new(),
            busy: Vec::new(),
            scroll_to: None,
            limit: crate::prefs::graph::initial_limit(),
            initial_limit: crate::prefs::graph::initial_limit(),
            ref_hover: Default::default(),
            sidebar_filter: String::new(),
            search: String::new(),
            search_open: false,
            search_focus: false,
            search_hits: Vec::new(),
            search_stamp: (String::new(), 0),
            collapsed: HashSet::new(),
            last_fs_refresh: Instant::now(),
            next_auto_fetch: Instant::now() + FIRST_AUTO_FETCH,
        }
    }

    /// Runs `job` on this repository's reader thread and keeps its return
    /// value until [`RepoTab::take_result`] is called with the same key.
    ///
    /// Use it for any read that may touch the disk (blame, file history,
    /// reading conflict sides...). A second job under the same key replaces
    /// the first result. Keys are free-form; prefix them with the feature name.
    pub fn spawn_job<T: Any + Send>(
        &mut self,
        key: impl Into<String>,
        job: impl FnOnce(&crate::git::repo::Loader) -> T + Send + 'static,
    ) {
        let key = key.into();
        self.results.remove(&key);
        if let Some(handle) = &self.handle {
            handle.job(key, job);
            self.jobs_pending += 1;
        }
    }

    /// Whether any job is still running on the reader thread.
    pub fn has_pending_jobs(&self) -> bool {
        self.jobs_pending > 0
    }

    /// How the operation labelled `label` that finished after `since` ended:
    /// `Some(true)` on success, `Some(false)` on failure, `None` while it is
    /// still running. For views that start a command with [`RepoTab::run`]
    /// and need to react to its result (the failure itself is already shown
    /// to the user as a toast).
    pub fn take_outcome(&mut self, label: &str, since: Instant) -> Option<bool> {
        let found = self
            .outcomes
            .iter()
            .rev()
            .find(|(l, when, _)| l == label && *when >= since)
            .map(|(_, _, ok)| *ok)?;
        self.outcomes.retain(|(l, _, _)| l != label);
        Some(found)
    }

    /// Collects the result of a finished job. `None` while it is still
    /// running (or if `T` is not the type the job returned).
    pub fn take_result<T: Any>(&mut self, key: &str) -> Option<T> {
        if !self.results.get(key)?.is::<T>() {
            return None;
        }
        let boxed: Box<dyn Any> = self.results.remove(key)?;
        boxed.downcast::<T>().ok().map(|value| *value)
    }

    /// Whether an operation whose label starts with `prefix` is in flight.
    pub fn is_running(&self, prefix: &str) -> bool {
        self.busy.iter().any(|op| op.label.starts_with(prefix))
    }

    /// The operation to show as "in progress", if any is worth showing.
    pub fn busy_label(&self) -> Option<&str> {
        self.busy.iter().find(|op| !op.quiet).map(|op| op.label.as_str())
    }

    /// Starts the background threads the first time the tab is shown.
    pub fn start(&mut self, ctx: &egui::Context) {
        if self.handle.is_none() && self.error.is_none() {
            let handle = RepoHandle::spawn(self.path.clone(), ctx.clone(), self.limit);
            handle.refresh();
            self.handle = Some(handle);
        }
    }

    pub fn refresh(&mut self) {
        if let Some(handle) = &self.handle {
            handle.refresh();
            self.last_fs_refresh = Instant::now();
        }
    }

    pub fn run(&mut self, cmd: GitCmd) {
        self.run_as(cmd, Origin::User);
    }

    fn run_as(&mut self, cmd: GitCmd, origin: Origin) {
        if let Some(handle) = &self.handle {
            self.busy.push(BusyOp {
                label: cmd.label.clone(),
                quiet: cmd.quiet || cmd.background,
                since: Instant::now(),
            });
            handle.run(cmd, origin);
        }
    }

    pub fn undo(&mut self) {
        if let Some(entry) = self.undo.pop() {
            self.run_as(entry.undo.clone(), Origin::Undo(entry));
        }
    }

    pub fn redo(&mut self) {
        if let Some(entry) = self.redo.pop() {
            self.run_as(entry.redo.clone(), Origin::Redo(entry));
        }
    }

    pub fn select(&mut self, selection: Selection) {
        self.selection = selection;
        if let Selection::Commit(oid) = selection
            && self.details_for != Some(oid) {
                self.details = None;
                self.details_for = Some(oid);
                let is_stash = self.snap.as_ref().is_some_and(|snap| {
                    snap.row_of(oid)
                        .is_some_and(|row| snap.history.commits[row].kind == RowKind::Stash)
                });
                if let Some(handle) = &self.handle {
                    handle.details(oid, is_stash);
                }
            }
    }

    pub fn select_row(&mut self, row: usize) {
        let Some(snap) = &self.snap else { return };
        let Some(commit) = snap.history.commits.get(row) else { return };
        let selection = match commit.kind {
            RowKind::Wip => Selection::Wip,
            _ => Selection::Commit(commit.oid),
        };
        self.select(selection);
    }

    /// Selects a commit and scrolls the graph to it.
    pub fn jump_to(&mut self, oid: Oid) {
        let Some(row) = self.snap.as_ref().and_then(|s| s.row_of(oid)) else { return };
        self.select(Selection::Commit(oid));
        self.scroll_to = Some(row);
        self.diff = None;
    }

    pub fn selected_row(&self) -> Option<usize> {
        let snap = self.snap.as_ref()?;
        match self.selection {
            Selection::None => None,
            Selection::Wip => (!snap.status.is_clean()).then_some(0),
            Selection::Commit(oid) => snap.row_of(oid),
        }
    }

    pub fn open_diff(&mut self, target: DiffTarget) {
        if self.diff.as_ref().is_some_and(|d| d.target == target) {
            return;
        }
        if let Some(handle) = &self.handle {
            handle.diff(target.clone());
        }
        // A file was asked for: history and rebase views make way. The
        // conflict resolver stays, because it may have unsaved choices and
        // handles the hand-over itself.
        if matches!(self.overlay, Some(Overlay::History(_) | Overlay::Rebase(_))) {
            self.overlay = None;
        }
        self.diff = Some(DiffView { target, data: None });
    }

    pub fn load_more(&mut self) {
        let Some(snap) = &self.snap else { return };
        // Only once the current page has arrived, or scrolling at the bottom
        // would keep doubling the limit.
        if snap.history.has_more && snap.history.commits.len() >= self.limit {
            self.limit = self.limit.saturating_mul(2);
            if let Some(handle) = &self.handle {
                handle.set_limit(self.limit);
                handle.refresh();
            }
        }
    }

    /// Whether a bigger page of history has been asked for and not arrived.
    pub fn loading_more(&self) -> bool {
        self.snap.as_ref().is_some_and(|snap| snap.history.has_more && snap.history.commits.len() < self.limit)
    }

    /// Follows a change of the "initial commits" preference (`usize::MAX`
    /// for all of them): the history is loaded again to that size, dropping
    /// pages loaded by scrolling. Nothing happens while it is unchanged, so
    /// the graph calls this every frame.
    pub fn set_initial_limit(&mut self, limit: usize) {
        if self.initial_limit == limit {
            return;
        }
        self.initial_limit = limit;
        self.limit = limit;
        if let Some(handle) = &self.handle {
            handle.set_limit(limit);
            handle.refresh();
        }
    }

    /// Drains worker responses and schedules refreshes for file changes.
    pub fn poll(&mut self, ctx: &egui::Context, shared: &mut Shared, is_active: bool) {
        let mut responses = Vec::new();
        if let Some(handle) = &self.handle {
            while let Ok(response) = handle.rx.try_recv() {
                responses.push(response);
            }
        }
        for response in responses {
            match response {
                Response::Snapshot(snap) => self.on_snapshot(snap),
                Response::LoadError(e) => {
                    self.error = Some(e);
                    self.handle = None;
                }
                Response::Diff(target, result) => {
                    if let Some(view) = &mut self.diff
                        && view.target == target {
                            view.data = Some(result);
                        }
                }
                Response::Details(oid, result) => {
                    if self.details_for == Some(oid) {
                        self.details = Some(result);
                    }
                }
                Response::Job { key, result } => {
                    self.jobs_pending = self.jobs_pending.saturating_sub(1);
                    self.results.insert(key, result);
                }
                Response::OpDone { cmd, result, undo, origin } => {
                    self.on_op_done(shared, cmd, result, undo, origin);
                }
            }
        }

        if !is_active {
            return;
        }
        if let Some(handle) = &self.handle
            && handle.fs_dirty.load(Ordering::Relaxed) {
                let since = self.last_fs_refresh.elapsed();
                if since >= FS_DEBOUNCE {
                    handle.fs_dirty.store(false, Ordering::Relaxed);
                    handle.refresh();
                    self.last_fs_refresh = Instant::now();
                } else {
                    ctx.request_repaint_after(FS_DEBOUNCE - since);
                }
            }

        // Keep remote branches current in the background.
        let minutes = shared.settings.auto_fetch_minutes;
        let has_remote = self.snap.as_ref().is_some_and(|s| !s.remotes.is_empty());
        if minutes > 0 && has_remote {
            let now = Instant::now();
            crate::prefs::general::reschedule(&mut self.next_auto_fetch, now, minutes);
            if now >= self.next_auto_fetch {
                self.next_auto_fetch = now + Duration::from_secs(minutes as u64 * 60);
                if self.busy.is_empty() {
                    self.run(crate::git::ops::auto_fetch());
                }
            } else {
                ctx.request_repaint_after(self.next_auto_fetch - now);
            }
        }
    }

    fn on_snapshot(&mut self, snap: Arc<RepoSnapshot>) {
        self.error = None;
        let head_commit = snap.head.oid.filter(|oid| snap.row_of(*oid).is_some());
        let dirty = !snap.status.is_clean();

        let fallback = match (dirty, head_commit) {
            (true, _) => Selection::Wip,
            (false, Some(oid)) => Selection::Commit(oid),
            (false, None) => Selection::None,
        };
        let selection = match self.selection {
            Selection::None => fallback,
            // The working tree became clean (commit, stash, discard).
            Selection::Wip if !dirty => fallback,
            Selection::Commit(oid) if snap.row_of(oid).is_none() => fallback,
            keep => keep,
        };

        // Follow an open working-tree diff as its file moves between the
        // staged and unstaged lists, and close it once the change is gone.
        let mut reload = None;
        if let Some(view) = &mut self.diff {
            let path = &view.target.path;
            let in_unstaged = snap.status.unstaged.iter().any(|f| &f.path == path);
            let in_staged = snap.status.staged.iter().any(|f| &f.path == path);
            let source = match view.target.source {
                DiffSource::Unstaged if in_unstaged => Some(DiffSource::Unstaged),
                DiffSource::Unstaged if in_staged => Some(DiffSource::Staged),
                DiffSource::Staged if in_staged => Some(DiffSource::Staged),
                DiffSource::Staged if in_unstaged => Some(DiffSource::Unstaged),
                DiffSource::Unstaged | DiffSource::Staged => None,
                commit @ DiffSource::Commit(_) => Some(commit),
            };
            match source {
                Some(source) => {
                    if !matches!(source, DiffSource::Commit(_)) {
                        if source != view.target.source {
                            view.target.source = source;
                            let list = match source {
                                DiffSource::Staged => &snap.status.staged,
                                _ => &snap.status.unstaged,
                            };
                            view.target.old_path = list
                                .iter()
                                .find(|f| &f.path == path)
                                .and_then(|f| f.old_path.clone());
                        }
                        reload = Some(view.target.clone());
                    }
                }
                None => self.diff = None,
            }
        }
        if let (Some(target), Some(handle)) = (reload, &self.handle) {
            handle.diff(target);
        }

        self.snap = Some(snap);
        self.select(selection);
    }

    fn on_op_done(
        &mut self,
        shared: &mut Shared,
        cmd: GitCmd,
        result: Result<String, String>,
        undo: Option<UndoEntry>,
        origin: Origin,
    ) {
        if let Some(i) = self.busy.iter().position(|op| op.label == cmd.label) {
            self.busy.remove(i);
        }
        if cmd.background {
            // Nobody asked for this; stay silent whether it worked or not.
            return;
        }
        self.outcomes.push((cmd.label.clone(), Instant::now(), result.is_ok()));
        if self.outcomes.len() > 32 {
            self.outcomes.remove(0);
        }

        match result {
            Ok(output) => {
                match origin {
                    Origin::User => {
                        if let Some(entry) = undo {
                            self.undo.push(entry);
                            self.redo.clear();
                        }
                        if matches!(cmd.undo, UndoKind::Commit) {
                            self.summary.clear();
                            self.description.clear();
                            self.amend = false;
                        }
                    }
                    Origin::Undo(entry) => self.redo.push(entry),
                    Origin::Redo(entry) => self.undo.push(entry),
                }
                if !cmd.quiet {
                    // Git's own one-line summary is usually the most useful
                    // detail; line-ending and advice chatter is not.
                    let detail = output
                        .lines()
                        .map(str::trim)
                        .rfind(|line| {
                            !line.is_empty() && !line.starts_with("warning:") && !line.starts_with("hint:")
                        })
                        .unwrap_or_default()
                        .to_owned();
                    shared.toast(ToastKind::Success, cmd.success.unwrap_or(cmd.label), detail);
                }
            }
            Err(message) => {
                let rejected = cmd.label == "Push"
                    && ["[rejected]", "non-fast-forward", "fetch first"]
                        .iter()
                        .any(|needle| message.contains(needle));
                // Stopping on conflicts is the operation asking for help, not
                // failing: say what to do next.
                let conflicts = message.contains("CONFLICT") || message.contains("could not apply");
                if rejected {
                    shared.show(Dialog::PushRejected);
                } else if conflicts {
                    let note = cmd.conflict_note.as_deref().map(|note| format!(" {note}")).unwrap_or_default();
                    shared.toast(
                        ToastKind::Error,
                        format!("{} stopped on conflicts", cmd.label),
                        format!(
                            "Resolve the conflicted files in the list on the right, then press Continue.{note}\n{}",
                            conflict_lines(&message)
                        ),
                    );
                } else {
                    shared.toast(ToastKind::Error, format!("{} failed", cmd.label), message);
                }
            }
        }
    }

    pub fn is_loading(&self) -> bool {
        self.snap.is_none() && self.error.is_none()
    }

    /// Whether the tab can show another repository without losing anything
    /// the user has not finished: a commit message, an open overlay (the
    /// conflict resolver may hold unsaved choices), an operation in flight.
    pub fn can_switch(&self) -> bool {
        self.summary.trim().is_empty()
            && self.description.trim().is_empty()
            && self.overlay.is_none()
            && self.busy.iter().all(|op| op.quiet)
    }
}

/// The "CONFLICT (content): ..." lines of git's output, which name the files.
fn conflict_lines(message: &str) -> String {
    message
        .lines()
        .filter(|line| line.contains("CONFLICT"))
        .take(6)
        .collect::<Vec<_>>()
        .join("\n")
}

/// `Nov 10, 2026 15:04` in local time, or in the date format chosen under
/// Preferences > UI Customization (with the time of day).
pub fn format_time(secs: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(secs, 0).single() {
        Some(time) => crate::prefs::appearance::custom_date(&time, true)
            .unwrap_or_else(|| time.format("%b %-d, %Y %H:%M").to_string()),
        None => String::new(),
    }
}

/// `3 hours ago` for recent times; empty once the date alone says it better.
pub fn relative_time(secs: i64) -> String {
    use chrono::{Local, TimeZone};
    let Some(time) = Local.timestamp_opt(secs, 0).single() else { return String::new() };
    let delta = Local::now().signed_duration_since(time);
    let plural = |n: i64, unit: &str| {
        if n == 1 { format!("1 {unit} ago") } else { format!("{n} {unit}s ago") }
    };
    if delta.num_seconds() < 0 || delta.num_days() >= 31 {
        String::new()
    } else if delta.num_seconds() < 60 {
        "just now".to_owned()
    } else if delta.num_minutes() < 60 {
        plural(delta.num_minutes(), "minute")
    } else if delta.num_hours() < 24 {
        plural(delta.num_hours(), "hour")
    } else {
        plural(delta.num_days(), "day")
    }
}
