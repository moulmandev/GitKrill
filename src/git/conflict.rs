//! Reading the sides of a merge conflict.
//!
//! A conflicted path has up to three entries in the index: the common
//! ancestor (stage 1), our version (stage 2) and their version (stage 3).
//! [`load`] reads them and, for text, merges them again in memory into a
//! list of [`Segment`]s: stretches git merges on its own, and the regions
//! where both sides changed the same lines. Working from the index rather
//! than from the markers in the working file means the resolver still knows
//! both sides after the file has been edited.
//!
//! A [`Draft`] holds what the user decided for each region. [`render`] turns
//! the segments plus a draft into the bytes of the resolved file, copying
//! every untouched line byte for byte (line endings, a missing final
//! newline and non-UTF-8 text all survive). [`save`] and [`keep`] /
//! [`delete`] build the commands that write the outcome.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use git2::{BranchType, ObjectType, Repository, RepositoryState};

use super::ops::{GitCmd, Step};
use super::repo::{lossy, read_head};
use super::text::{self, Encoding, Eol, FileEncoding};
use super::{Oid, short};

/// Git looks for a NUL in this many leading bytes to call a file binary.
const BINARY_SNIFF: usize = 8000;
/// Larger files get the whole-file choice instead of the line editor.
const MAX_TEXT_BYTES: usize = 16 * 1024 * 1024;
/// Width of the markers written for regions that are still unresolved.
const MARKER_SIZE: usize = 7;

fn err(e: git2::Error) -> String {
    e.message().to_owned()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// Stage 2: what HEAD had when the operation stopped.
    Ours,
    /// Stage 3: what is being merged, picked or replayed.
    Theirs,
}

/// Some lines of a file, kept as the exact bytes they came with.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Chunk {
    bytes: Vec<u8>,
    /// Offset at which each line starts.
    starts: Vec<u32>,
}

impl Chunk {
    pub fn new(bytes: Vec<u8>) -> Self {
        let mut starts = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            starts.push(at as u32);
            at = match bytes[at..].iter().position(|b| *b == b'\n') {
                Some(i) => at + i + 1,
                None => bytes.len(),
            };
        }
        Chunk { bytes, starts }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Number of lines.
    pub fn len(&self) -> usize {
        self.starts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.starts.is_empty()
    }

    fn offset(&self, line: usize) -> usize {
        self.starts.get(line).map_or(self.bytes.len(), |s| *s as usize)
    }

    /// Lines `from..to` with their line endings.
    fn raw(&self, from: usize, to: usize) -> &[u8] {
        &self.bytes[self.offset(from)..self.offset(to)]
    }

    fn has_newline(&self, line: usize) -> bool {
        self.raw(line, line + 1).ends_with(b"\n")
    }

    /// Whether the last line is terminated. Only the last line of a file can
    /// lack its newline, so this is false for at most one chunk per side.
    pub fn ends_with_newline(&self) -> bool {
        self.bytes.ends_with(b"\n")
    }

    /// Line `line` without its line ending, for display. Chunks are UTF-8
    /// (see [`load`]); a byte order mark at the start of the file is not shown.
    pub fn line(&self, line: usize) -> Cow<'_, str> {
        Encoding::Utf8.decode(strip_eol(self.raw(line, line + 1)))
    }

    /// All lines as editor text: joined by `\n`, no trailing newline.
    pub fn text(&self) -> String {
        (0..self.len()).map(|i| self.line(i)).collect::<Vec<_>>().join("\n")
    }

    /// Longest line in bytes, an upper bound for its width in characters.
    pub fn longest_line(&self) -> usize {
        (0..self.len()).map(|i| self.offset(i + 1) - self.offset(i)).max().unwrap_or(0)
    }
}

fn strip_eol(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// Splits into lines, each keeping its line ending.
fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    bytes.split_inclusive(|b| *b == b'\n').collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Segment {
    /// Lines that merge without help: unchanged, or changed on one side only.
    Stable(Chunk),
    /// Lines both sides changed differently.
    Conflict { ours: Chunk, theirs: Chunk },
}

/// A three-way merge of one text file, split where it conflicts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextMerge {
    pub segments: Vec<Segment>,
    /// Index into `segments` of every conflict, in file order.
    pub conflicts: Vec<usize>,
    /// The file uses CRLF line endings, so lines we add do too.
    pub crlf: bool,
}

impl TextMerge {
    /// Both sides of conflict number `index`.
    pub fn conflict(&self, index: usize) -> Option<(&Chunk, &Chunk)> {
        match self.segments.get(*self.conflicts.get(index)?)? {
            Segment::Conflict { ours, theirs } => Some((ours, theirs)),
            Segment::Stable(_) => None,
        }
    }
}

/// What to put in place of one conflict.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Resolution {
    #[default]
    Unresolved,
    Ours,
    Theirs,
    /// Both, ours first.
    OursTheirs,
    /// Both, theirs first.
    TheirsOurs,
    /// Drop the lines of both sides.
    Neither,
    /// Text typed by hand: lines joined by `\n`, empty for no lines at all.
    Custom(String),
}

/// Every decision made for one file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Draft {
    /// One entry per conflict, in file order.
    pub choices: Vec<Resolution>,
    /// Lines outside conflicts that were rewritten by hand, keyed by
    /// (segment, line). The text replaces that one line and may itself be
    /// several lines, or empty to delete the line.
    pub edits: BTreeMap<(usize, usize), String>,
}

impl Draft {
    pub fn unresolved(merge: &TextMerge) -> Self {
        Draft { choices: vec![Resolution::Unresolved; merge.conflicts.len()], edits: BTreeMap::new() }
    }

    pub fn open_conflicts(&self) -> usize {
        self.choices.iter().filter(|c| **c == Resolution::Unresolved).count()
    }
}

/// The smallest marker width that no line of the inputs can be mistaken for.
fn marker_size(inputs: [&[u8]; 3]) -> Result<u16, String> {
    let mut longest = 0;
    for input in inputs {
        for line in split_lines(input) {
            let Some(first) = line.first().filter(|b| b"<=>|".contains(b)) else { continue };
            longest = longest.max(line.iter().take_while(|b| *b == first).count());
        }
    }
    u16::try_from(longest.max(MARKER_SIZE - 1) + 1)
        .map_err(|_| "This file has lines that look like enormous conflict markers.".to_owned())
}

/// Merges three versions of a text file the way git does, keeping the
/// conflicting regions apart instead of writing markers.
///
/// The merge itself is libgit2's, which shares its line-merge code with git.
/// It is asked for markers wider than any marker-like line in the inputs, so
/// its output can be taken apart again without ambiguity.
pub fn merge_text(base: &[u8], ours: &[u8], theirs: &[u8]) -> Result<TextMerge, String> {
    let size = marker_size([base, ours, theirs])?;
    // `merge_file` is one of the few git2 entry points that do not start
    // libgit2 themselves; creating a buffer does, and costs nothing.
    let _started = git2::Buf::new();
    let mut options = git2::MergeFileOptions::new();
    options.marker_size(size).our_label("o").their_label("t").style_standard(true);
    let (mut a, mut o, mut t) =
        (git2::MergeFileInput::new(), git2::MergeFileInput::new(), git2::MergeFileInput::new());
    a.content(base);
    o.content(ours);
    t.content(theirs);
    let result = git2::merge_file(&a, &o, &t, Some(&mut options)).map_err(err)?;

    let size = size as usize;
    let is_marker = |line: &[u8], mark: u8| line.len() >= size && line[..size].iter().all(|b| *b == mark);
    enum In {
        Stable,
        Ours,
        Theirs,
    }
    let mut state = In::Stable;
    let mut segments = Vec::new();
    let (mut stable, mut mine, mut yours) = (Vec::new(), Vec::new(), Vec::new());
    let mut ends_in_conflict = false;
    for line in split_lines(result.content()) {
        ends_in_conflict = false;
        match state {
            In::Stable if is_marker(line, b'<') => {
                if !stable.is_empty() {
                    segments.push(Segment::Stable(Chunk::new(std::mem::take(&mut stable))));
                }
                state = In::Ours;
            }
            In::Stable => stable.extend_from_slice(line),
            In::Ours if is_marker(line, b'=') => state = In::Theirs,
            In::Ours => mine.extend_from_slice(line),
            In::Theirs if is_marker(line, b'>') => {
                segments.push(Segment::Conflict {
                    ours: Chunk::new(std::mem::take(&mut mine)),
                    theirs: Chunk::new(std::mem::take(&mut yours)),
                });
                state = In::Stable;
                ends_in_conflict = true;
            }
            In::Theirs => yours.extend_from_slice(line),
        }
    }
    if !matches!(state, In::Stable) {
        return Err("The merge produced output that could not be understood.".to_owned());
    }
    if !stable.is_empty() {
        segments.push(Segment::Stable(Chunk::new(stable)));
    }

    // A conflict that runs to the end of the file gets a newline added before
    // its closing marker when a side has none. Take those lines from the
    // side itself so a missing final newline stays missing.
    if ends_in_conflict && let Some(Segment::Conflict { ours: left, theirs: right }) = segments.last_mut() {
        for (chunk, side) in [(left, ours), (right, theirs)] {
            let lines = split_lines(side);
            if chunk.is_empty() || chunk.len() > lines.len() {
                continue;
            }
            let tail = lines[lines.len() - chunk.len()..].concat();
            if chunk.bytes().starts_with(&tail) {
                *chunk = Chunk::new(tail);
            }
        }
    }

    let conflicts = segments
        .iter()
        .enumerate()
        .filter(|(_, s)| matches!(s, Segment::Conflict { .. }))
        .map(|(i, _)| i)
        .collect();
    let sample = if ours.is_empty() { theirs } else { ours };
    let newlines = sample.iter().filter(|b| **b == b'\n').count();
    let crlf = sample.windows(2).filter(|w| w == b"\r\n").count() * 2 > newlines;
    Ok(TextMerge { segments, conflicts, crlf })
}

/// Appends `bytes`, first finishing a line that was left without a newline.
fn push(out: &mut Vec<u8>, bytes: &[u8], eol: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    if !out.is_empty() && !out.ends_with(b"\n") {
        out.extend_from_slice(eol);
    }
    out.extend_from_slice(bytes);
}

/// Appends hand-typed text using the file's line ending. `terminate` says
/// whether the last line gets a newline (false keeps a file's missing final
/// newline missing).
fn push_text(out: &mut Vec<u8>, text: &str, eol: &[u8], terminate: bool) {
    if text.is_empty() {
        return;
    }
    let mut bytes = Vec::with_capacity(text.len() + 8);
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            bytes.extend_from_slice(eol);
        }
        bytes.extend_from_slice(line.strip_suffix('\r').unwrap_or(line).as_bytes());
    }
    if terminate {
        bytes.extend_from_slice(eol);
    }
    push(out, &bytes, eol);
}

/// The file that results from `draft`.
///
/// Conflicts that are still unresolved are written with conflict markers,
/// labelled `labels.0` (ours) and `labels.1` (theirs), as git writes them.
pub fn render(merge: &TextMerge, draft: &Draft, labels: (&str, &str)) -> Vec<u8> {
    let eol: &[u8] = if merge.crlf { b"\r\n" } else { b"\n" };
    let mut out = Vec::new();
    let mut conflict = 0;
    for (index, segment) in merge.segments.iter().enumerate() {
        match segment {
            Segment::Stable(chunk) => {
                let mut from = 0;
                for (&(_, line), text) in draft.edits.range((index, 0)..(index + 1, 0)) {
                    if line < from || line >= chunk.len() {
                        continue;
                    }
                    push(&mut out, chunk.raw(from, line), eol);
                    push_text(&mut out, text, eol, chunk.has_newline(line));
                    from = line + 1;
                }
                push(&mut out, chunk.raw(from, chunk.len()), eol);
            }
            Segment::Conflict { ours, theirs } => {
                match draft.choices.get(conflict).unwrap_or(&Resolution::Unresolved) {
                    Resolution::Unresolved => {
                        let marker = |mark: u8, label: &str| {
                            let mut line = vec![mark; MARKER_SIZE];
                            if !label.is_empty() {
                                line.push(b' ');
                                line.extend_from_slice(label.as_bytes());
                            }
                            line.extend_from_slice(eol);
                            line
                        };
                        push(&mut out, &marker(b'<', labels.0), eol);
                        push(&mut out, ours.bytes(), eol);
                        push(&mut out, &marker(b'=', ""), eol);
                        push(&mut out, theirs.bytes(), eol);
                        push(&mut out, &marker(b'>', labels.1), eol);
                    }
                    Resolution::Ours => push(&mut out, ours.bytes(), eol),
                    Resolution::Theirs => push(&mut out, theirs.bytes(), eol),
                    Resolution::OursTheirs => {
                        push(&mut out, ours.bytes(), eol);
                        push(&mut out, theirs.bytes(), eol);
                    }
                    Resolution::TheirsOurs => {
                        push(&mut out, theirs.bytes(), eol);
                        push(&mut out, ours.bytes(), eol);
                    }
                    Resolution::Neither => {}
                    Resolution::Custom(text) => {
                        // End the way the lines being replaced ended.
                        let model = if ours.is_empty() { theirs } else { ours };
                        push_text(&mut out, text, eol, model.is_empty() || model.ends_with_newline());
                    }
                }
                conflict += 1;
            }
        }
    }
    out
}

fn has_crlf(bytes: &[u8]) -> bool {
    bytes.windows(2).any(|pair| pair == b"\r\n")
}

/// Where `needle` (whole lines) next occurs in `file` at the start of a
/// line, at or after `from`. Gives up on files so repetitive that the search
/// would take noticeable time; the caller then simply starts afresh.
fn find_lines(file: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    let budget = 64 * (file.len() + needle.len()) + (1 << 20);
    let mut work = 0;
    let mut at = from;
    loop {
        let rest = &file[at..];
        let common = rest.iter().zip(needle).take_while(|(a, b)| a == b).count();
        if common == needle.len() {
            return Some(at);
        }
        work += common + 1;
        if work > budget {
            return None;
        }
        at += rest.iter().position(|b| *b == b'\n')? + 1;
    }
}

/// Reads back the choices already present in the working file.
///
/// The stable segments are located in `worktree`, in order; whatever sits
/// between two of them is what that conflict currently looks like on disk.
/// This is how a file saved earlier without being marked resolved, or one
/// partly fixed in an editor, opens with that work intact. `None` when the
/// file was changed outside the conflicts too, so it cannot be mapped.
pub fn adopt(merge: &TextMerge, worktree: &[u8]) -> Option<Draft> {
    let stored_crlf = merge.segments.iter().any(|segment| match segment {
        Segment::Stable(chunk) => has_crlf(chunk.bytes()),
        Segment::Conflict { ours, theirs } => has_crlf(ours.bytes()) || has_crlf(theirs.bytes()),
    });
    // With core.autocrlf the working file has CRLF where the index has LF.
    let normalised: Cow<'_, [u8]> = if !stored_crlf && has_crlf(worktree) {
        let mut bytes = Vec::with_capacity(worktree.len());
        for (i, b) in worktree.iter().enumerate() {
            if !(*b == b'\r' && worktree.get(i + 1) == Some(&b'\n')) {
                bytes.push(*b);
            }
        }
        Cow::Owned(bytes)
    } else {
        Cow::Borrowed(worktree)
    };
    let file: &[u8] = &normalised;

    let eol: &[u8] = if merge.crlf { b"\r\n" } else { b"\n" };
    let mut choices = Vec::with_capacity(merge.conflicts.len());
    let mut at = 0;
    let mut pending: Option<(&Chunk, &Chunk)> = None;
    let last = merge.segments.len().saturating_sub(1);
    for (index, segment) in merge.segments.iter().enumerate() {
        match segment {
            Segment::Conflict { ours, theirs } => pending = Some((ours, theirs)),
            Segment::Stable(chunk) => {
                let stable = chunk.bytes();
                let start = match pending {
                    None => file[at..].starts_with(stable).then_some(at)?,
                    // The last segment has to end the file.
                    Some(_) if index == last => {
                        let start = file.len().checked_sub(stable.len()).filter(|start| *start >= at)?;
                        file.ends_with(stable).then_some(start)?
                    }
                    Some(_) => find_lines(file, at, stable)?,
                };
                if let Some((ours, theirs)) = pending.take() {
                    choices.push(classify(&file[at..start], ours, theirs, eol)?);
                }
                at = start + stable.len();
            }
        }
    }
    match pending {
        Some((ours, theirs)) => choices.push(classify(&file[at..], ours, theirs, eol)?),
        None if at != file.len() => return None,
        None => {}
    }
    Some(Draft { choices, edits: BTreeMap::new() })
}

/// Which resolution the bytes found in place of a conflict amount to.
fn classify(found: &[u8], ours: &Chunk, theirs: &Chunk, eol: &[u8]) -> Option<Resolution> {
    let both = |first: &Chunk, second: &Chunk| {
        let mut bytes = Vec::new();
        push(&mut bytes, first.bytes(), eol);
        push(&mut bytes, second.bytes(), eol);
        bytes
    };
    Some(if found.starts_with(&[b'<'; MARKER_SIZE]) {
        Resolution::Unresolved
    } else if found == ours.bytes() {
        Resolution::Ours
    } else if found == theirs.bytes() {
        Resolution::Theirs
    } else if found == both(ours, theirs) {
        Resolution::OursTheirs
    } else if found == both(theirs, ours) {
        Resolution::TheirsOurs
    } else if found.is_empty() {
        Resolution::Neither
    } else {
        // Text we could not show faithfully is better left alone.
        let text = std::str::from_utf8(found).ok()?;
        let text = text.strip_suffix('\n').unwrap_or(text);
        let text = text.strip_suffix('\r').unwrap_or(text);
        Resolution::Custom(text.replace("\r\n", "\n"))
    })
}

/// One side of the conflict, described for the person resolving it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SideInfo {
    /// "Current" or "Incoming".
    pub role: &'static str,
    /// What the side is: a branch, or the commit being applied.
    pub name: String,
    /// One more line about it, usually the commit behind it.
    pub detail: String,
    /// Label for conflict markers written to the file.
    pub marker: String,
}

/// One of the three index entries of a conflicted path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stage {
    pub id: Oid,
    pub mode: u32,
    pub size: usize,
    pub binary: bool,
}

/// Why a conflict is settled by choosing a whole file, not line by line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Whole {
    Binary,
    /// We deleted the file, they changed it.
    DeletedByUs,
    /// They deleted the file, we changed it.
    DeletedByThem,
    /// Only the ancestor is left, typically the old name of a file both
    /// sides renamed.
    BothDeleted,
    /// Only our side has this path, typically our new name in a rename conflict.
    AddedByUs,
    /// Only their side has this path.
    AddedByThem,
    TooLarge,
    /// A symbolic link or submodule is involved.
    Special,
}

pub enum Content {
    Text {
        merge: TextMerge,
        /// Where the resolver starts: what the working file already says,
        /// or everything unresolved.
        draft: Draft,
        /// The working file has no conflict markers left and does not match
        /// the merge: somebody resolved it in an editor.
        edited_outside: bool,
    },
    Whole(Whole),
}

/// Everything the resolver needs to know about one conflicted path.
pub struct ConflictFile {
    pub path: String,
    pub base: Option<Stage>,
    pub ours: Option<Stage>,
    pub theirs: Option<Stage>,
    pub ours_info: SideInfo,
    pub theirs_info: SideInfo,
    pub content: Content,
    /// The repository's git directory, home of the scratch index that
    /// [`save`] uses to write a file without staging it.
    pub git_dir: PathBuf,
    /// How the file's text is stored, to write it back that way.
    pub encoding: FileEncoding,
    /// The sides were turned from `encoding` into UTF-8 to be merged and
    /// shown; the result is turned back. Files in UTF-8 are worked on as
    /// they are, byte for byte.
    pub transcoded: bool,
    /// The repository's encoding when the file was read (Preferences >
    /// Encoding), to notice it changing.
    pub default_encoding: Encoding,
}

impl ConflictFile {
    pub fn stage_ids(&self) -> [Option<Oid>; 3] {
        [&self.base, &self.ours, &self.theirs].map(|stage| stage.as_ref().map(|s| s.id))
    }

    pub fn info(&self, side: Side) -> &SideInfo {
        match side {
            Side::Ours => &self.ours_info,
            Side::Theirs => &self.theirs_info,
        }
    }

    pub fn stage(&self, side: Side) -> Option<&Stage> {
        match side {
            Side::Ours => self.ours.as_ref(),
            Side::Theirs => self.theirs.as_ref(),
        }
    }

    /// The resolved file for `draft`, in UTF-8 when it was transcoded (see
    /// [`ConflictFile::output`]); `None` when this is not a text conflict.
    pub fn render(&self, draft: &Draft) -> Option<Vec<u8>> {
        match &self.content {
            Content::Text { merge, .. } => {
                Some(render(merge, draft, (&self.ours_info.marker, &self.theirs_info.marker)))
            }
            Content::Whole(_) => None,
        }
    }

    /// The bytes to write for `draft`: line endings made `eol`, and the text
    /// back in the file's encoding.
    pub fn output(&self, draft: &Draft, eol: Eol) -> Option<Vec<u8>> {
        let rendered = self.render(draft)?;
        let bytes = text::convert_eol(&rendered, eol);
        if self.transcoded {
            return Some(self.encoding.encode(&String::from_utf8_lossy(&bytes)));
        }
        let bom = Encoding::Utf8Bom.bom();
        if self.encoding.encoding == Encoding::Utf8Bom && !bytes.is_empty() && !bytes.starts_with(bom) {
            return Some([bom, &bytes].concat());
        }
        Some(bytes.into_owned())
    }
}

const MODE_TYPE: u32 = 0o170000;
const MODE_FILE: u32 = 0o100000;

fn is_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(BINARY_SNIFF)].contains(&0)
}

/// The index entries of `path` at stages 1 to 3 (base, ours, theirs). All
/// `None` once the path is no longer in conflict.
pub fn stage_ids(repo: &Repository, path: &str) -> Result<[Option<Oid>; 3], String> {
    let mut index = repo.index().map_err(err)?;
    index.read(false).map_err(err)?;
    Ok([1, 2, 3].map(|stage| index.get_path(Path::new(path), stage).map(|entry| entry.id)))
}

/// Reads the conflict at `path` from the index, its text in `default` (the
/// repository's encoding) unless a byte order mark says otherwise. Runs on
/// the reader thread.
pub fn load(repo: &Repository, path: &str, default: Encoding) -> Result<ConflictFile, String> {
    let mut index = repo.index().map_err(err)?;
    index.read(false).map_err(err)?;
    let entries = [1, 2, 3].map(|stage| index.get_path(Path::new(path), stage));
    if entries.iter().all(Option::is_none) {
        return Err(format!("{path} is no longer in conflict."));
    }

    let special = entries.iter().flatten().any(|entry| entry.mode & MODE_TYPE != MODE_FILE);
    let mut stages: [Option<Stage>; 3] = [None, None, None];
    let mut blobs: [Vec<u8>; 3] = Default::default();
    for (i, entry) in entries.iter().enumerate() {
        let Some(entry) = entry else { continue };
        let mut stage = Stage { id: entry.id, mode: entry.mode, size: 0, binary: false };
        // Submodule entries name a commit in another repository, not a blob.
        if let Ok(blob) = repo.find_blob(entry.id) {
            stage.size = blob.size();
            stage.binary = is_binary(blob.content());
            if !special && stage.size <= MAX_TEXT_BYTES {
                blobs[i] = blob.content().to_vec();
            }
        } else if entry.mode & MODE_TYPE == MODE_FILE {
            return Err(format!("Could not read the versions of {path} from the repository."));
        }
        stages[i] = Some(stage);
    }

    // The file's encoding: a byte order mark on a side, else the
    // repository's setting. Other than UTF-8, the sides are merged and
    // shown as UTF-8 and the result is turned back when saved. UTF-16 is
    // binary to git (it is full of zero bytes); as text it merges fine.
    let sample = [&blobs[1], &blobs[2], &blobs[0]].into_iter().find(|blob| !blob.is_empty());
    let encoding = text::detect(sample.map_or(&[][..], |blob| blob.as_slice()), default);
    let transcoded = !matches!(encoding.encoding, Encoding::Utf8 | Encoding::Utf8Bom);
    if transcoded {
        for (blob, stage) in blobs.iter_mut().zip(&mut stages) {
            if let Some(stage) = stage
                && !blob.is_empty()
            {
                *blob = text::decode_file(blob, encoding.encoding).0.into_bytes();
                stage.binary = is_binary(blob);
            }
        }
    }
    let [base, ours, theirs] = stages;

    let whole = if special {
        Some(Whole::Special)
    } else {
        match (&ours, &theirs) {
            (None, None) => Some(Whole::BothDeleted),
            (None, Some(_)) if base.is_some() => Some(Whole::DeletedByUs),
            (None, Some(_)) => Some(Whole::AddedByThem),
            (Some(_), None) if base.is_some() => Some(Whole::DeletedByThem),
            (Some(_), None) => Some(Whole::AddedByUs),
            (Some(a), Some(b)) => {
                if a.binary || b.binary || base.as_ref().is_some_and(|s| s.binary) {
                    Some(Whole::Binary)
                } else if [Some(a), Some(b), base.as_ref()].iter().flatten().any(|s| s.size > MAX_TEXT_BYTES) {
                    Some(Whole::TooLarge)
                } else {
                    None
                }
            }
        }
    };

    let content = match whole {
        Some(whole) => Content::Whole(whole),
        None => {
            let merge = merge_text(&blobs[0], &blobs[1], &blobs[2])?;
            let mut worktree = repo.workdir().and_then(|dir| std::fs::read(dir.join(path)).ok());
            if transcoded {
                worktree = worktree.map(|bytes| text::decode_file(&bytes, encoding.encoding).0.into_bytes());
            }
            // Git leaves its own side of a file it took for binary, without
            // markers: that is not a resolution to start from.
            let adopted = worktree
                .as_deref()
                .filter(|_| !encoding.encoding.is_utf16())
                .and_then(|bytes| adopt(&merge, bytes));
            let has_markers = worktree.as_deref().is_some_and(|bytes| {
                split_lines(bytes).iter().any(|line| line.starts_with(&[b'<'; MARKER_SIZE]))
            });
            let edited_outside =
                adopted.is_none() && worktree.is_some() && !has_markers && !encoding.encoding.is_utf16();
            let draft = adopted.unwrap_or_else(|| Draft::unresolved(&merge));
            Content::Text { merge, draft, edited_outside }
        }
    };

    let (ours_info, theirs_info) = describe(repo);
    Ok(ConflictFile {
        path: path.to_owned(),
        base,
        ours,
        theirs,
        ours_info,
        theirs_info,
        content,
        git_dir: repo.path().to_path_buf(),
        encoding,
        transcoded,
        default_encoding: default,
    })
}

/// `1a2b3c4 Summary line` for a commit.
fn commit_line(repo: &Repository, oid: Oid) -> String {
    match repo.find_commit(oid) {
        Ok(commit) => {
            format!("{} {}", short(oid), commit.summary_bytes().map(lossy).unwrap_or_default())
        }
        Err(_) => short(oid),
    }
}

/// `1a2b3c4 (Summary line)`, the form git uses in conflict markers.
fn marker_for(repo: &Repository, oid: Oid) -> String {
    match repo.find_commit(oid) {
        Ok(commit) => {
            format!("{} ({})", short(oid), commit.summary_bytes().map(lossy).unwrap_or_default())
        }
        Err(_) => short(oid),
    }
}

/// A branch that points exactly at `oid`, local ones first.
fn branch_at(repo: &Repository, oid: Oid) -> Option<String> {
    let mut remote = None;
    for (branch, kind) in repo.branches(None).ok()?.flatten() {
        if branch.get().target() != Some(oid) {
            continue;
        }
        let Ok(name) = branch.name_bytes().map(lossy) else { continue };
        match kind {
            BranchType::Local => return Some(name),
            BranchType::Remote => remote = remote.or(Some(name)),
        }
    }
    remote
}

/// The name in `Merge branch 'name'` and its relatives.
fn merged_name(message: &str) -> Option<String> {
    let line = message.lines().next()?;
    let rest = line.strip_prefix("Merge ")?;
    let (kind, rest) = rest.split_once(" '")?;
    let (name, _) = rest.split_once('\'')?;
    match kind {
        "branch" | "remote-tracking branch" | "tag" | "branches" | "tags" => Some(name.to_owned()),
        _ => None,
    }
}

/// Says what stage 2 and stage 3 stand for in the operation under way.
///
/// In a merge they are the current branch and the branch being merged. In a
/// rebase the roles are swapped relative to intuition: "ours" is the branch
/// being rebased onto (plus what was replayed so far), "theirs" is the
/// commit of your own branch that is being replayed.
fn describe(repo: &Repository) -> (SideInfo, SideInfo) {
    let head = read_head(repo);
    let mut ours = SideInfo {
        role: "Current",
        name: match (&head.branch, head.oid) {
            (Some(branch), _) => branch.clone(),
            (None, Some(oid)) => short(oid),
            _ => "HEAD".to_owned(),
        },
        detail: head.oid.map(|oid| commit_line(repo, oid)).unwrap_or_default(),
        marker: "HEAD".to_owned(),
    };
    let mut theirs = SideInfo {
        role: "Incoming",
        name: "applied changes".to_owned(),
        detail: String::new(),
        marker: "incoming".to_owned(),
    };

    let git_dir = repo.path();
    let read = |name: &str| {
        std::fs::read_to_string(git_dir.join(name)).ok().map(|text| text.trim().to_owned())
    };
    let read_oid = |name: &str| {
        read(name).and_then(|text| Oid::from_str(text.lines().next().unwrap_or_default().trim()).ok())
    };
    // MERGE_HEAD and friends: files in the git directory holding a commit id.
    let pseudo_ref =
        |name: &str| repo.find_reference(name).ok().and_then(|r| r.target()).or_else(|| read_oid(name));

    match repo.state() {
        RepositoryState::Merge => {
            if let Some(oid) = pseudo_ref("MERGE_HEAD") {
                let name = read("MERGE_MSG")
                    .and_then(|message| merged_name(&message))
                    .or_else(|| branch_at(repo, oid))
                    .unwrap_or_else(|| short(oid));
                theirs.marker = name.clone();
                theirs.name = name;
                theirs.detail = commit_line(repo, oid);
            }
        }
        RepositoryState::Rebase | RepositoryState::RebaseInteractive | RepositoryState::RebaseMerge => {
            let dir = if git_dir.join("rebase-merge").is_dir() { "rebase-merge" } else { "rebase-apply" };
            let branch = read(&format!("{dir}/head-name"))
                .map(|name| name.strip_prefix("refs/heads/").unwrap_or(&name).to_owned())
                .filter(|name| !name.is_empty() && name != "detached HEAD");
            let onto = read_oid(&format!("{dir}/onto"));
            let replayed = pseudo_ref("REBASE_HEAD")
                .or_else(|| read_oid(&format!("{dir}/stopped-sha")))
                .or_else(|| read_oid(&format!("{dir}/original-commit")));

            if let Some(onto) = onto {
                ours.name = branch_at(repo, onto).unwrap_or_else(|| short(onto));
            }
            ours.detail = match &branch {
                Some(branch) => format!("the base {branch} is being rebased onto"),
                None => "the base being rebased onto".to_owned(),
            };
            if let Some(oid) = replayed {
                theirs.name = branch.unwrap_or_else(|| format!("commit {}", short(oid)));
                theirs.detail = format!("replaying {}", commit_line(repo, oid));
                theirs.marker = marker_for(repo, oid);
            }
        }
        RepositoryState::CherryPick | RepositoryState::CherryPickSequence => {
            if let Some(oid) = pseudo_ref("CHERRY_PICK_HEAD") {
                theirs.name = format!("commit {}", short(oid));
                theirs.detail = format!("cherry-picking {}", commit_line(repo, oid));
                theirs.marker = marker_for(repo, oid);
            }
        }
        RepositoryState::Revert | RepositoryState::RevertSequence => {
            if let Some(oid) = pseudo_ref("REVERT_HEAD") {
                theirs.name = format!("revert of {}", short(oid));
                theirs.detail = format!("undoing {}", commit_line(repo, oid));
                theirs.marker = format!("parent of {}", marker_for(repo, oid));
            }
        }
        _ => {}
    }
    (ours, theirs)
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|a| (*a).to_owned()).collect()
}

/// Writes `bytes` as the content of the conflicted file.
///
/// `bytes` are in the form git stores (what [`render`] produces from the
/// index stages). They become a blob first; git then checks that blob out,
/// so line-ending conversion and other filters apply exactly as on any
/// checkout and a CRLF working file stays CRLF.
///
/// With `mark_resolved` the blob also replaces the three conflict stages in
/// the index, which is what `git add` does to a resolved file. Without it
/// the checkout goes through a scratch index and the real one keeps its
/// conflict.
pub fn save(file: &ConflictFile, bytes: Vec<u8>, mark_resolved: bool) -> Result<GitCmd, String> {
    let oid = Oid::hash_object(ObjectType::Blob, &bytes).map_err(err)?.to_string();
    let mode = [&file.ours, &file.theirs, &file.base]
        .into_iter()
        .flatten()
        .map(|stage| stage.mode)
        .find(|mode| mode & MODE_TYPE == MODE_FILE)
        .unwrap_or(0o100644);
    let mode = format!("{mode:o}");
    let path = file.path.as_str();
    let entry = ["update-index", "--add", "--cacheinfo", mode.as_str(), oid.as_str(), path];

    // Quiet: the only thing these steps print is the blob id, which makes a
    // poor toast. The resolver reports the outcome itself.
    let label = if mark_resolved { format!("Resolve {path}") } else { format!("Save {path}") };
    let mut cmd = GitCmd::empty(label).quiet();
    cmd.steps.push(Step::new(args(&["hash-object", "-w", "--stdin"]), Some(bytes)));
    if mark_resolved {
        cmd.steps.push(Step::new(args(&entry), None));
        cmd.steps.push(Step::new(args(&["checkout-index", "-f", "-u", "--", path]), None));
    } else {
        let scratch = file.git_dir.join("gitkrill-resolve.index").to_string_lossy().into_owned();
        for step in [&["read-tree", "--empty"][..], &entry, &["checkout-index", "-f", "--", path]] {
            cmd.steps.push(Step::new(args(step), None).env("GIT_INDEX_FILE", scratch.clone()));
        }
    }
    Ok(cmd)
}

/// Resolves a conflict by taking one side's whole file.
pub fn keep(path: &str, side: Side) -> GitCmd {
    let flag = match side {
        Side::Ours => "--ours",
        Side::Theirs => "--theirs",
    };
    let mut cmd = GitCmd::empty(format!("Resolve {path}")).quiet();
    cmd.steps.push(Step::with_paths(args(&["checkout", flag, "--", path]), None));
    cmd.steps.push(Step::with_paths(args(&["add", "--", path]), None));
    cmd
}

/// Resolves a conflict by deleting the file.
pub fn delete(path: &str) -> GitCmd {
    let mut cmd = GitCmd::empty(format!("Resolve {path}")).quiet();
    cmd.steps.push(Step::with_paths(args(&["rm", "-q", "--", path]), None));
    cmd
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use super::*;
    use crate::git::{ChangeKind, RepoState, ops};
    use crate::tests::{Fixture, git};
    use crate::worker::Origin;

    const FILE: &str = "f.txt";

    fn put(fx: &Fixture, path: &str, content: Option<&[u8]>) {
        let full = fx.dir.join(path);
        match content {
            Some(bytes) => std::fs::write(full, bytes).unwrap(),
            None => {
                let _ = std::fs::remove_file(full);
            }
        }
    }

    /// Commits the three versions of `FILE` (`None` = the file is absent):
    /// `base` on main, `theirs` on `feature`, `ours` on main again. Leaves
    /// main checked out.
    fn diverge(fx: &Fixture, base: Option<&[u8]>, ours: Option<&[u8]>, theirs: Option<&[u8]>) {
        // A second file per commit, so no commit is ever empty.
        fx.write("base.txt", "base\n");
        put(fx, FILE, base);
        fx.commit_all("base");
        git(&fx.dir, &["checkout", "-q", "-b", "feature"]);
        fx.write("theirs.txt", "theirs\n");
        put(fx, FILE, theirs);
        fx.commit_all("their change");
        git(&fx.dir, &["checkout", "-q", "main"]);
        fx.write("ours.txt", "ours\n");
        put(fx, FILE, ours);
        fx.commit_all("our change");
    }

    /// A repository stopped in `git merge feature` with `FILE` in conflict.
    fn merging(name: &str, base: Option<&[u8]>, ours: Option<&[u8]>, theirs: Option<&[u8]>) -> Fixture {
        let fx = Fixture::new(name);
        diverge(&fx, base, ours, theirs);
        stop_on_conflict(&fx, ops::merge("feature"), RepoState::Merge);
        fx
    }

    fn stop_on_conflict(fx: &Fixture, cmd: GitCmd, state: RepoState) {
        let done = fx.run_as(cmd, Origin::User);
        assert!(done.result.is_err(), "the operation should stop on a conflict");
        assert_eq!(done.snap.state, state);
        assert!(done.snap.status.unstaged.iter().any(|f| f.kind == ChangeKind::Conflicted));
    }

    fn load_file(fx: &Fixture, path: &'static str) -> ConflictFile {
        load_in(fx, path, Encoding::Utf8)
    }

    fn load_in(fx: &Fixture, path: &'static str, encoding: Encoding) -> ConflictFile {
        fx.job(move |loader| load(loader.repo(), path, encoding)).expect("conflict loads")
    }

    fn text(file: &ConflictFile) -> (&TextMerge, &Draft) {
        match &file.content {
            Content::Text { merge, draft, .. } => (merge, draft),
            Content::Whole(why) => panic!("expected a text conflict, got {why:?}"),
        }
    }

    fn whole(file: &ConflictFile) -> Whole {
        match &file.content {
            Content::Whole(why) => *why,
            Content::Text { .. } => panic!("expected a whole-file conflict"),
        }
    }

    fn draft(choices: &[Resolution]) -> Draft {
        Draft { choices: choices.to_vec(), edits: BTreeMap::new() }
    }

    fn git_bytes(dir: &Path, args: &[&str]) -> Vec<u8> {
        let out = Command::new("git").current_dir(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        out.stdout
    }

    fn on_disk(fx: &Fixture, path: &str) -> Vec<u8> {
        std::fs::read(fx.dir.join(path)).unwrap()
    }

    fn staged(fx: &Fixture, path: &str) -> Vec<u8> {
        git_bytes(&fx.dir, &["show", &format!(":{path}")])
    }

    fn unmerged(fx: &Fixture) -> String {
        git(&fx.dir, &["ls-files", "-u"])
    }

    fn show(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    /// Saves `choices` and marks the file resolved; checks the working file
    /// and the index both hold exactly `expected`.
    fn resolve(fx: &Fixture, file: &ConflictFile, choices: &Draft, expected: &[u8]) {
        let bytes = file.render(choices).unwrap();
        assert_eq!(show(&bytes), show(expected));
        let done = fx.run(save(file, bytes, true).unwrap());
        assert_eq!(show(&on_disk(fx, FILE)), show(expected), "working file");
        assert_eq!(show(&staged(fx, FILE)), show(expected), "index entry");
        assert_eq!(unmerged(fx), "", "no conflict stages are left");
        assert!(done.snap.status.unstaged.iter().all(|f| f.path != FILE), "nothing left unstaged");
    }

    fn numbered(change: impl Fn(usize) -> Option<String>) -> Vec<u8> {
        (1..=30)
            .map(|i| change(i).unwrap_or_else(|| format!("line {i}")) + "\n")
            .collect::<String>()
            .into_bytes()
    }

    fn three_regions(name: &str) -> (Fixture, ConflictFile) {
        let base = numbered(|_| None);
        let ours = numbered(|i| match i {
            3 | 15 | 27 => Some(format!("ours {i}")),
            21 => Some("only ours changed 21".to_owned()),
            _ => None,
        });
        let theirs = numbered(|i| match i {
            3 | 15 | 27 => Some(format!("theirs {i}")),
            9 => Some("only theirs changed 9".to_owned()),
            _ => None,
        });
        let fx = merging(name, Some(&base), Some(&ours), Some(&theirs));
        let file = load_file(&fx, FILE);
        (fx, file)
    }

    /// The merged file with `at(3)`, `at(15)` and `at(27)` in place of the
    /// three conflicting lines.
    fn merged(at: impl Fn(usize) -> &'static str) -> Vec<u8> {
        (1..=30)
            .map(|i| match i {
                3 | 15 | 27 => at(i).to_owned(),
                9 => "only theirs changed 9\n".to_owned(),
                21 => "only ours changed 21\n".to_owned(),
                _ => format!("line {i}\n"),
            })
            .collect::<String>()
            .into_bytes()
    }

    #[test]
    fn regions_come_from_the_index_and_the_rest_merges_cleanly() {
        let (fx, file) = three_regions("conflict-regions");
        let (merge, start) = text(&file);

        assert_eq!(merge.conflicts.len(), 3);
        assert_eq!(merge.segments.len(), 7);
        assert!(!merge.crlf);
        let (ours, theirs) = merge.conflict(1).unwrap();
        assert_eq!((ours.text().as_str(), theirs.text().as_str()), ("ours 15", "theirs 15"));
        // Changes only one side made are already merged into the stable text.
        let Segment::Stable(between) = &merge.segments[2] else { panic!("stable text") };
        assert!(between.text().contains("only theirs changed 9"));
        assert_eq!(*start, Draft::unresolved(merge));
        assert_eq!(start.open_conflicts(), 3);

        assert_eq!((file.ours_info.role, file.ours_info.name.as_str()), ("Current", "main"));
        assert_eq!((file.theirs_info.role, file.theirs_info.name.as_str()), ("Incoming", "feature"));
        assert!(file.theirs_info.detail.ends_with("their change"), "{}", file.theirs_info.detail);

        // The working file is ignored as a source: wreck it and load again.
        fx.write(FILE, "<<<<<<< nonsense\nedited beyond recognition\n");
        let again = load_file(&fx, FILE);
        assert_eq!(text(&again).0, merge);
    }

    #[test]
    fn each_region_takes_ours_theirs_or_both_and_the_merge_can_finish() {
        let (fx, file) = three_regions("conflict-choices");
        use Resolution::*;

        let render = |choices: &[Resolution]| show(&file.render(&draft(choices)).unwrap());
        assert_eq!(
            render(&[TheirsOurs, Neither, Custom("typed\nby hand".to_owned())]),
            show(&merged(|i| match i {
                3 => "theirs 3\nours 3\n",
                15 => "",
                _ => "typed\nby hand\n",
            }))
        );
        // Unresolved regions keep git-style markers.
        assert_eq!(
            render(&[Ours, Unresolved, Theirs]),
            show(&merged(|i| match i {
                3 => "ours 3\n",
                15 => "<<<<<<< HEAD\nours 15\n=======\ntheirs 15\n>>>>>>> feature\n",
                _ => "theirs 27\n",
            }))
        );

        let expected = merged(|i| match i {
            3 => "ours 3\n",
            15 => "theirs 15\n",
            _ => "ours 27\ntheirs 27\n",
        });
        resolve(&fx, &file, &draft(&[Ours, Theirs, OursTheirs]), &expected);

        // With the file resolved, Continue finishes the merge.
        let done = fx.run(ops::continue_op(RepoState::Merge).unwrap());
        assert_eq!(done.snap.state, RepoState::Clean);
        assert!(done.snap.status.is_clean());
        assert_eq!(show(&git_bytes(&fx.dir, &["show", "HEAD:f.txt"])), show(&expected));
        assert_eq!(git(&fx.dir, &["rev-list", "--count", "--merges", "HEAD"]), "1");
    }

    #[test]
    fn saving_without_resolving_keeps_the_conflict_and_reopens_with_the_choices() {
        let (fx, file) = three_regions("conflict-save-only");
        use Resolution::*;
        let choices = draft(&[Theirs, Unresolved, Custom("something else".to_owned())]);

        let bytes = file.render(&choices).unwrap();
        fx.run(save(&file, bytes.clone(), false).unwrap());
        assert_eq!(show(&on_disk(&fx, FILE)), show(&bytes));
        // All three stages are still there, and the scratch index is no
        // concern of the repository's.
        assert_eq!(unmerged(&fx).lines().count(), 3);
        assert_eq!(fx.job(|loader| stage_ids(loader.repo(), FILE)).unwrap(), file.stage_ids());
        assert_eq!(git(&fx.dir, &["status", "--porcelain", "--", FILE]), "UU f.txt");

        let reopened = load_file(&fx, FILE);
        let (merge, start) = text(&reopened);
        assert_eq!(merge, text(&file).0);
        assert_eq!(*start, choices);

        // Fixed entirely by hand in an editor, beyond the conflicts.
        fx.write(FILE, "rewritten from scratch\n");
        let Content::Text { draft: start, edited_outside, .. } = load_file(&fx, FILE).content else {
            panic!("text conflict")
        };
        assert!(edited_outside);
        assert_eq!(start.open_conflicts(), 3);
    }

    #[test]
    fn crlf_files_stay_crlf() {
        let fx = merging(
            "conflict-crlf",
            Some(b"one\r\ntwo\r\nthree\r\n"),
            Some(b"one\r\nours\r\nthree\r\n"),
            Some(b"one\r\ntheirs\r\nthree\r\n"),
        );
        let file = load_file(&fx, FILE);
        let (merge, start) = text(&file);
        assert!(merge.crlf);
        assert_eq!(merge.conflicts.len(), 1);
        // Line endings never leak into what is displayed.
        assert_eq!(merge.conflict(0).unwrap().0.line(0), "ours");
        // Git's own markers in the working file read as "unresolved".
        assert_eq!(*start, Draft::unresolved(merge));
        use Resolution::*;

        // Markers and typed lines get the file's line ending too.
        let unresolved = file.render(&draft(&[Unresolved])).unwrap();
        assert_eq!(
            show(&unresolved),
            "one\r\n<<<<<<< HEAD\r\nours\r\n=======\r\ntheirs\r\n>>>>>>> feature\r\nthree\r\n"
        );
        fx.run(save(&file, unresolved.clone(), false).unwrap());
        assert_eq!(on_disk(&fx, FILE), unresolved);
        assert_eq!(
            show(&file.render(&draft(&[Custom("typed\nlines".to_owned())])).unwrap()),
            "one\r\ntyped\r\nlines\r\nthree\r\n"
        );

        resolve(&fx, &file, &draft(&[OursTheirs]), b"one\r\nours\r\ntheirs\r\nthree\r\n");
    }

    #[test]
    fn autocrlf_checkouts_keep_crlf_on_disk_and_lf_in_the_index() {
        let fx = Fixture::new("conflict-autocrlf");
        git(&fx.dir, &["config", "core.autocrlf", "true"]);
        diverge(
            &fx,
            Some(b"one\r\ntwo\r\nthree\r\n"),
            Some(b"one\r\nours\r\nthree\r\n"),
            Some(b"one\r\ntheirs\r\nthree\r\n"),
        );
        stop_on_conflict(&fx, ops::merge("feature"), RepoState::Merge);
        assert!(show(&on_disk(&fx, FILE)).contains("=======\r\n"));

        let file = load_file(&fx, FILE);
        let (merge, start) = text(&file);
        // The index holds LF; the CRLF working file still maps onto it.
        assert!(!merge.crlf);
        assert_eq!(*start, Draft::unresolved(merge));
        use Resolution::*;

        let bytes = file.render(&draft(&[Theirs])).unwrap();
        assert_eq!(show(&bytes), "one\ntheirs\nthree\n");
        fx.run(save(&file, bytes.clone(), false).unwrap());
        assert_eq!(show(&on_disk(&fx, FILE)), "one\r\ntheirs\r\nthree\r\n");
        assert_eq!(unmerged(&fx).lines().count(), 3);
        // Reopening finds the choice in the CRLF file.
        assert_eq!(*text(&load_file(&fx, FILE)).1, draft(&[Theirs]));

        let done = fx.run(save(&file, bytes, true).unwrap());
        assert_eq!(show(&on_disk(&fx, FILE)), "one\r\ntheirs\r\nthree\r\n");
        assert_eq!(show(&staged(&fx, FILE)), "one\ntheirs\nthree\n");
        assert_eq!(unmerged(&fx), "");
        // Git agrees the working file matches what was staged.
        assert!(done.snap.status.unstaged.iter().all(|f| f.path != FILE));
        assert_eq!(git(&fx.dir, &["status", "--porcelain", "--", FILE]), "M  f.txt");
    }

    #[test]
    fn a_missing_final_newline_stays_missing() {
        use Resolution::*;
        let fx = merging("conflict-no-eol", Some(b"a\nx"), Some(b"a\nb"), Some(b"a\nc"));
        let file = load_file(&fx, FILE);
        let (merge, _) = text(&file);
        let (ours, theirs) = merge.conflict(0).unwrap();
        assert_eq!((ours.bytes(), theirs.bytes()), (&b"b"[..], &b"c"[..]));

        let render = |choice: Resolution| show(&file.render(&draft(&[choice])).unwrap());
        assert_eq!(render(Ours), "a\nb");
        assert_eq!(render(Theirs), "a\nc");
        assert_eq!(render(OursTheirs), "a\nb\nc");
        assert_eq!(render(TheirsOurs), "a\nc\nb");
        assert_eq!(render(Custom("typed".to_owned())), "a\ntyped");
        assert_eq!(render(Unresolved), "a\n<<<<<<< HEAD\nb\n=======\nc\n>>>>>>> feature\n");
        resolve(&fx, &file, &draft(&[TheirsOurs]), b"a\nc\nb");

        // Only one side lacks it: each side keeps its own ending.
        let fx = merging("conflict-no-eol-mixed", Some(b"a\nx\n"), Some(b"a\nb\n"), Some(b"a\nc"));
        let file = load_file(&fx, FILE);
        let render = |choice: Resolution| show(&file.render(&draft(&[choice])).unwrap());
        assert_eq!(render(Ours), "a\nb\n");
        assert_eq!(render(OursTheirs), "a\nb\nc");
        assert_eq!(render(TheirsOurs), "a\nc\nb\n");
        resolve(&fx, &file, &draft(&[Theirs]), b"a\nc");
    }

    #[test]
    fn bytes_that_are_not_utf8_pass_through_untouched() {
        use Resolution::*;
        let fx = merging(
            "conflict-latin1",
            Some(b"caf\xe9\nmiddle\nna\xefve\nend \xff\n"),
            Some(b"caf\xe9\nours \xe0\nna\xefve\nend \xff\n"),
            Some(b"caf\xe9\ntheirs \xe8\nna\xefve\nend \xff\n"),
        );
        let file = load_file(&fx, FILE);
        let (merge, _) = text(&file);
        assert_eq!(merge.conflicts.len(), 1);

        // Typing over one line outside the conflict changes that line only.
        let mut edited = draft(&[Theirs]);
        edited.edits.insert((2, 0), "naive".to_owned());
        assert_eq!(file.render(&edited).unwrap(), b"caf\xe9\ntheirs \xe8\nnaive\nend \xff\n");
        // ... and typing several lines, or none, replaces just that line.
        edited.edits.insert((2, 0), "one\ntwo".to_owned());
        edited.edits.insert((2, 1), String::new());
        assert_eq!(file.render(&edited).unwrap(), b"caf\xe9\ntheirs \xe8\none\ntwo\n");

        let expected = b"caf\xe9\nours \xe0\nna\xefve\nend \xff\n";
        let bytes = file.render(&draft(&[Ours])).unwrap();
        assert_eq!(bytes, expected);
        fx.run(save(&file, bytes, true).unwrap());
        assert_eq!(on_disk(&fx, FILE), expected);
        assert_eq!(staged(&fx, FILE), expected);
        assert_eq!(unmerged(&fx), "");
    }

    #[test]
    fn files_in_another_encoding_are_shown_decoded_and_written_back_in_it() {
        use Resolution::*;
        // Windows-1252: e9 is "é", 93/94 are curly quotes, 80 is the euro sign.
        let fx = merging(
            "conflict-cp1252",
            Some(b"caf\xe9\r\nmiddle\r\nend\r\n"),
            Some(b"caf\xe9\r\nours \x93x\x94\r\nend\r\n"),
            Some(b"caf\xe9\r\ntheirs \x80\r\nend\r\n"),
        );
        let file = load_in(&fx, FILE, Encoding::Windows1252);
        assert!(file.transcoded);
        assert_eq!(file.encoding, FileEncoding { encoding: Encoding::Windows1252, bom: false });
        let (merge, _) = text(&file);
        let Segment::Stable(first) = &merge.segments[0] else { panic!("a stable start") };
        assert_eq!(first.line(0), "café");
        let (ours, theirs) = merge.conflict(0).unwrap();
        assert_eq!((ours.line(0), theirs.line(0)), ("ours “x”".into(), "theirs €".into()));

        // Typed text goes back in the file's encoding, line endings too.
        let typed = draft(&[Custom("both € and “quotes”".to_owned())]);
        let bytes = file.output(&typed, Eol::Keep).unwrap();
        assert_eq!(show(&bytes), show(b"caf\xe9\r\nboth \x80 and \x93quotes\x94\r\nend\r\n"));
        fx.run(save(&file, bytes.clone(), true).unwrap());
        assert_eq!(on_disk(&fx, FILE), bytes);
        assert_eq!(staged(&fx, FILE), bytes);

        // Read as UTF-8 instead, the bytes are kept as they are.
        let fx = merging("conflict-cp1252-as-utf8", Some(b"caf\xe9\n"), Some(b"caf\xe9 a\n"), Some(b"caf\xe9 b\n"));
        let file = load_file(&fx, FILE);
        assert!(!file.transcoded);
        assert_eq!(file.output(&draft(&[Theirs]), Eol::Keep).unwrap(), b"caf\xe9 b\n");
    }

    #[test]
    fn utf16_files_with_a_byte_order_mark_merge_as_text() {
        use Resolution::*;
        let utf16 = |text: &str| FileEncoding { encoding: Encoding::Utf16Le, bom: true }.encode(text);
        let fx = merging(
            "conflict-utf16",
            Some(&utf16("one\r\ntwo\r\nthree\r\n")),
            Some(&utf16("one\r\nTWO (ours)\r\nthree\r\n")),
            Some(&utf16("one\r\ntwo, theirs\r\nthree\r\n")),
        );
        // Git calls the file binary; read with its mark, it is text, whatever
        // the repository's encoding says.
        let file = load_in(&fx, FILE, Encoding::Windows1252);
        assert!(file.transcoded);
        assert_eq!(file.encoding, FileEncoding { encoding: Encoding::Utf16Le, bom: true });
        let (merge, draft_from_disk) = text(&file);
        assert_eq!(merge.conflicts.len(), 1);
        assert_eq!(draft_from_disk.open_conflicts(), 1, "git's own side on disk is no resolution");
        let (ours, theirs) = merge.conflict(0).unwrap();
        assert_eq!((ours.line(0), theirs.line(0)), ("TWO (ours)".into(), "two, theirs".into()));

        let expected = utf16("one\r\ntwo, theirs\r\nthree\r\n");
        let bytes = file.output(&draft(&[Theirs]), Eol::Keep).unwrap();
        assert_eq!(bytes, expected);
        fx.run(save(&file, bytes, true).unwrap());
        assert_eq!(staged(&fx, FILE), expected);
        assert_eq!(on_disk(&fx, FILE), expected);
        assert_eq!(unmerged(&fx), "");
    }

    #[test]
    fn line_endings_are_written_as_asked() {
        use Resolution::*;
        let fx = merging("conflict-eol", Some(b"a\nb\nc\n"), Some(b"a\nours\nc\n"), Some(b"a\ntheirs\nc\n"));
        let file = load_file(&fx, FILE);
        let choice = draft(&[Custom("one\ntwo".to_owned())]);
        assert_eq!(file.output(&choice, Eol::Keep).unwrap(), b"a\none\ntwo\nc\n");
        assert_eq!(file.output(&choice, Eol::Crlf).unwrap(), b"a\r\none\r\ntwo\r\nc\r\n");
        let bytes = file.output(&draft(&[Ours]), Eol::Crlf).unwrap();
        fx.run(save(&file, bytes, true).unwrap());
        assert_eq!(staged(&fx, FILE), b"a\r\nours\r\nc\r\n");

        let fx = merging("conflict-eol-lf", Some(b"a\r\nb\r\n"), Some(b"a\r\nours\r\n"), Some(b"a\r\ntheirs\r\n"));
        let file = load_file(&fx, FILE);
        assert_eq!(file.output(&draft(&[Theirs]), Eol::Lf).unwrap(), b"a\ntheirs\n");
        assert_eq!(file.output(&draft(&[Theirs]), Eol::Keep).unwrap(), b"a\r\ntheirs\r\n");
    }

    #[test]
    fn lines_that_look_like_markers_are_not_mistaken_for_them() {
        let base = b"intro\n<<<<<<< HEAD\nsample\n=======\nx\n>>>>>>> other\noutro\n";
        let ours = b"intro\n<<<<<<< HEAD\nsample\n=======\nours\n========== ours\n>>>>>>> other\noutro\n";
        let theirs = b"intro\n<<<<<<< HEAD\nsample\n=======\ntheirs\n>>>>>>> other\noutro\n";
        let merge = merge_text(base, ours, theirs).unwrap();
        assert_eq!(merge.conflicts.len(), 1);
        let (mine, yours) = merge.conflict(0).unwrap();
        assert_eq!(show(mine.bytes()), "ours\n========== ours\n");
        assert_eq!(show(yours.bytes()), "theirs\n");
        let take_ours = Draft { choices: vec![Resolution::Ours], edits: BTreeMap::new() };
        assert_eq!(show(&render(&merge, &take_ours, ("a", "b"))), show(ours));

        // No common ancestor (both sides added the file): everything that
        // differs is one conflict, and identical input merges to itself.
        let merge = merge_text(b"", b"same\nmine\n", b"same\nyours\n").unwrap();
        assert_eq!(merge.conflicts.len(), 1);
        let clean = merge_text(b"a\n", b"a\nb\n", b"a\nb\n").unwrap();
        assert!(clean.conflicts.is_empty());
        assert_eq!(render(&clean, &Draft::unresolved(&clean), ("a", "b")), b"a\nb\n");
    }

    #[test]
    fn the_executable_bit_survives_resolving() {
        let fx = Fixture::new("conflict-mode");
        // The bit lives in the index only, whatever the file system says.
        git(&fx.dir, &["config", "core.filemode", "false"]);
        fx.write(FILE, "one\ntwo\n");
        git(&fx.dir, &["add", FILE]);
        git(&fx.dir, &["update-index", "--chmod=+x", FILE]);
        diverge(&fx, Some(b"one\ntwo\n"), Some(b"one\nours\n"), Some(b"one\ntheirs\n"));
        stop_on_conflict(&fx, ops::merge("feature"), RepoState::Merge);

        let file = load_file(&fx, FILE);
        resolve(&fx, &file, &draft(&[Resolution::Theirs]), b"one\ntheirs\n");
        assert!(git(&fx.dir, &["ls-files", "-s", "--", FILE]).starts_with("100755 "));
    }

    #[test]
    fn delete_modify_conflicts_keep_a_side_or_delete() {
        // Deleted here, changed there: keep their version.
        let fx = merging("conflict-deleted-by-us", Some(b"one\ntwo\n"), None, Some(b"one\nchanged\n"));
        let file = load_file(&fx, FILE);
        assert_eq!(whole(&file), Whole::DeletedByUs);
        assert!(file.ours.is_none() && file.theirs.is_some() && file.base.is_some());
        let done = fx.run(keep(FILE, Side::Theirs));
        assert_eq!(show(&on_disk(&fx, FILE)), "one\nchanged\n");
        assert_eq!(show(&staged(&fx, FILE)), "one\nchanged\n");
        assert_eq!(unmerged(&fx), "");
        assert!(done.snap.status.unstaged.is_empty());

        // Deleted here, changed there: stay deleted.
        let fx = merging("conflict-deleted-by-us-rm", Some(b"one\ntwo\n"), None, Some(b"one\nchanged\n"));
        let done = fx.run(delete(FILE));
        assert!(!fx.dir.join(FILE).exists());
        assert_eq!(git(&fx.dir, &["ls-files", "-s", "--", FILE]), "");
        assert!(done.snap.status.unstaged.is_empty());
        let done = fx.run(ops::continue_op(RepoState::Merge).unwrap());
        assert_eq!(done.snap.state, RepoState::Clean);

        // Changed here, deleted there: keep ours, or delete.
        let fx = merging("conflict-deleted-by-them", Some(b"one\ntwo\n"), Some(b"one\nmine\n"), None);
        let file = load_file(&fx, FILE);
        assert_eq!(whole(&file), Whole::DeletedByThem);
        fx.run(keep(FILE, Side::Ours));
        assert_eq!(show(&on_disk(&fx, FILE)), "one\nmine\n");
        assert_eq!(show(&staged(&fx, FILE)), "one\nmine\n");
        assert_eq!(unmerged(&fx), "");

        let fx = merging("conflict-deleted-by-them-rm", Some(b"one\ntwo\n"), Some(b"one\nmine\n"), None);
        fx.run(delete(FILE));
        assert!(!fx.dir.join(FILE).exists());
        assert_eq!(git(&fx.dir, &["ls-files", "-s", "--", FILE]), "");
    }

    #[test]
    fn binary_conflicts_keep_one_side_whole() {
        let (base, ours, theirs) = (&b"\x00\x01base\xff"[..], &b"\x00\x01ours\r\n\xfe"[..], &b"\x00\x02theirs"[..]);
        let fx = merging("conflict-binary-ours", Some(base), Some(ours), Some(theirs));
        let file = load_file(&fx, FILE);
        assert_eq!(whole(&file), Whole::Binary);
        assert!(file.ours.as_ref().unwrap().binary);
        assert_eq!(file.theirs.as_ref().unwrap().size, theirs.len());
        fx.run(keep(FILE, Side::Ours));
        assert_eq!(on_disk(&fx, FILE), ours);
        assert_eq!(staged(&fx, FILE), ours);
        assert_eq!(unmerged(&fx), "");

        let fx = merging("conflict-binary-theirs", Some(base), Some(ours), Some(theirs));
        let done = fx.run(keep(FILE, Side::Theirs));
        assert_eq!(on_disk(&fx, FILE), theirs);
        assert_eq!(staged(&fx, FILE), theirs);
        assert_eq!(unmerged(&fx), "");
        assert!(done.snap.status.unstaged.is_empty());
    }

    #[test]
    fn rename_conflicts_are_settled_path_by_path() {
        let fx = Fixture::new("conflict-rename");
        let body: String = (1..=12).map(|i| format!("a line that makes the rename detectable {i}\n")).collect();
        fx.write("old.txt", &body);
        fx.commit_all("base");
        git(&fx.dir, &["checkout", "-q", "-b", "feature"]);
        git(&fx.dir, &["mv", "old.txt", "theirs.txt"]);
        fx.commit_all("their name");
        git(&fx.dir, &["checkout", "-q", "main"]);
        git(&fx.dir, &["mv", "old.txt", "ours.txt"]);
        fx.commit_all("our name");
        stop_on_conflict(&fx, ops::merge("feature"), RepoState::Merge);

        assert_eq!(whole(&load_file(&fx, "old.txt")), Whole::BothDeleted);
        let ours = load_file(&fx, "ours.txt");
        assert_eq!(whole(&ours), Whole::AddedByUs);
        assert_eq!(whole(&load_file(&fx, "theirs.txt")), Whole::AddedByThem);

        fx.run(delete("old.txt"));
        fx.run(keep("ours.txt", Side::Ours));
        let done = fx.run(delete("theirs.txt"));
        assert_eq!(unmerged(&fx), "");
        assert!(done.snap.status.unstaged.is_empty());
        assert_eq!(fx.read("ours.txt"), body);
        assert!(!fx.dir.join("theirs.txt").exists());
        let done = fx.run(ops::continue_op(RepoState::Merge).unwrap());
        assert_eq!(done.snap.state, RepoState::Clean);
    }

    #[test]
    fn sides_are_named_after_the_operation_in_progress() {
        let versions = (&b"one\ntwo\n"[..], &b"one\nours\n"[..], &b"one\ntheirs\n"[..]);

        // Cherry-pick: the incoming side is the commit being picked.
        let fx = Fixture::new("conflict-names-pick");
        diverge(&fx, Some(versions.0), Some(versions.1), Some(versions.2));
        let picked = git(&fx.dir, &["rev-parse", "feature"]);
        let oid = Oid::from_str(&picked).unwrap();
        stop_on_conflict(&fx, ops::cherry_pick(oid, false), RepoState::CherryPick);
        let file = load_file(&fx, FILE);
        assert_eq!(file.ours_info.name, "main");
        assert_eq!(file.theirs_info.name, format!("commit {}", &picked[..7]));
        assert!(file.theirs_info.detail.ends_with("their change"));
        assert_eq!(file.theirs_info.marker, format!("{} (their change)", &picked[..7]));

        // Revert: the incoming side undoes a commit.
        let fx = Fixture::new("conflict-names-revert");
        fx.write(FILE, "one\ntwo\n");
        fx.commit_all("base");
        fx.write(FILE, "one\nsecond\n");
        fx.commit_all("second");
        let second = git(&fx.dir, &["rev-parse", "HEAD"]);
        fx.write(FILE, "one\nthird\n");
        fx.commit_all("third");
        stop_on_conflict(&fx, ops::revert(Oid::from_str(&second).unwrap(), false), RepoState::Revert);
        let file = load_file(&fx, FILE);
        assert_eq!(file.theirs_info.name, format!("revert of {}", &second[..7]));
        let (merge, _) = text(&file);
        assert_eq!(merge.conflict(0).unwrap().1.text(), "two");

        // Rebase: "ours" is the branch rebased onto, "theirs" our own commit.
        let fx = Fixture::new("conflict-names-rebase");
        diverge(&fx, Some(versions.0), Some(versions.1), Some(versions.2));
        git(&fx.dir, &["checkout", "-q", "feature"]);
        stop_on_conflict(&fx, ops::rebase("main"), RepoState::Rebase);
        let file = load_file(&fx, FILE);
        assert_eq!((file.ours_info.role, file.ours_info.name.as_str()), ("Current", "main"));
        assert_eq!((file.theirs_info.role, file.theirs_info.name.as_str()), ("Incoming", "feature"));
        assert!(file.theirs_info.detail.ends_with("their change"), "{}", file.theirs_info.detail);
        let (merge, _) = text(&file);
        let (ours, theirs) = merge.conflict(0).unwrap();
        assert_eq!((ours.text().as_str(), theirs.text().as_str()), ("ours", "theirs"));
        // Resolving and continuing completes the rebase.
        resolve(&fx, &file, &draft(&[Resolution::OursTheirs]), b"one\nours\ntheirs\n");
        let done = fx.run(ops::continue_op(RepoState::Rebase).unwrap());
        assert_eq!(done.snap.state, RepoState::Clean);
        assert_eq!(done.snap.head.branch.as_deref(), Some("feature"));
    }
}

