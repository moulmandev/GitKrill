//! Single-file diffs, parsed into hunks for display and kept as raw patch
//! bytes so individual hunks can be staged, unstaged or discarded.

use git2::{DiffFindOptions, DiffFormat, DiffOptions};

use super::repo::{Loader, delta_kind, lossy};
use super::picture;
use super::text::{self, Encoding, display_text_in, source_text_in};
use super::{DiffLine, DiffSource, DiffTarget, FileDiff, Hunk};

/// Beyond this many lines the rest of the diff is not shown.
const MAX_LINES: usize = 50_000;

impl Loader {
    pub fn diff(&self, target: &DiffTarget) -> Result<FileDiff, String> {
        let repo = &self.repo;
        let err = |e: git2::Error| e.message().to_owned();

        let mut opts = DiffOptions::new();
        opts.context_lines(3).disable_pathspec_match(true).pathspec(target.path.as_str());
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
        };

        if target.old_path.is_some() {
            let mut find = DiffFindOptions::new();
            find.renames(true);
            let _ = diff.find_similar(Some(&mut find));
        }

        // How lines become text: the repository's encoding, the tab width.
        let format = text::format_for(repo.workdir().unwrap_or(repo.path()));
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

        let is_image = picture::is_image_path(&target.path)
            || target.old_path.as_deref().is_some_and(picture::is_image_path);
        if out.binary && is_image && let Some((old, new)) = blobs {
            // An unstaged file has no blob yet: it is read from disk, unless
            // it is the deletion being shown.
            let on_disk = (target.source == DiffSource::Unstaged && out.kind != Some(super::ChangeKind::Deleted))
                .then(|| repo.workdir().map(|dir| dir.join(&target.path)))
                .flatten();
            let images = picture::load(repo, Some(old), Some(new), on_disk.as_deref());
            if images.old.is_some() || images.new.is_some() {
                out.images = Some(std::sync::Arc::new(images));
            }
        }
        Ok(out)
    }
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
}
