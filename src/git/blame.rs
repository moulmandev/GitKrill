//! File history and blame.
//!
//! **History** is walked in-process with libgit2, one commit at a time, the
//! way `git log --follow -- <path>` does: a commit is listed when the file
//! differs from all of its parents, merges that took one side unchanged are
//! skipped, and when the file appears out of nowhere the walk looks for the
//! file it was renamed from and carries on under the old name. Unlike
//! `git log --follow` the name is tracked per branch of the walk, and the walk
//! can stop and resume anywhere ([`HistoryCursor`]), which is what makes
//! paging cheap: the first page costs the same whether the file has ten
//! revisions or ten thousand.
//!
//! **Blame** runs `git blame --porcelain` instead of libgit2's blame, for
//! three reasons:
//!
//! * Speed where it matters. On a 20,000-line file with 4,000 revisions git
//!   needed 11 s and libgit2 38 s (both attributing every line identically).
//!   On small files libgit2 is the quicker one, by the cost of starting a
//!   process (some 50-200 ms on Windows), which nobody waits for.
//! * It is the reference: `blame.ignoreRevsFile`, `.mailmap` and the rest of
//!   the user's configuration apply as they do in a terminal.
//! * It blames the working tree file directly, line endings converted the
//!   way git would on commit, and says which lines are not committed yet.
//!   With libgit2 that takes a second pass over a buffer we would have to
//!   filter ourselves.
//!
//! Its output also names, for every commit, the parent and path to blame to
//! look past it, which is what "Blame before this commit" follows.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use git2::{Commit, Delta, DiffFindOptions, Repository, Tree};

use super::repo::lossy;
use super::text::{self, Format, shown_with, source_text_in};
use super::{ChangeKind, Oid, cli, short};

fn err(e: git2::Error) -> String {
    e.message().to_owned()
}

// ---- history ----------------------------------------------------------------

/// One commit that changed the file.
#[derive(Clone, Debug)]
pub struct FileCommit {
    pub oid: Oid,
    pub summary: String,
    pub author: String,
    pub email: String,
    /// Author time, seconds since the epoch.
    pub time: i64,
    /// What the file was called in this commit (for a deletion: just before).
    pub path: String,
    /// What it was called in the parent, when this commit renamed it.
    pub old_path: Option<String>,
    /// Relative to the first parent, like the diff shown for the commit.
    pub kind: ChangeKind,
}

/// A tree entry's identity: blob id and file mode. `None` when the path is
/// not in the tree.
type Entry = Option<(Oid, i32)>;

fn entry_at(tree: &Tree<'_>, path: &str) -> Entry {
    let entry = tree.get_path(Path::new(path)).ok()?;
    Some((entry.id(), entry.filemode()))
}

/// A commit waiting to be examined, with the name the file has there.
struct Pending {
    /// Committer time: what orders the walk, as in `git log`.
    time: i64,
    /// Insertion order, to keep commits with equal times stable.
    seq: u64,
    oid: Oid,
    path: Arc<str>,
    /// The file in that commit, looked up when the commit was queued.
    entry: Entry,
}

impl PartialEq for Pending {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Pending {}

impl PartialOrd for Pending {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Pending {
    fn cmp(&self, other: &Self) -> Ordering {
        // Newest first; among equals, the one queued first.
        self.time.cmp(&other.time).then(other.seq.cmp(&self.seq))
    }
}

/// A walk through the history of one file that can be advanced a page at a
/// time. It holds no libgit2 objects, so it can travel between the UI and the
/// reader thread.
pub struct HistoryCursor {
    queue: BinaryHeap<Pending>,
    seen: HashSet<Oid>,
    seq: u64,
    /// Whether the file exists in the commit the walk started from.
    pub exists_at_start: bool,
}

impl HistoryCursor {
    /// Starts at `at`, or at HEAD when `None`.
    pub fn new(repo: &Repository, at: Option<Oid>, path: &str) -> Result<Self, String> {
        let start = match at {
            Some(oid) => repo.find_commit(oid).map_err(err)?,
            None => repo
                .head()
                .and_then(|head| head.peel_to_commit())
                .map_err(|_| "This repository has no commits yet.".to_owned())?,
        };
        let entry = entry_at(&start.tree().map_err(err)?, path);
        let mut cursor = HistoryCursor {
            queue: BinaryHeap::new(),
            seen: HashSet::new(),
            seq: 0,
            exists_at_start: entry.is_some(),
        };
        cursor.push(&start, path.into(), entry);
        Ok(cursor)
    }

    /// True once every reachable commit has been examined.
    pub fn is_done(&self) -> bool {
        self.queue.is_empty()
    }

    fn push(&mut self, commit: &Commit<'_>, path: Arc<str>, entry: Entry) {
        // A commit reached twice (through both sides of a merge) is examined
        // once, under the name it was first reached with.
        if self.seen.insert(commit.id()) {
            self.seq += 1;
            self.queue.push(Pending {
                time: commit.time().seconds(),
                seq: self.seq,
                oid: commit.id(),
                path,
                entry,
            });
        }
    }

    /// Walks on until `limit` more commits that changed the file are found,
    /// history runs out, or `budget` is spent. Returns what it found, newest
    /// first; an empty result does not mean the end, [`Self::is_done`] does.
    ///
    /// The budget exists because the stretch between two changes can be most
    /// of a repository's history (a file added last week, in a project that
    /// is twenty years old): the reader thread must not be held up for that.
    pub fn advance(
        &mut self,
        repo: &Repository,
        limit: usize,
        budget: Duration,
    ) -> Result<Vec<FileCommit>, String> {
        let started = Instant::now();
        let mut found = Vec::new();
        let mut examined = 0u32;

        while found.len() < limit {
            examined += 1;
            if examined.is_multiple_of(32) && started.elapsed() >= budget {
                break;
            }
            let Some(item) = self.queue.pop() else { break };
            let commit = repo.find_commit(item.oid).map_err(err)?;

            if commit.parent_count() == 0 {
                if item.entry.is_some() {
                    found.push(file_commit(&commit, &item.path, None, ChangeKind::Added));
                }
                continue;
            }

            let mut parents = Vec::with_capacity(commit.parent_count());
            for parent in commit.parents() {
                let tree = parent.tree().map_err(err)?;
                let entry = entry_at(&tree, &item.path);
                parents.push((parent, tree, entry));
            }

            // Unchanged from a parent: nothing happened here, and that parent
            // alone explains the file (this is how git simplifies history).
            if let Some((parent, _, entry)) = parents.iter().find(|(_, _, entry)| *entry == item.entry) {
                self.push(parent, item.path.clone(), *entry);
                continue;
            }

            let mut shown: Option<(ChangeKind, Option<String>)> = None;
            for (parent, parent_tree, entry) in &parents {
                let mut follow = (item.path.clone(), *entry);
                let kind = match (item.entry, entry) {
                    (Some(_), None) => {
                        let tree = commit.tree().map_err(err)?;
                        match renamed_from(repo, parent_tree, &tree, &item.path) {
                            Some(old) => {
                                follow = (old.as_str().into(), entry_at(parent_tree, &old));
                                (ChangeKind::Renamed, Some(old))
                            }
                            // A new file. The walk still continues under the
                            // same name: it may have existed before and been
                            // deleted in between.
                            None => (ChangeKind::Added, None),
                        }
                    }
                    (None, Some(_)) => (ChangeKind::Deleted, None),
                    _ => (ChangeKind::Modified, None),
                };
                shown.get_or_insert(kind);
                self.push(parent, follow.0, follow.1);
            }
            let (kind, old_path) = shown.unwrap_or((ChangeKind::Modified, None));
            found.push(file_commit(&commit, &item.path, old_path, kind));
        }
        Ok(found)
    }
}

fn file_commit(commit: &Commit<'_>, path: &str, old_path: Option<String>, kind: ChangeKind) -> FileCommit {
    let author = commit.author();
    FileCommit {
        oid: commit.id(),
        summary: commit.summary_bytes().map(lossy).unwrap_or_default(),
        author: lossy(author.name_bytes()),
        email: lossy(author.email_bytes()),
        time: author.when().seconds(),
        path: path.to_owned(),
        old_path,
        kind,
    }
}

/// The path `path` had in `old`, if going from `old` to `new` renamed it.
fn renamed_from(repo: &Repository, old: &Tree<'_>, new: &Tree<'_>, path: &str) -> Option<String> {
    // The old name can be anywhere, so the whole trees are compared; libgit2
    // only descends into directories that differ.
    let mut diff = repo.diff_tree_to_tree(Some(old), Some(new), None).ok()?;
    let mut find = DiffFindOptions::new();
    find.renames(true);
    diff.find_similar(Some(&mut find)).ok()?;
    diff.deltas()
        .find(|delta| {
            delta.status() == Delta::Renamed && delta.new_file().path_bytes() == Some(path.as_bytes())
        })
        .and_then(|delta| delta.old_file().path_bytes().map(lossy))
}

// ---- blame ------------------------------------------------------------------

/// Files larger than this are not blamed.
const MAX_BLAME_BYTES: u64 = 32 * 1024 * 1024;
/// How much of a file is inspected to decide whether it is text (git's rule).
const BINARY_SNIFF_BYTES: usize = 8_000;

/// A commit some lines of the file come from.
#[derive(Clone, Debug)]
pub struct BlameCommit {
    /// All zeroes for lines that are not committed yet.
    pub oid: Oid,
    pub summary: String,
    pub author: String,
    pub email: String,
    /// Author time, seconds since the epoch.
    pub time: i64,
    /// What the file was called in this commit.
    pub path: String,
    /// Where to blame to look past this commit: its parent and the file's
    /// name there. `None` when the commit added the file.
    pub previous: Option<(Oid, String)>,
}

impl BlameCommit {
    /// Lines that exist only in the working tree or the index.
    pub fn is_uncommitted(&self) -> bool {
        self.oid.is_zero()
    }
}

/// Consecutive lines that come from the same commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlameBlock {
    /// Index of the first line, from 0.
    pub start: usize,
    pub len: usize,
    /// Index into [`Blame::commits`].
    pub commit: usize,
}

#[derive(Clone, Debug, Default)]
pub struct Blame {
    pub commits: Vec<BlameCommit>,
    /// Every line belongs to exactly one block; blocks are in file order.
    pub blocks: Vec<BlameBlock>,
    /// The file's lines, prepared for display (tabs expanded, no line ends).
    pub lines: Vec<String>,
    /// The file's own text of the lines that `lines` shows differently
    /// (tabs, control characters, cut lines), by index: what copying gives.
    pub originals: BTreeMap<usize, String>,
    /// Longest line in characters, for sizing the horizontal scroll area.
    pub max_chars: usize,
    /// The commit whose version was blamed; `None` for the working tree.
    pub rev: Option<Oid>,
    /// The encoding and tab width the lines were prepared with.
    pub format: Format,
}

impl Blame {
    /// Index of the block containing line `line` (from 0).
    pub fn block_of(&self, line: usize) -> Option<usize> {
        let after = self.blocks.partition_point(|block| block.start <= line);
        let index = after.checked_sub(1)?;
        (line < self.blocks[index].start + self.blocks[index].len).then_some(index)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlameError {
    /// The file is not text.
    Binary,
    /// The file is not in the commit that was asked for (HEAD by default).
    Missing,
    /// The file is on disk but git has never been told about it.
    Untracked,
    /// The repository has no commits.
    NoCommits,
    TooLarge,
    /// Stopped through its [`Cancel`] handle.
    Cancelled,
    /// Anything git itself complained about.
    Git(String),
}

impl BlameError {
    /// A sentence for the user. `at` is the commit that was asked for.
    pub fn message(&self, path: &str, at: Option<Oid>) -> String {
        let place = match at {
            Some(oid) => format!("in commit {}", short(oid)),
            None => "at HEAD".to_owned(),
        };
        match self {
            BlameError::Binary => format!("{path} is a binary file - there are no lines to blame"),
            BlameError::Missing => format!("{path} does not exist {place}"),
            BlameError::Untracked => {
                format!("{path} has never been committed - there is nothing to blame yet")
            }
            BlameError::NoCommits => "This repository has no commits yet".to_owned(),
            BlameError::TooLarge => format!("{path} is too large to blame"),
            BlameError::Cancelled => "Blame was cancelled".to_owned(),
            BlameError::Git(message) => message.clone(),
        }
    }
}

/// Lets whoever started a blame stop its git process, so closing the view on
/// a file that takes a minute to blame does not leave git running.
#[derive(Clone, Default)]
pub struct Cancel(Arc<Mutex<CancelState>>);

#[derive(Default)]
struct CancelState {
    cancelled: bool,
    child: Option<Child>,
}

impl Cancel {
    pub fn cancel(&self) {
        let mut state = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.cancelled = true;
        if let Some(child) = &mut state.child {
            let _ = child.kill();
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, CancelState> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Blames `path` as of commit `at`, or the working tree file when `at` is
/// `None` (lines not committed yet are attributed to an all-zero commit).
///
/// Blocks until git is done, which can take a while on a long history: call
/// it off the UI thread, and not on a thread other work is queued behind.
pub fn blame(
    repo: &Repository,
    at: Option<Oid>,
    path: &str,
    cancel: &Cancel,
) -> Result<Blame, BlameError> {
    let git = |e: git2::Error| BlameError::Git(e.message().to_owned());
    let workdir = repo
        .workdir()
        .ok_or_else(|| BlameError::Git("Repository has no working directory".to_owned()))?;

    // Settle the cases git reports poorly (or not at all) before starting it.
    let commit = match at {
        Some(oid) => repo.find_commit(oid).map_err(git)?,
        None => repo
            .head()
            .and_then(|head| head.peel_to_commit())
            .map_err(|_| BlameError::NoCommits)?,
    };
    let committed = commit
        .tree()
        .map_err(git)?
        .get_path(Path::new(path))
        .ok()
        .and_then(|entry| repo.find_blob(entry.id()).ok());

    let mut rev = at;
    let format = text::format_for(workdir);
    // A UTF-16 file is blamed as bytes and read back as text.
    let utf16;
    if at.is_some() {
        let blob = committed.as_ref().ok_or(BlameError::Missing)?;
        utf16 = check_text(blob.content(), blob.size() as u64, format.encoding)?;
    } else {
        let staged = repo.index().ok().is_some_and(|index| index.get_path(Path::new(path), 0).is_some());
        match std::fs::File::open(workdir.join(path)) {
            Ok(mut file) => {
                if committed.is_none() && !staged {
                    return Err(BlameError::Untracked);
                }
                let size = file.metadata().map(|meta| meta.len()).unwrap_or(0);
                let mut head = vec![0u8; BINARY_SNIFF_BYTES.min(size as usize)];
                let read = file.read(&mut head).unwrap_or(0);
                utf16 = check_text(&head[..read], size, format.encoding)?;
            }
            // Deleted from the working tree: the committed version is all
            // there is to show.
            Err(_) => {
                let blob = committed.as_ref().ok_or(BlameError::Missing)?;
                utf16 = check_text(blob.content(), blob.size() as u64, format.encoding)?;
                rev = Some(commit.id());
            }
        }
    }

    let mut command = cli::command(workdir);
    command.args(["blame", "--porcelain"]);
    if let Some(oid) = rev {
        command.arg(oid.to_string());
    }
    command.arg("--").arg(path);
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());

    let (mut stdout, mut stderr) = {
        let mut state = cancel.state();
        if state.cancelled {
            return Err(BlameError::Cancelled);
        }
        let mut child = command.spawn().map_err(|e| {
            BlameError::Git(if e.kind() == std::io::ErrorKind::NotFound {
                cli::NOT_FOUND.to_owned()
            } else {
                format!("Failed to start git: {e}")
            })
        })?;
        let pipes = (child.stdout.take(), child.stderr.take());
        state.child = Some(child);
        pipes
    };

    // Both pipes are drained at once: git blocks when either one fills up.
    let mut out = Vec::new();
    let mut errors = Vec::new();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            if let Some(pipe) = &mut stderr {
                let _ = pipe.read_to_end(&mut errors);
            }
        });
        if let Some(pipe) = &mut stdout {
            let _ = pipe.read_to_end(&mut out);
        }
    });

    let (child, cancelled) = {
        let mut state = cancel.state();
        (state.child.take(), state.cancelled)
    };
    let status = child.map(|mut child| child.wait());
    if cancelled {
        return Err(BlameError::Cancelled);
    }
    match status {
        Some(Ok(status)) if status.success() => {}
        _ => {
            let message = String::from_utf8_lossy(&errors).trim().to_owned();
            return Err(BlameError::Git(if message.is_empty() {
                "git blame failed".to_owned()
            } else {
                message
            }));
        }
    }

    let mut blame = match utf16 {
        Some(found) => porcelain_rows(&out).map(|(commits, rows)| {
            // Commits with the same time: the one that has the other in its
            // history came later.
            let newer = |a: usize, b: usize| {
                let (a, b) = (&commits[a], &commits[b]);
                super::utf16::newer_by_time(a, b)
                    .unwrap_or_else(|| repo.graph_descendant_of(b.oid, a.oid).unwrap_or(false))
            };
            let lines = super::utf16::blame_lines(&rows, newer, found);
            assemble(commits, lines, &format)
        }),
        None => parse_porcelain(&out, &format),
    }
    .map_err(BlameError::Git)?;
    blame.rev = rev;
    Ok(blame)
}

/// Whether a file of `size` bytes that starts with `start` can be blamed,
/// and if it is UTF-16, how to read it (`text::utf16_encoding`; `default` is
/// the repository's encoding).
fn check_text(start: &[u8], size: u64, default: text::Encoding) -> Result<Option<text::FileEncoding>, BlameError> {
    if size > MAX_BLAME_BYTES {
        return Err(BlameError::TooLarge);
    }
    let start = &start[..start.len().min(BINARY_SNIFF_BYTES)];
    let marked = text::sniff_bom(start).is_some_and(|(encoding, _)| encoding.is_utf16());
    if marked || start.contains(&0) {
        return text::utf16_encoding(start, default).map(Some).ok_or(BlameError::Binary);
    }
    Ok(None)
}

/// Parses the output of `git blame --porcelain`.
///
/// Every line of the file comes as a header `<sha> <orig line> <final line>
/// [<count>]`, then (the first time a commit appears) `key value` lines
/// describing the commit, then the line itself behind a tab.
fn parse_porcelain(out: &[u8], format: &Format) -> Result<Blame, String> {
    let (commits, rows) = porcelain_rows(out)?;
    let lines = rows.into_iter().map(|(commit, content)| (commit, source_text_in(content, format.encoding)));
    Ok(assemble(commits, lines, format))
}

/// The commits of `git blame --porcelain` output, and the commit (an index
/// into them) and bytes of every line, in order.
#[allow(clippy::type_complexity)]
fn porcelain_rows(out: &[u8]) -> Result<(Vec<BlameCommit>, Vec<(usize, &[u8])>), String> {
    let bad = || "Could not understand the output of git blame".to_owned();
    let mut commits: Vec<BlameCommit> = Vec::new();
    let mut lines = Vec::new();
    let mut index: HashMap<Oid, usize> = HashMap::new();

    let mut rows = out.split(|byte| *byte == b'\n');
    while let Some(header) = rows.next() {
        if header.is_empty() {
            // The piece after the final newline.
            continue;
        }
        let mut fields = header.split(|byte| *byte == b' ');
        let oid = fields
            .next()
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| Oid::from_str(hex).ok())
            .ok_or_else(bad)?;
        let commit = *index.entry(oid).or_insert_with(|| {
            commits.push(BlameCommit {
                oid,
                summary: String::new(),
                author: String::new(),
                email: String::new(),
                time: 0,
                path: String::new(),
                previous: None,
            });
            commits.len() - 1
        });

        let content = loop {
            let row = rows.next().ok_or_else(bad)?;
            if let Some(content) = row.strip_prefix(b"\t") {
                break content;
            }
            let split = row.iter().position(|byte| *byte == b' ').unwrap_or(row.len());
            let (key, value) = (&row[..split], row.get(split + 1..).unwrap_or_default());
            let info = &mut commits[commit];
            match key {
                b"author" => info.author = lossy(value),
                b"author-mail" => {
                    info.email = lossy(value).trim_matches(['<', '>']).to_owned();
                }
                b"author-time" => {
                    info.time = std::str::from_utf8(value).ok().and_then(|v| v.parse().ok()).unwrap_or(0);
                }
                b"summary" => info.summary = lossy(value),
                // A commit can contribute lines under several names; the
                // first one seen is as good as any for looking past it.
                b"filename" if info.path.is_empty() => info.path = unquote(value),
                b"previous" if info.previous.is_none() => {
                    let split = value.iter().position(|byte| *byte == b' ').ok_or_else(bad)?;
                    let parent = std::str::from_utf8(&value[..split])
                        .ok()
                        .and_then(|hex| Oid::from_str(hex).ok())
                        .ok_or_else(bad)?;
                    info.previous = Some((parent, unquote(&value[split + 1..])));
                }
                _ => {}
            }
        };

        lines.push((commit, content));
    }
    Ok((commits, lines))
}

/// A blame of `lines`, the file's own text of each line and the commit it
/// comes from (an index into `commits`), prepared for display in `format`.
fn assemble<'a>(commits: Vec<BlameCommit>, lines: impl IntoIterator<Item = (usize, Cow<'a, str>)>, format: &Format) -> Blame {
    let mut blame = Blame { commits, format: *format, ..Blame::default() };
    for (commit, source) in lines {
        let line = blame.lines.len();
        match blame.blocks.last_mut() {
            Some(block) if block.commit == commit => block.len += 1,
            _ => blame.blocks.push(BlameBlock { start: line, len: 1, commit }),
        }
        let text = shown_with(&source, format.tab_width);
        if text != source {
            blame.originals.insert(line, source.into_owned());
        }
        blame.max_chars = blame.max_chars.max(text.chars().count());
        blame.lines.push(text);
    }
    blame
}

/// Undoes git's C-style quoting of unusual file names (`"a\tb\303\251"`).
fn unquote(raw: &[u8]) -> String {
    let Some(inner) = raw.strip_prefix(b"\"").and_then(|rest| rest.strip_suffix(b"\"")) else {
        return lossy(raw);
    };
    let mut out = Vec::with_capacity(inner.len());
    let mut bytes = inner.iter().copied().peekable();
    while let Some(byte) = bytes.next() {
        if byte != b'\\' {
            out.push(byte);
            continue;
        }
        match bytes.next() {
            Some(b'n') => out.push(b'\n'),
            Some(b't') => out.push(b'\t'),
            Some(b'r') => out.push(b'\r'),
            Some(b'a') => out.push(7),
            Some(b'b') => out.push(8),
            Some(b'f') => out.push(12),
            Some(b'v') => out.push(11),
            Some(digit @ b'0'..=b'7') => {
                let mut value = (digit - b'0') as u32;
                for _ in 0..2 {
                    match bytes.peek() {
                        Some(next @ b'0'..=b'7') => {
                            value = value * 8 + (*next - b'0') as u32;
                            bytes.next();
                        }
                        _ => break,
                    }
                }
                out.push(value as u8);
            }
            Some(other) => out.push(other),
            None => {}
        }
    }
    lossy(&out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{Fixture, git};

    const NO_BUDGET: Duration = Duration::from_secs(3600);

    fn head(fx: &Fixture) -> Oid {
        Oid::from_str(&git(&fx.dir, &["rev-parse", "HEAD"])).unwrap()
    }

    fn commit_as(fx: &Fixture, author: &str, message: &str) -> Oid {
        git(&fx.dir, &["add", "-A"]);
        let name = format!("user.name={author}");
        git(&fx.dir, &["-c", &name, "commit", "-q", "-m", message]);
        head(fx)
    }

    /// The whole history of `path` from HEAD, in pages of `page`.
    fn history(fx: &Fixture, path: &str, page: usize) -> (Vec<FileCommit>, bool) {
        let path = path.to_owned();
        fx.job(move |loader| {
            let repo = loader.repo();
            let mut cursor = HistoryCursor::new(repo, None, &path).unwrap();
            let exists = cursor.exists_at_start;
            let mut all = Vec::new();
            while !cursor.is_done() {
                let found = cursor.advance(repo, page, NO_BUDGET).unwrap();
                assert!(found.len() <= page);
                all.extend(found);
            }
            (all, exists)
        })
    }

    fn summaries(commits: &[FileCommit]) -> Vec<&str> {
        commits.iter().map(|c| c.summary.as_str()).collect()
    }

    fn blame_job(fx: &Fixture, at: Option<Oid>, path: &str) -> Result<Blame, BlameError> {
        let path = path.to_owned();
        fx.job(move |loader| blame(loader.repo(), at, &path, &Cancel::default()))
    }

    /// The commit each line is attributed to.
    fn line_oids(blame: &Blame) -> Vec<Oid> {
        (0..blame.lines.len())
            .map(|line| blame.commits[blame.blocks[blame.block_of(line).unwrap()].commit].oid)
            .collect()
    }

    const BODY: &str = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n";

    #[test]
    fn history_continues_past_a_rename_under_the_old_name() {
        let fx = Fixture::new("history-rename");
        fx.write("old/name.txt", BODY);
        fx.write("other.txt", "unrelated\n");
        fx.commit_all("create");
        fx.write("old/name.txt", &BODY.replace("two", "TWO"));
        fx.commit_all("edit before");
        fx.write("other.txt", "unrelated, changed\n");
        fx.commit_all("touch something else");
        git(&fx.dir, &["mv", "old/name.txt", "new.txt"]);
        fx.write("new.txt", &BODY.replace("two", "TWO").replace("seven", "SEVEN"));
        fx.commit_all("rename and edit");
        fx.write("new.txt", &BODY.replace("two", "TWO").replace("seven", "7"));
        fx.commit_all("edit after");

        let (commits, exists) = history(&fx, "new.txt", 50);
        assert!(exists);
        assert_eq!(summaries(&commits), ["edit after", "rename and edit", "edit before", "create"]);
        let paths: Vec<&str> = commits.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["new.txt", "new.txt", "old/name.txt", "old/name.txt"]);
        let kinds: Vec<ChangeKind> = commits.iter().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            [ChangeKind::Modified, ChangeKind::Renamed, ChangeKind::Modified, ChangeKind::Added]
        );
        assert_eq!(commits[1].old_path.as_deref(), Some("old/name.txt"));
        assert!(commits[0].old_path.is_none());

        // The rename commit's diff, asked for the way the UI does, is a
        // rename with its content change rather than a whole new file.
        let target = crate::git::DiffTarget {
            source: crate::git::DiffSource::Commit(commits[1].oid),
            path: commits[1].path.clone(),
            old_path: commits[1].old_path.clone(),
        };
        let diff = fx.job(move |loader| loader.diff(&target)).unwrap();
        assert_eq!(diff.kind, Some(ChangeKind::Renamed));
        assert_eq!((diff.additions, diff.deletions), (1, 1));
    }

    #[test]
    fn history_comes_in_pages_without_gaps_or_repeats() {
        let fx = Fixture::new("history-pages");
        for i in 0..12 {
            fx.write("a.txt", &format!("version {i}\n"));
            fx.write("noise.txt", &format!("noise {i}\n"));
            fx.commit_all(&format!("a {i}"));
            fx.write("noise.txt", &format!("more noise {i}\n"));
            fx.commit_all(&format!("noise {i}"));
        }

        let pages: Vec<Vec<String>> = fx.job(|loader| {
            let repo = loader.repo();
            let mut cursor = HistoryCursor::new(repo, None, "a.txt").unwrap();
            let mut pages = Vec::new();
            while !cursor.is_done() {
                let found = cursor.advance(repo, 5, NO_BUDGET).unwrap();
                pages.push(found.into_iter().map(|c| c.summary).collect());
            }
            pages
        });
        let sizes: Vec<usize> = pages.iter().map(Vec::len).collect();
        assert_eq!(sizes, [5, 5, 2]);
        let all: Vec<String> = pages.into_iter().flatten().collect();
        let expected: Vec<String> = (0..12).rev().map(|i| format!("a {i}")).collect();
        assert_eq!(all, expected);
    }

    #[test]
    fn a_spent_budget_pauses_the_walk_without_losing_commits() {
        let fx = Fixture::new("history-budget");
        fx.write("a.txt", "first\n");
        fx.commit_all("a");
        for i in 0..100 {
            fx.write("noise.txt", &format!("{i}\n"));
            fx.commit_all(&format!("noise {i}"));
        }

        let (rounds, found) = fx.job(|loader| {
            let repo = loader.repo();
            let mut cursor = HistoryCursor::new(repo, None, "a.txt").unwrap();
            let (mut rounds, mut found) = (0, Vec::new());
            while !cursor.is_done() {
                found.extend(cursor.advance(repo, 50, Duration::ZERO).unwrap());
                rounds += 1;
            }
            (rounds, found)
        });
        // 101 commits, at most 31 examined per round.
        assert!(rounds >= 4, "only {rounds} rounds");
        assert_eq!(summaries(&found), ["a"]);
    }

    #[test]
    fn history_of_a_deleted_file_is_still_found() {
        let fx = Fixture::new("history-deleted");
        fx.write("gone.txt", "here\n");
        fx.write("keep.txt", "keep\n");
        fx.commit_all("add");
        fx.write("gone.txt", "here, changed\n");
        fx.commit_all("change");
        std::fs::remove_file(fx.dir.join("gone.txt")).unwrap();
        fx.commit_all("remove");
        fx.write("keep.txt", "keep more\n");
        fx.commit_all("later");

        let (commits, exists) = history(&fx, "gone.txt", 50);
        assert!(!exists);
        assert_eq!(summaries(&commits), ["remove", "change", "add"]);
        assert_eq!(commits[0].kind, ChangeKind::Deleted);

        // Brought back later: both lives are listed.
        fx.write("gone.txt", "back again\n");
        fx.commit_all("restore");
        let (commits, exists) = history(&fx, "gone.txt", 50);
        assert!(exists);
        assert_eq!(summaries(&commits), ["restore", "remove", "change", "add"]);

        let (commits, exists) = history(&fx, "never/was.txt", 50);
        assert!(!exists);
        assert!(commits.is_empty());
    }

    #[test]
    fn merges_that_took_one_side_are_not_listed() {
        let fx = Fixture::new("history-merge");
        fx.write("a.txt", BODY);
        fx.write("b.txt", "b\n");
        fx.commit_all("base");
        git(&fx.dir, &["checkout", "-q", "-b", "side"]);
        fx.write("a.txt", &BODY.replace("two", "2"));
        fx.commit_all("side edit");
        git(&fx.dir, &["checkout", "-q", "main"]);
        fx.write("b.txt", "b on main\n");
        fx.commit_all("main edit elsewhere");
        git(&fx.dir, &["merge", "-q", "--no-ff", "side", "-m", "merge side"]);

        let (commits, _) = history(&fx, "a.txt", 50);
        assert_eq!(summaries(&commits), ["side edit", "base"]);

        // Both sides changed the file: the merge itself is a change too.
        git(&fx.dir, &["checkout", "-q", "-b", "side2"]);
        fx.write("a.txt", &BODY.replace("two", "2").replace("eight", "8"));
        fx.commit_all("side2 edit");
        git(&fx.dir, &["checkout", "-q", "main"]);
        fx.write("a.txt", &BODY.replace("two", "2").replace("one", "1"));
        fx.commit_all("main edit");
        git(&fx.dir, &["merge", "-q", "--no-ff", "side2", "-m", "merge side2"]);

        let (commits, _) = history(&fx, "a.txt", 50);
        let mut found = summaries(&commits);
        assert_eq!(found.remove(0), "merge side2");
        assert_eq!(found.len(), 4);
        for expected in ["side2 edit", "main edit", "side edit", "base"] {
            assert!(found.contains(&expected), "{expected} missing from {found:?}");
        }
        assert_eq!(found.last(), Some(&"base"));
    }

    #[test]
    fn history_before_the_first_commit_is_an_error_not_a_panic() {
        let fx = Fixture::new("history-unborn");
        fx.write("a.txt", "one\n");
        let result = fx.job(|loader| HistoryCursor::new(loader.repo(), None, "a.txt").map(|_| ()));
        assert!(result.is_err());
        assert_eq!(blame_job(&fx, None, "a.txt").unwrap_err(), BlameError::NoCommits);
    }

    #[test]
    fn blame_attributes_each_line_to_the_commit_that_wrote_it() {
        let fx = Fixture::new("blame-lines");
        fx.write("a.txt", BODY);
        let first = commit_as(&fx, "Ada", "create");
        fx.write("a.txt", &BODY.replace("two", "TWO").replace("three", "THREE"));
        let second = commit_as(&fx, "Grace", "shout");
        fx.write("a.txt", &BODY.replace("two", "TWO").replace("three", "THREE").replace("eight", "eight\nnine"));
        let third = commit_as(&fx, "Linus", "count further");

        let blame = blame_job(&fx, None, "a.txt").unwrap();
        assert_eq!(blame.rev, None);
        assert_eq!(blame.lines, ["one", "TWO", "THREE", "four", "five", "six", "seven", "eight", "nine"]);
        assert_eq!(
            line_oids(&blame),
            [first, second, second, first, first, first, first, first, third]
        );
        // Consecutive lines of one commit form one block.
        let blocks: Vec<(usize, usize)> = blame.blocks.iter().map(|b| (b.start, b.len)).collect();
        assert_eq!(blocks, [(0, 1), (1, 2), (3, 5), (8, 1)]);
        assert_eq!(blame.max_chars, 5);

        let grace = blame.commits.iter().find(|c| c.oid == second).unwrap();
        assert_eq!(grace.author, "Grace");
        assert_eq!(grace.email, "test@example.com");
        assert_eq!(grace.summary, "shout");
        assert_eq!(grace.path, "a.txt");
        assert_eq!(grace.previous, Some((first, "a.txt".to_owned())));
        assert!(grace.time > 1_700_000_000);
        let ada = blame.commits.iter().find(|c| c.oid == first).unwrap();
        assert_eq!(ada.previous, None);
    }

    #[test]
    fn blame_before_a_commit_follows_the_file_to_its_old_name() {
        let fx = Fixture::new("blame-parent");
        fx.write("old.txt", BODY);
        let first = commit_as(&fx, "Ada", "create");
        fx.write("old.txt", &BODY.replace("four", "FOUR"));
        let second = commit_as(&fx, "Grace", "edit");
        git(&fx.dir, &["mv", "old.txt", "new.txt"]);
        fx.write("new.txt", &BODY.replace("four", "FOUR").replace("six", "SIX"));
        let third = commit_as(&fx, "Linus", "rename and edit");

        let blame = blame_job(&fx, None, "new.txt").unwrap();
        assert_eq!(line_oids(&blame), [first, first, first, second, first, third, first, first]);

        // Looking past the rename lands on the parent, under the old name.
        let renamer = blame.commits.iter().find(|c| c.oid == third).unwrap();
        assert_eq!(renamer.path, "new.txt");
        let (parent, path) = renamer.previous.clone().unwrap();
        assert_eq!((parent, path.as_str()), (second, "old.txt"));

        let before = blame_job(&fx, Some(parent), &path).unwrap();
        assert_eq!(before.rev, Some(second));
        assert_eq!(before.lines[5], "six");
        assert_eq!(line_oids(&before), [first, first, first, second, first, first, first, first]);

        // And once more, to the very first version.
        let editor = before.commits.iter().find(|c| c.oid == second).unwrap();
        let (parent, path) = editor.previous.clone().unwrap();
        let original = blame_job(&fx, Some(parent), &path).unwrap();
        assert_eq!(original.lines[3], "four");
        assert!(line_oids(&original).iter().all(|oid| *oid == first));

        // The new name does not exist back then.
        assert_eq!(blame_job(&fx, Some(second), "new.txt").unwrap_err(), BlameError::Missing);
    }

    #[test]
    fn binary_files_are_refused_with_a_reason() {
        let fx = Fixture::new("blame-binary");
        std::fs::write(fx.dir.join("image.bin"), [0x89u8, b'P', b'N', b'G', 0, 0, 1, 2, 0, 255]).unwrap();
        fx.write("text.txt", "text\n");
        fx.commit_all("add");
        let oid = head(&fx);

        assert_eq!(blame_job(&fx, None, "image.bin").unwrap_err(), BlameError::Binary);
        assert_eq!(blame_job(&fx, Some(oid), "image.bin").unwrap_err(), BlameError::Binary);
        assert!(BlameError::Binary.message("image.bin", None).contains("binary"));

        // Its history is still there, and the commit's diff says "binary".
        let (commits, _) = history(&fx, "image.bin", 50);
        assert_eq!(summaries(&commits), ["add"]);
        let diff = fx.diff(crate::git::DiffSource::Commit(oid), "image.bin");
        assert!(diff.binary);
    }

    #[test]
    fn working_tree_blame_marks_uncommitted_lines() {
        let fx = Fixture::new("blame-worktree");
        fx.write("a.txt", BODY);
        fx.commit_all("create");
        let first = head(&fx);
        fx.write("a.txt", &BODY.replace("five", "FIVE (edited)"));
        fx.write("untracked.txt", "new\n");

        let blame = blame_job(&fx, None, "a.txt").unwrap();
        assert_eq!(blame.rev, None);
        assert_eq!(blame.lines[4], "FIVE (edited)");
        let oids = line_oids(&blame);
        assert!(oids[4].is_zero());
        assert!(oids.iter().enumerate().all(|(i, oid)| i == 4 || *oid == first));
        let pending = blame.commits.iter().find(|c| c.is_uncommitted()).unwrap();
        // Looking past the uncommitted lines means blaming HEAD.
        assert_eq!(pending.previous, Some((first, "a.txt".to_owned())));

        // The committed version on request.
        let committed = blame_job(&fx, Some(first), "a.txt").unwrap();
        assert_eq!(committed.lines[4], "five");
        assert!(committed.commits.iter().all(|c| !c.is_uncommitted()));

        assert_eq!(blame_job(&fx, None, "untracked.txt").unwrap_err(), BlameError::Untracked);
        assert_eq!(blame_job(&fx, None, "nowhere.txt").unwrap_err(), BlameError::Missing);

        // Deleted on disk only: the version at HEAD is what there is to show.
        std::fs::remove_file(fx.dir.join("a.txt")).unwrap();
        let deleted = blame_job(&fx, None, "a.txt").unwrap();
        assert_eq!(deleted.rev, Some(first));
        assert_eq!(deleted.lines[4], "five");
    }

    #[test]
    fn a_staged_rename_is_blamed_through_to_the_old_name() {
        let fx = Fixture::new("blame-staged-rename");
        fx.write("old.txt", BODY);
        fx.commit_all("create");
        let first = head(&fx);
        git(&fx.dir, &["mv", "old.txt", "new.txt"]);

        let blame = blame_job(&fx, None, "new.txt").unwrap();
        assert_eq!(blame.lines.len(), 8);
        assert!(line_oids(&blame).iter().all(|oid| *oid == first));
    }

    #[test]
    fn a_cancelled_blame_reports_it() {
        let fx = Fixture::new("blame-cancel");
        fx.write("a.txt", BODY);
        fx.commit_all("create");
        let result = fx.job(|loader| {
            let cancel = Cancel::default();
            cancel.cancel();
            blame(loader.repo(), None, "a.txt", &cancel)
        });
        assert_eq!(result.unwrap_err(), BlameError::Cancelled);
    }

    #[test]
    fn porcelain_details_are_parsed() {
        let a = "1111111111111111111111111111111111111111";
        let b = "2222222222222222222222222222222222222222";
        let out = format!(
            "{a} 1 1 2\nauthor Ada L\nauthor-mail <ada@example.com>\nauthor-time 1700000000\n\
             author-tz +0100\ncommitter C\ncommitter-mail <c@example.com>\ncommitter-time 1700000001\n\
             committer-tz +0000\nsummary First  one\nboundary\nfilename \"sp\\303\\251cial\\t\\\"x\\\".txt\"\n\
             \tfn main() {{\n{a} 2 2\n\t\tindented\r\n\
             {b} 7 3 1\nauthor Bob\nauthor-mail <bob@example.com>\nauthor-time 1700000500\n\
             summary Second\nprevious {a} old name.txt\nfilename new.txt\n\t\n{a} 3 4 1\n\tlast\n"
        );
        let blame = parse_porcelain(out.as_bytes(), &Format::default()).unwrap();
        assert_eq!(blame.lines, ["fn main() {", "    indented", "", "last"]);
        // The tab is kept for copying; lines shown as they are need nothing.
        let originals: Vec<(usize, &str)> = blame.originals.iter().map(|(line, text)| (*line, text.as_str())).collect();
        assert_eq!(originals, [(1, "\tindented")]);
        assert_eq!(blame.max_chars, 12);
        let blocks: Vec<(usize, usize, usize)> =
            blame.blocks.iter().map(|b| (b.start, b.len, b.commit)).collect();
        assert_eq!(blocks, [(0, 2, 0), (2, 1, 1), (3, 1, 0)]);
        assert_eq!(blame.block_of(1), Some(0));
        assert_eq!(blame.block_of(3), Some(2));
        assert_eq!(blame.block_of(4), None);

        let first = &blame.commits[0];
        assert_eq!(first.author, "Ada L");
        assert_eq!(first.email, "ada@example.com");
        assert_eq!(first.time, 1_700_000_000);
        assert_eq!(first.summary, "First  one");
        assert_eq!(first.path, "spécial\t\"x\".txt");
        assert_eq!(first.previous, None);
        let second = &blame.commits[1];
        assert_eq!(second.previous, Some((Oid::from_str(a).unwrap(), "old name.txt".to_owned())));
        assert_eq!(second.path, "new.txt");

        assert!(parse_porcelain(b"", &Format::default()).unwrap().lines.is_empty());
        assert!(parse_porcelain(b"not a blame\n", &Format::default()).is_err());
        assert!(parse_porcelain(format!("{a} 1 1 1\nauthor A\n").as_bytes(), &Format::default()).is_err());
    }

    #[test]
    fn blamed_lines_are_decoded_and_tabbed_as_set() {
        let a = "1111111111111111111111111111111111111111";
        let mut out = format!("{a} 1 1 2\nauthor A\nsummary S\nfilename f.txt\n\t").into_bytes();
        out.extend_from_slice(b"caf\xe9\tna\xefve\n");
        out.extend_from_slice(format!("{a} 2 2\n\t").as_bytes());
        out.extend_from_slice(b"\x93quoted\x94\n");
        let format = Format { encoding: text::Encoding::Windows1252, tab_width: 8, generation: 7 };
        let blame = parse_porcelain(&out, &format).unwrap();
        assert_eq!(blame.lines, ["café    naïve", "“quoted”"]);
        assert_eq!(blame.originals.get(&0).map(String::as_str), Some("café\tnaïve"));
        assert_eq!(blame.format, format);
        // In UTF-8 the same bytes are not text.
        let utf8 = parse_porcelain(&out, &Format::default()).unwrap();
        assert!(utf8.lines[0].contains('\u{fffd}'));
    }
}
