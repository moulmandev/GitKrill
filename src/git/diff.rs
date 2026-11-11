//! Single-file diffs, parsed into hunks for display and kept as raw patch
//! bytes so individual hunks can be staged, unstaged or discarded.

use std::sync::atomic::{AtomicU8, Ordering};

use git2::{DiffFindOptions, DiffFormat, DiffOptions};

use super::repo::{Loader, delta_kind, lossy};
use super::picture;
use super::text::{self, Encoding, display_text_in, source_text_in};
use super::{DiffLine, DiffSource, DiffTarget, FileDiff, Hunk};

/// Beyond this many lines the rest of the diff is not shown.
pub(super) const MAX_LINES: usize = 50_000;
/// Binary files larger than this are not looked at as UTF-16 text.
const MAX_TRANSCODED_BYTES: usize = 16 * 1024 * 1024;
/// Lines of context around each change, as `git diff` has.
const CONTEXT: u32 = 3;
/// Context that takes in every line of a file. Not `u32::MAX`: xdiff adds
/// it to line numbers in a C `long`, which has 32 bits on Windows.
const WHOLE_FILE_CONTEXT: u32 = 1 << 24;
/// A diff of the hunks of a file no larger than this (either version) comes
/// with all its lines (`FileDiff::whole`), so that syntax colours can follow
/// the file from its start. Larger files are coloured hunk by hunk.
pub const WHOLE_FOR_COLOURS_BYTES: u64 = 1024 * 1024;

// ---- how a diff is worked out ---------------------------------------------------

/// How a diff is worked out, besides what it is a diff of. Picked in the
/// diff view's header and kept in Preferences > Editor. A request for a diff
/// carries the mode it was made with, and so does the answer
/// (`FileDiff::mode`), so an answer for another mode is never taken for one
/// of this.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Mode {
    /// Every line of the file with the changes in place, not only the hunks.
    /// The hunks stay those of three lines of context, so each can still be
    /// staged on its own; the unchanged lines between them come as
    /// `Hunk::gap` hunks.
    pub whole_file: bool,
    /// Lines that differ only in whitespace count as the same: `git diff -w`
    /// (`--ignore-all-space`), libgit2's `ignore_whitespace`. Such a diff
    /// does not match the files byte for byte, so its hunks and lines must
    /// not be handed to `git apply`.
    pub ignore_whitespace: bool,
}

impl Mode {
    fn pack(self) -> u8 {
        u8::from(self.whole_file) | u8::from(self.ignore_whitespace) << 1
    }

    fn unpack(bits: u8) -> Self {
        Mode { whole_file: bits & 1 != 0, ignore_whitespace: bits & 2 != 0 }
    }
}

static MODE: AtomicU8 = AtomicU8::new(0);

/// The mode set in Preferences > Editor (put there by `prefs::editor::apply`).
pub fn mode() -> Mode {
    Mode::unpack(MODE.load(Ordering::Relaxed))
}

pub fn set_mode(mode: Mode) {
    MODE.store(mode.pack(), Ordering::Relaxed);
}

impl Loader {
    /// The diff of one file, hunks with three lines of context, whitespace
    /// counted: what can be staged in part, whatever the diff view shows.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn diff(&self, target: &DiffTarget) -> Result<FileDiff, String> {
        self.diff_with(target, Mode::default())
    }

    /// The diff of one file, worked out as `mode` says.
    pub fn diff_with(&self, target: &DiffTarget, mode: Mode) -> Result<FileDiff, String> {
        let repo = &self.repo;
        // How lines become text: the repository's encoding, the tab width.
        let format = text::format_for(repo.workdir().unwrap_or(repo.path()));
        if let DiffSource::At(commit) = target.source {
            return self.file_at(commit, &target.path, mode, format);
        }
        let (mut out, blobs) = self.collect(target, mode, CONTEXT, format)?;
        out.mode = mode;
        if let Some(change) = &mut out.submodule {
            // git writes "Subproject commit <id>-dirty" for the working tree
            // of a submodule whose own files changed.
            change.dirty = out.hunks.iter().flat_map(|h| &h.lines).any(|l| l.origin == '+' && l.text.trim_end().ends_with("-dirty"));
            change.describe(repo, &target.path);
            return Ok(out);
        }

        let is_image = picture::is_image_path(&target.path)
            || target.old_path.as_deref().is_some_and(picture::is_image_path);
        if out.binary && is_image && let Some((old, new)) = blobs {
            // An unstaged file has no blob yet: it is read from disk, unless
            // it is the deletion being shown.
            let on_disk = (target.source.reads_working_tree() && out.kind != Some(super::ChangeKind::Deleted))
                .then(|| repo.workdir().map(|dir| dir.join(&target.path)))
                .flatten();
            let images = picture::load(repo, Some(old), Some(new), on_disk.as_deref());
            if images.old.is_some() || images.new.is_some() {
                out.images = Some(std::sync::Arc::new(images));
            }
        } else if out.binary && let Some((old, new)) = blobs {
            let _ = self.utf16_diff(target, old, new, mode, &mut out);
        } else if mode.whole_file && !out.binary && !out.hunks.is_empty() {
            let (whole, _) = self.collect(target, mode, WHOLE_FILE_CONTEXT, format)?;
            fill_gaps(&mut out, whole);
        } else if !out.binary
            && !out.hunks.is_empty()
            && crate::prefs::editor::look().highlight
            && blobs.is_some_and(|(old, new)| self.small_for_colours(target, old, new, out.kind))
        {
            let (whole, _) = self.collect(target, mode, WHOLE_FILE_CONTEXT, format)?;
            out.whole = only_hunk(whole);
        }

        // An SVG is text, and a picture too: both versions drawn, unless one
        // does not parse (its text alone is shown then).
        if !out.binary
            && picture::is_svg_path(&target.path)
            && let Some((old, new)) = blobs
        {
            let on_disk = (target.source == DiffSource::Unstaged && out.kind != Some(super::ChangeKind::Deleted))
                .then(|| repo.workdir().map(|dir| dir.join(&target.path)))
                .flatten();
            let images = picture::load_svg(repo, Some(old), Some(new), on_disk.as_deref());
            if images.all_drawn() {
                out.images = Some(std::sync::Arc::new(images));
            }
        }

        // Nothing left once whitespace is ignored: say whether that is
        // because only whitespace changed. (libgit2 may then leave the file
        // out altogether, so its kind comes from the full diff too.)
        if mode.ignore_whitespace && out.hunks.is_empty() && !out.binary {
            let full = self.diff_with(target, Mode::default())?;
            out.only_whitespace = !full.hunks.is_empty();
            if out.kind.is_none() {
                (out.kind, out.old_path) = (full.kind, full.old_path);
            }
        }
        Ok(out)
    }

    /// Runs the diff for `target` with `context` lines of context and reads
    /// its lines, with the blob ids of both sides of the file.
    fn collect(
        &self,
        target: &DiffTarget,
        mode: Mode,
        context: u32,
        format: text::Format,
    ) -> Result<(FileDiff, Option<(git2::Oid, git2::Oid)>), String> {
        let repo = &self.repo;
        let err = |e: git2::Error| e.message().to_owned();

        let mut opts = DiffOptions::new();
        opts.context_lines(context)
            .ignore_whitespace(mode.ignore_whitespace)
            .disable_pathspec_match(true)
            .pathspec(target.path.as_str());
        if let Some(old) = &target.old_path {
            opts.pathspec(old.as_str());
        }

        let mut diff = match target.source {
            DiffSource::Unstaged => {
                opts.include_untracked(true)
                    .recurse_untracked_dirs(true)
                    .show_untracked_content(true);
                repo.diff_index_to_workdir(None, Some(&mut opts)).map_err(err)?
            }
            DiffSource::Staged => {
                let tree = repo.head().ok().and_then(|h| h.peel_to_tree().ok());
                repo.diff_tree_to_index(tree.as_ref(), None, Some(&mut opts)).map_err(err)?
            }
            DiffSource::Commit(oid) => {
                let commit = repo.find_commit(oid).map_err(err)?;
                let tree = commit.tree().map_err(err)?;
                let parent_tree = match commit.parent(0) {
                    Ok(parent) => Some(parent.tree().map_err(err)?),
                    Err(_) => None,
                };
                repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut opts))
                    .map_err(err)?
            }
            DiffSource::Compare { from, to } => {
                let from = repo.find_commit(from).and_then(|c| c.tree()).map_err(err)?;
                let to = repo.find_commit(to).and_then(|c| c.tree()).map_err(err)?;
                repo.diff_tree_to_tree(Some(&from), Some(&to), Some(&mut opts)).map_err(err)?
            }
            DiffSource::WorkingTree { from } => {
                let tree = repo.find_commit(from).and_then(|c| c.tree()).map_err(err)?;
                opts.include_untracked(true)
                    .recurse_untracked_dirs(true)
                    .show_untracked_content(true);
                repo.diff_tree_to_workdir_with_index(Some(&tree), Some(&mut opts)).map_err(err)?
            }
            // Not a diff: `diff_with` reads it with `file_at`.
            DiffSource::At(_) => return Err("A file of a commit is not a diff.".to_owned()),
        };

        if target.old_path.is_some() {
            let mut find = DiffFindOptions::new();
            find.renames(true);
            let _ = diff.find_similar(Some(&mut find));
        }

        let mut out = FileDiff { format, ..FileDiff::default() };
        // Blob ids of both sides of the file shown, for the image preview.
        let mut blobs = None;
        let mut wanted = false;
        let mut seen_file = false;
        let mut shown = 0usize;
        let path = target.path.as_bytes();

        let result = diff.print(DiffFormat::Patch, |delta, _hunk, line| {
            let origin = line.origin();
            let content = line.content();

            if origin == 'F' {
                // With a rename that was not detected there are two deltas
                // (delete + add); show the one for the path that was asked for.
                let new_path = delta.new_file().path_bytes();
                let old_path = delta.old_file().path_bytes();
                wanted = !seen_file && (new_path == Some(path) || old_path == Some(path));
                if wanted {
                    seen_file = true;
                    blobs = Some((delta.old_file().id(), delta.new_file().id()));
                    out.file_header = content.to_vec();
                    out.kind = delta_kind(delta.status());
                    out.old_path = old_path
                        .filter(|old| Some(*old) != new_path)
                        .map(lossy);
                    // A submodule: the "file" is the commit it is at.
                    if [delta.old_file().mode(), delta.new_file().mode()].contains(&git2::FileMode::Commit) {
                        out.submodule = Some(super::submodule::Change::between(delta.old_file().id(), delta.new_file().id()));
                    }
                }
                return true;
            }
            if !wanted {
                return true;
            }

            match origin {
                'H' => {
                    out.hunks.push(Hunk {
                        header: lossy(content).trim_end().to_owned(),
                        lines: Vec::new(),
                        raw: content.to_vec(),
                        gap: false,
                    });
                }
                ' ' | '+' | '-' => {
                    let Some(hunk) = out.hunks.last_mut() else { return true };
                    hunk.raw.push(origin as u8);
                    hunk.raw.extend_from_slice(content);

                    match origin {
                        '+' => out.additions += 1,
                        '-' => out.deletions += 1,
                        _ => {}
                    }
                    if shown >= MAX_LINES {
                        out.truncated = true;
                        return true;
                    }
                    shown += 1;

                    let text = display_text_in(content, &format);
                    out.max_chars = out.max_chars.max(text.chars().count());
                    hunk.lines.push(DiffLine {
                        origin,
                        old_no: line.old_lineno(),
                        new_no: line.new_lineno(),
                        text,
                    });
                }
                'B' => out.binary = true,
                // "\ No newline at end of file" markers ('=', '>', '<') are
                // part of the patch but not something to show as a line.
                _ => {
                    if let Some(hunk) = out.hunks.last_mut() {
                        hunk.raw.extend_from_slice(content);
                    }
                }
            }
            true
        });
        result.map_err(err)?;

        // A hunk-less header (binary files, mode changes) cannot be applied
        // hunk by hunk.
        if out.hunks.is_empty() {
            out.file_header.clear();
        }
        Ok((out, blobs))
    }

    /// Whether both versions of the file are small enough to come with the
    /// diff whole, for syntax colours (`WHOLE_FOR_COLOURS_BYTES`).
    fn small_for_colours(&self, target: &DiffTarget, old: git2::Oid, new: git2::Oid, kind: Option<super::ChangeKind>) -> bool {
        let repo = &self.repo;
        let blob_size = |id: git2::Oid| {
            if id.is_zero() {
                return Some(0);
            }
            repo.odb().ok()?.read_header(id).ok().map(|(size, _)| size as u64)
        };
        let new_size = match (target.source, kind) {
            (DiffSource::Unstaged, kind) if kind != Some(super::ChangeKind::Deleted) => {
                repo.workdir().and_then(|dir| std::fs::metadata(dir.join(&target.path)).ok()).map(|meta| meta.len())
            }
            _ => blob_size(new),
        };
        [blob_size(old), new_size].iter().all(|size| size.is_some_and(|size| size <= WHOLE_FOR_COLOURS_BYTES))
    }

    /// UTF-16 is binary to git: it is full of zero bytes. When a version of
    /// the file is UTF-16 (`text::utf16_encoding`: a byte order mark, the
    /// repository set to UTF-16, or text that plainly is), both are turned
    /// into UTF-8 and diffed as text. Its hunks and lines are applied by
    /// GitKrill itself, in the file's encoding (`FileDiff::transcoded`).
    fn utf16_diff(
        &self,
        target: &DiffTarget,
        old: git2::Oid,
        new: git2::Oid,
        mode: Mode,
        out: &mut FileDiff,
    ) -> Option<()> {
        let repo = &self.repo;
        // Every binary file comes through here: big ones are not read at all.
        let blob = |id: git2::Oid| {
            if id.is_zero() {
                return Some(Vec::new());
            }
            let (size, _) = repo.odb().ok()?.read_header(id).ok()?;
            (size <= MAX_TRANSCODED_BYTES).then(|| repo.find_blob(id).ok().map(|blob| blob.content().to_vec()))?
        };
        let new_bytes = match (target.source, out.kind) {
            (source, kind) if source.reads_working_tree() && kind != Some(super::ChangeKind::Deleted) => {
                let file = repo.workdir()?.join(&target.path);
                let small = std::fs::metadata(&file).is_ok_and(|meta| meta.len() <= MAX_TRANSCODED_BYTES as u64);
                small.then(|| std::fs::read(&file).ok()).flatten()
            }
            _ => blob(new),
        };
        let (old_bytes, new_bytes) = (blob(old)?, new_bytes?);
        let default = out.format.encoding;
        let utf16 = |bytes: &[u8]| (!bytes.is_empty()).then(|| text::utf16_encoding(bytes, default)).flatten();
        let (old_found, new_found) = (utf16(&old_bytes), utf16(&new_bytes));
        if old_found.is_none() && new_found.is_none() {
            return None;
        }
        let read = |bytes: &[u8], found: Option<text::FileEncoding>| {
            let found = found.unwrap_or_else(|| text::detect(bytes, default));
            (text::decode_as(bytes, found), found)
        };
        let (old_text, old_encoding) = read(&old_bytes, old_found);
        let (new_text, new_encoding) = read(&new_bytes, new_found);
        // Zero bytes left after decoding: binary after all.
        if old_text.contains('\0') || new_text.contains('\0') {
            return None;
        }

        // The lines are UTF-8 now, whatever the file is in.
        let format = text::Format { encoding: Encoding::Utf8, ..out.format };
        let path = std::path::Path::new(&target.path);
        let patch = |context: u32| {
            let mut opts = DiffOptions::new();
            opts.context_lines(context).ignore_whitespace(mode.ignore_whitespace);
            let patch = git2::Patch::from_buffers(
                old_text.as_bytes(),
                Some(path),
                new_text.as_bytes(),
                Some(path),
                Some(&mut opts),
            )
            .ok()?;
            patch_lines(&patch, &format)
        };
        let lines = patch(CONTEXT)?;
        out.hunks = lines.hunks;
        out.binary = false;
        out.truncated = lines.truncated;
        (out.additions, out.deletions, out.max_chars) = (lines.additions, lines.deletions, lines.max_chars);
        out.format = format;
        out.file_header.clear();
        if !out.hunks.is_empty() {
            let whole = || {
                let whole = patch(WHOLE_FILE_CONTEXT)?;
                Some(FileDiff { hunks: whole.hunks, truncated: whole.truncated, max_chars: whole.max_chars, ..FileDiff::default() })
            };
            let small = [&old_text, &new_text].iter().all(|text| text.len() as u64 <= WHOLE_FOR_COLOURS_BYTES);
            if mode.whole_file {
                fill_gaps(out, whole()?);
            } else if small && crate::prefs::editor::look().highlight {
                out.whole = whole().and_then(only_hunk);
            }
        }

        // What it takes to stage, unstage and discard its hunks and lines.
        let old_side = (!old.is_zero()).then_some(&old_bytes[..]);
        let new_side = (out.kind != Some(super::ChangeKind::Deleted)).then_some(&new_bytes[..]);
        let sides = super::utf16::Sides::new(
            &target.path,
            super::utf16::Side::new(old_side, old_text.clone(), old_encoding),
            &old_bytes,
            super::utf16::Side::new(new_side, new_text.clone(), new_encoding),
            &new_bytes,
        );
        if let Some(sides) = sides {
            out.file_header = sides.header();
            out.transcoded = Some(std::sync::Arc::new(sides));
        }
        Some(())
    }
}

/// The lines of a patch made from two buffers, as the diff view takes them.
struct PatchLines {
    hunks: Vec<Hunk>,
    additions: usize,
    deletions: usize,
    max_chars: usize,
    truncated: bool,
}

fn patch_lines(patch: &git2::Patch, format: &text::Format) -> Option<PatchLines> {
    let mut hunks = Vec::new();
    let (mut additions, mut deletions, mut max_chars, mut shown) = (0, 0, 0, 0usize);
    for h in 0..patch.num_hunks() {
        let (hunk, count) = patch.hunk(h).ok()?;
        let mut made =
            Hunk { header: lossy(hunk.header()).trim_end().to_owned(), lines: Vec::new(), raw: hunk.header().to_vec(), gap: false };
        for l in 0..count {
            let line = patch.line_in_hunk(h, l).ok()?;
            let origin = line.origin();
            match origin {
                ' ' | '+' | '-' => {
                    made.raw.push(origin as u8);
                    made.raw.extend_from_slice(line.content());
                    match origin {
                        '+' => additions += 1,
                        '-' => deletions += 1,
                        _ => {}
                    }
                    if shown < MAX_LINES {
                        shown += 1;
                        let text = display_text_in(line.content(), format);
                        max_chars = max_chars.max(text.chars().count());
                        made.lines.push(DiffLine { origin, old_no: line.old_lineno(), new_no: line.new_lineno(), text });
                    }
                }
                _ => made.raw.extend_from_slice(line.content()),
            }
        }
        hunks.push(made);
    }
    let lines: usize = hunks.iter().map(|hunk| split_hunk(&hunk.raw).map_or(0, |(_, lines)| lines.len())).sum();
    Some(PatchLines { hunks, additions, deletions, max_chars, truncated: lines > shown })
}

// ---- the whole file -----------------------------------------------------------------

/// The one hunk of a diff worked out with a context that takes in the whole
/// file, unless it was cut short.
fn only_hunk(whole: FileDiff) -> Option<std::sync::Arc<Hunk>> {
    if whole.truncated {
        return None;
    }
    let [all]: [Hunk; 1] = whole.hunks.try_into().ok()?;
    Some(std::sync::Arc::new(all))
}

/// Puts the unchanged lines between the hunks of `out` back, from `whole`:
/// the same diff worked out with a context that takes in the whole file.
/// When the file is too long to show whole, `out` keeps only its hunks and
/// says so; if the two diffs do not line up (they should), it keeps them
/// without a word.
fn fill_gaps(out: &mut FileDiff, whole: FileDiff) {
    if whole.truncated {
        out.too_long_for_whole_file = true;
        return;
    }
    let [all] = whole.hunks.as_slice() else { return };
    if with_gaps(&mut out.hunks, all) {
        out.max_chars = out.max_chars.max(whole.max_chars);
    }
}

/// Interleaves `hunks` with `gap` hunks holding the lines of `all` (one
/// hunk with every line of the file) that are in none of them. The lines of
/// each hunk are a run of the lines of `all`, found by their numbers; false,
/// with `hunks` as they were, if one is not there.
fn with_gaps(hunks: &mut Vec<Hunk>, all: &Hunk) -> bool {
    let Some((_, raw)) = split_hunk(&all.raw) else { return false };
    if raw.len() != all.lines.len() {
        return false;
    }
    let same = |a: &DiffLine, b: &DiffLine| {
        (a.origin, a.old_no, a.new_no) == (b.origin, b.old_no, b.new_no) && a.text == b.text
    };

    // Where each hunk starts among all the lines, checked before anything
    // is changed.
    let mut starts = Vec::with_capacity(hunks.len());
    let mut at = 0;
    for hunk in hunks.iter() {
        let Some(first) = hunk.lines.first() else { return false };
        let Some(start) = all.lines[at..].iter().position(|line| same(line, first)) else { return false };
        let start = at + start;
        let end = start + hunk.lines.len();
        if end > all.lines.len() || !all.lines[start..end].iter().zip(&hunk.lines).all(|(a, b)| same(a, b)) {
            return false;
        }
        starts.push(start);
        at = end;
    }

    let gap = |from: usize, to: usize| {
        // The lines as context, without a header, for copying.
        let mut bytes = b"\n".to_vec();
        for line in &raw[from..to] {
            bytes.push(b' ');
            bytes.extend_from_slice(line.body);
            bytes.push(b'\n');
            if !line.eol {
                bytes.extend_from_slice(NO_EOL_MARKER);
            }
        }
        Hunk { header: String::new(), lines: all.lines[from..to].to_vec(), raw: bytes, gap: true }
    };
    let mut out = Vec::with_capacity(hunks.len() * 2 + 1);
    let mut at = 0;
    for (hunk, start) in std::mem::take(hunks).into_iter().zip(starts) {
        if start > at {
            out.push(gap(at, start));
        }
        at = start + hunk.lines.len();
        out.push(hunk);
    }
    if at < all.lines.len() {
        out.push(gap(at, all.lines.len()));
    }
    *hunks = out;
    true
}

// ---- line-level patches -----------------------------------------------------

/// How a line-level patch is going to be handed to `git apply`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineApply {
    /// Applied as is to the side the diff starts from: staging lines of an
    /// unstaged diff into the index.
    Forward,
    /// Applied with `--reverse` to the side the diff ends at: unstaging lines
    /// from the index, or discarding lines from the working tree.
    Reverse,
}

const NO_EOL_MARKER: &[u8] = b"\\ No newline at end of file\n";

/// One line of a hunk as it appears in the patch.
struct RawLine<'a> {
    /// b' ', b'+' or b'-'.
    origin: u8,
    /// The line without its origin character and without the final `\n`
    /// (a `\r` before it, in CRLF files, is part of the body).
    body: &'a [u8],
    /// False for the last line of a file that does not end in a newline.
    eol: bool,
}

/// Which side of the patch being built a line is on. `Base` is the side git
/// matches against the existing file (the old side going forward, the new
/// side in reverse); `Target` is what that file is turned into.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Both,
    Base,
    Target,
}

struct OutLine<'a> {
    side: Side,
    body: &'a [u8],
    eol: bool,
    /// Terminator given to a line that had none in the original diff.
    added_eol: &'static [u8],
}

impl OutLine<'_> {
    fn in_target(&self) -> bool {
        self.side != Side::Base
    }
}

/// Splits `Hunk::raw` into its header line and content lines.
fn split_hunk(raw: &[u8]) -> Option<(&[u8], Vec<RawLine<'_>>)> {
    let header_end = raw.iter().position(|b| *b == b'\n')?;
    let header = &raw[..header_end];
    let mut lines: Vec<RawLine> = Vec::new();
    for piece in raw[header_end + 1..].split_inclusive(|b| *b == b'\n') {
        let text = piece.strip_suffix(b"\n").unwrap_or(piece);
        match text.first() {
            Some(b'\\') => {
                if let Some(last) = lines.last_mut() {
                    last.eol = false;
                }
            }
            Some(origin @ (b' ' | b'+' | b'-')) => {
                lines.push(RawLine { origin: *origin, body: &text[1..], eol: true });
            }
            // Some tools write an empty context line with no leading space.
            None => lines.push(RawLine { origin: b' ', body: text, eol: true }),
            Some(_) => return None,
        }
    }
    Some((header, lines))
}

impl Hunk {
    /// Each line's text as it is in the file (see `text::source_text`), in
    /// the order of `lines`: what copying gives back. `encoding` is the one
    /// the diff was shown in (`FileDiff::format`). `None` if the patch
    /// cannot be read back into lines.
    pub fn sources(&self, encoding: Encoding) -> Option<Vec<std::borrow::Cow<'_, str>>> {
        let (_, lines) = split_hunk(&self.raw)?;
        Some(lines.iter().map(|line| source_text_in(line.body, encoding)).collect())
    }
}

/// `(old start, new start)` from a `@@ -a,b +c,d @@` line.
fn hunk_starts(header: &[u8]) -> Option<(u64, u64)> {
    let text = std::str::from_utf8(header).ok()?;
    let mut parts = text.strip_prefix("@@ -")?.split(' ');
    let start = |range: &str| range.split(',').next()?.parse::<u64>().ok();
    let old = start(parts.next()?)?;
    let new = start(parts.next()?.strip_prefix('+')?)?;
    Some((old, new))
}

/// The header of a patch that modifies one file where it already is, made
/// from the header of a diff that creates, deletes or renames it.
///
/// Needed when part of a creation or deletion is applied: the file then
/// exists on both sides, and git rejects "new file" patches that have old
/// content (and "deleted file" patches that leave content behind). And for
/// renames, where applying the original header would move the file back or
/// forth along with the selected lines.
///
/// The file is the one on the side git matches the patch against; for
/// creations and deletions, whose own side is `/dev/null`, the other one.
fn in_place_header(header: &[u8], apply: LineApply) -> Option<Vec<u8>> {
    // What follows the `a/` or `b/` of a `--- ` / `+++ ` line, and whether
    // the path is quoted (`--- "a/na\303\257ve.txt"`). `None` for /dev/null.
    let side = |marker: &[u8], prefix: &[u8]| {
        let line = header.split(|b| *b == b'\n').find(|line| line.starts_with(marker))?;
        let rest = &line[marker.len()..];
        let (quoted, rest) = match rest.strip_prefix(b"\"") {
            Some(rest) => (true, rest),
            None => (false, rest),
        };
        Some((quoted, rest.strip_prefix(prefix)?))
    };
    let (old, new) = (side(b"--- ", b"a/"), side(b"+++ ", b"b/"));
    let (quoted, path) = match apply {
        LineApply::Forward => old.or(new),
        LineApply::Reverse => new.or(old),
    }?;

    let quote: &[u8] = if quoted { b"\"" } else { b"" };
    let name = |side: &[u8]| [quote, side, path].concat();
    let (a, b) = (name(b"a/"), name(b"b/"));
    Some([b"diff --git ", &a[..], b" ", &b[..], b"\n--- ", &a[..], b"\n+++ ", &b[..], b"\n"].concat())
}

impl FileDiff {
    /// A standalone patch that carries only the selected changed lines.
    ///
    /// `selected` holds `(hunk index, index into Hunk::lines)` pairs; entries
    /// that do not name an added or removed line are ignored. Returns `None`
    /// when nothing applicable is selected or the diff cannot be patched line
    /// by line (binary files, mode-only changes).
    ///
    /// What happens to the changes that were *not* selected depends on the
    /// side git matches the patch against:
    ///
    /// * [`LineApply::Forward`] (stage): the file being patched is the old
    ///   side. An unselected removal is a line that stays, so it becomes
    ///   context; an unselected addition is not there yet and is dropped.
    /// * [`LineApply::Reverse`] (unstage, discard): the file being patched is
    ///   the new side. An unselected addition stays and becomes context; an
    ///   unselected removal is not in the file and is dropped.
    ///
    /// Either way the matched side of every hunk is byte for byte what the
    /// original diff had, so the patch applies wherever the full hunk would.
    pub fn lines_patch(&self, selected: &[(usize, usize)], apply: LineApply) -> Option<Vec<u8>> {
        if self.file_header.is_empty() {
            return None;
        }
        let mut selected: Vec<(usize, usize)> = selected.to_vec();
        selected.sort_unstable();
        selected.dedup();

        let mut body = Vec::new();
        // How far the target side has drifted from the base side so far.
        let mut drift: i64 = 0;
        let (mut total_base, mut total_target) = (0u64, 0u64);

        let mut rest = selected.as_slice();
        while let Some(&(h, _)) = rest.first() {
            let len = rest.iter().take_while(|(hunk, _)| *hunk == h).count();
            let (picked, tail) = rest.split_at(len);
            rest = tail;

            let hunk = self.hunks.get(h)?;
            // The unchanged lines of a whole-file view have nothing to apply.
            if hunk.gap {
                continue;
            }
            let (header, lines) = split_hunk(&hunk.raw)?;
            let (old_start, new_start) = hunk_starts(header)?;
            let is_picked = |i: usize| picked.binary_search(&(h, i)).is_ok();

            let (kept_sign, dropped_sign) = match apply {
                LineApply::Forward => (b'-', b'+'),
                LineApply::Reverse => (b'+', b'-'),
            };
            // The terminator to give a line that has none but must not stay
            // last: whatever the lines around it use.
            let eol_style: &'static [u8] =
                if lines.iter().any(|l| l.eol && l.body.ends_with(b"\r")) { b"\r\n" } else { b"\n" };

            let mut out: Vec<OutLine> = Vec::with_capacity(lines.len());
            let mut changes = 0usize;
            for (i, line) in lines.iter().enumerate() {
                let side = if line.origin == b' ' {
                    Side::Both
                } else if is_picked(i) {
                    changes += 1;
                    if line.origin == kept_sign { Side::Base } else { Side::Target }
                } else if line.origin == kept_sign {
                    // Already in the file being patched, and it stays.
                    Side::Both
                } else {
                    debug_assert_eq!(line.origin, dropped_sign);
                    continue;
                };
                out.push(OutLine { side, body: line.body, eol: line.eol, added_eol: b"" });
            }
            if changes == 0 {
                continue;
            }

            // Only the last line of a file may lack a newline. On the base
            // side that holds by construction; on the target side a line
            // without one can end up followed by others (keeping "beta" and
            // adding "gamma" after it), and then it needs a terminator.
            let last_target = out.iter().rposition(OutLine::in_target);
            let mut fixed: Vec<OutLine> = Vec::with_capacity(out.len() + 1);
            for (i, line) in out.into_iter().enumerate() {
                if line.eol || !line.in_target() || Some(i) == last_target {
                    fixed.push(line);
                    continue;
                }
                if line.side == Side::Both {
                    // Unterminated in the file as it is now, terminated in
                    // the result: no longer the same line on both sides.
                    fixed.push(OutLine { side: Side::Base, ..line });
                }
                fixed.push(OutLine { side: Side::Target, body: line.body, eol: true, added_eol: eol_style });
            }

            let base_count = fixed.iter().filter(|l| l.side != Side::Target).count() as u64;
            let target_count = fixed.iter().filter(|l| l.side != Side::Base).count() as u64;
            total_base += base_count;
            total_target += target_count;

            // An empty range is written as "the line before it, 0 lines".
            let base_start = match apply {
                LineApply::Forward => old_start,
                LineApply::Reverse => new_start,
            };
            let first_line = if base_count == 0 { base_start + 1 } else { base_start };
            let target_line = (first_line as i64 + drift).max(1) as u64;
            let target_start = if target_count == 0 { target_line - 1 } else { target_line };
            drift += target_count as i64 - base_count as i64;

            let ((old_start, old_count), (new_start, new_count)) = match apply {
                LineApply::Forward => ((base_start, base_count), (target_start, target_count)),
                LineApply::Reverse => ((target_start, target_count), (base_start, base_count)),
            };
            body.extend_from_slice(
                format!("@@ -{old_start},{old_count} +{new_start},{new_count} @@\n").as_bytes(),
            );
            for line in &fixed {
                body.push(match (line.side, apply) {
                    (Side::Both, _) => b' ',
                    (Side::Base, LineApply::Forward) | (Side::Target, LineApply::Reverse) => b'-',
                    (Side::Target, LineApply::Forward) | (Side::Base, LineApply::Reverse) => b'+',
                });
                body.extend_from_slice(line.body);
                if !line.added_eol.is_empty() {
                    body.extend_from_slice(line.added_eol);
                } else {
                    body.push(b'\n');
                    if !line.eol {
                        body.extend_from_slice(NO_EOL_MARKER);
                    }
                }
            }
        }
        if body.is_empty() {
            return None;
        }

        // A creation or deletion applied in part leaves the file existing on
        // both sides. (Such a diff has one hunk, so the totals are those of
        // the whole file.) A rename stays as it is: the lines are changed in
        // the file where it is now.
        let (old_total, new_total) = match apply {
            LineApply::Forward => (total_base, total_target),
            LineApply::Reverse => (total_target, total_base),
        };
        let has_line = |start: &[u8]| self.file_header.split(|b| *b == b'\n').any(|l| l.starts_with(start));
        let creates = has_line(b"--- /dev/null");
        let deletes = has_line(b"+++ /dev/null");
        let moves = has_line(b"rename from ") || has_line(b"copy from ");
        let in_place = if (creates && old_total > 0) || (deletes && new_total > 0) || moves {
            in_place_header(&self.file_header, apply)
        } else {
            None
        };
        let mut patch = in_place.unwrap_or_else(|| self.file_header.clone());
        patch.extend_from_slice(&body);
        Some(patch)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::path::Path;
    use std::process::{Command, Stdio};

    use super::LineApply::{Forward, Reverse};
    use crate::git::DiffSource::{Commit, Staged, Unstaged};
    use crate::git::text::{Encoding, FileEncoding};
    use crate::git::{ChangeKind, DiffTarget, FileDiff, ops};
    use crate::tests::{Fixture, TIMEOUT, git};
    use crate::worker::Response;

    /// Exact bytes of `git show <spec>` (the shared helper trims its output).
    fn show(dir: &Path, spec: &str) -> String {
        let out = Command::new("git").current_dir(dir).args(["show", spec]).output().unwrap();
        assert!(out.status.success(), "git show {spec}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }

    fn index(fx: &Fixture, path: &str) -> String {
        show(&fx.dir, &format!(":{path}"))
    }

    /// Selection entries for the given changed lines, by origin and text.
    fn pick(diff: &FileDiff, wanted: &[(char, &str)]) -> Vec<(usize, usize)> {
        wanted
            .iter()
            .map(|(origin, text)| {
                diff.hunks
                    .iter()
                    .enumerate()
                    .find_map(|(h, hunk)| {
                        let i = hunk.lines.iter().position(|l| l.origin == *origin && l.text == *text)?;
                        Some((h, i))
                    })
                    .unwrap_or_else(|| panic!("no line {origin}{text} in the diff"))
            })
            .collect()
    }

    /// The changed lines of a diff, in order.
    fn changes(diff: &FileDiff) -> Vec<(char, String)> {
        diff.hunks
            .iter()
            .flat_map(|h| h.lines.iter())
            .filter(|l| l.origin != ' ')
            .map(|l| (l.origin, l.text.clone()))
            .collect()
    }

    fn owned(lines: &[(char, &str)]) -> Vec<(char, String)> {
        lines.iter().map(|(o, t)| (*o, (*t).to_owned())).collect()
    }

    fn stage(fx: &Fixture, path: &str, wanted: &[(char, &str)]) {
        let diff = fx.diff(Unstaged, path);
        let patch = diff.lines_patch(&pick(&diff, wanted), Forward).expect("a patch");
        fx.run(ops::apply_patch("Stage lines", patch, true, false));
    }

    fn unstage(fx: &Fixture, path: &str, wanted: &[(char, &str)]) {
        let diff = fx.diff(Staged, path);
        let patch = diff.lines_patch(&pick(&diff, wanted), Reverse).expect("a patch");
        fx.run(ops::apply_patch("Unstage lines", patch, true, true));
    }

    fn discard(fx: &Fixture, path: &str, wanted: &[(char, &str)]) {
        let diff = fx.diff(Unstaged, path);
        let patch = diff.lines_patch(&pick(&diff, wanted), Reverse).expect("a patch");
        fx.run(ops::apply_patch("Discard lines", patch, false, true));
    }

    #[test]
    fn one_of_two_changed_lines_is_staged_unstaged_and_discarded() {
        let fx = Fixture::new("lines-basic");
        fx.write("f.txt", "one\ntwo\nthree\nfour\nfive\n");
        fx.commit_all("base");
        fx.write("f.txt", "one\nTWO\nthree\nFOUR\nfive\n");
        assert_eq!(fx.diff(Unstaged, "f.txt").hunks.len(), 1);

        // Stage the change to the second line only.
        stage(&fx, "f.txt", &[('-', "two"), ('+', "TWO")]);
        assert_eq!(index(&fx, "f.txt"), "one\nTWO\nthree\nfour\nfive\n");
        assert_eq!(changes(&fx.diff(Staged, "f.txt")), owned(&[('-', "two"), ('+', "TWO")]));
        assert_eq!(changes(&fx.diff(Unstaged, "f.txt")), owned(&[('-', "four"), ('+', "FOUR")]));
        assert_eq!(fx.read("f.txt"), "one\nTWO\nthree\nFOUR\nfive\n");

        // Unstage just the added line: the removal of "two" stays staged.
        unstage(&fx, "f.txt", &[('+', "TWO")]);
        assert_eq!(index(&fx, "f.txt"), "one\nthree\nfour\nfive\n");
        assert_eq!(changes(&fx.diff(Staged, "f.txt")), owned(&[('-', "two")]));
        // ...then the removal too, and the index is back at HEAD.
        unstage(&fx, "f.txt", &[('-', "two")]);
        assert_eq!(index(&fx, "f.txt"), "one\ntwo\nthree\nfour\nfive\n");
        assert!(fx.snapshot().status.staged.is_empty());
        assert_eq!(fx.read("f.txt"), "one\nTWO\nthree\nFOUR\nfive\n");

        // Discard the change to the fourth line from the working tree.
        discard(&fx, "f.txt", &[('-', "four"), ('+', "FOUR")]);
        assert_eq!(fx.read("f.txt"), "one\nTWO\nthree\nfour\nfive\n");
        // Discarding only a removal brings the old line back next to the new.
        discard(&fx, "f.txt", &[('-', "two")]);
        assert_eq!(fx.read("f.txt"), "one\ntwo\nTWO\nthree\nfour\nfive\n");
        assert_eq!(index(&fx, "f.txt"), "one\ntwo\nthree\nfour\nfive\n");
    }

    #[test]
    fn an_added_line_can_be_staged_without_the_removal_it_replaces() {
        let fx = Fixture::new("lines-add-only");
        fx.write("f.txt", "one\ntwo\nthree\n");
        fx.commit_all("base");
        fx.write("f.txt", "one\nTWO\nthree\n");

        stage(&fx, "f.txt", &[('+', "TWO")]);
        assert_eq!(index(&fx, "f.txt"), "one\ntwo\nTWO\nthree\n");
        assert_eq!(changes(&fx.diff(Unstaged, "f.txt")), owned(&[('-', "two")]));
    }

    #[test]
    fn separate_runs_inside_one_hunk_are_applied_together() {
        let fx = Fixture::new("lines-runs");
        fx.write("f.txt", "a\nb\nc\nd\ne\nf\ng\n");
        fx.commit_all("base");
        // Three changes close enough to share a hunk: b replaced, an
        // insertion after d, f removed.
        fx.write("f.txt", "a\nB\nc\nd\nnew\ne\ng\n");
        let diff = fx.diff(Unstaged, "f.txt");
        assert_eq!(diff.hunks.len(), 1);

        // First and last run, skipping the insertion in the middle.
        stage(&fx, "f.txt", &[('-', "b"), ('+', "B"), ('-', "f")]);
        assert_eq!(index(&fx, "f.txt"), "a\nB\nc\nd\ne\ng\n");
        assert_eq!(changes(&fx.diff(Unstaged, "f.txt")), owned(&[('+', "new")]));

        // Take the middle one out of a fully staged file again.
        git(&fx.dir, &["add", "f.txt"]);
        unstage(&fx, "f.txt", &[('+', "new")]);
        assert_eq!(index(&fx, "f.txt"), "a\nB\nc\nd\ne\ng\n");

        // Discard the outer two from the working tree, keeping the middle.
        git(&fx.dir, &["reset", "-q"]);
        discard(&fx, "f.txt", &[('-', "b"), ('+', "B"), ('-', "f")]);
        assert_eq!(fx.read("f.txt"), "a\nb\nc\nd\nnew\ne\nf\ng\n");
    }

    #[test]
    fn lines_from_several_hunks_go_into_one_patch() {
        let fx = Fixture::new("lines-hunks");
        let original: String = (1..=30).map(|i| format!("line {i}\n")).collect();
        fx.write("f.txt", &original);
        fx.commit_all("base");
        let edited = original
            .replace("line 2\n", "line two\nextra\n")
            .replace("line 28\n", "line twenty-eight\n");
        fx.write("f.txt", &edited);
        assert_eq!(fx.diff(Unstaged, "f.txt").hunks.len(), 2);

        stage(&fx, "f.txt", &[('+', "extra"), ('-', "line 28"), ('+', "line twenty-eight")]);
        let expected = original
            .replace("line 2\n", "line 2\nextra\n")
            .replace("line 28\n", "line twenty-eight\n");
        assert_eq!(index(&fx, "f.txt"), expected);
        assert_eq!(
            changes(&fx.diff(Unstaged, "f.txt")),
            owned(&[('-', "line 2"), ('+', "line two")])
        );

        // The same selection in reverse, from the working tree.
        git(&fx.dir, &["reset", "-q"]);
        discard(&fx, "f.txt", &[('+', "extra"), ('-', "line 28"), ('+', "line twenty-eight")]);
        assert_eq!(fx.read("f.txt"), original.replace("line 2\n", "line two\n"));
    }

    #[test]
    fn a_missing_trailing_newline_survives_every_direction() {
        let fx = Fixture::new("lines-no-eol");
        fx.write("f.txt", "alpha\nbeta");
        fx.commit_all("base");
        fx.write("f.txt", "alpha\ngamma");
        let reset = || git(&fx.dir, &["reset", "-q"]);

        // Both lines: the same as staging the hunk.
        stage(&fx, "f.txt", &[('-', "beta"), ('+', "gamma")]);
        assert_eq!(index(&fx, "f.txt"), "alpha\ngamma");
        assert!(fx.snapshot().status.unstaged.is_empty());

        // From there, unstage the addition only: nothing replaces "beta".
        unstage(&fx, "f.txt", &[('+', "gamma")]);
        assert_eq!(index(&fx, "f.txt"), "alpha\n");
        // Or the removal only: "beta" returns, and since it is no longer the
        // last line it gets the newline it never had.
        git(&fx.dir, &["add", "f.txt"]);
        unstage(&fx, "f.txt", &[('-', "beta")]);
        assert_eq!(index(&fx, "f.txt"), "alpha\nbeta\ngamma");

        // Staging only the addition keeps "beta" in front of it.
        reset();
        stage(&fx, "f.txt", &[('+', "gamma")]);
        assert_eq!(index(&fx, "f.txt"), "alpha\nbeta\ngamma");
        // Staging only the removal.
        reset();
        stage(&fx, "f.txt", &[('-', "beta")]);
        assert_eq!(index(&fx, "f.txt"), "alpha\n");

        // Discarding from the working tree.
        reset();
        discard(&fx, "f.txt", &[('-', "beta")]);
        assert_eq!(fx.read("f.txt"), "alpha\nbeta\ngamma");
        fx.write("f.txt", "alpha\ngamma");
        discard(&fx, "f.txt", &[('+', "gamma")]);
        assert_eq!(fx.read("f.txt"), "alpha\n");
        fx.write("f.txt", "alpha\ngamma");
        discard(&fx, "f.txt", &[('-', "beta"), ('+', "gamma")]);
        assert_eq!(fx.read("f.txt"), "alpha\nbeta");
    }

    #[test]
    fn newline_only_changes_at_the_end_of_a_file_are_handled() {
        let fx = Fixture::new("lines-eol-change");

        // The file gains a final newline and a line.
        fx.write("grow.txt", "a\nb");
        // The unchanged last line has no newline on either side.
        fx.write("ctx.txt", "a\nb\nc");
        // The final newline goes away.
        fx.write("shrink.txt", "a\nb\n");
        fx.commit_all("base");
        fx.write("grow.txt", "a\nb\nc\n");
        fx.write("ctx.txt", "a\nB\nc");
        fx.write("shrink.txt", "a\nb");

        // "c" cannot follow a line that has no newline, so "b" gets one.
        stage(&fx, "grow.txt", &[('+', "c")]);
        assert_eq!(index(&fx, "grow.txt"), "a\nb\nc\n");
        assert!(fx.diff(Unstaged, "grow.txt").hunks.is_empty());

        stage(&fx, "ctx.txt", &[('+', "B")]);
        assert_eq!(index(&fx, "ctx.txt"), "a\nb\nB\nc");
        unstage(&fx, "ctx.txt", &[('+', "B")]);
        assert_eq!(index(&fx, "ctx.txt"), "a\nb\nc");
        discard(&fx, "ctx.txt", &[('-', "b")]);
        assert_eq!(fx.read("ctx.txt"), "a\nb\nB\nc");

        stage(&fx, "shrink.txt", &[('-', "b"), ('+', "b")]);
        assert_eq!(index(&fx, "shrink.txt"), "a\nb");
        unstage(&fx, "shrink.txt", &[('-', "b"), ('+', "b")]);
        assert_eq!(index(&fx, "shrink.txt"), "a\nb\n");
    }

    #[test]
    fn crlf_line_endings_are_preserved_byte_for_byte() {
        let fx = Fixture::new("lines-crlf");
        fx.write("f.txt", "one\r\ntwo\r\nthree\r\nfour\r\n");
        fx.write("tail.txt", "one\r\ntwo");
        fx.commit_all("base");
        fx.write("f.txt", "one\r\nTWO\r\nthree\r\nFOUR\r\n");
        fx.write("tail.txt", "one\r\ntwo\r\nthree");

        stage(&fx, "f.txt", &[('-', "two"), ('+', "TWO")]);
        assert_eq!(index(&fx, "f.txt"), "one\r\nTWO\r\nthree\r\nfour\r\n");
        assert_eq!(changes(&fx.diff(Unstaged, "f.txt")), owned(&[('-', "four"), ('+', "FOUR")]));

        unstage(&fx, "f.txt", &[('+', "TWO")]);
        assert_eq!(index(&fx, "f.txt"), "one\r\nthree\r\nfour\r\n");
        unstage(&fx, "f.txt", &[('-', "two")]);
        assert_eq!(index(&fx, "f.txt"), "one\r\ntwo\r\nthree\r\nfour\r\n");

        discard(&fx, "f.txt", &[('+', "FOUR")]);
        assert_eq!(fx.read("f.txt"), "one\r\nTWO\r\nthree\r\n");

        // A line that needs a terminator gets the one its neighbours use.
        stage(&fx, "tail.txt", &[('+', "three")]);
        assert_eq!(index(&fx, "tail.txt"), "one\r\ntwo\r\nthree");
    }

    #[test]
    fn new_files_can_be_staged_unstaged_and_discarded_in_part() {
        let fx = Fixture::new("lines-new-file");
        fx.write("keep.txt", "keep\n");
        fx.commit_all("base");
        fx.write("n.txt", "a\nb\nc\n");

        // Part of an untracked file: the index gets a file with just that.
        stage(&fx, "n.txt", &[('+', "b")]);
        assert_eq!(index(&fx, "n.txt"), "b\n");
        let snap = fx.snapshot();
        assert_eq!(snap.status.staged[0].kind, ChangeKind::Added);
        assert_eq!(changes(&fx.diff(Unstaged, "n.txt")), owned(&[('+', "a"), ('+', "c")]));

        // Part of a staged new file: the rest stays staged.
        git(&fx.dir, &["add", "n.txt"]);
        unstage(&fx, "n.txt", &[('+', "b")]);
        assert_eq!(index(&fx, "n.txt"), "a\nc\n");
        // All of it: the file leaves the index altogether.
        unstage(&fx, "n.txt", &[('+', "a"), ('+', "c")]);
        let snap = fx.snapshot();
        assert!(snap.status.staged.is_empty());
        assert_eq!(snap.status.unstaged[0].kind, ChangeKind::Untracked);

        // Part of an untracked file, from the working tree.
        discard(&fx, "n.txt", &[('+', "c")]);
        assert_eq!(fx.read("n.txt"), "a\nb\n");
    }

    #[test]
    fn deleted_files_can_be_staged_unstaged_and_restored_in_part() {
        let fx = Fixture::new("lines-deleted-file");
        fx.write("d.txt", "x\ny\nz\n");
        fx.commit_all("base");
        std::fs::remove_file(fx.dir.join("d.txt")).unwrap();

        // Part of a deletion: the file stays in the index, shorter.
        stage(&fx, "d.txt", &[('-', "y")]);
        assert_eq!(index(&fx, "d.txt"), "x\nz\n");

        // Part of a staged deletion: the file returns with only those lines.
        git(&fx.dir, &["rm", "-q", "--cached", "d.txt"]);
        unstage(&fx, "d.txt", &[('-', "y")]);
        assert_eq!(index(&fx, "d.txt"), "y\n");

        // Part of a deletion, restored in the working tree.
        git(&fx.dir, &["reset", "-q"]);
        discard(&fx, "d.txt", &[('-', "x"), ('-', "z")]);
        assert_eq!(fx.read("d.txt"), "x\nz\n");
    }

    #[test]
    fn lines_of_a_renamed_file_are_unstaged_without_undoing_the_rename() {
        let fx = Fixture::new("lines-rename");
        let original: String = (1..=12).map(|i| format!("line {i}\n")).collect();
        fx.write("old.txt", &original);
        fx.commit_all("base");
        git(&fx.dir, &["mv", "old.txt", "new.txt"]);
        let edited = original.replace("line 3\n", "line three\n").replace("line 10\n", "line ten\n");
        fx.write("new.txt", &edited);
        git(&fx.dir, &["add", "new.txt"]);

        // The diff as the view asks for it: with the old path, so the rename
        // is detected and the patch header says "rename from / rename to".
        let renamed = fx.snapshot().status.staged[0].clone();
        assert_eq!((renamed.kind, renamed.old_path.as_deref()), (ChangeKind::Renamed, Some("old.txt")));
        let target = DiffTarget::new(Staged, &renamed);
        fx.handle.diff(target.clone());
        let diff = loop {
            match fx.handle.rx.recv_timeout(TIMEOUT).expect("diff arrives") {
                Response::Diff(t, result) if t == target => break result.expect("diff ok"),
                _ => {}
            }
        };
        assert!(diff.file_header.windows(11).any(|w| w == b"rename from"));

        let patch = diff.lines_patch(&pick(&diff, &[('-', "line 10"), ('+', "line ten")]), Reverse).unwrap();
        fx.run(ops::apply_patch("Unstage lines", patch, true, true));
        assert_eq!(index(&fx, "new.txt"), original.replace("line 3\n", "line three\n"));
        // Still renamed in the index, and untouched in the working tree.
        assert_eq!(git(&fx.dir, &["ls-files"]), "new.txt");
        assert_eq!(fx.read("new.txt"), edited);
    }

    #[test]
    fn awkward_file_names_survive_a_rewritten_header() {
        let fx = Fixture::new("lines-names");
        fx.write("keep.txt", "keep\n");
        fx.commit_all("base");
        // New files get their header rewritten when staged in part.
        let name = "dir with space/naïve [draft].txt";
        fx.write(name, "a\nb\nc\n");

        stage(&fx, name, &[('+', "b")]);
        assert_eq!(index(&fx, name), "b\n");
        discard(&fx, name, &[('+', "c")]);
        assert_eq!(fx.read(name), "a\nb\n");
        git(&fx.dir, &["add", "-A"]);
        unstage(&fx, name, &[('+', "a")]);
        assert_eq!(index(&fx, name), "b\n");
    }

    #[test]
    fn utf16_files_with_a_byte_order_mark_get_a_text_diff() {
        let fx = Fixture::new("diff-utf16");
        let utf16 = |text: &str| FileEncoding { encoding: Encoding::Utf16Le, bom: true }.encode(text);
        std::fs::write(fx.dir.join("notes.txt"), utf16("caf\u{e9}\r\nsecond\r\nthird\r\n")).unwrap();
        fx.commit_all("notes");
        std::fs::write(fx.dir.join("notes.txt"), utf16("caf\u{e9}\r\nsecond, changed\r\nthird\r\n")).unwrap();

        let target = DiffTarget { source: Unstaged, path: "notes.txt".to_owned(), old_path: None };
        let diff = fx.job(move |loader| loader.diff(&target)).unwrap();
        assert!(!diff.binary);
        assert_eq!(changes(&diff), owned(&[('-', "second"), ('+', "second, changed")]));
        assert_eq!(diff.hunks[0].lines[0].text, "caf\u{e9}");
        assert_eq!((diff.additions, diff.deletions), (1, 1));
        // Its parts are applied in the file's own encoding (see `git::utf16`).
        assert!(diff.transcoded.is_some() && diff.hunk_patch(0).is_some());
        assert_eq!(diff.hunks[0].sources(diff.format.encoding).unwrap()[1], "second");

        // Committed, the same.
        fx.commit_all("change");
        let head = fx.job(|loader| loader.repo().head().unwrap().target().unwrap());
        let target = DiffTarget { source: Commit(head), path: "notes.txt".to_owned(), old_path: None };
        let diff = fx.job(move |loader| loader.diff(&target)).unwrap();
        assert_eq!(changes(&diff), owned(&[('-', "second"), ('+', "second, changed")]));

        // Real binary data is still binary.
        std::fs::write(fx.dir.join("data.bin"), b"\x00\x01\x02 not text").unwrap();
        let target = DiffTarget { source: Unstaged, path: "data.bin".to_owned(), old_path: None };
        assert!(fx.job(move |loader| loader.diff(&target)).unwrap().binary);
    }

    #[test]
    fn image_files_come_with_both_versions_decoded() {
        use crate::git::picture::Side;
        use crate::git::picture::tests::png;
        let fx = Fixture::new("diff-images");
        let put = |path: &str, bytes: Vec<u8>| std::fs::write(fx.dir.join(path), bytes).unwrap();
        put("logo.png", png(4, 4, [255, 0, 0, 255]));
        put("gone.png", png(2, 2, [0, 0, 0, 255]));
        fx.write("b.bin", "a b");
        fx.commit_all("base");
        put("logo.png", png(8, 4, [0, 255, 0, 255]));
        std::fs::remove_file(fx.dir.join("gone.png")).unwrap();
        fx.write("b.bin", "a c");

        let size = |side: &Option<Side>| {
            side.as_ref().map(|side| side.picture.as_ref().map(|p| (p.width, p.height)).unwrap())
        };
        let sizes = |diff: &FileDiff| {
            let images = diff.images.as_ref().expect("an image preview");
            (size(&images.old), size(&images.new))
        };

        // Unstaged: the index's version against the file on disk.
        assert_eq!(sizes(&fx.diff(Unstaged, "logo.png")), (Some((4, 4)), Some((8, 4))));
        // A deletion has only the old side.
        assert_eq!(sizes(&fx.diff(Unstaged, "gone.png")), (Some((2, 2)), None));
        // Binary but not an image: no preview.
        assert!(fx.diff(Unstaged, "b.bin").images.is_none());

        // Staged: HEAD against the index.
        git(&fx.dir, &["add", "-A"]);
        assert_eq!(sizes(&fx.diff(Staged, "logo.png")), (Some((4, 4)), Some((8, 4))));

        // Committed: the parent against the commit, and the first commit's
        // image is an addition.
        git(&fx.dir, &["commit", "-q", "-m", "green"]);
        let oid = |rev: &str| crate::git::Oid::from_str(git(&fx.dir, &["rev-parse", rev]).trim()).unwrap();
        assert_eq!(sizes(&fx.diff(Commit(oid("HEAD")), "logo.png")), (Some((4, 4)), Some((8, 4))));
        assert_eq!(sizes(&fx.diff(Commit(oid("HEAD~1")), "logo.png")), (None, Some((4, 4))));
        assert_eq!(sizes(&fx.diff(Commit(oid("HEAD")), "gone.png")), (Some((2, 2)), None));
    }

    #[test]
    fn selections_that_change_nothing_give_no_patch() {
        let fx = Fixture::new("lines-none");
        fx.write("f.txt", "one\ntwo\n");
        fx.write("b.bin", "a\0b");
        fx.commit_all("base");
        fx.write("f.txt", "one\nTWO\n");
        fx.write("b.bin", "a\0c");

        let diff = fx.diff(Unstaged, "f.txt");
        assert!(diff.lines_patch(&[], Forward).is_none());
        // A context line, a line past the end and a hunk that does not exist.
        assert!(diff.lines_patch(&[(0, 0)], Forward).is_none());
        assert!(diff.lines_patch(&[(0, 99)], Reverse).is_none());
        assert!(diff.lines_patch(&[(7, 0)], Forward).is_none());
        // Selecting everything is the hunk itself.
        let all = pick(&diff, &[('-', "two"), ('+', "TWO")]);
        fx.run(ops::apply_patch("Stage lines", diff.lines_patch(&all, Forward).unwrap(), true, false));
        assert!(fx.snapshot().status.unstaged.iter().all(|f| f.path != "f.txt"));

        let binary = fx.diff(Unstaged, "b.bin");
        assert!(binary.binary);
        assert!(binary.lines_patch(&[(0, 0)], Forward).is_none());
    }

    /// Feeds a patch to `git apply --cached`, without the worker in between.
    fn apply_cached(dir: &Path, patch: &[u8], reverse: bool) {
        let mut cmd = Command::new("git");
        cmd.current_dir(dir).args(["apply", "--cached", "--whitespace=nowarn"]);
        if reverse {
            cmd.arg("--reverse");
        }
        let mut child = cmd.arg("-").stdin(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(patch).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "git apply failed: {}\n{}",
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(patch)
        );
    }

    /// Every possible selection of a hunk, checked against a model of what
    /// the selection means: going forward the old file keeps its unselected
    /// removals and gains the selected additions; in reverse the new file
    /// keeps its unselected additions and regains the selected removals.
    #[test]
    fn every_selection_of_a_hunk_applies_and_means_what_it_says() {
        let fx = Fixture::new("lines-exhaustive");
        for (eol, old, new) in [
            ("\n", "a b c d e", "a X c Y e"),
            ("\n", "a b c", "p a q c r"),
            ("\r\n", "a b c d", "b Z d w"),
        ] {
            let text = |words: &str| words.split(' ').map(|w| format!("{w}{eol}")).collect::<String>();
            fx.write("f.txt", &text(old));
            fx.commit_all("base");
            fx.write("f.txt", &text(new));
            git(&fx.dir, &["add", "f.txt"]);

            // The file is small enough for one hunk to cover all of it, so
            // the hunk alone says what both sides are. HEAD -> index here is
            // the same diff as index -> working tree would be before staging.
            let diff = fx.diff(Staged, "f.txt");
            assert_eq!(diff.hunks.len(), 1);
            let lines = &diff.hunks[0].lines;
            let changed: Vec<usize> = (0..lines.len()).filter(|i| lines[*i].origin != ' ').collect();

            for mask in 1u32..(1 << changed.len()) {
                let picked: Vec<usize> =
                    changed.iter().enumerate().filter(|(bit, _)| mask >> bit & 1 == 1).map(|(_, i)| *i).collect();
                let selection: Vec<(usize, usize)> = picked.iter().map(|i| (0, *i)).collect();
                let model = |kept: char, gained: char| -> String {
                    lines
                        .iter()
                        .enumerate()
                        .filter(|(i, l)| {
                            l.origin == ' '
                                || (l.origin == kept && !picked.contains(i))
                                || (l.origin == gained && picked.contains(i))
                        })
                        .map(|(_, l)| format!("{}{eol}", l.text))
                        .collect()
                };

                // Stage into an index that is still at HEAD.
                git(&fx.dir, &["reset", "-q"]);
                apply_cached(&fx.dir, &diff.lines_patch(&selection, Forward).unwrap(), false);
                assert_eq!(show(&fx.dir, ":f.txt"), model('-', '+'), "stage {picked:?} of {old:?} -> {new:?}");

                // Unstage from an index that has everything.
                git(&fx.dir, &["add", "f.txt"]);
                apply_cached(&fx.dir, &diff.lines_patch(&selection, Reverse).unwrap(), true);
                assert_eq!(show(&fx.dir, ":f.txt"), model('+', '-'), "unstage {picked:?} of {old:?} -> {new:?}");
            }
            git(&fx.dir, &["add", "f.txt"]);
            fx.commit_all("next");
        }
    }

    // ---- modes ------------------------------------------------------------------

    use super::Mode;

    const WHOLE: Mode = Mode { whole_file: true, ignore_whitespace: false };
    const NO_SPACE: Mode = Mode { whole_file: false, ignore_whitespace: true };

    fn diff_in(fx: &Fixture, source: crate::git::DiffSource, path: &str, mode: Mode) -> FileDiff {
        let target = DiffTarget { source, path: path.to_owned(), old_path: None };
        fx.job(move |loader| loader.diff_with(&target, mode)).expect("diff ok")
    }

    /// One side of a diff as text: the old file (`'-'`) or the new (`'+'`).
    fn side(diff: &FileDiff, keep: char) -> String {
        diff.hunks
            .iter()
            .flat_map(|h| h.lines.iter())
            .filter(|l| l.origin == ' ' || l.origin == keep)
            .map(|l| format!("{}\n", l.text))
            .collect()
    }

    fn numbered(lines: std::ops::RangeInclusive<usize>) -> String {
        lines.map(|i| format!("line {i}\n")).collect()
    }

    fn head(fx: &Fixture) -> crate::git::Oid {
        crate::git::Oid::from_str(&git(&fx.dir, &["rev-parse", "HEAD"])).unwrap()
    }

    #[test]
    fn the_whole_file_comes_with_its_hunks_unchanged_between_the_gaps() {
        let fx = Fixture::new("diff-whole-file");
        let original = numbered(1..=40);
        fx.write("f.txt", &original);
        fx.commit_all("base");
        let edited = original.replace("line 6\n", "line six\n").replace("line 30\n", "line 30\nadded\n");
        fx.write("f.txt", &edited);

        let hunks = diff_in(&fx, Unstaged, "f.txt", Mode::default());
        let whole = diff_in(&fx, Unstaged, "f.txt", WHOLE);
        assert_eq!(hunks.hunks.len(), 2);
        assert_eq!((whole.mode, whole.too_long_for_whole_file), (WHOLE, false));
        // Gap, hunk, gap, hunk, gap: every line of both versions, in order.
        let gaps: Vec<bool> = whole.hunks.iter().map(|h| h.gap).collect();
        assert_eq!(gaps, [true, false, true, false, true]);
        assert_eq!(side(&whole, '-'), original);
        assert_eq!(side(&whole, '+'), edited);
        assert_eq!(changes(&whole), changes(&hunks));
        assert_eq!((whole.additions, whole.deletions), (2, 1));
        // Line numbers run on through the gaps, shifted after an insertion.
        let last = whole.hunks.last().unwrap().lines.last().unwrap();
        assert_eq!((last.old_no, last.new_no, last.text.as_str()), (Some(40), Some(41), "line 40"));

        // The hunks are those of the usual diff, byte for byte, so either can
        // be staged on its own; the gaps have nothing to stage.
        let real: Vec<&Vec<u8>> = whole.hunks.iter().filter(|h| !h.gap).map(|h| &h.raw).collect();
        assert_eq!(real, hunks.hunks.iter().map(|h| &h.raw).collect::<Vec<_>>());
        assert!(whole.hunk_patch(0).is_none() && whole.hunk_patch(2).is_none());
        assert!(whole.lines_patch(&[(0, 0), (2, 1)], Forward).is_none());
        fx.run(ops::apply_patch("Stage hunk", whole.hunk_patch(3).unwrap(), true, false));
        assert_eq!(index(&fx, "f.txt"), original.replace("line 30\n", "line 30\nadded\n"));
        // Lines picked in the whole file stage as they do in the hunks.
        let whole = diff_in(&fx, Unstaged, "f.txt", WHOLE);
        let picked = pick(&whole, &[('+', "line six")]);
        fx.run(ops::apply_patch("Stage lines", whole.lines_patch(&picked, Forward).unwrap(), true, false));
        assert_eq!(
            index(&fx, "f.txt"),
            original.replace("line 6\n", "line 6\nline six\n").replace("line 30\n", "line 30\nadded\n")
        );

        // A gap's lines copy as they are in the file, the last one without
        // the newline it lacks.
        fx.write("tail.txt", "a\n\tb\nc\nd\ne\nf\ng\nh");
        fx.commit_all("tail");
        fx.write("tail.txt", "A\n\tb\nc\nd\ne\nf\ng\nh");
        let whole = diff_in(&fx, Unstaged, "tail.txt", WHOLE);
        let gap = whole.hunks.last().unwrap();
        assert!(gap.gap);
        assert_eq!(gap.sources(Encoding::Utf8).unwrap(), ["e", "f", "g", "h"]);
        assert_eq!(whole.hunks[0].sources(Encoding::Utf8).unwrap()[2], "\tb");

        // Committed diffs and new files: a new file is one hunk already.
        fx.commit_all("more");
        let committed = diff_in(&fx, Commit(head(&fx)), "tail.txt", WHOLE);
        assert_eq!(side(&committed, '+'), "A\n    b\nc\nd\ne\nf\ng\nh\n");
        fx.write("new.txt", "x\ny\n");
        let new = diff_in(&fx, Unstaged, "new.txt", WHOLE);
        assert_eq!((new.hunks.len(), new.hunks[0].gap), (1, false));
        // A file whose hunk takes in every line has no gaps.
        fx.write("small.txt", "1\n2\n3\n");
        fx.commit_all("small");
        fx.write("small.txt", "1\nTWO\n3\n");
        assert_eq!(diff_in(&fx, Unstaged, "small.txt", WHOLE).hunks.len(), 1);
    }

    #[test]
    fn a_file_too_long_to_show_whole_keeps_its_hunks() {
        let fx = Fixture::new("diff-whole-too-long");
        let original = numbered(1..=super::MAX_LINES + 10);
        fx.write("big.txt", &original);
        fx.commit_all("base");
        fx.write("big.txt", &original.replace("line 20\n", "line twenty\n"));
        let diff = diff_in(&fx, Unstaged, "big.txt", WHOLE);
        assert!(diff.too_long_for_whole_file && !diff.truncated);
        assert_eq!(diff.hunks.len(), 1);
        assert!(!diff.hunks[0].gap);
        assert_eq!(changes(&diff), owned(&[('-', "line 20"), ('+', "line twenty")]));
    }

    #[test]
    fn utf16_files_can_be_shown_whole() {
        let fx = Fixture::new("diff-whole-utf16");
        let utf16 = |text: &str| FileEncoding { encoding: Encoding::Utf16Le, bom: true }.encode(text);
        let original = numbered(1..=12);
        std::fs::write(fx.dir.join("notes.txt"), utf16(&original)).unwrap();
        fx.commit_all("notes");
        std::fs::write(fx.dir.join("notes.txt"), utf16(&original.replace("line 9\n", "line nine\n"))).unwrap();
        let diff = diff_in(&fx, Unstaged, "notes.txt", WHOLE);
        assert!(!diff.binary);
        assert_eq!(diff.hunks.iter().map(|h| h.gap).collect::<Vec<_>>(), [true, false]);
        assert_eq!(side(&diff, '-'), original);
    }

    #[test]
    fn ignoring_whitespace_hides_indentation_and_shows_real_changes() {
        let fx = Fixture::new("diff-ignore-space");
        let original = "fn main() {\nfoo();\nbar();\n}\n".to_owned() + &numbered(1..=20);
        fx.write("src file.rs", &original);
        fx.write("eol.txt", "a\r\nb\r\n");
        fx.commit_all("base");
        // Indented, and one real change far enough away for a hunk of its own.
        let edited = original
            .replace("foo();\nbar();\n", "    foo();\n\tbar();  \n")
            .replace("line 18\n", "line eighteen\n");
        fx.write("src file.rs", &edited);
        fx.write("eol.txt", "a\nb\n");

        let all = diff_in(&fx, Unstaged, "src file.rs", Mode::default());
        assert_eq!(all.hunks.len(), 2);
        let ignored = diff_in(&fx, Unstaged, "src file.rs", NO_SPACE);
        assert_eq!(ignored.mode, NO_SPACE);
        assert_eq!(changes(&ignored), owned(&[('-', "line 18"), ('+', "line eighteen")]));
        assert_eq!((ignored.additions, ignored.deletions, ignored.only_whitespace), (1, 1, false));

        // With the whole file: the indented lines are context now.
        let whole = diff_in(&fx, Unstaged, "src file.rs", Mode { whole_file: true, ignore_whitespace: true });
        assert_eq!(changes(&whole), changes(&ignored));
        assert_eq!(whole.hunks.iter().flat_map(|h| &h.lines).count(), 25);

        // Only whitespace changed: nothing to show, and the diff says why.
        let eol = diff_in(&fx, Unstaged, "eol.txt", NO_SPACE);
        assert!(eol.hunks.is_empty() && eol.only_whitespace);
        assert_eq!(eol.kind, Some(ChangeKind::Modified));
        assert!(!diff_in(&fx, Unstaged, "eol.txt", Mode::default()).only_whitespace);

        // Staged and committed diffs ignore it the same way.
        git(&fx.dir, &["add", "-A"]);
        assert_eq!(changes(&diff_in(&fx, Staged, "src file.rs", NO_SPACE)), changes(&ignored));
        fx.commit_all("indent");
        assert_eq!(changes(&diff_in(&fx, Commit(head(&fx)), "src file.rs", NO_SPACE)), changes(&ignored));
        assert!(diff_in(&fx, Commit(head(&fx)), "eol.txt", NO_SPACE).only_whitespace);
    }

    #[test]
    fn the_answer_carries_the_mode_it_was_asked_for() {
        let fx = Fixture::new("diff-mode-request");
        fx.write("f.txt", "one\ntwo\n");
        fx.commit_all("base");
        fx.write("f.txt", "one\n  two\n");
        let target = DiffTarget { source: Unstaged, path: "f.txt".to_owned(), old_path: None };
        for mode in [NO_SPACE, Mode::default(), WHOLE] {
            fx.handle.diff_with(target.clone(), mode);
            let diff = loop {
                match fx.handle.rx.recv_timeout(TIMEOUT).expect("diff arrives") {
                    Response::Diff(t, result) if t == target => break result.expect("diff ok"),
                    _ => {}
                }
            };
            assert_eq!(diff.mode, mode);
            assert_eq!(diff.hunks.is_empty(), mode.ignore_whitespace);
        }
    }

    #[test]
    fn gaps_are_only_filled_when_the_hunks_line_up() {
        use crate::git::{DiffLine, Hunk};
        let line = |origin: char, old: u32, new: u32, text: &str| DiffLine {
            origin,
            old_no: (origin != '+').then_some(old),
            new_no: (origin != '-').then_some(new),
            text: text.to_owned(),
        };
        let raw = |lines: &[DiffLine]| {
            let mut raw = b"@@ -1 +1 @@\n".to_vec();
            for l in lines {
                raw.push(l.origin as u8);
                raw.extend_from_slice(l.text.as_bytes());
                raw.push(b'\n');
            }
            raw
        };
        let lines = vec![line(' ', 1, 1, "a"), line('-', 2, 0, "b"), line('+', 0, 2, "B"), line(' ', 3, 3, "c")];
        let all = Hunk { raw: raw(&lines), lines: lines.clone(), ..Hunk::default() };
        let hunk = |lines: &[DiffLine]| Hunk { header: "@@".to_owned(), raw: raw(lines), lines: lines.to_vec(), gap: false };

        let mut hunks = vec![hunk(&lines[1..3])];
        assert!(super::with_gaps(&mut hunks, &all));
        let shape: Vec<(bool, usize)> = hunks.iter().map(|h| (h.gap, h.lines.len())).collect();
        assert_eq!(shape, [(true, 1), (false, 2), (true, 1)]);
        assert_eq!(hunks[2].sources(Encoding::Utf8).unwrap(), ["c"]);

        // A hunk whose lines are not among them leaves the hunks alone.
        let mut stray = vec![hunk(&[line('+', 0, 2, "X")])];
        assert!(!super::with_gaps(&mut stray, &all));
        assert_eq!(stray.len(), 1);
        assert!(!stray[0].gap);
    }

    #[test]
    fn a_diff_of_the_hunks_comes_with_every_line_of_a_small_file() {
        let fx = Fixture::new("diff-whole-for-colours");
        let original = "/* a comment\n".to_owned() + &numbered(1..=20) + "*/\n";
        fx.write("lib.rs", &original);
        fx.commit_all("lib");
        fx.write("lib.rs", &original.replace("line 15\n", "line fifteen\n"));
        let diff = diff_in(&fx, Unstaged, "lib.rs", Mode::default());
        assert_eq!(diff.hunks.len(), 1);
        let whole = diff.whole.as_ref().expect("every line comes with it");
        let lines: Vec<(char, &str)> = whole.lines.iter().map(|l| (l.origin, l.text.as_str())).collect();
        assert_eq!(lines.len(), 23);
        assert_eq!(lines[0], (' ', "/* a comment"));
        assert!(lines.contains(&('-', "line 15")) && lines.contains(&('+', "line fifteen")));
        // A whole-file diff has them all already.
        assert!(diff_in(&fx, Unstaged, "lib.rs", WHOLE).whole.is_none());

        // Too large: hunk by hunk.
        let big = numbered(1..=60_000);
        fx.write("big.txt", &big);
        fx.commit_all("big");
        fx.write("big.txt", &big.replace("line 7\n", "line seven\n"));
        assert!(diff_in(&fx, Unstaged, "big.txt", Mode::default()).whole.is_none());
    }

    #[test]
    fn svg_changes_come_drawn_unless_they_do_not_parse() {
        let fx = Fixture::new("diff-svg");
        let svg = |fill: &str| {
            format!("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"4\" height=\"4\">\n<rect width=\"4\" height=\"4\" fill=\"{fill}\"/>\n</svg>\n")
        };
        fx.write("art/logo one.svg", &svg("red"));
        fx.commit_all("logo");
        fx.write("art/logo one.svg", &svg("blue"));
        let diff = diff_in(&fx, Unstaged, "art/logo one.svg", Mode::default());
        assert!(!diff.binary && !diff.file_header.is_empty(), "still a text diff, stageable");
        let images = diff.images.as_ref().expect("drawn");
        let red = &images.old.as_ref().unwrap().picture.as_ref().unwrap().rgba[..4];
        let blue = &images.new.as_ref().unwrap().picture.as_ref().unwrap().rgba[..4];
        assert_eq!((red, blue), (&[255, 0, 0, 255][..], &[0, 0, 255, 255][..]));

        // Half written: the text alone.
        fx.write("art/logo one.svg", "<svg xmlns=\"http://www.w3.org/2000/svg\">\n<rect\n");
        let broken = diff_in(&fx, Unstaged, "art/logo one.svg", Mode::default());
        assert!(broken.images.is_none() && !broken.hunks.is_empty());

        // Committed, and added: one picture.
        fx.write("art/logo one.svg", &svg("blue"));
        fx.write("new.svg", &svg("green"));
        fx.commit_all("blue");
        let added = diff_in(&fx, Commit(head(&fx)), "new.svg", Mode::default());
        let images = added.images.as_ref().expect("drawn");
        assert!(images.old.is_none() && images.new.is_some());
    }
}
