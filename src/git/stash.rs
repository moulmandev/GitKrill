//! Stashing with options: with or without untracked files, keeping the
//! staged changes, only the staged changes, or only some files.
//!
//! `GIT_LITERAL_PATHSPECS` breaks `git stash push --include-untracked`, so
//! paths are made literal one by one with the `:(literal)` pathspec magic
//! instead, which also keeps a name starting with `-` from reading as an
//! option.

use std::path::Path;

use super::ops::{GitCmd, Step};

/// What `git stash push` is asked to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StashOptions {
    pub message: String,
    /// `--include-untracked`: untracked files go into the stash too.
    pub untracked: bool,
    /// `--keep-index`: staged changes are stashed and also stay staged.
    pub keep_index: bool,
    /// `--staged`: only the staged changes (git 2.35 or later); the other
    /// two do not apply then.
    pub staged: bool,
    /// Only these files or folders (relative to the root); all when empty.
    pub paths: Vec<String>,
}

impl Default for StashOptions {
    fn default() -> Self {
        StashOptions { message: String::new(), untracked: true, keep_index: false, staged: false, paths: Vec::new() }
    }
}

/// The first git with `git stash push --staged`.
pub const STAGED_SINCE: (u32, u32) = (2, 35);

/// `(major, minor)` of `git --version`'s answer ("git version
/// 2.51.2.windows.1", "git version 2.39.3 (Apple Git-145)").
pub fn parse_version(text: &str) -> Option<(u32, u32)> {
    let number = text.strip_prefix("git version ")?.split_whitespace().next()?;
    let mut parts = number.split('.').map(|part| part.parse::<u32>().ok());
    Some((parts.next()??, parts.next()??))
}

/// Why the installed git cannot stash only the staged changes, if it is
/// known to be too old.
pub fn staged_unsupported() -> Option<String> {
    let text = super::cli::known_version()?;
    let version = parse_version(&text)?;
    (version < STAGED_SINCE).then(|| {
        let shown = text.strip_prefix("git version ").unwrap_or(&text);
        format!("Needs Git {}.{} or later; this is {shown}.", STAGED_SINCE.0, STAGED_SINCE.1)
    })
}

/// Whether `git stash push --staged` can take the staged changes back out
/// of the working tree. Git saves the stash first, then reverse-applies the
/// staged diff to the working files; when unstaged edits touch the same
/// lines that fails ("Cannot remove worktree changes"), leaving the stash
/// saved and every change still in place. This runs the same reverse apply
/// with `--check`; `Err` names the files it would fail on. For the reader
/// thread.
pub fn staged_check(workdir: &Path) -> Result<(), String> {
    use std::io::Write;
    use std::process::Stdio;
    // Plumbing, as git uses, so diff settings (prefixes) cannot interfere.
    let diff = super::cli::command(workdir)
        .args(["diff-index", "--cached", "-p", "--binary", "-U1", "HEAD"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("Failed to start git: {e}"))?;
    if !diff.status.success() || diff.stdout.is_empty() {
        // No HEAD or nothing staged: git says so itself.
        return Ok(());
    }
    let mut apply = super::cli::command(workdir)
        .args(["apply", "-R", "--check"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to start git: {e}"))?;
    if let Some(mut pipe) = apply.stdin.take() {
        let _ = pipe.write_all(&diff.stdout);
    }
    let out = apply.wait_with_output().map_err(|e| format!("git failed: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    // "error: docs/notes.md: patch does not apply"
    let stderr = String::from_utf8_lossy(&out.stderr);
    let files: Vec<&str> = stderr
        .lines()
        .filter_map(|line| line.strip_prefix("error: ")?.strip_suffix(": patch does not apply"))
        .collect();
    Err(match files.as_slice() {
        [] => stderr.trim().to_owned(),
        [one] => format!("Unstaged changes in {one} touch the staged lines, so git could not take them out of the working tree."),
        many => format!(
            "Unstaged changes in {} touch the staged lines, so git could not take them out of the working tree.",
            many.join(", ")
        ),
    })
}

/// `git stash push` as `options` say. Labelled "Stash changes" whatever
/// the options, so the toolbar and the palette see it running.
pub fn push(options: &StashOptions) -> GitCmd {
    let mut args: Vec<String> = vec!["stash".into(), "push".into()];
    if options.staged {
        args.push("--staged".into());
    } else {
        if options.untracked {
            args.push("--include-untracked".into());
        }
        if options.keep_index {
            args.push("--keep-index".into());
        }
    }
    let message = options.message.trim();
    if !message.is_empty() {
        args.extend(["-m".into(), message.to_owned()]);
    }
    // On the command line: `--pathspec-from-file` cannot go with `--staged`.
    if !options.paths.is_empty() {
        args.push("--".into());
        args.extend(options.paths.iter().map(|path| format!(":(literal){path}")));
    }

    let mut cmd = GitCmd::empty("Stash changes");
    cmd.steps.push(Step::new(args, None));
    let what = match options.paths.as_slice() {
        [] if options.staged => "the staged changes".to_owned(),
        [] => return cmd,
        [one] => super::ignore::file_name(one).to_owned(),
        many => format!("{} paths", many.len()),
    };
    cmd.done(format!("Stashed {what}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{ChangeKind, FileChange};
    use crate::tests::{Fixture, git};

    fn status(files: &[FileChange]) -> Vec<(&str, ChangeKind)> {
        files.iter().map(|f| (f.path.as_str(), f.kind)).collect()
    }

    /// The files a stash holds, untracked ones included.
    fn stashed(fx: &Fixture) -> String {
        git(&fx.dir, &["-c", "core.quotepath=false", "stash", "show", "--include-untracked", "--name-only", "stash@{0}"])
    }

    /// A repository with a committed `a.txt` and `b.txt`; `a.txt` has a
    /// staged and an unstaged change, `b.txt` an unstaged one, `new.txt` is
    /// untracked.
    fn mixed(name: &str) -> Fixture {
        let fx = Fixture::new(name);
        fx.write("a.txt", "a1\n");
        fx.write("b.txt", "b1\n");
        fx.commit_all("first");
        fx.write("a.txt", "a1\na2\n");
        git(&fx.dir, &["add", "a.txt"]);
        fx.write("a.txt", "a1\na2\na3\n");
        fx.write("b.txt", "b1\nb2\n");
        fx.write("new.txt", "new\n");
        fx
    }

    #[test]
    fn parses_git_versions() {
        assert_eq!(parse_version("git version 2.51.2.windows.1"), Some((2, 51)));
        assert_eq!(parse_version("git version 2.39.3 (Apple Git-145)"), Some((2, 39)));
        assert_eq!(parse_version("git version 2.34.1"), Some((2, 34)));
        assert_eq!(parse_version("git version 2"), None);
        assert_eq!(parse_version("hub version 2.40"), None);
        assert!(parse_version("git version 2.34.1").unwrap() < STAGED_SINCE);
        assert!(parse_version("git version 2.35.0").unwrap() >= STAGED_SINCE);
    }

    #[test]
    fn builds_the_command_line() {
        let args = |options: &StashOptions| push(options).steps[0].args.clone();
        assert_eq!(args(&StashOptions::default()), ["stash", "push", "--include-untracked"]);
        let options = StashOptions { untracked: false, keep_index: true, message: " wip ".into(), ..Default::default() };
        assert_eq!(args(&options), ["stash", "push", "--keep-index", "-m", "wip"]);
        // Only staged changes rules the other two out.
        let options = StashOptions { staged: true, keep_index: true, ..Default::default() };
        assert_eq!(args(&options), ["stash", "push", "--staged"]);
        let options = StashOptions { paths: vec!["-x".into(), "a b/[c]".into()], ..Default::default() };
        assert_eq!(args(&options), ["stash", "push", "--include-untracked", "--", ":(literal)-x", ":(literal)a b/[c]"]);
        assert!(!push(&options).steps[0].literal_paths, "GIT_LITERAL_PATHSPECS breaks --include-untracked");
    }

    #[test]
    fn stashes_everything_by_default() {
        let fx = mixed("stash-everything");
        let done = fx.run(push(&StashOptions::default()));
        assert!(done.snap.status.is_clean());
        assert_eq!(done.snap.stashes.len(), 1);
        assert_eq!(stashed(&fx), "a.txt\nb.txt\nnew.txt");
    }

    #[test]
    fn can_leave_untracked_files_out() {
        let fx = mixed("stash-tracked-only");
        let options = StashOptions { untracked: false, message: "tracked only".into(), ..Default::default() };
        let done = fx.run(push(&options));
        assert_eq!(status(&done.snap.status.unstaged), [("new.txt", ChangeKind::Untracked)]);
        assert!(done.snap.status.staged.is_empty());
        assert_eq!(done.snap.stashes[0].message, "On main: tracked only");
    }

    #[test]
    fn keep_index_stashes_staged_changes_and_keeps_them() {
        let fx = mixed("stash-keep-index");
        let options = StashOptions { keep_index: true, ..Default::default() };
        let done = fx.run(push(&options));
        assert_eq!(status(&done.snap.status.staged), [("a.txt", ChangeKind::Modified)]);
        assert!(done.snap.status.unstaged.is_empty());
        // The working file is the staged version now.
        assert_eq!(fx.read("a.txt"), "a1\na2\n");
        assert_eq!(stashed(&fx), "a.txt\nb.txt\nnew.txt");
    }

    #[test]
    fn staged_only_leaves_unstaged_and_untracked_changes() {
        let fx = Fixture::new("stash-staged");
        fx.write("a.txt", "a1\n");
        fx.write("b.txt", "b1\n");
        fx.commit_all("first");
        fx.write("a.txt", "a1\na2\n");
        git(&fx.dir, &["add", "a.txt"]);
        fx.write("b.txt", "b1\nb2\n");
        fx.write("new.txt", "new\n");

        let options = StashOptions { staged: true, ..Default::default() };
        let done = fx.run(push(&options));
        assert!(done.snap.status.staged.is_empty());
        assert_eq!(status(&done.snap.status.unstaged), [("b.txt", ChangeKind::Modified), ("new.txt", ChangeKind::Untracked)]);
        assert_eq!(fx.read("a.txt"), "a1\n");
        assert_eq!(fx.read("b.txt"), "b1\nb2\n");
        assert_eq!(stashed(&fx), "a.txt");
    }

    #[test]
    fn staged_only_is_checked_against_unstaged_changes_on_the_same_lines() {
        let fx = Fixture::new("stash-staged-overlap");
        fx.write("notes.md", "one\ntwo\n");
        fx.write("other.md", "a\nb\nc\nd\ne\nf\n");
        fx.commit_all("first");
        // Staged and unstaged changes far apart in one file: fine.
        fx.write("other.md", "a\nB\nc\nd\ne\nf\n");
        git(&fx.dir, &["add", "other.md"]);
        fx.write("other.md", "a\nB\nc\nd\ne\nF\n");
        assert_eq!(staged_check(&fx.dir), Ok(()));

        // An unstaged line right after a staged one.
        fx.write("notes.md", "one\ntwo\nstaged\n");
        git(&fx.dir, &["add", "notes.md"]);
        fx.write("notes.md", "one\ntwo\nstaged\nand more\n");
        let why = staged_check(&fx.dir).unwrap_err();
        assert!(why.contains("notes.md") && !why.contains("other.md"), "{why}");

        // Which is what git trips over: the stash is saved, nothing is
        // taken out of the working tree, and the command fails.
        let done = fx.run_as(push(&StashOptions { staged: true, ..Default::default() }), crate::worker::Origin::User);
        assert!(done.result.unwrap_err().contains("Cannot remove worktree changes"));
        assert_eq!(done.snap.stashes.len(), 1);
        assert_eq!(fx.read("notes.md"), "one\ntwo\nstaged\nand more\n");

        // Once the unstaged edit is gone, the check passes and so does git.
        git(&fx.dir, &["stash", "drop", "-q"]);
        fx.write("notes.md", "one\ntwo\nstaged\n");
        assert_eq!(staged_check(&fx.dir), Ok(()));
        fx.run(push(&StashOptions { staged: true, ..Default::default() }));
        assert_eq!(fx.read("notes.md"), "one\ntwo\n");
        assert_eq!(fx.read("other.md"), "a\nb\nc\nd\ne\nF\n");
    }

    #[test]
    fn staged_only_with_nothing_staged_is_refused_by_git() {
        let fx = Fixture::new("stash-staged-nothing");
        fx.write("a.txt", "a1\n");
        fx.commit_all("first");
        fx.write("a.txt", "a2\n");
        let done = fx.run_as(push(&StashOptions { staged: true, ..Default::default() }), crate::worker::Origin::User);
        assert!(done.result.unwrap_err().contains("No staged changes"));
        assert_eq!(fx.read("a.txt"), "a2\n");
    }

    #[test]
    fn a_file_with_staged_and_unstaged_changes_is_stashed_whole() {
        let fx = mixed("stash-one-file");
        let options = StashOptions { paths: vec!["a.txt".into()], ..Default::default() };
        let done = fx.run(push(&options));
        assert_eq!(done.result.as_deref().map(|_| ()), Ok(()));
        // As git does: the file's index entry and working file go back to
        // HEAD; the other files are left alone.
        assert_eq!(fx.read("a.txt"), "a1\n");
        assert!(done.snap.status.staged.is_empty());
        assert_eq!(status(&done.snap.status.unstaged), [("b.txt", ChangeKind::Modified), ("new.txt", ChangeKind::Untracked)]);

        // Applied with its index, both changes come back as they were.
        git(&fx.dir, &["stash", "apply", "--index"]);
        assert_eq!(fx.read("a.txt"), "a1\na2\na3\n");
        assert_eq!(git(&fx.dir, &["show", ":a.txt"]), "a1\na2");
    }

    #[test]
    fn keep_index_with_a_path_keeps_its_staged_change() {
        let fx = mixed("stash-one-file-keep-index");
        let options = StashOptions { paths: vec!["a.txt".into()], keep_index: true, ..Default::default() };
        let done = fx.run(push(&options));
        assert_eq!(status(&done.snap.status.staged), [("a.txt", ChangeKind::Modified)]);
        assert_eq!(fx.read("a.txt"), "a1\na2\n");
        assert_eq!(fx.read("b.txt"), "b1\nb2\n");
    }

    #[test]
    fn untracked_files_and_folders_by_path() {
        let fx = Fixture::new("stash-paths-untracked");
        fx.write("app/a.txt", "a1\n");
        fx.write("keep.txt", "k1\n");
        fx.commit_all("first");
        fx.write("app/a.txt", "a2\n");
        fx.write("app/new/n.txt", "n\n");
        fx.write("keep.txt", "k2\n");
        fx.write("loose.txt", "l\n");

        // A folder: tracked and untracked changes inside it, nothing outside.
        let done = fx.run(push(&StashOptions { paths: vec!["app".into()], ..Default::default() }));
        assert_eq!(status(&done.snap.status.unstaged), [("keep.txt", ChangeKind::Modified), ("loose.txt", ChangeKind::Untracked)]);
        assert_eq!(stashed(&fx), "app/a.txt\napp/new/n.txt");

        // One untracked file.
        let done = fx.run(push(&StashOptions { paths: vec!["loose.txt".into()], ..Default::default() }));
        assert_eq!(status(&done.snap.status.unstaged), [("keep.txt", ChangeKind::Modified)]);
        assert_eq!(done.snap.stashes.len(), 2);
    }

    #[test]
    fn a_staged_rename_is_stashed_by_its_new_path() {
        let fx = Fixture::new("stash-rename");
        fx.write("old name.txt", "same\n");
        fx.write("other.txt", "o1\n");
        fx.commit_all("first");
        git(&fx.dir, &["mv", "old name.txt", "new name.txt"]);
        fx.write("other.txt", "o2\n");
        // Git cannot stash a path only HEAD has (the old name): it refuses,
        // or with untracked files saves the stash and then fails. So only
        // the new name goes, and the deletion of the old one stays staged.
        let options = StashOptions { paths: vec!["new name.txt".into()], ..Default::default() };
        let done = fx.run(push(&options));
        assert_eq!(status(&done.snap.status.staged), [("old name.txt", ChangeKind::Deleted)]);
        assert_eq!(status(&done.snap.status.unstaged), [("other.txt", ChangeKind::Modified)]);
        assert!(!fx.dir.join("new name.txt").exists());
        assert_eq!(done.snap.stashes.len(), 1);
    }

    #[test]
    fn odd_file_names_are_taken_literally() {
        let fx = Fixture::new("stash-odd-names");
        // Each named file next to one its name would match as a glob or
        // read as an option.
        let mut named = vec!["a b.txt", "[x].txt", "-dash.txt", "ünï 文件.txt", "dir with space/f.txt"];
        let mut others = vec!["x.txt", "ab.txt", "dash.txt", "dir/f.txt"];
        if !cfg!(windows) {
            named.extend(["*.txt", "q?.md"]);
            others.extend(["qa.md"]);
        }
        for file in named.iter().chain(&others) {
            fx.write(file, "one\n");
        }
        fx.commit_all("first");
        for file in named.iter().chain(&others) {
            fx.write(file, "two\n");
        }
        fx.write("[n].txt", "untracked\n");
        fx.write("n.txt", "untracked\n");

        let mut paths: Vec<String> = named.iter().map(|p| (*p).to_owned()).collect();
        paths.push("[n].txt".to_owned());
        let done = fx.run(push(&StashOptions { paths, message: "odd".into(), ..Default::default() }));
        let mut left: Vec<&str> = done.snap.status.unstaged.iter().map(|f| f.path.as_str()).collect();
        left.sort_unstable();
        let mut expected = others.clone();
        expected.push("n.txt");
        expected.sort_unstable();
        assert_eq!(left, expected);
        for file in &named {
            assert_eq!(fx.read(file), "one\n", "{file} was stashed");
        }
        assert!(!fx.dir.join("[n].txt").exists());
    }

    #[test]
    fn a_path_without_changes_stashes_nothing() {
        let fx = mixed("stash-clean-path");
        fx.write("clean.txt", "c\n");
        fx.commit_all("second");
        fx.write("b.txt", "b1\nb2\n");
        let done = fx.run(push(&StashOptions { paths: vec!["clean.txt".into()], ..Default::default() }));
        assert!(done.snap.stashes.is_empty());
        assert_eq!(fx.read("b.txt"), "b1\nb2\n");
    }
}
