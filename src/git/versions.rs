//! Files as they were in a commit.
//!
//! - [`tree`] lists every file of a commit's tree, folders first, for the
//!   commit details panel's "All files" (`ui::browse`).
//! - [`Loader::file_at`] reads one of them for the diff view
//!   (`DiffSource::At`): the whole file as one `Hunk::gap`, compared with
//!   nothing, so the diff view's fonts, colours, selection and copying all
//!   apply.
//! - [`read`] gives a file's bytes as a checkout would write them (line
//!   endings, LFS and other filters applied), which [`write_temp`] and
//!   [`save`] put somewhere for "Open this version" and "Save this version
//!   as…" (`ui::versions`).
//! - [`restore`] puts a file back in the working tree as it was in a commit
//!   (`git restore --source`), undoably.

use std::borrow::Cow;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use git2::{ObjectType, Repository};

use super::diff::{MAX_LINES, Mode};
use super::internal::{FileState, FileUndo};
use super::ops::{GitCmd, Step, UndoKind};
use super::repo::{Loader, lossy};
use super::text::{self, Encoding, Format};
use super::{DiffLine, FileDiff, Hunk, Oid, cli, picture, short};

fn err(e: git2::Error) -> String {
    e.message().to_owned()
}

// ---- a commit's tree ---------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// A folder; it holds the entries after it up to `end` (exclusive).
    Dir { end: u32 },
    File,
    /// A symbolic link: its content is the path it points to.
    Link,
    /// A submodule, at the commit the tree records.
    Submodule(Oid),
}

/// One file or folder of a commit's tree.
#[derive(Clone, Debug)]
pub struct Entry {
    /// Repository-relative, with `/`.
    pub path: String,
    /// Where the last component of `path` starts.
    pub name_at: usize,
    /// Folders above it.
    pub depth: u16,
    pub kind: EntryKind,
}

impl Entry {
    pub fn name(&self) -> &str {
        &self.path[self.name_at..]
    }

    pub fn is_dir(&self) -> bool {
        matches!(self.kind, EntryKind::Dir { .. })
    }
}

/// Every file and folder of a commit's tree.
#[derive(Debug)]
pub struct Tree {
    pub commit: Oid,
    /// In the order a file browser shows them: in each folder its folders
    /// first, then its files, each by name; what is inside a folder comes
    /// right after it.
    pub entries: Vec<Entry>,
    /// The entries that are not folders, by path: the flat list.
    pub by_path: Vec<u32>,
}

/// Lists the tree of `commit`. Costs a read of every folder of it: run it on
/// the reader thread.
pub fn tree(repo: &Repository, commit: Oid) -> Result<Tree, String> {
    let root = repo.find_commit(commit).and_then(|c| c.tree()).map_err(err)?;
    let mut entries = Vec::new();
    walk(repo, &root, "", 0, &mut entries)?;
    let mut by_path: Vec<u32> =
        (0..entries.len() as u32).filter(|i| !entries[*i as usize].is_dir()).collect();
    by_path.sort_by(|a, b| entries[*a as usize].path.cmp(&entries[*b as usize].path));
    Ok(Tree { commit, entries, by_path })
}

/// Names sort as a person would look for them: case aside first.
fn by_name(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| a.cmp(b))
}

fn walk(repo: &Repository, tree: &git2::Tree<'_>, prefix: &str, depth: u16, out: &mut Vec<Entry>) -> Result<(), String> {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in tree.iter() {
        let name = lossy(entry.name_bytes());
        match entry.kind() {
            Some(ObjectType::Tree) => dirs.push((name, entry.id())),
            Some(ObjectType::Commit) => files.push((name, EntryKind::Submodule(entry.id()))),
            Some(ObjectType::Blob) if entry.filemode() == 0o120000 => files.push((name, EntryKind::Link)),
            Some(ObjectType::Blob) => files.push((name, EntryKind::File)),
            _ => {}
        }
    }
    dirs.sort_by(|a, b| by_name(&a.0, &b.0));
    files.sort_by(|a, b| by_name(&a.0, &b.0));
    for (name, id) in dirs {
        let path = format!("{prefix}{name}");
        let name_at = prefix.len();
        let at = out.len();
        out.push(Entry { path: path.clone(), name_at, depth, kind: EntryKind::Dir { end: 0 } });
        let inner = repo.find_tree(id).map_err(err)?;
        walk(repo, &inner, &format!("{path}/"), depth + 1, out)?;
        out[at].kind = EntryKind::Dir { end: out.len() as u32 };
    }
    for (name, kind) in files {
        out.push(Entry { path: format!("{prefix}{name}"), name_at: prefix.len(), depth, kind });
    }
    Ok(())
}

// ---- one file, shown whole ---------------------------------------------------

impl Loader {
    /// `path` as it is in `commit`, for the diff view (`DiffSource::At`):
    /// every line in one gap hunk, or the picture of an image, or nothing
    /// but `binary` for other binary files. `mode` is only carried, so the
    /// view knows the answer is current; nothing is compared.
    pub fn file_at(&self, commit: Oid, path: &str, mode: Mode, format: Format) -> Result<FileDiff, String> {
        let repo = self.repo();
        let tree = repo.find_commit(commit).and_then(|c| c.tree()).map_err(err)?;
        let entry = tree
            .get_path(Path::new(path))
            .map_err(|_| format!("{path} is not in commit {}", short(commit)))?;
        match entry.kind() {
            Some(ObjectType::Blob) => {}
            Some(ObjectType::Commit) => {
                return Err(format!("{path} is a submodule, at commit {}", short(entry.id())));
            }
            _ => return Err(format!("{path} is a folder")),
        }
        let mut out = FileDiff { format, mode, ..FileDiff::default() };
        if picture::is_image_path(path) {
            out.binary = true;
            out.images = Some(Arc::new(picture::load(repo, None, Some(entry.id()), None)));
            return Ok(out);
        }
        let blob = repo.find_blob(entry.id()).map_err(err)?;
        let bytes = blob.content();
        let content: Cow<'_, [u8]> = if blob.is_binary() {
            // UTF-16 is binary to git; it reads as text once decoded.
            if !text::detect(bytes, format.encoding).encoding.is_utf16() {
                out.binary = true;
                return Ok(out);
            }
            let (decoded, _) = text::decode_file(bytes, format.encoding);
            if decoded.contains('\0') {
                out.binary = true;
                return Ok(out);
            }
            out.format = Format { encoding: Encoding::Utf8, ..format };
            Cow::Owned(decoded.into_bytes())
        } else {
            Cow::Borrowed(bytes)
        };
        whole_file(&content, &mut out);
        Ok(out)
    }
}

/// Fills `out` with every line of `content` as one gap hunk (unchanged
/// lines, no header), in `out.format`, up to `MAX_LINES`.
fn whole_file(content: &[u8], out: &mut FileDiff) {
    let format = out.format;
    let mut lines = Vec::new();
    // As a gap hunk has it: an empty header line, then the lines as context.
    let mut raw = b"\n".to_vec();
    for piece in content.split_inclusive(|b| *b == b'\n') {
        if lines.len() >= MAX_LINES {
            out.truncated = true;
            break;
        }
        let body = piece.strip_suffix(b"\n").unwrap_or(piece);
        raw.push(b' ');
        raw.extend_from_slice(body);
        raw.push(b'\n');
        if !piece.ends_with(b"\n") {
            raw.extend_from_slice(b"\\ No newline at end of file\n");
        }
        let text = text::display_text_in(piece, &format);
        out.max_chars = out.max_chars.max(text.chars().count());
        let number = Some(lines.len() as u32 + 1);
        lines.push(DiffLine { origin: ' ', old_no: number, new_no: number, text });
    }
    if !lines.is_empty() {
        out.hunks.push(Hunk { header: String::new(), lines, raw, gap: true });
    }
}

// ---- writing a version out ---------------------------------------------------

/// The bytes of `path` in `commit` as a checkout would write them: through
/// the repository's filters (line endings, LFS), by `git cat-file
/// --filters`. Runs git; call it from a background thread.
pub fn read(workdir: &Path, commit: Oid, path: &str) -> Result<Vec<u8>, String> {
    let out = cli::command(workdir)
        .args(["cat-file", "--filters"])
        .arg(format!("{commit}:{path}"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("Could not run git: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        return Err(if stderr.is_empty() {
            format!("{path} could not be read from commit {}.", short(commit))
        } else {
            stderr
        });
    }
    Ok(out.stdout)
}

/// Where [`write_temp`] puts `path` of `commit` under `base`: a folder per
/// commit holding the file under its own path, so the file keeps its name
/// (and the program its extension calls for opens it). `None` for a path
/// that would leave that folder.
pub fn temp_path(base: &Path, commit: Oid, path: &str) -> Option<PathBuf> {
    let relative = Path::new(path);
    if path.is_empty() || !relative.components().all(|part| matches!(part, Component::Normal(_))) {
        return None;
    }
    // Part by part, so the path is spelled with this system's separator.
    let mut dest = base.join(short(commit));
    dest.extend(path.split('/'));
    Some(dest)
}

/// The folder "Open this version" writes to.
pub fn temp_base() -> PathBuf {
    std::env::temp_dir().join("gitkrill-versions")
}

/// Writes `path` of `commit` under `base` (see [`temp_path`]), read-only,
/// for opening in another program. Returns where it is.
pub fn write_temp(workdir: &Path, base: &Path, commit: Oid, path: &str) -> Result<PathBuf, String> {
    let dest = temp_path(base, commit, path).ok_or_else(|| format!("{path} is not a path inside the repository."))?;
    let bytes = read(workdir, commit, path)?;
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
    }
    // Written before (and made read-only then): writable again first.
    if let Ok(meta) = std::fs::metadata(&dest) {
        let mut permissions = meta.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        let _ = std::fs::set_permissions(&dest, permissions);
    }
    std::fs::write(&dest, bytes).map_err(|e| format!("Could not write {}: {e}", dest.display()))?;
    // A reminder, in the program it opens in, that this is not the file in
    // the working tree.
    if let Ok(meta) = std::fs::metadata(&dest) {
        let mut permissions = meta.permissions();
        permissions.set_readonly(true);
        let _ = std::fs::set_permissions(&dest, permissions);
    }
    Ok(dest)
}

/// Writes `path` of `commit` to `dest` ("Save this version as…").
pub fn save(workdir: &Path, commit: Oid, path: &str, dest: &Path) -> Result<(), String> {
    let bytes = read(workdir, commit, path)?;
    std::fs::write(dest, bytes).map_err(|e| format!("Could not write {}: {e}", dest.display()))
}

/// Puts `path` in the working tree of `workdir` back as it was in `commit`
/// (`git restore --source=<commit> --worktree`), leaving the index as it
/// is. Undo writes back what the file held before, or removes it when there
/// was none.
pub fn restore(workdir: &Path, commit: Oid, path: &str) -> GitCmd {
    let name = path.rsplit('/').next().unwrap_or(path);
    let mut cmd = GitCmd::empty(format!("Restore {name}"));
    let args = ["restore", &format!("--source={commit}"), "--worktree", "--", path].map(str::to_owned);
    cmd.steps.push(Step::with_paths(args.to_vec(), None));
    let files = vec![FileState::new(workdir.join(path))];
    cmd.done(format!("Restored {name} as it was in {}", short(commit)))
        .undoable(UndoKind::Files(FileUndo { files, undo_steps: Vec::new() }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{Fixture, git};
    use crate::worker::Origin;

    fn head(fx: &Fixture) -> Oid {
        Oid::from_str(&git(&fx.dir, &["rev-parse", "HEAD"])).unwrap()
    }

    fn shown(tree: &Tree) -> Vec<String> {
        tree.entries
            .iter()
            .map(|e| format!("{}{}{}", "  ".repeat(e.depth as usize), e.name(), if e.is_dir() { "/" } else { "" }))
            .collect()
    }

    #[test]
    fn a_tree_lists_folders_first_with_their_files_after_them() {
        let fx = Fixture::new("versions-tree");
        fx.write("b.txt", "b\n");
        fx.write("A.txt", "a\n");
        fx.write("src/z.rs", "z\n");
        fx.write("src/ui/main ü.rs", "m\n");
        fx.write("docs/read me.md", "r\n");
        fx.commit_all("files");
        let oid = head(&fx);
        let tree = fx.job(move |loader| tree(loader.repo(), oid)).unwrap();
        assert_eq!(
            shown(&tree),
            ["docs/", "  read me.md", "src/", "  ui/", "    main ü.rs", "  z.rs", "A.txt", "b.txt"]
        );
        // A folder knows where its contents end.
        assert_eq!(tree.entries[2].kind, EntryKind::Dir { end: 6 });
        assert_eq!(tree.entries[3].kind, EntryKind::Dir { end: 5 });
        let flat: Vec<&str> = tree.by_path.iter().map(|i| tree.entries[*i as usize].path.as_str()).collect();
        assert_eq!(flat, ["A.txt", "b.txt", "docs/read me.md", "src/ui/main ü.rs", "src/z.rs"]);
    }

    #[test]
    fn a_file_of_an_older_commit_is_shown_whole() {
        let fx = Fixture::new("versions-file-at");
        fx.write("notes.txt", "one\n\ttwo\nthree");
        fx.commit_all("first");
        let first = head(&fx);
        fx.write("notes.txt", "changed\n");
        fx.commit_all("second");

        let target = crate::git::DiffTarget { source: crate::git::DiffSource::At(first), path: "notes.txt".into(), old_path: None };
        let diff = fx.job(move |loader| loader.diff_with(&target, Mode::default())).unwrap();
        let [hunk] = diff.hunks.as_slice() else { panic!("one hunk: {:?}", diff.hunks) };
        assert!(hunk.gap && !diff.binary && diff.file_header.is_empty());
        let lines: Vec<(Option<u32>, &str)> = hunk.lines.iter().map(|l| (l.new_no, l.text.as_str())).collect();
        assert_eq!(lines, [(Some(1), "one"), (Some(2), "    two"), (Some(3), "three")]);
        // Copying gives the file's own text back, tab included.
        let sources = hunk.sources(Encoding::Utf8).unwrap();
        assert_eq!(sources, ["one", "\ttwo", "three"]);
        assert_eq!((diff.additions, diff.deletions), (0, 0));
    }

    #[test]
    fn empty_binary_missing_and_utf16_files_are_told_apart() {
        let fx = Fixture::new("versions-kinds");
        fx.write("empty.txt", "");
        std::fs::write(fx.dir.join("blob.bin"), [0u8, 1, 2, 3, 0, 9]).unwrap();
        let mut utf16 = vec![0xFF, 0xFE];
        utf16.extend("hé\r\nho\r\n".encode_utf16().flat_map(u16::to_le_bytes));
        std::fs::write(fx.dir.join("wide.txt"), utf16).unwrap();
        fx.write("sub/dir.txt", "x\n");
        fx.commit_all("kinds");
        let oid = head(&fx);
        let at = |path: &str| {
            let path = path.to_owned();
            fx.job(move |loader| loader.file_at(oid, &path, Mode::default(), Format::default()))
        };
        assert!(at("empty.txt").unwrap().hunks.is_empty());
        let binary = at("blob.bin").unwrap();
        assert!(binary.binary && binary.hunks.is_empty());
        let wide = at("wide.txt").unwrap();
        let texts: Vec<&str> = wide.hunks[0].lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["hé", "ho"]);
        assert!(at("gone.txt").unwrap_err().contains("not in commit"));
        assert!(at("sub").unwrap_err().contains("folder"));
    }

    #[test]
    fn a_version_is_written_through_the_filters_and_saved() {
        let fx = Fixture::new("versions-write");
        fx.write(".gitattributes", "*.txt text eol=crlf\n");
        fx.write("dir with space/naïve.txt", "a\nb\n");
        fx.commit_all("first");
        let first = head(&fx);
        fx.write("dir with space/naïve.txt", "changed\n");
        fx.commit_all("second");

        let base = fx.dir.join("..").join(format!("versions-out-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let path = write_temp(&fx.dir, &base, first, "dir with space/naïve.txt").unwrap();
        assert!(path.ends_with(Path::new(&short(first)).join("dir with space").join("naïve.txt")));
        // As a checkout writes it: CRLF, as .gitattributes asks.
        assert_eq!(std::fs::read(&path).unwrap(), b"a\r\nb\r\n");
        assert!(std::fs::metadata(&path).unwrap().permissions().readonly());
        // Written again over the read-only copy.
        write_temp(&fx.dir, &base, first, "dir with space/naïve.txt").unwrap();

        let dest = base.join("saved.txt");
        save(&fx.dir, first, "dir with space/naïve.txt", &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"a\r\nb\r\n");
        assert!(save(&fx.dir, first, "missing.txt", &dest).is_err());
        assert!(temp_path(&base, first, "../escape.txt").is_none());

        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        std::fs::set_permissions(&path, permissions).unwrap();
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn restoring_a_version_overwrites_local_changes_and_can_be_undone() {
        let fx = Fixture::new("versions-restore");
        fx.write("a file.txt", "one\n");
        fx.commit_all("first");
        let first = head(&fx);
        fx.write("a file.txt", "two\n");
        fx.commit_all("second");
        fx.write("a file.txt", "local edit\n");

        let done = fx.run(restore(&fx.dir, first, "a file.txt"));
        assert_eq!(fx.read("a file.txt"), "one\n");
        // Only the working tree: the index still has the second version.
        assert_eq!(git(&fx.dir, &["show", ":a file.txt"]), "two");
        let entry = done.undo.expect("undoable");
        fx.run_as(entry.undo.clone(), Origin::Undo(entry.clone())).result.expect("undo works");
        assert_eq!(fx.read("a file.txt"), "local edit\n");
        fx.run_as(entry.redo.clone(), Origin::Redo(entry)).result.expect("redo works");
        assert_eq!(fx.read("a file.txt"), "one\n");
    }

    #[test]
    fn restoring_a_deleted_file_brings_it_back_and_undo_removes_it() {
        let fx = Fixture::new("versions-restore-deleted");
        fx.write("kept.txt", "k\n");
        fx.write("gone.txt", "was here\n");
        fx.commit_all("first");
        let first = head(&fx);
        git(&fx.dir, &["rm", "-q", "gone.txt"]);
        fx.commit_all("delete");

        let done = fx.run(restore(&fx.dir, first, "gone.txt"));
        assert_eq!(fx.read("gone.txt"), "was here\n");
        let entry = done.undo.expect("undoable");
        fx.run_as(entry.undo.clone(), Origin::Undo(entry)).result.expect("undo works");
        assert!(!fx.dir.join("gone.txt").exists());

        // A path the commit does not have: git refuses, nothing is written.
        let refused = fx.run_as(restore(&fx.dir, first, "never.txt"), Origin::User);
        assert!(refused.result.is_err());
        assert!(refused.undo.is_none());
    }

    #[test]
    fn history_and_blame_can_start_at_an_older_commit() {
        use crate::git::blame::{Cancel, HistoryCursor, blame};
        let fx = Fixture::new("versions-history-start");
        fx.write("story.txt", "one\n");
        fx.commit_all("first");
        fx.write("story.txt", "one\ntwo\n");
        fx.commit_all("second");
        let second = head(&fx);
        fx.write("story.txt", "one\ntwo\nthree\n");
        fx.commit_all("third");

        // As the history view opened from the second commit's files walks it.
        let summaries = fx.job(move |loader| {
            let mut cursor = HistoryCursor::new(loader.repo(), Some(second), "story.txt").unwrap();
            let found = cursor.advance(loader.repo(), 100, std::time::Duration::from_secs(5)).unwrap();
            found.into_iter().map(|c| c.summary).collect::<Vec<_>>()
        });
        assert_eq!(summaries, ["second", "first"]);
        let repo = Repository::open(&fx.dir).unwrap();
        let blamed = blame(&repo, Some(second), "story.txt", &Cancel::default()).unwrap();
        assert_eq!(blamed.lines, ["one", "two"]);
    }

    #[test]
    fn restoring_what_is_there_already_leaves_nothing_to_undo() {
        let fx = Fixture::new("versions-restore-same");
        fx.write("same.txt", "x\n");
        fx.commit_all("first");
        let done = fx.run(restore(&fx.dir, head(&fx), "same.txt"));
        assert!(done.undo.is_none());
    }
}
