//! Workspaces: folders of repositories. The user picks a parent folder once
//! and GitKrill lists every Git repository under it, so moving between them
//! is a click in the repository picker (or a few letters in the command
//! palette) instead of a trip through the folder dialog for each one.
//!
//! Two kinds of background work happen here, both off the UI thread:
//! - *scans* walk a workspace's folders looking for repositories;
//! - *summaries* read one repository's branch, ahead / behind counts and
//!   pending changes, for the rows of the picker.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::git::repo::lossy;
use crate::state::{Shared, ToastKind};

/// How many folder levels below a workspace's root are searched.
pub const MAX_DEPTH: usize = 5;
/// Folders looked into per scan at most, so that picking a whole drive ends
/// in seconds rather than minutes.
const MAX_FOLDERS: usize = 60_000;
/// A summary on screen is read again once it is this old.
const FRESH: Duration = Duration::from_secs(20);
/// Repositories summarised at the same time.
const SUMMARY_THREADS: usize = 3;

/// Folders that hold dependencies, build output, caches or system data, and
/// are never where someone keeps their own projects. Not searched.
const SKIP: &[&str] = &[
    "node_modules",
    "bower_components",
    "vendor",
    "target",
    ".gradle",
    ".venv",
    "venv",
    "__pycache__",
    ".idea",
    ".vs",
    ".vscode",
    ".cache",
    ".npm",
    ".cargo",
    ".rustup",
    "$RECYCLE.BIN",
    "System Volume Information",
];

/// Where a repository picked from a list opens.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Opens {
    /// In the current tab, in place of the repository there.
    Here,
    /// In a tab of its own.
    NewTab,
}

/// A folder the user added, and the repositories found in it.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Workspace {
    pub root: PathBuf,
    /// What the last scan found, sorted. Kept in the settings so the list is
    /// there at once on the next start, while the rescan runs.
    pub repos: Vec<PathBuf>,
}

impl Workspace {
    pub fn name(&self) -> String {
        display_name(&self.root)
    }

    pub fn contains(&self, repo: &Path) -> bool {
        self.repos.iter().any(|r| r == repo)
    }

    /// The folders between the root and `repo` ("tools/libs" for
    /// `root/tools/libs/core`), empty for a repository directly inside.
    pub fn folder_of(&self, repo: &Path) -> String {
        let Ok(relative) = repo.strip_prefix(&self.root) else { return String::new() };
        let parent = relative.parent().unwrap_or(Path::new(""));
        parent.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/")
    }
}

/// The last part of a path, or the whole path for a drive root.
pub fn display_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string())
}

/// Normalises separators and drops a trailing one, so paths from the folder
/// dialog, from libgit2 and from a scan compare equal.
pub fn clean(path: &Path) -> PathBuf {
    path.components().collect()
}

/// Every Git repository at or below `root`, sorted ignoring case.
///
/// A folder is a repository when it has a `.git` entry: a folder for a
/// normal clone, a file for worktrees and submodules. Repositories inside
/// repositories are found too (the search does not stop at the first
/// `.git`). Symbolic links and junctions are not followed, so the walk
/// cannot loop.
pub fn scan(root: &Path) -> Vec<PathBuf> {
    let root = clean(root);
    let mut found = Vec::new();
    let mut queue = VecDeque::from([(root, 0)]);
    let mut visited = 0;
    while let Some((dir, depth)) = queue.pop_front() {
        visited += 1;
        if visited > MAX_FOLDERS {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut is_repo = false;
        let mut children = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name == ".git" {
                is_repo = true;
                continue;
            }
            if depth >= MAX_DEPTH {
                continue;
            }
            // `is_dir` is false for links and junctions: they are skipped.
            let Ok(kind) = entry.file_type() else { continue };
            if !kind.is_dir() || kind.is_symlink() {
                continue;
            }
            let name = name.to_string_lossy();
            if SKIP.iter().any(|skip| skip.eq_ignore_ascii_case(&name)) {
                continue;
            }
            children.push(entry.path());
        }
        if is_repo {
            found.push(dir);
        }
        queue.extend(children.into_iter().map(|child| (child, depth + 1)));
    }
    found.sort_by_cached_key(|path| path.to_string_lossy().to_lowercase());
    found
}

/// What the picker shows about a repository.
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    /// The branch checked out, or the short commit when detached.
    pub head: String,
    pub detached: bool,
    /// Commits ahead of and behind the branch's upstream.
    pub ahead: usize,
    pub behind: usize,
    /// Files with changes, staged or not, untracked ones included.
    pub changes: usize,
    pub conflicts: bool,
}

impl Summary {
    /// The same facts, from a tab's snapshot.
    pub fn of_snapshot(snap: &crate::git::RepoSnapshot) -> Self {
        let branch = snap.current_branch();
        Summary {
            head: snap.head_label(),
            detached: snap.head.detached,
            ahead: branch.map_or(0, |b| b.ahead),
            behind: branch.map_or(0, |b| b.behind),
            changes: snap.status.file_count(),
            conflicts: snap.status.unstaged.iter().any(|f| f.kind == crate::git::ChangeKind::Conflicted),
        }
    }
}

/// Reads a summary of the repository at `path`. Never writes to it.
pub fn summarize(path: &Path) -> Result<Summary, String> {
    let err = |e: git2::Error| e.message().to_owned();
    let repo = git2::Repository::open(path).map_err(err)?;
    if repo.is_bare() {
        return Err("bare repository".to_owned());
    }

    let (head, detached, ahead, behind) = match repo.head() {
        Ok(head) if head.is_branch() => {
            let name = lossy(head.shorthand_bytes());
            let local = head.target();
            let upstream = git2::Branch::wrap(head).upstream().ok().and_then(|u| u.get().target());
            let (ahead, behind) = match (local, upstream) {
                (Some(local), Some(upstream)) if local != upstream => {
                    repo.graph_ahead_behind(local, upstream).unwrap_or((0, 0))
                }
                _ => (0, 0),
            };
            (name, false, ahead, behind)
        }
        Ok(head) => (head.target().map(crate::git::short).unwrap_or_default(), true, 0, 0),
        // A new repository: HEAD names a branch that has no commit yet.
        Err(e) if e.code() == git2::ErrorCode::UnbornBranch => {
            let target = repo.find_reference("HEAD").ok().and_then(|r| r.symbolic_target_bytes().map(lossy));
            let name = target.unwrap_or_default();
            (name.strip_prefix("refs/heads/").unwrap_or(&name).to_owned(), false, 0, 0)
        }
        Err(e) => return Err(err(e)),
    };

    let mut options = git2::StatusOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(false)
        .include_ignored(false)
        .exclude_submodules(true);
    let statuses = repo.statuses(Some(&mut options)).map_err(err)?;
    let changes = statuses.iter().filter(|s| !s.status().is_ignored() && s.status() != git2::Status::CURRENT).count();
    let conflicts = statuses.iter().any(|s| s.status().is_conflicted());
    Ok(Summary { head, detached, ahead, behind, changes, conflicts })
}

struct Known {
    asked: Instant,
    /// `None` until the first answer.
    summary: Option<Result<Summary, String>>,
    /// Asked and not answered yet.
    pending: bool,
}

/// Scans and summaries in flight, and the summaries read so far.
#[derive(Default)]
pub struct Workspaces {
    scans: Vec<(PathBuf, Receiver<Vec<PathBuf>>)>,
    /// Workspaces the user just added: the result of their first scan is
    /// worth telling.
    announce: HashSet<PathBuf>,
    known: HashMap<PathBuf, Known>,
    requests: Option<Sender<PathBuf>>,
    answers: Option<Receiver<(PathBuf, Result<Summary, String>)>>,
    /// The saved workspaces have been rescanned since the app started.
    rescanned: bool,
    /// Set when a workspace was added, so the menu it was added from (the
    /// repository picker or the new-tab menu) opens and shows it filling up.
    pub reveal: Option<Opens>,
    /// When a repository list was last on screen.
    pub shown: Option<Instant>,
    /// The open tabs' repositories, with a summary of their latest snapshot
    /// once loaded. Updated every frame by the application.
    pub open_tabs: Vec<(PathBuf, Option<Summary>)>,
}

impl Workspaces {
    pub fn is_scanning(&self, root: &Path) -> bool {
        self.scans.iter().any(|(r, _)| r == root)
    }

    /// Starts looking for repositories under `root` again.
    pub fn rescan(&mut self, ctx: &egui::Context, root: &Path) {
        if self.is_scanning(root) {
            return;
        }
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        let dir = root.to_path_buf();
        std::thread::spawn(move || {
            let _ = tx.send(scan(&dir));
            ctx.request_repaint();
        });
        self.scans.push((root.to_path_buf(), rx));
    }

    pub fn is_open(&self, repo: &Path) -> bool {
        self.open_tabs.iter().any(|(path, _)| path == repo)
    }

    /// The latest summary of `repo`. A repository open in a tab is described
    /// from that tab's snapshot, which is always current; for the others a
    /// new reading is started when there is none or it is getting old. Call
    /// it only for rows on screen.
    pub fn summary(&mut self, ctx: &egui::Context, repo: &Path) -> Option<Result<Summary, String>> {
        if let Some((_, Some(summary))) = self.open_tabs.iter().find(|(path, _)| path == repo) {
            return Some(Ok(summary.clone()));
        }
        let stale = match self.known.get(repo) {
            None => true,
            Some(known) => !known.pending && known.asked.elapsed() >= FRESH,
        };
        if stale {
            self.ask(ctx, repo);
        }
        self.known.get(repo).and_then(|k| k.summary.clone())
    }

    fn ask(&mut self, ctx: &egui::Context, repo: &Path) {
        if self.requests.is_none() {
            let (request_tx, request_rx) = channel::<PathBuf>();
            let (answer_tx, answer_rx) = channel();
            let request_rx = std::sync::Arc::new(std::sync::Mutex::new(request_rx));
            for _ in 0..SUMMARY_THREADS {
                let (requests, answers, ctx) = (request_rx.clone(), answer_tx.clone(), ctx.clone());
                std::thread::spawn(move || {
                    loop {
                        // The lock is held only while waiting for the next path.
                        let next = requests.lock().ok().and_then(|rx| rx.recv().ok());
                        let Some(path) = next else { break };
                        let summary = summarize(&path);
                        if answers.send((path, summary)).is_err() {
                            break;
                        }
                        ctx.request_repaint();
                    }
                });
            }
            self.requests = Some(request_tx);
            self.answers = Some(answer_rx);
        }
        let known = self.known.entry(repo.to_path_buf()).or_insert(Known {
            asked: Instant::now(),
            summary: None,
            pending: false,
        });
        known.asked = Instant::now();
        known.pending = true;
        if let Some(tx) = &self.requests {
            let _ = tx.send(repo.to_path_buf());
        }
    }
}

/// Adds the folder `root` as a workspace (or rescans it if it is one
/// already) and opens the menu it was added from, `from`, to show what is
/// found.
pub fn add(ctx: &egui::Context, shared: &mut Shared, root: &Path, from: Opens) {
    let root = clean(root);
    if !shared.settings.workspaces.iter().any(|w| w.root == root) {
        shared.settings.workspaces.push(Workspace { root: root.clone(), repos: Vec::new() });
    }
    shared.workspaces.announce.insert(root.clone());
    shared.workspaces.rescan(ctx, &root);
    shared.workspaces.reveal = Some(from);
    // A list is about to show the workspace filling up (the menu opened
    // above, or the welcome page): no toast unless it is closed before the
    // scan ends.
    shared.workspaces.shown = Some(Instant::now());
}

/// Forgets a workspace. Its folders are not touched.
pub fn remove(shared: &mut Shared, root: &Path) {
    shared.settings.workspaces.retain(|w| w.root != root);
    shared.workspaces.announce.remove(root);
}

/// Collects finished scans and summaries. Call it once per frame.
pub fn poll(ctx: &egui::Context, shared: &mut Shared) {
    let Shared { settings, workspaces: state, .. } = shared;

    // New repositories may have appeared since the last run.
    if !state.rescanned {
        state.rescanned = true;
        for workspace in &mut settings.workspaces {
            workspace.root = clean(&workspace.root);
        }
        for root in settings.workspaces.iter().map(|w| w.root.clone()).collect::<Vec<_>>() {
            state.rescan(ctx, &root);
        }
    }

    if let Some(answers) = &state.answers {
        while let Ok((path, summary)) = answers.try_recv() {
            if let Some(known) = state.known.get_mut(&path) {
                known.summary = Some(summary);
                known.pending = false;
            }
        }
    }

    let mut finished = Vec::new();
    state.scans.retain(|(root, rx)| match rx.try_recv() {
        Ok(repos) => {
            finished.push((root.clone(), repos));
            false
        }
        Err(std::sync::mpsc::TryRecvError::Empty) => true,
        Err(std::sync::mpsc::TryRecvError::Disconnected) => false,
    });
    let mut toasts = Vec::new();
    for (root, repos) in finished {
        let announce = state.announce.remove(&root);
        let Some(workspace) = settings.workspaces.iter_mut().find(|w| w.root == root) else { continue };
        let name = workspace.name();
        if repos.is_empty() && announce {
            // Nothing to show: the folder was probably not the one meant.
            settings.workspaces.retain(|w| w.root != root);
            toasts.push((
                ToastKind::Error,
                format!("No Git repositories in {name}"),
                format!("Nothing was found in {} or the {MAX_DEPTH} folder levels below it.", root.display()),
            ));
            continue;
        }
        workspace.repos = repos;
        let count = workspace.repos.len();
        // The list on screen already shows what was found.
        let on_screen = state.shown.is_some_and(|at| at.elapsed() < Duration::from_millis(500));
        if announce && !on_screen {
            let what = if count == 1 { "1 repository".to_owned() } else { format!("{count} repositories") };
            toasts.push((
                ToastKind::Success,
                format!("Found {what} in {name}"),
                "Pick one from the repository menu or the command palette (Ctrl+P).".to_owned(),
            ));
        }
    }
    for (kind, title, body) in toasts {
        shared.toast(kind, title, body);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::git;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("gitkrill-tests").join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn init(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q", "-b", "main"]);
        git(dir, &["config", "user.name", "Test User"]);
        git(dir, &["config", "user.email", "test@example.com"]);
        git(dir, &["config", "commit.gpgsign", "false"]);
        git(dir, &["config", "core.autocrlf", "false"]);
    }

    fn names(root: &Path, found: &[PathBuf]) -> Vec<String> {
        found
            .iter()
            .map(|p| p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/"))
            .collect()
    }

    #[test]
    fn finds_repositories_at_any_depth_and_skips_dependency_folders() {
        let root = temp("workspace-scan");
        init(&root.join("alpha"));
        init(&root.join("Beta"));
        init(&root.join("group/one"));
        init(&root.join("group/two"));
        // A repository inside a repository is still found.
        init(&root.join("alpha/plugins/inner"));
        // A worktree or submodule has a `.git` file instead of a folder.
        std::fs::create_dir_all(root.join("linked")).unwrap();
        std::fs::write(root.join("linked/.git"), "gitdir: elsewhere\n").unwrap();
        // Not searched: dependencies and build output.
        init(&root.join("alpha/node_modules/dep"));
        init(&root.join("group/target/built"));
        // Too deep.
        init(&root.join("a/b/c/d/e/f"));
        // Plain folders are not repositories.
        std::fs::create_dir_all(root.join("notes/drafts")).unwrap();

        let found = scan(&root);
        assert_eq!(names(&root, &found), ["alpha", "alpha/plugins/inner", "Beta", "group/one", "group/two", "linked"]);
    }

    #[test]
    fn the_root_itself_can_be_a_repository() {
        let root = temp("workspace-root");
        init(&root);
        init(&root.join("sub"));
        let found = scan(&root);
        assert_eq!(found, [clean(&root), clean(&root.join("sub"))]);
    }

    #[test]
    fn folder_names_the_levels_between_root_and_repository() {
        let workspace = Workspace { root: PathBuf::from("/w"), repos: Vec::new() };
        assert_eq!(workspace.folder_of(Path::new("/w/app")), "");
        assert_eq!(workspace.folder_of(Path::new("/w/tools/libs/core")), "tools/libs");
        assert_eq!(workspace.folder_of(Path::new("/w")), "");
        assert_eq!(workspace.folder_of(Path::new("/elsewhere/x")), "");
    }

    #[test]
    fn summary_reports_branch_upstream_and_changes() {
        let root = temp("workspace-summary");
        let origin = root.join("origin");
        init(&origin);
        std::fs::write(origin.join("a.txt"), "one\n").unwrap();
        git(&origin, &["add", "-A"]);
        git(&origin, &["commit", "-q", "-m", "first"]);

        let clone = root.join("clone");
        git(&root, &["clone", "-q", origin.to_str().unwrap(), clone.to_str().unwrap()]);
        git(&clone, &["config", "user.name", "Test User"]);
        git(&clone, &["config", "user.email", "test@example.com"]);
        git(&clone, &["config", "commit.gpgsign", "false"]);
        let clean_summary = summarize(&clone).unwrap();
        assert_eq!(
            clean_summary,
            Summary { head: "main".into(), detached: false, ahead: 0, behind: 0, changes: 0, conflicts: false }
        );

        // One commit ahead, one behind, a modified file and an untracked one.
        std::fs::write(origin.join("b.txt"), "theirs\n").unwrap();
        git(&origin, &["add", "-A"]);
        git(&origin, &["commit", "-q", "-m", "theirs"]);
        git(&clone, &["fetch", "-q"]);
        std::fs::write(clone.join("c.txt"), "mine\n").unwrap();
        git(&clone, &["add", "-A"]);
        git(&clone, &["commit", "-q", "-m", "mine"]);
        std::fs::write(clone.join("a.txt"), "changed\n").unwrap();
        std::fs::write(clone.join("new.txt"), "new\n").unwrap();
        let summary = summarize(&clone).unwrap();
        assert_eq!((summary.ahead, summary.behind, summary.changes), (1, 1, 2));

        // Detached, and a repository with no commits yet.
        git(&clone, &["checkout", "-q", "--detach", "HEAD"]);
        let summary = summarize(&clone).unwrap();
        assert!(summary.detached);
        assert_eq!(summary.head.len(), 7);
        let empty = root.join("empty");
        init(&empty);
        assert_eq!(summarize(&empty).unwrap().head, "main");
        assert!(summarize(&root.join("missing")).is_err());
    }
}
