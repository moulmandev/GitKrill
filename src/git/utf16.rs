//! UTF-16 files as text: staging, unstaging and discarding parts of them,
//! and blaming them.
//!
//! Git takes UTF-16 for binary (every other byte of Latin text is zero), so
//! the diff view decodes both versions and diffs the text instead
//! (`Loader::utf16_diff`). Such a diff is of UTF-8 lines, which `git apply`
//! cannot apply to the file's own bytes. Its hunks and lines are applied
//! here instead: the selected changes are made to the decoded text of the
//! side being changed, which is encoded back the way that file was (UTF-16
//! LE or BE, its byte order mark, its line endings, all kept) and written
//! to the index (`git hash-object -w`, `git update-index`) or the working
//! tree. Each refuses when the file is no longer what the diff was made
//! from. As for every other file, these have no undo.
//!
//! `git blame` reads a UTF-16 file as bytes and cuts it into "lines" at
//! every byte 0x0A, which is half of a UTF-16 newline and sometimes half of
//! another character (U+4E0A 上, U+300A 《). [`blame_lines`] puts the bytes
//! back together, decodes them, and gives each real line the commit that
//! last changed any of its pieces.

use std::borrow::Cow;
use std::path::Path;
use std::sync::Arc;

use super::blame::BlameCommit;
use super::internal::Internal;
use super::ops::{self, GitCmd, Step};
use super::text::FileEncoding;
use super::{FileDiff, Oid, cli};

/// One version of a file whose diff was made from its decoded text.
#[derive(Debug)]
pub struct Side {
    /// The file's text, without its byte order mark, line endings kept.
    pub text: String,
    /// How it was read, and so how it is written.
    pub encoding: FileEncoding,
    /// Its blob id, the hash of its bytes; `None` when this side has no file
    /// (an untracked or added file's old side, a deleted file's new side).
    pub blob: Option<Oid>,
    /// It had no bytes at all (not even a byte order mark).
    pub empty: bool,
}

impl Side {
    /// A version of a file as read: its bytes (`None` when there is no such
    /// file), their text and how they were read.
    pub fn new(bytes: Option<&[u8]>, text: String, encoding: FileEncoding) -> Self {
        let blob = bytes.and_then(|bytes| Oid::hash_object(git2::ObjectType::Blob, bytes).ok());
        Side { text, encoding, blob, empty: bytes.is_none_or(<[u8]>::is_empty) }
    }

    /// The text written back reproduces the bytes exactly.
    fn round_trips(&self, bytes: &[u8]) -> bool {
        bytes.is_empty() || self.encoding.encode(&self.text) == bytes
    }
}

/// Both versions of a file whose diff is of decoded text, kept with the diff
/// (`FileDiff::transcoded`) so that its hunks and lines can be applied.
#[derive(Debug)]
pub struct Sides {
    /// Where the file is, relative to the working directory.
    pub path: String,
    pub old: Side,
    pub new: Side,
}

impl Sides {
    /// Both versions as read (`old_bytes` / `new_bytes` with their text in
    /// `old` / `new`). `None` when parts of the diff could not be written
    /// back faithfully: a side whose text does not encode back to its bytes
    /// (broken UTF-16), or two non-empty sides read in different encodings.
    pub fn new(path: &str, old: Side, old_bytes: &[u8], new: Side, new_bytes: &[u8]) -> Option<Self> {
        let same = old.empty || new.empty || old.encoding == new.encoding;
        (same && old.round_trips(old_bytes) && new.round_trips(new_bytes)).then(|| Sides { path: path.to_owned(), old, new })
    }

    /// The header of the UTF-8 patches built from the diff (`hunk_patch`,
    /// `lines_patch`). It is never given to git; it only says whether the
    /// file is created or deleted, as a real one would.
    pub fn header(&self) -> Vec<u8> {
        let path = &self.path;
        let old = if self.old.blob.is_some() { format!("a/{path}") } else { "/dev/null".to_owned() };
        let new = if self.new.blob.is_some() { format!("b/{path}") } else { "/dev/null".to_owned() };
        format!("diff --git a/{path} b/{path}\n--- {old}\n+++ {new}\n").into_bytes()
    }

    /// What applying `patch` (one built from the diff) to the index
    /// (`cached`) or the working tree, in reverse or not, does: the text of
    /// the side it is applied to with the patch's changes made, as bytes in
    /// that file's encoding, or `None` when the file goes away.
    fn result(&self, patch: &[u8], reverse: bool) -> Result<Option<Vec<u8>>, String> {
        let (base, other) = if reverse { (&self.new, &self.old) } else { (&self.old, &self.new) };
        // A creation applied in reverse, or a deletion applied forwards,
        // takes the file away, as `git apply` does with such a patch.
        let header_line = |line: &[u8]| patch.split(|b| *b == b'\n').take_while(|l| !l.starts_with(b"@@")).any(|l| l == line);
        let removes = if reverse { header_line(b"--- /dev/null") } else { header_line(b"+++ /dev/null") };
        let text = apply_text(&base.text, patch, reverse)?;
        if removes && text.is_empty() {
            return Ok(None);
        }
        // Everything applied: the file becomes the other side, byte for byte.
        if text == other.text && other.blob.is_some() {
            return Ok(Some(if other.empty { Vec::new() } else { other.encoding.encode(&other.text) }));
        }
        let encoding = if base.empty { other.encoding } else { base.encoding };
        Ok(Some(encoding.encode(&text)))
    }

    /// The command that applies `patch` the way `git apply [--cached]
    /// [--reverse]` would apply a patch of the file's bytes.
    pub fn apply(self: &Arc<Self>, label: &str, patch: Vec<u8>, cached: bool, reverse: bool) -> GitCmd {
        let sides = self.clone();
        let mut cmd = GitCmd::empty(label).quiet();
        cmd.steps.push(Step::internal(Internal::new(format!("{label} of {}", self.path), move |workdir| {
            let base = if reverse { &sides.new } else { &sides.old };
            let content = sides.result(&patch, reverse)?;
            if cached {
                write_index(workdir, &sides.path, base.blob, content)
            } else {
                write_file(workdir, &sides.path, base.blob, content)
            }
        })));
        cmd
    }
}

impl FileDiff {
    /// The command that applies `patch` (made with `hunk_patch` or
    /// `lines_patch` from this diff) to the index (`cached`) or the working
    /// tree, in reverse or not: `git apply`, or for a diff of decoded text
    /// (UTF-16 files) GitKrill's own writing of the result.
    pub fn apply_part(&self, label: &str, patch: Vec<u8>, cached: bool, reverse: bool) -> GitCmd {
        match &self.transcoded {
            Some(sides) => sides.apply(label, patch, cached, reverse),
            None => ops::apply_patch(label, patch, cached, reverse),
        }
    }
}

fn changed_since(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    format!("{name} has changed since the diff was shown. Look at it again and select the lines anew.")
}

/// Puts `content` in the index as `path` (or takes `path` out, for `None`),
/// refusing unless the index has the blob `expect` there now.
fn write_index(workdir: &Path, path: &str, expect: Option<Oid>, content: Option<Vec<u8>>) -> Result<String, String> {
    let entry = {
        let repo = git2::Repository::open(workdir).map_err(|e| e.message().to_owned())?;
        let index = repo.index().map_err(|e| e.message().to_owned())?;
        index.get_path(Path::new(path), 0)
    };
    if entry.as_ref().map(|entry| entry.id) != expect {
        return Err(changed_since(path));
    }
    let git = |args: &[&str], stdin: Option<&[u8]>| {
        let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        cli::run(workdir, &args, cli::Options { stdin, ..cli::Options::default() })
    };
    let Some(bytes) = content else {
        return git(&["update-index", "--force-remove", "--", path], None).map(|_| String::new());
    };
    // `--stdin` without `--path` stores the bytes as they are, no filters.
    let written = git(&["hash-object", "-w", "--stdin"], Some(&bytes))?;
    let id = written.lines().next().unwrap_or("").trim();
    if Oid::from_str(id).is_err() {
        return Err(format!("git hash-object answered {written:?}"));
    }
    let mode = entry.map_or_else(|| new_file_mode(&workdir.join(path)), |entry| entry.mode);
    let info = format!("{mode:o} {id}\t{path}\0");
    git(&["update-index", "-z", "--index-info"], Some(info.as_bytes())).map(|_| String::new())
}

/// The index mode of a file that has no index entry yet.
fn new_file_mode(file: &Path) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::metadata(file).is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0) {
            return 0o100755;
        }
    }
    #[cfg(not(unix))]
    let _ = file;
    0o100644
}

/// Makes the working tree file `path` hold `content` (or removes it, for
/// `None`), refusing unless it holds the blob `expect` now.
fn write_file(workdir: &Path, path: &str, expect: Option<Oid>, content: Option<Vec<u8>>) -> Result<String, String> {
    let file = workdir.join(path);
    let now = std::fs::read(&file).ok().and_then(|bytes| Oid::hash_object(git2::ObjectType::Blob, &bytes).ok());
    if now != expect {
        return Err(changed_since(path));
    }
    let done = match content {
        Some(bytes) => std::fs::write(&file, bytes),
        None => std::fs::remove_file(&file),
    };
    done.map(|()| String::new()).map_err(|e| format!("Could not write {}: {e}", file.display()))
}

// ---- applying a patch to text -----------------------------------------------------

/// One line of a patch: its text without the line ending's `\n`, and
/// whether it has one.
type Line<'a> = (&'a [u8], bool);

/// A hunk of a patch, as the lines git matches and the lines it writes.
struct Hunk<'a> {
    /// Where the matched lines start (1-based; for none, the line before).
    start: usize,
    base: Vec<Line<'a>>,
    target: Vec<Line<'a>>,
}

fn hunks(patch: &[u8], reverse: bool) -> Result<Vec<Hunk<'_>>, String> {
    let bad = || "The patch could not be read.".to_owned();
    let mut out: Vec<Hunk> = Vec::new();
    // Which list the last line went into, for a "\ No newline" after it.
    let mut last: Option<(bool, bool)> = None;
    for piece in patch.split_inclusive(|b| *b == b'\n') {
        let text = piece.strip_suffix(b"\n").unwrap_or(piece);
        if let Some(header) = text.strip_prefix(b"@@ -") {
            let header = std::str::from_utf8(header).map_err(|_| bad())?;
            let mut parts = header.split(' ');
            let range = |part: Option<&str>| -> Option<(usize, usize)> {
                let mut numbers = part?.split(',');
                let start = numbers.next()?.parse().ok()?;
                let count = numbers.next().map_or(Some(1), |n| n.parse().ok())?;
                Some((start, count))
            };
            let old = range(parts.next()).ok_or_else(bad)?;
            let new = range(parts.next().and_then(|p| p.strip_prefix('+'))).ok_or_else(bad)?;
            let (start, count) = if reverse { new } else { old };
            out.push(Hunk { start: if count == 0 { start + 1 } else { start }, base: Vec::new(), target: Vec::new() });
            last = None;
            continue;
        }
        let Some(hunk) = out.last_mut() else { continue };
        let (in_base, in_target) = match (text.first(), reverse) {
            (Some(b' '), _) => (true, true),
            (Some(b'-'), false) | (Some(b'+'), true) => (true, false),
            (Some(b'+'), false) | (Some(b'-'), true) => (false, true),
            (Some(b'\\'), _) => {
                // The line before it has no line ending.
                if let Some((in_base, in_target)) = last {
                    if in_base && let Some(line) = hunk.base.last_mut() {
                        line.1 = false;
                    }
                    if in_target && let Some(line) = hunk.target.last_mut() {
                        line.1 = false;
                    }
                }
                continue;
            }
            _ => return Err(bad()),
        };
        let line = (&text[1..], true);
        if in_base {
            hunk.base.push(line);
        }
        if in_target {
            hunk.target.push(line);
        }
        last = Some((in_base, in_target));
    }
    Ok(out)
}

/// `base` with the changes of `patch` made (in reverse: from the new side
/// back to the old one), as `git apply` makes them to a file. Fails when the
/// lines the patch expects are not where it says.
pub fn apply_text(base: &str, patch: &[u8], reverse: bool) -> Result<String, String> {
    let lines: Vec<Line> = base
        .as_bytes()
        .split_inclusive(|b| *b == b'\n')
        .map(|piece| match piece.strip_suffix(b"\n") {
            Some(body) => (body, true),
            None => (piece, false),
        })
        .collect();
    let mismatch = || "The file does not match the diff any more. Look at it again and select the lines anew.".to_owned();
    let mut out: Vec<u8> = Vec::with_capacity(base.len() + 64);
    let push = |out: &mut Vec<u8>, (body, eol): Line| {
        out.extend_from_slice(body);
        if eol {
            out.push(b'\n');
        }
    };
    let mut at = 0;
    for hunk in hunks(patch, reverse)? {
        let first = hunk.start.checked_sub(1).ok_or_else(mismatch)?;
        if first < at || first + hunk.base.len() > lines.len() || lines[first..first + hunk.base.len()] != hunk.base[..] {
            return Err(mismatch());
        }
        for line in &lines[at..first] {
            push(&mut out, *line);
        }
        for line in &hunk.target {
            push(&mut out, *line);
        }
        at = first + hunk.base.len();
    }
    for line in &lines[at..] {
        push(&mut out, *line);
    }
    String::from_utf8(out).map_err(|_| mismatch())
}

// ---- blame ------------------------------------------------------------------------

/// Whether commit `b` of a blame came after commit `a`, as far as their
/// times tell: lines not committed yet are the newest, then the later
/// author time; `None` for a tie.
pub fn newer_by_time(a: &BlameCommit, b: &BlameCommit) -> Option<bool> {
    let key = |c: &BlameCommit| (c.is_uncommitted(), c.time);
    (key(a) != key(b)).then(|| key(b) > key(a))
}

/// The real lines of a UTF-16 file that `git blame` read as bytes, from its
/// pieces (the commit and bytes of every line it printed, in order), each
/// with the commit that last changed any of its pieces: `newer(a, b)` says
/// whether commit `b` came after commit `a` (indexes into the blame's
/// commits; see [`newer_by_time`]). The text is the line's own, without its
/// line ending.
pub fn blame_lines<'a>(
    pieces: &[(usize, &[u8])],
    newer: impl Fn(usize, usize) -> bool,
    found: FileEncoding,
) -> Vec<(usize, Cow<'a, str>)> {
    // The file's bytes again, and where each piece starts in them. Git cut
    // after every byte 0x0A and printed each piece without it; a file that
    // ended in one (UTF-16 BE, "\0\n") left an odd count.
    let mut bytes = Vec::new();
    let mut starts = Vec::with_capacity(pieces.len() + 1);
    for (i, (_, piece)) in pieces.iter().enumerate() {
        if i > 0 {
            bytes.push(b'\n');
        }
        starts.push(bytes.len());
        bytes.extend_from_slice(piece);
    }
    if bytes.len() % 2 != 0 {
        bytes.push(b'\n');
    }
    // A piece takes in the 0x0A byte that ended it.
    starts.push(bytes.len() + 1);

    let latest = |a: usize, b: usize| if a != b && newer(a, b) { b } else { a };
    let piece_at = |offset: usize| starts.partition_point(|start| *start <= offset).saturating_sub(1).min(pieces.len().saturating_sub(1));
    let commit_of = |range: std::ops::Range<usize>| {
        let (first, last) = (piece_at(range.start), piece_at(range.end.saturating_sub(1).max(range.start)));
        (first..=last).filter_map(|p| pieces.get(p)).map(|(commit, _)| *commit).reduce(latest).unwrap_or(0)
    };

    let unit: fn([u8; 2]) -> u16 =
        if found.encoding == super::text::Encoding::Utf16Be { u16::from_be_bytes } else { u16::from_le_bytes };
    let skip = if found.bom { 2 } else { 0 };
    let mut out = Vec::new();
    let mut line_start = skip.min(bytes.len());
    let mut units: Vec<u16> = Vec::new();
    let finish = |units: &mut Vec<u16>, range: std::ops::Range<usize>, out: &mut Vec<(usize, Cow<'a, str>)>| {
        let mut text: String = char::decode_utf16(units.drain(..)).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)).collect();
        if text.ends_with('\r') {
            text.pop();
        }
        out.push((commit_of(range), Cow::Owned(text)));
    };
    let mut at = line_start;
    while at + 1 < bytes.len() {
        let u = unit([bytes[at], bytes[at + 1]]);
        if u == 0x0A {
            // The line, and the first byte of its newline: in UTF-16 LE that
            // byte (0x0A) ends the piece the line ends in.
            finish(&mut units, line_start..at + 1, &mut out);
            line_start = at + 2;
        } else {
            units.push(u);
        }
        at += 2;
    }
    // The last line, when the file does not end in a newline.
    if !units.is_empty() {
        finish(&mut units, line_start..bytes.len(), &mut out);
    }
    out
}

#[cfg(test)]
mod repo_tests {
    use std::process::Command;

    use super::*;
    use crate::git::DiffSource::{Commit, Staged, Unstaged};
    use crate::git::diff::LineApply::{Forward, Reverse};
    use crate::git::text::Encoding;
    use crate::tests::{Fixture, git};

    const LE_BOM: FileEncoding = FileEncoding { encoding: Encoding::Utf16Le, bom: true };
    const BE: FileEncoding = FileEncoding { encoding: Encoding::Utf16Be, bom: false };

    /// The bytes of `spec` (`:path` for the index), as git has them.
    fn blob(fx: &Fixture, spec: &str) -> Vec<u8> {
        let out = Command::new("git").current_dir(&fx.dir).args(["cat-file", "blob", spec]).output().unwrap();
        assert!(out.status.success(), "{spec}: {}", String::from_utf8_lossy(&out.stderr));
        out.stdout
    }

    fn put(fx: &Fixture, path: &str, encoding: FileEncoding, text: &str) {
        std::fs::write(fx.dir.join(path), encoding.encode(text)).unwrap();
    }

    fn on_disk(fx: &Fixture, path: &str) -> Vec<u8> {
        std::fs::read(fx.dir.join(path)).unwrap()
    }

    /// The changed lines of the diff that read as given.
    fn pick(diff: &FileDiff, wanted: &[(char, &str)]) -> Vec<(usize, usize)> {
        wanted
            .iter()
            .map(|(origin, text)| {
                diff.hunks
                    .iter()
                    .enumerate()
                    .find_map(|(h, hunk)| Some((h, hunk.lines.iter().position(|l| l.origin == *origin && l.text == *text)?)))
                    .unwrap_or_else(|| panic!("no line {origin}{text}"))
            })
            .collect()
    }

    fn numbered(change: impl Fn(usize) -> Option<String>) -> String {
        (1..=14).map(|n| change(n).unwrap_or_else(|| format!("café {n}")) + "\r\n").collect()
    }

    #[test]
    fn hunks_and_lines_of_a_utf16_file_are_staged_unstaged_and_discarded() {
        let fx = Fixture::new("utf16-parts");
        let path = "notes ü.txt";
        let original = numbered(|_| None);
        put(&fx, path, LE_BOM, &original);
        fx.commit_all("notes");
        let both = numbered(|n| match n {
            2 => Some("ünïcødé deux 中文".to_owned()),
            12 => Some("twelve, changed".to_owned()),
            _ => None,
        });
        put(&fx, path, LE_BOM, &both);

        let diff = fx.diff(Unstaged, path);
        assert!(diff.transcoded.is_some() && !diff.file_header.is_empty(), "the parts of the diff can be applied");
        assert_eq!(diff.hunks.len(), 2);

        // The first hunk into the index: written in UTF-16 LE with its byte
        // order mark and CRLF line endings, like the file.
        let first = numbered(|n| (n == 2).then(|| "ünïcødé deux 中文".to_owned()));
        fx.run(diff.apply_part("Stage hunk", diff.hunk_patch(0).unwrap(), true, false));
        assert_eq!(blob(&fx, &format!(":{path}")), LE_BOM.encode(&first));
        assert_eq!(on_disk(&fx, path), LE_BOM.encode(&both), "the working tree is left alone");

        // Unstaging the added line alone leaves the removal staged.
        let staged = fx.diff(Staged, path);
        let patch = staged.lines_patch(&pick(&staged, &[('+', "ünïcødé deux 中文")]), Reverse).unwrap();
        fx.run(staged.apply_part("Unstage lines", patch, true, true));
        let removed = numbered(|n| (n == 2).then(String::new)).replace("\r\n\r\n", "\r\n");
        assert_eq!(blob(&fx, &format!(":{path}")), LE_BOM.encode(&removed));
        // And the rest of it: the index is HEAD again, byte for byte.
        let staged = fx.diff(Staged, path);
        fx.run(staged.apply_part("Unstage hunk", staged.hunk_patch(0).unwrap(), true, true));
        assert_eq!(blob(&fx, &format!(":{path}")), blob(&fx, &format!("HEAD:{path}")));

        // Discarding one changed line of the working tree file.
        let diff = fx.diff(Unstaged, path);
        let patch = diff.lines_patch(&pick(&diff, &[('-', "café 12"), ('+', "twelve, changed")]), Reverse).unwrap();
        fx.run(diff.apply_part("Discard lines", patch, false, true));
        assert_eq!(on_disk(&fx, path), LE_BOM.encode(&first));

        // A diff made before the file changed again is refused, and the file
        // is left as it is.
        let stale = fx.diff(Unstaged, path);
        put(&fx, path, LE_BOM, &both);
        let done = fx.run_as(stale.apply_part("Discard hunk", stale.hunk_patch(0).unwrap(), false, true), crate::worker::Origin::User);
        assert!(done.result.unwrap_err().contains("has changed since"));
        assert_eq!(on_disk(&fx, path), LE_BOM.encode(&both));
        // Like any hunk or line, nothing to undo.
        assert!(done.undo.is_none());
    }

    #[test]
    fn utf16_without_a_byte_order_mark_is_read_and_written_as_it_is() {
        let fx = Fixture::new("utf16-be");
        put(&fx, "list.txt", BE, "alpha\nbeta\ngamma\n");
        fx.commit_all("list");
        put(&fx, "list.txt", BE, "alpha\nBETA\ngamma\ndelta\n");

        let diff = fx.diff(Unstaged, "list.txt");
        assert!(!diff.binary);
        assert!(diff.transcoded.is_some());
        let patch = diff.lines_patch(&pick(&diff, &[('+', "delta")]), Forward).unwrap();
        fx.run(diff.apply_part("Stage lines", patch, true, false));
        assert_eq!(blob(&fx, ":list.txt"), BE.encode("alpha\nbeta\ngamma\ndelta\n"));

        // Committed, the diff reads the same.
        git(&fx.dir, &["commit", "-q", "-m", "delta"]);
        let head = Oid::from_str(&git(&fx.dir, &["rev-parse", "HEAD"])).unwrap();
        let diff = fx.diff(Commit(head), "list.txt");
        let changed: Vec<_> = diff.hunks.iter().flat_map(|h| &h.lines).filter(|l| l.origin != ' ').map(|l| (l.origin, l.text.as_str())).collect();
        assert_eq!(changed, [('+', "delta")]);
    }

    #[test]
    fn a_new_utf16_file_is_staged_in_part_and_taken_out_again() {
        let fx = Fixture::new("utf16-new");
        fx.write("other.txt", "x\n");
        fx.commit_all("start");
        put(&fx, "new.txt", LE_BOM, "one\r\ntwo\r\nthree\r\n");

        let diff = fx.diff(Unstaged, "new.txt");
        let patch = diff.lines_patch(&pick(&diff, &[('+', "one"), ('+', "three")]), Forward).unwrap();
        fx.run(diff.apply_part("Stage lines", patch, true, false));
        assert_eq!(blob(&fx, ":new.txt"), LE_BOM.encode("one\r\nthree\r\n"));
        assert!(git(&fx.dir, &["ls-files", "-s", "new.txt"]).starts_with("100644 "));

        // Every line unstaged: the file is not in the index any more.
        let staged = fx.diff(Staged, "new.txt");
        let patch = staged.lines_patch(&pick(&staged, &[('+', "one"), ('+', "three")]), Reverse).unwrap();
        fx.run(staged.apply_part("Unstage lines", patch, true, true));
        assert_eq!(git(&fx.dir, &["ls-files", "new.txt"]), "");
        assert_eq!(on_disk(&fx, "new.txt"), LE_BOM.encode("one\r\ntwo\r\nthree\r\n"));

        // Its only hunk discarded: the untracked file is gone.
        let diff = fx.diff(Unstaged, "new.txt");
        fx.run(diff.apply_part("Discard hunk", diff.hunk_patch(0).unwrap(), false, true));
        assert!(!fx.dir.join("new.txt").exists());
    }

    #[test]
    fn utf16_files_are_blamed_line_by_line() {
        use crate::git::blame::{Cancel, blame};
        let fx = Fixture::new("utf16-blame");
        for (name, encoding) in [("le.txt", LE_BOM), ("be.txt", BE)] {
            put(&fx, name, encoding, "first\r\nline 上 two\r\nthird\r\n");
        }
        fx.commit_all("first");
        for (name, encoding) in [("le.txt", LE_BOM), ("be.txt", BE)] {
            put(&fx, name, encoding, "first\r\nline 上 TWO\r\nthird\r\nfourth\r\n");
        }
        git(&fx.dir, &["-c", "user.name=Second", "commit", "-q", "-am", "second"]);
        put(&fx, "le.txt", LE_BOM, "first\r\nline 上 TWO\r\nthird\r\nfourth\r\nnot committed\r\n");

        for (name, uncommitted) in [("le.txt", true), ("be.txt", false)] {
            let path = name.to_owned();
            let blame = fx.job(move |loader| blame(loader.repo(), None, &path, &Cancel::default())).unwrap();
            let mut expected = vec!["first", "line 上 TWO", "third", "fourth"];
            if uncommitted {
                expected.push("not committed");
            }
            assert_eq!(blame.lines, expected, "{name}");
            let by: Vec<String> = (0..blame.lines.len())
                .map(|line| {
                    let commit = &blame.commits[blame.blocks[blame.block_of(line).unwrap()].commit];
                    if commit.is_uncommitted() { "-".to_owned() } else { commit.summary.clone() }
                })
                .collect();
            let mut want = vec!["first", "second", "first", "second"];
            if uncommitted {
                want.push("-");
            }
            assert_eq!(by, want, "{name}");
        }

        // At a commit, too; and a file that is really binary still is.
        let head = Oid::from_str(&git(&fx.dir, &["rev-parse", "HEAD~1"])).unwrap();
        let blamed = fx.job(move |loader| blame(loader.repo(), Some(head), "be.txt", &Cancel::default())).unwrap();
        assert_eq!(blamed.lines, ["first", "line 上 two", "third"]);
        std::fs::write(fx.dir.join("data.bin"), b"\x00\x01\x02\x03binary").unwrap();
        fx.commit_all("data");
        let refused = fx.job(|loader| blame(loader.repo(), None, "data.bin", &Cancel::default()));
        assert_eq!(refused.unwrap_err(), crate::git::blame::BlameError::Binary);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::text::Encoding;

    #[test]
    fn patches_are_applied_to_text_both_ways() {
        let base = "one\r\ntwo\r\nthree\r\n";
        let patch = b"diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n one\r\n-two\r\n+TWO\r\n three\r\n";
        assert_eq!(apply_text(base, patch, false).unwrap(), "one\r\nTWO\r\nthree\r\n");
        assert_eq!(apply_text("one\r\nTWO\r\nthree\r\n", patch, true).unwrap(), base);
        // Insertion into an empty range, and at the very start.
        let insert = b"@@ -1,0 +2 @@\n+inserted\n";
        assert_eq!(apply_text("a\nb\n", insert, false).unwrap(), "a\ninserted\nb\n");
        let at_start = b"@@ -0,0 +1 @@\n+first\n";
        assert_eq!(apply_text("a\n", at_start, false).unwrap(), "first\na\n");
        // The missing newline at the end of a file, on either side.
        let no_eol = b"@@ -1 +1 @@\n-last\n\\ No newline at end of file\n+last\n";
        assert_eq!(apply_text("last", no_eol, false).unwrap(), "last\n");
        assert_eq!(apply_text("last\n", no_eol, true).unwrap(), "last");
        // Lines not where the patch says: refused, not applied elsewhere.
        assert!(apply_text("zero\none\r\ntwo\r\nthree\r\n", patch, false).is_err());
        assert!(apply_text("", patch, false).is_err());
    }

    fn side(text: &str, encoding: FileEncoding) -> (Side, Vec<u8>) {
        let bytes = encoding.encode(text);
        (Side::new(Some(&bytes), text.to_owned(), encoding), bytes)
    }

    #[test]
    fn sides_are_only_patched_when_they_write_back_exactly() {
        let le = FileEncoding { encoding: Encoding::Utf16Le, bom: true };
        let be = FileEncoding { encoding: Encoding::Utf16Be, bom: false };
        let (old, old_bytes) = side("a\n", le);
        let (new, new_bytes) = side("b\n", le);
        assert!(Sides::new("f", old, &old_bytes, new, &new_bytes).is_some());
        // Read in two different ways: what would one write?
        let (old, old_bytes) = side("a\n", le);
        let (new, new_bytes) = side("b\n", be);
        assert!(Sides::new("f", old, &old_bytes, new, &new_bytes).is_none());
        // Half a character at the end does not survive decoding.
        let (old, _) = side("a\n", le);
        let broken = [&le.encode("a\n")[..], b"x"].concat();
        let new = Side::new(Some(&broken), le.encoding.decode(&broken[2..]).into_owned(), le);
        assert!(Sides::new("f", old, &[], new, &broken).is_none());
    }

    fn pieces(bytes: &[u8]) -> Vec<&[u8]> {
        bytes.split(|b| *b == b'\n').collect()
    }

    fn commit(time: i64) -> BlameCommit {
        BlameCommit {
            oid: Oid::from_bytes(&[time as u8; 20]).unwrap(),
            summary: String::new(),
            author: String::new(),
            email: String::new(),
            time,
            path: String::new(),
            previous: None,
        }
    }

    #[test]
    fn blamed_pieces_become_real_lines() {
        let commits = [commit(100), commit(200)];
        for found in [
            FileEncoding { encoding: Encoding::Utf16Le, bom: true },
            FileEncoding { encoding: Encoding::Utf16Le, bom: false },
            FileEncoding { encoding: Encoding::Utf16Be, bom: true },
            FileEncoding { encoding: Encoding::Utf16Be, bom: false },
        ] {
            // 上 is U+4E0A: one of its bytes is 0x0A, so git cuts that line
            // in two. Its second half is from the newer commit.
            let bytes = found.encode("one\r\nline 上 two\r\nthree\r\n");
            let cut = pieces(&bytes);
            // Git prints no piece for nothing after a final 0x0A.
            let cut: Vec<&[u8]> = if cut.last().is_some_and(|p| p.is_empty()) { cut[..cut.len() - 1].to_vec() } else { cut };
            assert!(cut.len() > 3, "{found:?}: {}", cut.len());
            let tagged: Vec<(usize, &[u8])> =
                cut.iter().enumerate().map(|(i, piece)| (usize::from(i == 2 && found.encoding == Encoding::Utf16Le || i == 1 && found.encoding == Encoding::Utf16Be), *piece)).collect();
            let lines = blame_lines(&tagged, |a, b| newer_by_time(&commits[a], &commits[b]) == Some(true), found);
            let texts: Vec<&str> = lines.iter().map(|(_, text)| text.as_ref()).collect();
            assert_eq!(texts, ["one", "line 上 two", "three"], "{found:?}");
            assert_eq!(lines.iter().map(|(c, _)| *c).collect::<Vec<_>>(), [0, 1, 0], "{found:?}");
        }
        // No newline at the end, and an empty file.
        let found = FileEncoding { encoding: Encoding::Utf16Le, bom: false };
        let bytes = found.encode("a\nb");
        let tagged: Vec<(usize, &[u8])> = pieces(&bytes).into_iter().map(|p| (0, p)).collect();
        let texts: Vec<String> = blame_lines(&tagged, |_, _| false, found).into_iter().map(|(_, t)| t.into_owned()).collect();
        assert_eq!(texts, ["a", "b"]);
        assert!(blame_lines(&[], |_, _| false, found).is_empty());
    }
}
