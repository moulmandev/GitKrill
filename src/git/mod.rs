//! Git backend.
//!
//! Reads (history, refs, status, diffs) go through libgit2 in-process so they
//! are fast and never spawn a process. Anything that mutates the repository or
//! talks to a remote goes through the `git` CLI so hooks, signing, credential
//! helpers, LFS and the user's config behave exactly as they do in a terminal.

pub mod blame;
pub mod cli;
pub mod conflict;
pub mod diff;
pub mod gitflow;
pub mod graph;
pub mod lfs;
pub mod ops;
pub mod picture;
pub mod rebase;
pub mod repo;
pub mod text;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

pub use git2::Oid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    /// Pseudo-row for uncommitted changes.
    Wip,
    Commit,
    Stash,
}

#[derive(Clone, Debug)]
pub struct CommitInfo {
    pub oid: Oid,
    pub parents: Vec<Oid>,
    pub summary: String,
    /// Body flattened onto one line (newlines become " | "), for the graph row.
    pub body_preview: String,
    pub author: String,
    pub email: String,
    pub time: i64,
    pub kind: RowKind,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Head {
    pub branch: Option<String>,
    pub oid: Option<Oid>,
    pub detached: bool,
}

impl Head {
    /// Something `git checkout` accepts to get back here.
    pub fn refname(&self) -> Option<String> {
        match (&self.branch, self.oid) {
            (Some(b), _) => Some(b.clone()),
            (None, Some(oid)) => Some(oid.to_string()),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct BranchInfo {
    pub name: String,
    pub oid: Option<Oid>,
    pub is_head: bool,
    pub upstream: Option<String>,
    pub ahead: usize,
    pub behind: usize,
}

#[derive(Clone, Debug)]
pub struct RemoteBranch {
    pub remote: String,
    /// Name without the remote prefix, e.g. `main`.
    pub name: String,
    /// `origin/main`
    pub full: String,
    pub oid: Oid,
}

#[derive(Clone, Debug)]
pub struct RemoteInfo {
    pub name: String,
    pub url: String,
}

#[derive(Clone, Debug)]
pub struct TagInfo {
    pub name: String,
    pub oid: Oid,
}

#[derive(Clone, Debug)]
pub struct StashInfo {
    pub index: usize,
    pub message: String,
    pub oid: Oid,
}

#[derive(Clone, Debug)]
pub struct SubmoduleInfo {
    pub name: String,
    pub path: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Typechange,
    Untracked,
    Conflicted,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FileChange {
    pub path: String,
    pub old_path: Option<String>,
    pub kind: ChangeKind,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkingStatus {
    pub unstaged: Vec<FileChange>,
    pub staged: Vec<FileChange>,
}

impl WorkingStatus {
    pub fn is_clean(&self) -> bool {
        self.unstaged.is_empty() && self.staged.is_empty()
    }

    /// Number of distinct paths with changes.
    pub fn file_count(&self) -> usize {
        let mut paths: Vec<&str> = self
            .unstaged
            .iter()
            .chain(self.staged.iter())
            .map(|f| f.path.as_str())
            .collect();
        paths.sort_unstable();
        paths.dedup();
        paths.len()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepoState {
    Clean,
    Merge,
    Rebase,
    CherryPick,
    Revert,
    Bisect,
    Other,
}

impl RepoState {
    pub fn label(self) -> Option<&'static str> {
        match self {
            RepoState::Clean => None,
            RepoState::Merge => Some("Merge in progress"),
            RepoState::Rebase => Some("Rebase in progress"),
            RepoState::CherryPick => Some("Cherry-pick in progress"),
            RepoState::Revert => Some("Revert in progress"),
            RepoState::Bisect => Some("Bisect in progress"),
            RepoState::Other => Some("Operation in progress"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefKind {
    Head,
    Local,
    Remote,
    Tag,
}

/// A label shown next to a commit in the graph.
#[derive(Clone, Debug)]
pub struct RefLabel {
    pub name: String,
    pub kind: RefKind,
    /// The currently checked out branch.
    pub is_head: bool,
    /// A local branch whose same-named remote branch points at the same commit.
    pub synced_remote: bool,
}

/// The commit list plus its graph layout. Shared between snapshots while the
/// refs do not move, so a status-only refresh costs nothing here.
pub struct History {
    pub commits: Vec<CommitInfo>,
    pub graph: graph::Graph,
    pub index_of: HashMap<Oid, u32>,
    pub has_more: bool,
}

pub struct RepoSnapshot {
    pub workdir: PathBuf,
    pub head: Head,
    pub state: RepoState,
    pub branches: Vec<BranchInfo>,
    pub remote_branches: Vec<RemoteBranch>,
    pub remotes: Vec<RemoteInfo>,
    pub tags: Vec<TagInfo>,
    pub stashes: Vec<StashInfo>,
    pub submodules: Vec<SubmoduleInfo>,
    pub status: WorkingStatus,
    /// Full message of the HEAD commit, used to prefill an amend.
    pub head_message: String,
    pub history: Arc<History>,
    pub refs_by_oid: HashMap<Oid, Vec<RefLabel>>,
    /// The repository's Gitflow setup, when it has one.
    pub gitflow: Option<gitflow::Config>,
}

impl RepoSnapshot {
    pub fn row_of(&self, oid: Oid) -> Option<usize> {
        self.history.index_of.get(&oid).map(|i| *i as usize)
    }

    pub fn current_branch(&self) -> Option<&BranchInfo> {
        self.branches.iter().find(|b| b.is_head)
    }

    pub fn head_label(&self) -> String {
        match (&self.head.branch, self.head.oid) {
            (Some(b), _) => b.clone(),
            (None, Some(oid)) => short(oid),
            _ => "(no branch)".to_owned(),
        }
    }

    pub fn default_remote(&self) -> Option<&str> {
        self.remotes
            .iter()
            .find(|r| r.name == "origin")
            .or_else(|| self.remotes.first())
            .map(|r| r.name.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DiffSource {
    /// Index -> working tree.
    Unstaged,
    /// HEAD -> index.
    Staged,
    /// First parent -> commit (also used for stashes).
    Commit(Oid),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DiffTarget {
    pub source: DiffSource,
    pub path: String,
    /// Where the file used to live, when it was renamed.
    pub old_path: Option<String>,
}

impl DiffTarget {
    pub fn new(source: DiffSource, file: &FileChange) -> Self {
        DiffTarget { source, path: file.path.clone(), old_path: file.old_path.clone() }
    }
}

#[derive(Clone, Debug)]
pub struct DiffLine {
    /// ' ', '+' or '-'.
    pub origin: char,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct Hunk {
    pub header: String,
    pub lines: Vec<DiffLine>,
    /// The exact patch bytes of this hunk (header included), so a single hunk
    /// can be fed back to `git apply`.
    pub raw: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
pub struct FileDiff {
    /// `diff --git ...` header up to and including the `+++` line.
    pub file_header: Vec<u8>,
    pub hunks: Vec<Hunk>,
    pub binary: bool,
    pub truncated: bool,
    pub additions: usize,
    pub deletions: usize,
    pub kind: Option<ChangeKind>,
    pub old_path: Option<String>,
    /// Longest line in characters, for sizing the horizontal scroll area.
    pub max_chars: usize,
    /// Both versions of a binary image file, decoded for the preview.
    pub images: Option<std::sync::Arc<picture::ImageDiff>>,
    /// The encoding and tab width the lines were prepared with.
    pub format: text::Format,
}

impl FileDiff {
    /// A standalone patch containing only hunk `i`.
    pub fn hunk_patch(&self, i: usize) -> Option<Vec<u8>> {
        let hunk = self.hunks.get(i)?;
        if self.file_header.is_empty() {
            return None;
        }
        let mut out = self.file_header.clone();
        out.extend_from_slice(&hunk.raw);
        Some(out)
    }
}

#[derive(Clone, Debug)]
pub struct CommitDetails {
    pub oid: Oid,
    pub parents: Vec<Oid>,
    pub summary: String,
    pub body: String,
    pub author: String,
    pub author_email: String,
    pub author_time: i64,
    pub committer: String,
    pub committer_time: i64,
    pub files: Vec<FileChange>,
}

pub fn short(oid: Oid) -> String {
    let s = oid.to_string();
    s[..7.min(s.len())].to_owned()
}
