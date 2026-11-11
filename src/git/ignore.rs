//! Ignoring files: patterns that name exactly one file, one folder or every
//! file with an extension, adding one to the root `.gitignore` or to
//! `.git/info/exclude`, and taking the tracked files a pattern matches out
//! of the index so the pattern applies to them too.
//!
//! The line is written by an internal step (`git::internal`) on the
//! operator thread, so it is undoable like any other command.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::internal::{FileState, FileUndo, Internal};
use super::ops::{GitCmd, Step, UndoKind};
use super::repo::Loader;
use super::cli;

/// Which file a pattern goes in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// The `.gitignore` at the root of the working tree: committed, so it
    /// applies to everyone.
    GitIgnore,
    /// `.git/info/exclude`: this clone only.
    Exclude,
}

impl Target {
    pub fn label(self) -> &'static str {
        match self {
            Target::GitIgnore => ".gitignore",
            Target::Exclude => ".git/info/exclude",
        }
    }
}

// ---- patterns ---------------------------------------------------------------

/// `name` (one path component) as gitignore text that matches exactly it:
/// wildcards (`*`, `?`, `[`) and backslashes escaped, a leading `#` (a
/// comment) or `!` (a negation) escaped, and spaces at either end escaped
/// (git drops unescaped trailing spaces). `None` for a name no line can
/// hold, one with a line break.
pub fn escape(name: &str) -> Option<String> {
    if name.is_empty() || name.contains(['\n', '\r']) {
        return None;
    }
    let lead = name.len() - name.trim_start_matches(' ').len();
    let trail = name.len() - name.trim_end_matches(' ').len();
    let mut out = String::with_capacity(name.len() + 4);
    for (i, c) in name.char_indices() {
        let edge_space = c == ' ' && (i < lead || i >= name.len() - trail);
        let special = matches!(c, '*' | '?' | '[' | '\\') || (i == 0 && matches!(c, '#' | '!'));
        if edge_space || special {
            out.push('\\');
        }
        out.push(c);
    }
    Some(out)
}

/// The pattern that ignores the file at `path` (relative to the root) and
/// nothing else: anchored with a leading `/`, every component escaped.
pub fn file_pattern(path: &str) -> Option<String> {
    let mut out = String::with_capacity(path.len() + 4);
    for part in path.split('/') {
        out.push('/');
        out.push_str(&escape(part)?);
    }
    Some(out)
}

/// The pattern that ignores the folder `dir` (relative to the root, no
/// trailing slash) and everything in it: `/dir/`.
pub fn folder_pattern(dir: &str) -> Option<String> {
    Some(format!("{}/", file_pattern(dir.trim_end_matches('/'))?))
}

/// `*.log` for `logs/run.log`: every file with that extension, in every
/// folder. `None` when the name has no extension (`Makefile`, `.env`).
pub fn extension_pattern(path: &str) -> Option<String> {
    let (stem, extension) = file_name(path).rsplit_once('.')?;
    if stem.is_empty() || extension.is_empty() {
        return None;
    }
    // The extension follows `*.`, so a leading `#` or `!` needs no escape.
    let escaped = escape(extension)?;
    let escaped = match extension.chars().next() {
        Some('#' | '!') => escaped[1..].to_owned(),
        _ => escaped,
    };
    Some(format!("*.{escaped}"))
}

/// The folders `path` is in, nearest first: `a/b/c.txt` gives `a/b`, `a`.
pub fn parents(path: &str) -> Vec<&str> {
    path.rmatch_indices('/').map(|(i, _)| &path[..i]).collect()
}

pub fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Whether `pattern` can be added as it is: one line, not a comment, not
/// blank.
pub fn valid(pattern: &str) -> bool {
    let trimmed = trim_trailing_spaces(pattern);
    !trimmed.is_empty() && !trimmed.starts_with('#') && !pattern.contains(['\n', '\r'])
}

/// A line as git reads it: unescaped trailing spaces dropped.
fn trim_trailing_spaces(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut end = bytes.len();
    let mut last_space: Option<usize> = None;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b' ' => {
                last_space.get_or_insert(i);
            }
            b'\\' => {
                i += 1;
                last_space = None;
            }
            _ => last_space = None,
        }
        i += 1;
    }
    if let Some(space) = last_space {
        end = space;
    }
    &line[..end]
}

// ---- ignore files -----------------------------------------------------------

/// Whether `content` (an ignore file) has `pattern` as one of its lines.
pub fn contains(content: &[u8], pattern: &str) -> bool {
    let text = String::from_utf8_lossy(content);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let wanted = trim_trailing_spaces(pattern);
    text.split('\n').any(|line| trim_trailing_spaces(line.strip_suffix('\r').unwrap_or(line)) == wanted)
}

/// `content` with `pattern` added as its last line, in the file's own line
/// endings (CRLF when its first line ends so), after a line break when the
/// file does not end with one. `None` when the pattern is there already.
pub fn append(content: &[u8], pattern: &str) -> Option<Vec<u8>> {
    if contains(content, pattern) {
        return None;
    }
    let crlf = content.iter().position(|&b| b == b'\n').is_some_and(|i| i > 0 && content[i - 1] == b'\r');
    let eol: &[u8] = if crlf { b"\r\n" } else { b"\n" };
    let mut out = content.to_vec();
    if !out.is_empty() && !out.ends_with(b"\n") {
        out.extend_from_slice(eol);
    }
    out.extend_from_slice(pattern.as_bytes());
    out.extend_from_slice(eol);
    Some(out)
}

/// The step that adds `pattern` to the ignore file at `file`, creating it
/// (and its folder) when needed. Nothing changes when the line is there.
/// It says what it did, naming the file `shown`.
pub fn append_step(file: &Path, pattern: &str, shown: &str) -> Step {
    let (file, pattern, shown) = (file.to_path_buf(), pattern.to_owned(), shown.to_owned());
    Step::internal(Internal::new(format!("add {pattern} to {}", file.display()), move |_| {
        let content = match std::fs::read(&file) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(format!("Could not read {}: {e}", file.display())),
        };
        if let Some(new) = append(&content, &pattern) {
            if let Some(dir) = file.parent() {
                std::fs::create_dir_all(dir).map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
            }
            std::fs::write(&file, new).map_err(|e| format!("Could not write {}: {e}", file.display()))?;
            Ok(format!("Added {pattern} to {shown}"))
        } else {
            Ok(format!("{shown} has {pattern} already"))
        }
    }))
}

/// Where the patterns of each [`Target`] live for this repository (a linked
/// worktree shares `info/exclude` with its main repository).
pub fn file_for(repo: &git2::Repository, target: Target) -> Option<PathBuf> {
    match target {
        Target::GitIgnore => repo.workdir().map(|dir| dir.join(".gitignore")),
        Target::Exclude => Some(repo.commondir().join("info").join("exclude")),
    }
}

// ---- what a pattern matches -------------------------------------------------

/// A tracked file a pattern matches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tracked {
    pub path: String,
    /// In the commit HEAD points at; otherwise only staged (a new file).
    pub committed: bool,
}

/// What adding a pattern would do, from [`check`].
#[derive(Clone, Debug, Default)]
pub struct Matches {
    /// Changed untracked files it hides (those of the list given to `check`).
    pub untracked: Vec<String>,
    /// Tracked files it matches, which git keeps tracking unless taken out
    /// of the index.
    pub tracked: Vec<Tracked>,
    /// The ignore file of each target ([`Target::GitIgnore`], then
    /// [`Target::Exclude`]), and whether it has the pattern already.
    pub files: [(PathBuf, bool); 2],
}

impl Matches {
    pub fn file(&self, target: Target) -> &(PathBuf, bool) {
        &self.files[target as usize]
    }
}

/// Which of `untracked` (changed untracked files, relative to the root)
/// `pattern` would hide, which tracked files it matches, and whether the
/// ignore files have it already. Runs `git ls-files`, so it gives git's own
/// answer; for the reader thread.
pub fn check(loader: &Loader, pattern: &str, untracked: &[String]) -> Result<Matches, String> {
    if !valid(pattern) {
        return Err("Not a pattern".to_owned());
    }
    let repo = loader.repo();
    let workdir = repo.workdir().ok_or("The repository has no working tree")?;
    let exclude = format!("--exclude={pattern}");

    // Untracked files the pattern (or a rule already there) ignores, whole
    // ignored folders as `dir/`, so ignored build output is not walked.
    let ignored = ls_files(workdir, &["--others", "--ignored", "--directory", "--exclude-standard", &exclude])?;
    let (folders, files): (HashSet<&str>, HashSet<&str>) = (
        ignored.iter().map(String::as_str).filter(|p| p.ends_with('/')).collect(),
        ignored.iter().map(String::as_str).filter(|p| !p.ends_with('/')).collect(),
    );
    let hidden = |path: &str| {
        files.contains(path) || path.match_indices('/').any(|(i, _)| folders.contains(&path[..=i]))
    };
    let untracked: Vec<String> = untracked.iter().filter(|p| hidden(p)).cloned().collect();

    // Tracked files matching this pattern alone.
    let head = repo.head().ok().and_then(|head| head.peel_to_tree().ok());
    let mut seen = HashSet::new();
    let tracked = ls_files(workdir, &["--cached", "--ignored", &exclude])?
        .into_iter()
        .filter(|path| seen.insert(path.clone()))
        .map(|path| {
            let committed = head.as_ref().is_some_and(|tree| tree.get_path(Path::new(&path)).is_ok());
            Tracked { path, committed }
        })
        .collect();

    let file = |target| {
        let path = file_for(repo, target).unwrap_or_default();
        let has = std::fs::read(&path).is_ok_and(|content| contains(&content, pattern));
        (path, has)
    };
    Ok(Matches { untracked, tracked, files: [file(Target::GitIgnore), file(Target::Exclude)] })
}

/// `git ls-files -z <args>`: the paths it lists.
fn ls_files(workdir: &Path, args: &[&str]) -> Result<Vec<String>, String> {
    let out = cli::command(workdir)
        .args(["ls-files", "-z"])
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("Failed to start git: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    Ok(out.stdout.split(|&b| b == 0).filter(|p| !p.is_empty()).map(|p| String::from_utf8_lossy(p).into_owned()).collect())
}

// ---- the command --------------------------------------------------------------

/// Adds `pattern` to `file` (one of [`file_for`]), after taking `untrack`
/// out of the index (`git rm --cached`: they stay on disk, and the next
/// commit deletes them from the repository). Undo takes the line out again
/// and puts the files back in the index.
pub fn ignore(pattern: &str, file: &Path, target: Target, untrack: &[Tracked]) -> GitCmd {
    let where_to = target.label();
    // Short toast titles; the step's output says where the pattern went.
    let mut cmd = match untrack {
        [] => GitCmd::empty(format!("Ignore {pattern}")).done(format!("Ignored {pattern}")),
        [one] => GitCmd::empty(format!("Stop tracking and ignore {pattern}"))
            .done(format!("Stopped tracking {}", file_name(&one.path))),
        many => GitCmd::empty(format!("Stop tracking and ignore {pattern}"))
            .done(format!("Stopped tracking {} files", many.len())),
    };
    // Out of the index first: when git refuses (staged changes it would
    // lose), nothing has been changed.
    if !untrack.is_empty() {
        cmd.steps.push(paths_step(&["rm", "--cached", "-q", "--ignore-unmatch"], untrack.iter().map(|t| t.path.as_str())));
    }
    cmd.steps.push(append_step(file, pattern, where_to));

    let mut undo_steps = Vec::new();
    let committed: Vec<&str> = untrack.iter().filter(|t| t.committed).map(|t| t.path.as_str()).collect();
    let added: Vec<&str> = untrack.iter().filter(|t| !t.committed).map(|t| t.path.as_str()).collect();
    if !committed.is_empty() {
        undo_steps.push(paths_step(&["reset", "-q", "HEAD"], committed.into_iter()));
    }
    if !added.is_empty() {
        undo_steps.push(paths_step(&["add", "-f"], added.into_iter()));
    }
    cmd.undoable(UndoKind::Files(FileUndo { files: vec![FileState::new(file)], undo_steps }))
}

/// A step taking its paths NUL-separated on stdin, literally.
fn paths_step<'a>(args: &[&str], paths: impl Iterator<Item = &'a str>) -> Step {
    let mut args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    args.push("--pathspec-from-file=-".to_owned());
    args.push("--pathspec-file-nul".to_owned());
    let mut stdin = Vec::new();
    for path in paths {
        stdin.extend_from_slice(path.as_bytes());
        stdin.push(0);
    }
    Step::with_paths(args, Some(stdin))
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;
    use crate::git::ChangeKind;
    use crate::tests::{Fixture, git};
    use crate::worker::Origin;

    #[test]
    fn escapes_what_gitignore_reads_specially() {
        assert_eq!(escape("plain.txt").as_deref(), Some("plain.txt"));
        assert_eq!(escape("a b.txt").as_deref(), Some("a b.txt"));
        assert_eq!(escape("#notes").as_deref(), Some("\\#notes"));
        assert_eq!(escape("!bang").as_deref(), Some("\\!bang"));
        assert_eq!(escape("mid#dle!").as_deref(), Some("mid#dle!"));
        assert_eq!(escape("[x]*?.txt").as_deref(), Some("\\[x]\\*\\?.txt"));
        assert_eq!(escape("back\\slash").as_deref(), Some("back\\\\slash"));
        assert_eq!(escape("  lead").as_deref(), Some("\\ \\ lead"));
        assert_eq!(escape("trail  ").as_deref(), Some("trail\\ \\ "));
        assert_eq!(escape("two\nlines"), None);
        assert_eq!(escape(""), None);
    }

    #[test]
    fn builds_anchored_patterns() {
        assert_eq!(file_pattern("run.log").as_deref(), Some("/run.log"));
        assert_eq!(file_pattern("logs/2026/run.log").as_deref(), Some("/logs/2026/run.log"));
        assert_eq!(file_pattern("#a/!b").as_deref(), Some("/\\#a/\\!b"));
        assert_eq!(folder_pattern("logs/2026").as_deref(), Some("/logs/2026/"));
        assert_eq!(extension_pattern("logs/run.log").as_deref(), Some("*.log"));
        assert_eq!(extension_pattern("a.tar.gz").as_deref(), Some("*.gz"));
        assert_eq!(extension_pattern("x.[1]").as_deref(), Some("*.\\[1]"));
        assert_eq!(extension_pattern("x.#1").as_deref(), Some("*.#1"));
        assert_eq!(extension_pattern(".env"), None);
        assert_eq!(extension_pattern("Makefile"), None);
        assert_eq!(extension_pattern("dir.d/Makefile"), None);
        assert_eq!(parents("a/b/c.txt"), ["a/b", "a"]);
        assert!(parents("c.txt").is_empty());
        assert!(valid("*.log") && valid("/a\\ ") && !valid("# comment") && !valid("   ") && !valid("a\nb"));
    }

    /// What `git check-ignore --no-index` says about each of `paths` when
    /// `pattern` is the only line of the root `.gitignore`: the paths it
    /// ignores. The files need not exist (a `*` cannot be in a Windows file
    /// name); a path under an ignored folder counts as ignored.
    fn ignored_by(dir: &Path, pattern: &str, paths: &[&str]) -> Vec<String> {
        std::fs::write(dir.join(".gitignore"), format!("{pattern}\n")).unwrap();
        let mut stdin = Vec::new();
        for path in paths {
            stdin.extend_from_slice(path.as_bytes());
            stdin.push(0);
        }
        let mut child = Command::new("git")
            .current_dir(dir)
            .args(["-c", "core.excludesFile=no-such-file", "check-ignore", "--no-index", "-v", "-n", "-z", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(&mut child.stdin.take().unwrap(), &stdin).unwrap();
        let out = child.wait_with_output().unwrap();
        // Records of four fields: source, line, pattern, path (source empty
        // when nothing matched).
        let fields: Vec<&[u8]> = out.stdout.split(|&b| b == 0).collect();
        fields
            .chunks(4)
            .filter(|record| record.len() == 4 && !record[0].is_empty())
            .map(|record| {
                assert_eq!(String::from_utf8_lossy(record[2]), pattern, "matched by the pattern itself");
                String::from_utf8_lossy(record[3]).into_owned()
            })
            .collect()
    }

    #[test]
    fn each_pattern_matches_exactly_the_intended_file() {
        let fx = Fixture::new("ignore-patterns");
        // (file, others that must not be ignored)
        let cases: &[(&str, &[&str])] = &[
            ("run.log", &["logs/run.log", "run.log.old", "xrun.log"]),
            ("logs/2026/run.log", &["other/logs/2026/run.log", "logs/2026/run.logx", "logs/run.log"]),
            ("a b.txt", &["a  b.txt", "ab.txt", "dir/a b.txt"]),
            ("#notes", &["notes", "x#notes"]),
            ("!bang", &["bang"]),
            ("[x].txt", &["x.txt", "[x].txtx"]),
            ("*.txt", &["a.txt", "b/*.txt"]),
            ("q?.txt", &["qa.txt", "q.txt"]),
            ("lead/ space", &["lead/space", "lead/  space"]),
            ("trail ", &["trail", "trail  "]),
            ("ünï 文件.txt", &["uni.txt", "ünï.txt"]),
            ("-dash", &["dash"]),
        ];
        // Git for Windows reads a backslash in a path as a folder separator.
        let backslash: &[(&str, &[&str])] = &[("back\\slash", &["back/slash", "backslash"])];
        let cases = cases.iter().chain(if cfg!(windows) { &[][..] } else { backslash });
        for (file, others) in cases {
            let pattern = file_pattern(file).unwrap();
            let mut paths = vec![*file];
            paths.extend_from_slice(others);
            assert_eq!(ignored_by(&fx.dir, &pattern, &paths), [*file], "{pattern}");
        }

        let folders: &[(&str, &[&str], &[&str])] = &[
            ("logs", &["logs/a.txt", "logs/deep/b.txt"], &["other/logs/a.txt", "logsx/a.txt", "logs"]),
            ("a [b]/c*", &["a [b]/c*/x"], &["a b/cc/x", "a [b]/cd/x"]),
            (" lead", &[" lead/x"], &["lead/x"]),
        ];
        for (dir, inside, outside) in folders {
            let pattern = folder_pattern(dir).unwrap();
            let mut paths = inside.to_vec();
            paths.extend_from_slice(outside);
            assert_eq!(ignored_by(&fx.dir, &pattern, &paths), *inside, "{pattern}");
        }

        let pattern = extension_pattern("x/report.[1]").unwrap();
        let ignored = ignored_by(&fx.dir, &pattern, &["report.[1]", "deep/er/a.[1]", "a.1", "a.[1]x"]);
        assert_eq!(ignored, ["report.[1]", "deep/er/a.[1]"]);
    }

    #[test]
    fn appends_in_the_files_own_style() {
        assert_eq!(append(b"", "/a").unwrap(), b"/a\n");
        assert_eq!(append(b"target/\n", "/a").unwrap(), b"target/\n/a\n");
        // No line break at the end: one goes in first.
        assert_eq!(append(b"target/", "/a").unwrap(), b"target/\n/a\n");
        // CRLF files stay CRLF.
        assert_eq!(append(b"target/\r\n*.o\r\n", "/a").unwrap(), b"target/\r\n*.o\r\n/a\r\n");
        assert_eq!(append(b"target/\r\n*.o", "/a").unwrap(), b"target/\r\n*.o\r\n/a\r\n");
        // Already there (in any line ending, with spaces git drops, after a
        // byte order mark): nothing to add.
        assert_eq!(append(b"/a\n", "/a"), None);
        assert_eq!(append(b"x\r\n/a\r\n", "/a"), None);
        assert_eq!(append(b"x\n/a   \n", "/a"), None);
        assert_eq!(append(b"\xef\xbb\xbf/a\n", "/a"), None);
        // An escaped trailing space is part of the pattern.
        assert!(append(b"/a\n", "/a\\ ").is_some());
        assert!(append(b"/a/\n", "/a").is_some());
    }

    /// Where `path` really is, for comparing paths spelt differently: git
    /// reports the temporary folder in its long form on Windows
    /// (`runneradmin`, not `RUNNER~1`) and through `/private` on macOS.
    /// The file itself may not exist yet, so its nearest existing folder
    /// is resolved and the rest appended.
    fn resolved(path: &std::path::Path) -> std::path::PathBuf {
        let mut rest = Vec::new();
        let mut at = path;
        loop {
            if let Ok(real) = std::fs::canonicalize(at) {
                return rest.iter().rev().fold(real, |out, part| out.join(part));
            }
            match (at.parent(), at.file_name()) {
                (Some(parent), Some(name)) => {
                    rest.push(name.to_owned());
                    at = parent;
                }
                _ => return path.to_owned(),
            }
        }
    }

    fn untracked(fx: &Fixture) -> Vec<String> {
        fx.snapshot().status.unstaged.iter().filter(|f| f.kind == ChangeKind::Untracked).map(|f| f.path.clone()).collect()
    }

    #[test]
    fn ignoring_a_file_hides_it_and_undo_brings_it_back() {
        let fx = Fixture::new("ignore-file");
        fx.write("kept.txt", "one\n");
        fx.commit_all("first");
        fx.write("logs/run 1.log", "x\n");
        fx.write("logs/other.log", "x\n");
        fx.write("notes.txt", "x\n");

        let pattern = file_pattern("logs/run 1.log").unwrap();
        let changed = untracked(&fx);
        let p = pattern.clone();
        let matches = fx.job(move |loader| check(loader, &p, &changed)).unwrap();
        assert_eq!(matches.untracked, ["logs/run 1.log"]);
        assert!(matches.tracked.is_empty());
        let (file, present) = matches.file(Target::GitIgnore);
        assert_eq!((resolved(file), *present), (resolved(&fx.dir.join(".gitignore")), false));

        let file = matches.file(Target::GitIgnore).0.clone();
        let done = fx.run(ignore(&pattern, &file, Target::GitIgnore, &[]));
        assert_eq!(fx.read(".gitignore"), "/logs/run 1.log\n");
        let paths: Vec<&str> = done.snap.status.unstaged.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, [".gitignore", "logs/other.log", "notes.txt"]);

        // Once it is there, the check says so, and adding it again changes
        // nothing and leaves nothing to undo.
        let p = pattern.clone();
        let matches = fx.job(move |loader| check(loader, &p, &[])).unwrap();
        assert!(matches.file(Target::GitIgnore).1);
        assert!(!matches.file(Target::Exclude).1);
        let again = fx.run(ignore(&pattern, &file, Target::GitIgnore, &[]));
        assert!(again.undo.is_none());
        assert_eq!(fx.read(".gitignore"), "/logs/run 1.log\n");

        // Undo removes the file it created, redo makes it again.
        let entry = done.undo.expect("undoable");
        let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry.clone()));
        undone.result.expect("undo works");
        assert!(!fx.dir.join(".gitignore").exists());
        assert!(undone.snap.status.unstaged.iter().any(|f| f.path == "logs/run 1.log"));
        fx.run_as(entry.redo.clone(), Origin::Redo(entry)).result.expect("redo works");
        assert_eq!(fx.read(".gitignore"), "/logs/run 1.log\n");
    }

    #[test]
    fn keeps_an_existing_gitignore_as_it_is() {
        let fx = Fixture::new("ignore-existing");
        fx.write(".gitignore", "target/\r\n*.o");
        fx.commit_all("first");
        fx.write("build.log", "x\n");
        fx.write("a.log", "x\n");

        let file = fx.dir.join(".gitignore");
        let done = fx.run(ignore("*.log", &file, Target::GitIgnore, &[]));
        assert_eq!(fx.read(".gitignore"), "target/\r\n*.o\r\n*.log\r\n");
        let paths: Vec<&str> = done.snap.status.unstaged.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, [".gitignore"]);

        let entry = done.undo.expect("undoable");
        fx.run_as(entry.undo.clone(), Origin::Undo(entry)).result.expect("undo works");
        assert_eq!(fx.read(".gitignore"), "target/\r\n*.o");
    }

    #[test]
    fn info_exclude_ignores_for_this_clone_only() {
        let fx = Fixture::new("ignore-exclude");
        fx.write("a.txt", "one\n");
        fx.commit_all("first");
        fx.write("local/scratch.txt", "x\n");
        // Made with its folder when missing.
        let exclude = fx.dir.join(".git").join("info").join("exclude");
        let _ = std::fs::remove_dir_all(fx.dir.join(".git").join("info"));

        let changed = untracked(&fx);
        let matches = fx.job(move |loader| check(loader, "/local/", &changed)).unwrap();
        assert_eq!(matches.untracked, ["local/scratch.txt"]);
        assert_eq!(resolved(&matches.file(Target::Exclude).0), resolved(&exclude));

        let done = fx.run(ignore("/local/", &exclude, Target::Exclude, &[]));
        assert_eq!(std::fs::read_to_string(&exclude).unwrap(), "/local/\n");
        assert!(done.snap.status.is_clean(), "nothing to commit: {:?}", done.snap.status);
        assert!(!fx.dir.join(".gitignore").exists());
    }

    #[test]
    fn stop_tracking_takes_files_out_of_the_index_and_undo_puts_them_back() {
        let fx = Fixture::new("ignore-untrack");
        fx.write("app/config.local.json", "{}\n");
        fx.write("app/main.rs", "fn main() {}\n");
        fx.write("debug.log", "old\n");
        fx.commit_all("first");
        fx.write("debug.log", "new\n");
        fx.write("new.log", "staged\n");
        git(&fx.dir, &["add", "new.log"]);

        let matches = fx.job(|loader| check(loader, "*.log", &[])).unwrap();
        assert_eq!(
            matches.tracked,
            [
                Tracked { path: "debug.log".to_owned(), committed: true },
                Tracked { path: "new.log".to_owned(), committed: false },
            ]
        );

        let file = fx.dir.join(".gitignore");
        let done = fx.run(ignore("*.log", &file, Target::GitIgnore, &matches.tracked));
        // On disk still, deleted from the repository by the next commit.
        assert_eq!(fx.read("debug.log"), "new\n");
        assert_eq!(fx.read("new.log"), "staged\n");
        let staged: Vec<(&str, ChangeKind)> = done.snap.status.staged.iter().map(|f| (f.path.as_str(), f.kind)).collect();
        assert_eq!(staged, [("debug.log", ChangeKind::Deleted)]);
        let unstaged: Vec<&str> = done.snap.status.unstaged.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(unstaged, [".gitignore"]);
        assert_eq!(git(&fx.dir, &["ls-files"]), "app/config.local.json\napp/main.rs");

        let entry = done.undo.expect("undoable");
        let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry.clone()));
        undone.result.expect("undo works");
        assert!(!file.exists());
        assert_eq!(git(&fx.dir, &["ls-files"]), "app/config.local.json\napp/main.rs\ndebug.log\nnew.log");
        assert_eq!(git(&fx.dir, &["status", "--porcelain"]), "M debug.log\nA  new.log");

        fx.run_as(entry.redo.clone(), Origin::Redo(entry)).result.expect("redo works");
        assert_eq!(git(&fx.dir, &["ls-files"]), "app/config.local.json\napp/main.rs");
        assert_eq!(fx.read(".gitignore"), "*.log\n");
    }

    #[test]
    fn a_folder_pattern_matches_the_tracked_files_inside() {
        let fx = Fixture::new("ignore-untrack-folder");
        fx.write("build out/a [1].o", "x\n");
        fx.write("build out/deep/b.o", "x\n");
        fx.write("src/build out/c.o", "x\n");
        fx.commit_all("first");
        let pattern = folder_pattern("build out").unwrap();
        let matches = fx.job(move |loader| check(loader, &pattern, &[])).unwrap();
        let paths: Vec<&str> = matches.tracked.iter().map(|t| t.path.as_str()).collect();
        assert_eq!(paths, ["build out/a [1].o", "build out/deep/b.o"]);
    }

    #[test]
    fn git_refusing_to_untrack_leaves_everything_as_it_was() {
        let fx = Fixture::new("ignore-untrack-refused");
        fx.write("secret.env", "one\n");
        fx.commit_all("first");
        // Staged content that is neither HEAD's nor the file's: git will not
        // drop it from the index without --force.
        fx.write("secret.env", "two\n");
        git(&fx.dir, &["add", "secret.env"]);
        fx.write("secret.env", "three\n");

        let matches = fx.job(|loader| check(loader, "/secret.env", &[])).unwrap();
        let file = fx.dir.join(".gitignore");
        let done = fx.run_as(ignore("/secret.env", &file, Target::GitIgnore, &matches.tracked), Origin::User);
        assert!(done.result.unwrap_err().contains("staged content different"));
        assert!(!file.exists());
        assert_eq!(git(&fx.dir, &["ls-files"]), "secret.env");
    }
}
