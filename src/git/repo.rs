//! Reading repository state through libgit2.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Arc;

use git2::{BranchType, ReferenceType, Repository, RepositoryState, Sort, Status, StatusOptions};

use super::graph::Graph;
use super::hidden::Hidden;
use super::{
    BranchInfo, ChangeKind, CommitDetails, CommitInfo, FileChange, Head, History, Oid, RefKind,
    RefLabel, RemoteBranch, RemoteInfo, RepoSnapshot, RepoState, RowKind, StashInfo,
    SubmoduleInfo, TagInfo, WorkingStatus,
};

pub fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn err(e: git2::Error) -> String {
    e.message().to_owned()
}

/// Owns the libgit2 handle for one repository plus the caches that make
/// repeated refreshes cheap.
pub struct Loader {
    pub(super) repo: Repository,
    /// Walked commits (with stash rows spliced in), keyed by a fingerprint of
    /// every ref. Walking is the expensive part of a refresh.
    walk_cache: Option<(u64, Arc<Vec<CommitInfo>>, bool)>,
    /// Final history including the optional WIP row and graph layout.
    history_cache: Option<(u64, bool, Arc<History>)>,
}

impl Loader {
    pub fn open(path: &Path) -> Result<Self, String> {
        let repo = Repository::discover(path).map_err(|_| {
            format!("{} is not a Git repository", path.display())
        })?;
        if repo.workdir().is_none() {
            return Err("Bare repositories are not supported".to_owned());
        }
        Ok(Loader { repo, walk_cache: None, history_cache: None })
    }

    /// The underlying libgit2 handle, for read-only queries run as jobs on
    /// the reader thread.
    pub fn repo(&self) -> &Repository {
        &self.repo
    }

    /// Everything the UI shows about the repository now. Refs that `hidden`
    /// hides are listed but not walked and not labelled; the checked out
    /// branch is never hidden.
    pub fn snapshot(&mut self, limit: usize, hidden: &Hidden) -> Result<RepoSnapshot, String> {
        let workdir = self.repo.workdir().ok_or("Repository has no working directory")?.to_path_buf();
        let head = read_head(&self.repo);
        let state = match self.repo.state() {
            RepositoryState::Clean => RepoState::Clean,
            RepositoryState::Merge => RepoState::Merge,
            RepositoryState::Revert | RepositoryState::RevertSequence => RepoState::Revert,
            RepositoryState::CherryPick | RepositoryState::CherryPickSequence => RepoState::CherryPick,
            RepositoryState::Bisect => RepoState::Bisect,
            RepositoryState::Rebase
            | RepositoryState::RebaseInteractive
            | RepositoryState::RebaseMerge => RepoState::Rebase,
            _ => RepoState::Other,
        };

        let remotes = self.remotes();
        let Refs { branches, remote_branches, tags } = self.refs(&head, &remotes);
        let stashes = self.stashes();
        let submodules = self.submodules();
        let status = self.status()?;

        let mut hasher = DefaultHasher::new();
        limit.hash(&mut hasher);
        head.oid.hash(&mut hasher);
        for b in &branches {
            b.oid.hash(&mut hasher);
        }
        for b in &remote_branches {
            b.oid.hash(&mut hasher);
        }
        for t in &tags {
            t.oid.hash(&mut hasher);
        }
        for st in &stashes {
            st.oid.hash(&mut hasher);
        }
        hidden.hash(&mut hasher);
        let fingerprint = hasher.finish();
        let shown = Shown::new(hidden, &branches, &remote_branches, &tags);

        let has_wip = !status.is_clean();
        let history = match &self.history_cache {
            Some((fp, wip, history)) if *fp == fingerprint && *wip == has_wip => history.clone(),
            _ => {
                let (base, has_more) = match &self.walk_cache {
                    Some((fp, base, more)) if *fp == fingerprint => (base.clone(), *more),
                    _ => {
                        let mut tips: Vec<Oid> = Vec::new();
                        tips.extend(head.oid);
                        tips.extend(shown.branches(&branches).filter_map(|b| b.oid));
                        tips.extend(shown.remotes(&remote_branches).map(|b| b.oid));
                        tips.extend(shown.tags(&tags).map(|t| t.oid));
                        let (commits, more) = self.walk(&tips, limit, &stashes);
                        let base = Arc::new(commits);
                        self.walk_cache = Some((fingerprint, base.clone(), more));
                        (base, more)
                    }
                };

                let mut commits = Vec::with_capacity(base.len() + 1);
                if has_wip {
                    commits.push(CommitInfo {
                        oid: Oid::ZERO_SHA1,
                        parents: head.oid.into_iter().collect(),
                        summary: "// WIP".to_owned(),
                        body_preview: String::new(),
                        author: String::new(),
                        email: String::new(),
                        time: 0,
                        kind: RowKind::Wip,
                    });
                }
                commits.extend(base.iter().cloned());

                let graph = Graph::build(&commits);
                let index_of =
                    commits.iter().enumerate().map(|(i, c)| (c.oid, i as u32)).collect();
                let history = Arc::new(History { commits, graph, index_of, has_more });
                self.history_cache = Some((fingerprint, has_wip, history.clone()));
                history
            }
        };

        let refs_by_oid = build_labels(&head, &branches, &remote_branches, &tags, &shown);
        let head_message = head
            .oid
            .and_then(|oid| self.repo.find_commit(oid).ok())
            .map(|c| lossy(c.message_bytes()).trim().to_owned())
            .unwrap_or_default();

        Ok(RepoSnapshot {
            workdir,
            head,
            state,
            branches,
            remote_branches,
            remotes,
            tags,
            stashes,
            submodules,
            status,
            head_message,
            history,
            refs_by_oid,
        })
    }

    fn remotes(&self) -> Vec<RemoteInfo> {
        let Ok(names) = self.repo.remotes() else { return Vec::new() };
        names
            .iter_bytes()
            .map(lossy)
            .map(|name| {
                let url = self
                    .repo
                    .find_remote(&name)
                    .map(|r| lossy(r.url_bytes()))
                    .unwrap_or_default();
                RemoteInfo { name, url }
            })
            .collect()
    }

    /// Every branch and tag in a single pass over the ref database.
    ///
    /// Upstreams come from one read of the config rather than a lookup per
    /// branch, which is what keeps repositories with thousands of branches
    /// quick to refresh.
    fn refs(&self, head: &Head, remotes: &[RemoteInfo]) -> Refs {
        let mut locals: Vec<(String, Option<Oid>)> = Vec::new();
        let mut remote_branches: Vec<RemoteBranch> = Vec::new();
        let mut tags: Vec<TagInfo> = Vec::new();

        if let Ok(references) = self.repo.references() {
            for reference in references.flatten() {
                let full = reference.name_bytes();
                if let Some(name) = full.strip_prefix(b"refs/heads/") {
                    let oid = reference.target().or_else(|| reference.resolve().ok()?.target());
                    locals.push((lossy(name), oid));
                } else if let Some(name) = full.strip_prefix(b"refs/remotes/") {
                    // Skip symbolic refs such as origin/HEAD.
                    if reference.kind() == Some(ReferenceType::Symbolic) {
                        continue;
                    }
                    let Some(oid) = reference.target() else { continue };
                    let full = lossy(name);
                    // Remote names may contain slashes, so match the known list.
                    let remote = remotes
                        .iter()
                        .filter(|r| {
                            full.len() > r.name.len()
                                && full.starts_with(&r.name)
                                && full.as_bytes()[r.name.len()] == b'/'
                        })
                        .max_by_key(|r| r.name.len())
                        .map(|r| r.name.clone())
                        .unwrap_or_else(|| full.split('/').next().unwrap_or_default().to_owned());
                    let name = full.get(remote.len() + 1..).unwrap_or(&full).to_owned();
                    remote_branches.push(RemoteBranch { remote, name, full, oid });
                } else if let Some(name) = full.strip_prefix(b"refs/tags/") {
                    // Annotated tags point at a tag object; packed refs often
                    // carry the peeled commit already, saving an object read.
                    let oid = reference
                        .target_peel()
                        .or_else(|| reference.peel_to_commit().ok().map(|c| c.id()));
                    // Tags on trees or blobs have no place in a commit graph.
                    if let Some(oid) = oid {
                        tags.push(TagInfo { name: lossy(name), oid });
                    }
                }
            }
        }

        let upstreams = self.upstreams();
        let mut branches: Vec<BranchInfo> = locals
            .iter()
            .map(|(name, oid)| {
                let config = upstreams.get(name);
                let upstream = config.map(|(remote, merge)| {
                    let short = merge.strip_prefix("refs/heads/").unwrap_or(merge);
                    if remote == "." { short.to_owned() } else { format!("{remote}/{short}") }
                });
                let upstream_oid = upstream.as_ref().and_then(|up| {
                    if config.is_some_and(|(remote, _)| remote == ".") {
                        locals.iter().find(|(n, _)| n == up).and_then(|(_, oid)| *oid)
                    } else {
                        remote_branches.iter().find(|r| r.full == *up).map(|r| r.oid)
                    }
                });
                let (ahead, behind) = match (*oid, upstream_oid) {
                    (Some(local), Some(remote)) if local != remote => {
                        self.repo.graph_ahead_behind(local, remote).unwrap_or((0, 0))
                    }
                    _ => (0, 0),
                };
                BranchInfo {
                    is_head: !head.detached && head.branch.as_deref() == Some(name.as_str()),
                    name: name.clone(),
                    oid: *oid,
                    upstream,
                    ahead,
                    behind,
                }
            })
            .collect();

        branches.sort_by_cached_key(|b| b.name.to_lowercase());
        remote_branches.sort_by_cached_key(|b| b.full.to_lowercase());
        tags.sort_by_cached_key(|t| t.name.to_lowercase());
        Refs { branches, remote_branches, tags }
    }

    /// `branch.<name>.remote` / `.merge` for every branch: name -> (remote, merge ref).
    fn upstreams(&self) -> HashMap<String, (String, String)> {
        let mut partial: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
        let Ok(config) = self.repo.config().and_then(|mut c| c.snapshot()) else {
            return HashMap::new();
        };
        if let Ok(mut entries) = config.entries(Some(r"^branch\..*\.(remote|merge)$")) {
            while let Some(Ok(entry)) = entries.next() {
                let key = lossy(entry.name_bytes());
                let Some(rest) = key.strip_prefix("branch.") else { continue };
                let Some((branch, field)) = rest.rsplit_once('.') else { continue };
                let value = lossy(entry.value_bytes());
                let slot = partial.entry(branch.to_owned()).or_default();
                match field {
                    "remote" => slot.0 = Some(value),
                    "merge" => slot.1 = Some(value),
                    _ => {}
                }
            }
        }
        partial
            .into_iter()
            .filter_map(|(branch, (remote, merge))| Some((branch, (remote?, merge?))))
            .collect()
    }

    fn stashes(&mut self) -> Vec<StashInfo> {
        let mut out = Vec::new();
        let _ = self.repo.stash_foreach(|index, message, oid| {
            out.push(StashInfo { index, message: message.to_owned(), oid: *oid });
            true
        });
        out
    }

    fn submodules(&self) -> Vec<SubmoduleInfo> {
        let Ok(subs) = self.repo.submodules() else { return Vec::new() };
        subs.iter()
            .map(|s| SubmoduleInfo {
                name: lossy(s.name_bytes()),
                path: s.path().to_string_lossy().replace('\\', "/"),
            })
            .collect()
    }

    fn status(&self) -> Result<WorkingStatus, String> {
        let mut opts = StatusOptions::new();
        opts.include_untracked(true)
            .recurse_untracked_dirs(true)
            .include_ignored(false)
            .renames_head_to_index(true);
        let statuses = self.repo.statuses(Some(&mut opts)).map_err(err)?;

        let mut status = WorkingStatus::default();
        for entry in statuses.iter() {
            let st = entry.status();
            let path = lossy(entry.path_bytes());

            if st.contains(Status::CONFLICTED) {
                status.unstaged.push(FileChange { path, old_path: None, kind: ChangeKind::Conflicted });
                continue;
            }

            let staged = if st.contains(Status::INDEX_RENAMED) {
                Some(ChangeKind::Renamed)
            } else if st.contains(Status::INDEX_NEW) {
                Some(ChangeKind::Added)
            } else if st.contains(Status::INDEX_DELETED) {
                Some(ChangeKind::Deleted)
            } else if st.contains(Status::INDEX_TYPECHANGE) {
                Some(ChangeKind::Typechange)
            } else if st.contains(Status::INDEX_MODIFIED) {
                Some(ChangeKind::Modified)
            } else {
                None
            };
            if let Some(kind) = staged {
                let (new_path, old_path) = delta_paths(entry.head_to_index(), &path);
                status.staged.push(FileChange {
                    path: new_path,
                    old_path: old_path.filter(|_| kind == ChangeKind::Renamed),
                    kind,
                });
            }

            let unstaged = if st.contains(Status::WT_NEW) {
                Some(ChangeKind::Untracked)
            } else if st.contains(Status::WT_RENAMED) {
                Some(ChangeKind::Renamed)
            } else if st.contains(Status::WT_DELETED) {
                Some(ChangeKind::Deleted)
            } else if st.contains(Status::WT_TYPECHANGE) {
                Some(ChangeKind::Typechange)
            } else if st.contains(Status::WT_MODIFIED) {
                Some(ChangeKind::Modified)
            } else {
                None
            };
            if let Some(kind) = unstaged {
                let (new_path, old_path) = delta_paths(entry.index_to_workdir(), &path);
                status.unstaged.push(FileChange {
                    path: new_path,
                    old_path: old_path.filter(|_| kind == ChangeKind::Renamed),
                    kind,
                });
            }
        }
        Ok(status)
    }

    /// Walks history from `tips`, newest first with parents after children,
    /// and splices each stash in directly above the commit it was made on.
    ///
    /// libgit2 walks all of history before yielding anything as soon as any
    /// sort mode is requested. Its unsorted mode, however, streams: it keeps
    /// the frontier ordered by commit time, exactly like `git log` does. So
    /// the tips are handed over ordered by time and the walk is left
    /// "unsorted", which makes the first page cost the same in a repository
    /// with a thousand commits as in one with a million.
    ///
    /// Clock skew can make a time-ordered walk yield a parent before its
    /// child, so the page is straightened out afterwards by [`children_first`].
    fn walk(&self, tips: &[Oid], limit: usize, stashes: &[StashInfo]) -> (Vec<CommitInfo>, bool) {
        let mut commits = Vec::new();
        let mut has_more = false;

        let mut timed: Vec<(i64, Oid)> = tips
            .iter()
            .filter_map(|oid| Some((self.repo.find_commit(*oid).ok()?.time().seconds(), *oid)))
            .collect();
        // Pushes are stacked, so the last one pushed is visited first: push
        // oldest to newest.
        timed.sort_unstable();
        timed.dedup();

        if let Ok(mut walk) = self.repo.revwalk() {
            let _ = walk.set_sorting(Sort::NONE);
            for (_, tip) in &timed {
                let _ = walk.push(*tip);
            }
            for oid in walk.flatten() {
                if commits.len() >= limit {
                    has_more = true;
                    break;
                }
                if let Ok(commit) = self.repo.find_commit(oid) {
                    commits.push(commit_info(&commit, RowKind::Commit));
                }
            }
        }
        let commits = children_first(commits);

        if stashes.is_empty() {
            return (commits, has_more);
        }

        let mut by_base: HashMap<Oid, Vec<CommitInfo>> = HashMap::new();
        for stash in stashes {
            let Ok(commit) = self.repo.find_commit(stash.oid) else { continue };
            let Ok(base) = commit.parent_id(0) else { continue };
            let mut info = commit_info(&commit, RowKind::Stash);
            // The other parents are internal (index / untracked snapshots).
            info.parents = vec![base];
            info.summary = stash.message.clone();
            info.body_preview.clear();
            by_base.entry(base).or_default().push(info);
        }

        let mut out = Vec::with_capacity(commits.len() + stashes.len());
        for commit in commits {
            if let Some(rows) = by_base.remove(&commit.oid) {
                out.extend(rows);
            }
            out.push(commit);
        }
        (out, has_more)
    }

    pub fn commit_details(&self, oid: Oid, is_stash: bool) -> Result<CommitDetails, String> {
        let commit = self.repo.find_commit(oid).map_err(err)?;
        let tree = commit.tree().map_err(err)?;
        let parent_tree = match commit.parent(0) {
            Ok(parent) => Some(parent.tree().map_err(err)?),
            Err(_) => None,
        };

        let mut diff = self
            .repo
            .diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), None)
            .map_err(err)?;
        let mut find = git2::DiffFindOptions::new();
        find.renames(true);
        let _ = diff.find_similar(Some(&mut find));

        let mut files: Vec<FileChange> = diff
            .deltas()
            .filter_map(|delta| {
                let kind = delta_kind(delta.status())?;
                let new_path = delta.new_file().path_bytes().map(lossy);
                let old_path = delta.old_file().path_bytes().map(lossy);
                let path = new_path.clone().or_else(|| old_path.clone())?;
                Some(FileChange {
                    path,
                    old_path: old_path.filter(|_| kind == ChangeKind::Renamed),
                    kind,
                })
            })
            .collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));

        let author = commit.author();
        let committer = commit.committer();
        // A stash is a merge commit under the hood; only its base is a real parent.
        let parents: Vec<Oid> = if is_stash {
            commit.parent_ids().take(1).collect()
        } else {
            commit.parent_ids().collect()
        };

        Ok(CommitDetails {
            oid,
            parents,
            summary: commit.summary_bytes().map(lossy).unwrap_or_default(),
            body: commit.body_bytes().map(lossy).unwrap_or_default().trim().to_owned(),
            author: lossy(author.name_bytes()),
            author_email: lossy(author.email_bytes()),
            author_time: author.when().seconds(),
            committer: lossy(committer.name_bytes()),
            committer_time: committer.when().seconds(),
            files,
        })
    }
}

struct Refs {
    branches: Vec<BranchInfo>,
    remote_branches: Vec<RemoteBranch>,
    tags: Vec<TagInfo>,
}

/// Reorders `commits` so every commit comes before its parents, otherwise
/// keeping the given order. Already-ordered input is returned untouched.
pub(super) fn children_first(commits: Vec<CommitInfo>) -> Vec<CommitInfo> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;

    let index: HashMap<Oid, usize> =
        commits.iter().enumerate().map(|(i, c)| (c.oid, i)).collect();
    let ordered = commits
        .iter()
        .enumerate()
        .all(|(i, c)| c.parents.iter().all(|p| index.get(p).is_none_or(|j| *j > i)));
    if ordered {
        return commits;
    }

    // Kahn's algorithm, always emitting the earliest commit whose children
    // have all been emitted, so the result stays as close to the input as a
    // valid order allows.
    let mut waiting_children = vec![0u32; commits.len()];
    for commit in &commits {
        for parent in &commit.parents {
            if let Some(j) = index.get(parent) {
                waiting_children[*j] += 1;
            }
        }
    }
    let mut ready: BinaryHeap<Reverse<usize>> = (0..commits.len())
        .filter(|i| waiting_children[*i] == 0)
        .map(Reverse)
        .collect();
    let mut order = Vec::with_capacity(commits.len());
    while let Some(Reverse(i)) = ready.pop() {
        order.push(i);
        for parent in &commits[i].parents {
            if let Some(&j) = index.get(parent) {
                waiting_children[j] -= 1;
                if waiting_children[j] == 0 {
                    ready.push(Reverse(j));
                }
            }
        }
    }

    let mut slots: Vec<Option<CommitInfo>> = commits.into_iter().map(Some).collect();
    order.into_iter().filter_map(|i| slots[i].take()).collect()
}

fn delta_paths(delta: Option<git2::DiffDelta<'_>>, fallback: &str) -> (String, Option<String>) {
    match delta {
        Some(delta) => {
            let new_path = delta.new_file().path_bytes().map(lossy);
            let old_path = delta.old_file().path_bytes().map(lossy);
            let path = new_path.or_else(|| old_path.clone()).unwrap_or_else(|| fallback.to_owned());
            let old_path = old_path.filter(|old| *old != path);
            (path, old_path)
        }
        None => (fallback.to_owned(), None),
    }
}

pub(super) fn delta_kind(status: git2::Delta) -> Option<ChangeKind> {
    use git2::Delta;
    Some(match status {
        Delta::Added | Delta::Copied => ChangeKind::Added,
        Delta::Deleted => ChangeKind::Deleted,
        Delta::Modified => ChangeKind::Modified,
        Delta::Renamed => ChangeKind::Renamed,
        Delta::Typechange => ChangeKind::Typechange,
        Delta::Untracked => ChangeKind::Untracked,
        Delta::Conflicted => ChangeKind::Conflicted,
        _ => return None,
    })
}

fn commit_info(commit: &git2::Commit<'_>, kind: RowKind) -> CommitInfo {
    let author = commit.author();
    let body_preview = commit
        .body_bytes()
        .map(|body| {
            let text = String::from_utf8_lossy(body);
            let mut flat = String::new();
            for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
                if !flat.is_empty() {
                    flat.push_str(" | ");
                }
                flat.push_str(line);
                if flat.len() > 240 {
                    break;
                }
            }
            flat
        })
        .unwrap_or_default();

    CommitInfo {
        oid: commit.id(),
        parents: commit.parent_ids().collect(),
        summary: commit.summary_bytes().map(lossy).unwrap_or_default(),
        body_preview,
        author: lossy(author.name_bytes()),
        email: lossy(author.email_bytes()),
        time: author.when().seconds(),
        kind,
    }
}

/// HEAD as a branch name and/or commit. Works on unborn branches too.
pub fn read_head(repo: &Repository) -> Head {
    match repo.head() {
        Ok(reference) => {
            let detached = repo.head_detached().unwrap_or(false);
            let branch = (!detached).then(|| lossy(reference.shorthand_bytes()));
            Head { branch, oid: reference.target(), detached }
        }
        Err(_) => {
            let branch = repo.find_reference("HEAD").ok().and_then(|r| {
                r.symbolic_target_bytes()
                    .map(lossy)
                    .map(|t| t.strip_prefix("refs/heads/").unwrap_or(&t).to_owned())
            });
            Head { branch, oid: None, detached: false }
        }
    }
}

pub fn branch_oid(repo: &Repository, name: &str) -> Option<Oid> {
    repo.find_branch(name, BranchType::Local).ok()?.get().target()
}

/// Which refs the graph shows: one flag per entry of the snapshot's branch,
/// remote branch and tag lists.
struct Shown {
    branches: Vec<bool>,
    remotes: Vec<bool>,
    tags: Vec<bool>,
}

impl Shown {
    fn new(hidden: &Hidden, branches: &[BranchInfo], remotes: &[RemoteBranch], tags: &[TagInfo]) -> Self {
        Shown {
            branches: branches.iter().map(|b| b.is_head || !hidden.hides_local(&b.name)).collect(),
            remotes: remotes.iter().map(|b| !hidden.hides_remote(&b.full)).collect(),
            tags: tags.iter().map(|t| !hidden.hides_tag(&t.name)).collect(),
        }
    }

    fn branches<'a>(&'a self, all: &'a [BranchInfo]) -> impl Iterator<Item = &'a BranchInfo> {
        all.iter().zip(&self.branches).filter(|(_, shown)| **shown).map(|(b, _)| b)
    }

    fn remotes<'a>(&'a self, all: &'a [RemoteBranch]) -> impl Iterator<Item = &'a RemoteBranch> {
        all.iter().zip(&self.remotes).filter(|(_, shown)| **shown).map(|(b, _)| b)
    }

    fn tags<'a>(&'a self, all: &'a [TagInfo]) -> impl Iterator<Item = &'a TagInfo> {
        all.iter().zip(&self.tags).filter(|(_, shown)| **shown).map(|(t, _)| t)
    }
}

fn build_labels(
    head: &Head,
    branches: &[BranchInfo],
    remote_branches: &[RemoteBranch],
    tags: &[TagInfo],
    shown: &Shown,
) -> HashMap<Oid, Vec<RefLabel>> {
    let mut map: HashMap<Oid, Vec<RefLabel>> = HashMap::new();

    if let (true, Some(oid)) = (head.detached, head.oid) {
        map.entry(oid).or_default().push(RefLabel {
            name: "HEAD".to_owned(),
            kind: RefKind::Head,
            is_head: true,
            synced_remote: false,
        });
    }

    for branch in shown.branches(branches) {
        let Some(oid) = branch.oid else { continue };
        // Whether or not that remote branch is hidden: it is a fact about
        // this branch.
        let synced_remote =
            remote_branches.iter().any(|r| r.name == branch.name && r.oid == oid);
        map.entry(oid).or_default().push(RefLabel {
            name: branch.name.clone(),
            kind: RefKind::Local,
            is_head: branch.is_head,
            synced_remote,
        });
    }

    for remote in shown.remotes(remote_branches) {
        // Folded into the local label above when both sit on the same commit.
        let folded = shown
            .branches(branches)
            .any(|b| b.name == remote.name && b.oid == Some(remote.oid));
        if folded {
            continue;
        }
        map.entry(remote.oid).or_default().push(RefLabel {
            name: remote.full.clone(),
            kind: RefKind::Remote,
            is_head: false,
            synced_remote: false,
        });
    }

    for tag in shown.tags(tags) {
        map.entry(tag.oid).or_default().push(RefLabel {
            name: tag.name.clone(),
            kind: RefKind::Tag,
            is_head: false,
            synced_remote: false,
        });
    }

    for labels in map.values_mut() {
        labels.sort_by_key(|l| (!l.is_head, l.kind as u8));
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(n: u8) -> Oid {
        let mut bytes = [0u8; 20];
        bytes[19] = n;
        Oid::from_bytes(&bytes).unwrap()
    }

    fn commit(id: u8, parents: &[u8]) -> CommitInfo {
        CommitInfo {
            oid: oid(id),
            parents: parents.iter().map(|p| oid(*p)).collect(),
            summary: String::new(),
            body_preview: String::new(),
            author: String::new(),
            email: String::new(),
            time: 0,
            kind: RowKind::Commit,
        }
    }

    fn ids(commits: &[CommitInfo]) -> Vec<u8> {
        commits.iter().map(|c| c.oid.as_bytes()[19]).collect()
    }

    #[test]
    fn ordered_history_is_left_alone() {
        let commits = vec![commit(3, &[2]), commit(2, &[1]), commit(1, &[])];
        assert_eq!(ids(&children_first(commits)), [3, 2, 1]);
    }

    #[test]
    fn a_parent_listed_too_early_moves_below_its_children() {
        // Clock skew: 2 (parent of 4) was yielded before 4.
        let commits = vec![commit(5, &[3]), commit(2, &[1]), commit(4, &[2]), commit(3, &[1]), commit(1, &[])];
        let fixed = ids(&children_first(commits));
        assert_eq!(fixed, [5, 4, 2, 3, 1]);
    }

    #[test]
    fn parents_outside_the_page_are_ignored() {
        let commits = vec![commit(3, &[9]), commit(2, &[8, 3])];
        assert_eq!(ids(&children_first(commits)), [2, 3]);
    }
}
