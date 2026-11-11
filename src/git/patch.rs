//! Patch files: commits saved as mails (`git format-patch`), uncommitted
//! changes saved as a diff, and a patch file checked ([`inspect`]) and
//! applied ([`apply`]): to the working tree with `git apply`, undoably, or
//! as commits with `git am`.

use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use super::cli;
use super::internal::{FileState, FileUndo, Internal};
use super::ops::{GitCmd, Step, UndoKind};
use super::Oid;

/// Runs git in `dir` and returns its standard output as bytes (patches need
/// not be UTF-8), feeding `stdin` and adding `env`.
fn git_bytes(dir: &Path, args: &[&str], stdin: Option<&[u8]>, env: &[(&str, &OsStr)]) -> Result<Vec<u8>, String> {
    let mut cmd = cli::command(dir);
    for (key, value) in env {
        cmd.env(key, value);
    }
    cmd.args(args)
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => cli::NOT_FOUND.to_owned(),
        _ => format!("Failed to start git: {e}"),
    })?;
    if let (Some(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) {
        let _ = pipe.write_all(bytes);
    }
    let out = child.wait_with_output().map_err(|e| format!("git failed: {e}"))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        Err(if err.is_empty() { format!("git {} failed ({})", args.join(" "), out.status) } else { err })
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string())
}

/// "1 patch", "3 patches".
pub fn patches(n: usize) -> String {
    if n == 1 { "1 patch".to_owned() } else { format!("{n} patches") }
}

// ---- saving ------------------------------------------------------------------

/// The name `git format-patch` gives the `number`th mail of a commit with
/// this subject: "0001-Fix-the-parser.patch".
pub fn mail_file_name(number: usize, subject: &str) -> String {
    let mut name = String::new();
    for c in subject.chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
            name.push(c);
        } else if !name.ends_with('-') {
            name.push('-');
        }
        if name.len() >= 64 {
            break;
        }
    }
    let name = name.trim_matches(['-', '.']);
    format!("{number:04}-{}.patch", if name.is_empty() { "patch" } else { name })
}

/// One `git format-patch` mail per commit in `dir`, numbered in the order
/// given (oldest first, so that `git am` applies them in that order).
/// Merge commits have no mail: leave them out.
pub fn save_each(commits: &[Oid], dir: &Path) -> GitCmd {
    let n = commits.len();
    let dir = dir.display().to_string();
    let mut cmd = GitCmd::empty(format!("Save {}", patches(n))).done(format!("Saved {}", patches(n)));
    for (i, oid) in commits.iter().enumerate() {
        let number = format!("--start-number={}", i + 1);
        cmd = cmd.step(&["format-patch", "-1", &number, "-o", &dir, &oid.to_string()]);
    }
    // The last line of the output is what the success toast says.
    cmd.steps.push(Step::internal(Internal::new("say where", move |_| Ok(format!("In {dir}")))));
    cmd
}

/// The commits' `git format-patch` mails one after the other in one file,
/// which `git am` reads as a mailbox.
pub fn save_combined(commits: &[Oid], file: &Path) -> GitCmd {
    let n = commits.len();
    let shas: Vec<String> = commits.iter().map(Oid::to_string).collect();
    let file = file.to_path_buf();
    let mut cmd = GitCmd::empty(format!("Save {}", patches(n))).done(format!("Saved {}", file_name(&file)));
    cmd.steps.push(Step::internal(Internal::new(format!("format-patch into {}", file.display()), move |workdir| {
        let mut mailbox = Vec::new();
        for sha in &shas {
            mailbox.extend(git_bytes(workdir, &["format-patch", "-1", "--stdout", sha], None, &[])?);
        }
        if mailbox.is_empty() {
            return Err("Merge commits have no patch to save.".to_owned());
        }
        std::fs::write(&file, mailbox).map_err(|e| format!("Could not write {}: {e}", file.display()))?;
        Ok(file.display().to_string())
    })));
    cmd
}

/// Which uncommitted changes [`save_changes`] writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Changes {
    /// The index against HEAD.
    Staged,
    /// The working tree against the index, untracked files as new files.
    Unstaged,
    /// The working tree against HEAD, untracked files as new files.
    All,
}

impl Changes {
    pub fn label(self) -> &'static str {
        match self {
            Changes::Staged => "staged changes",
            Changes::Unstaged => "unstaged changes",
            Changes::All => "all changes",
        }
    }

    /// The file name a save dialog suggests.
    pub fn file_name(self) -> &'static str {
        match self {
            Changes::Staged => "staged.patch",
            Changes::Unstaged => "unstaged.patch",
            Changes::All => "changes.patch",
        }
    }
}

/// What `git diff` needs to write a patch `git apply` reads back whatever
/// the user's configuration says (no prefixes, external diff tools,
/// textconv filters, a relative root, submodule logs).
const DIFF: &[&str] = &[
    "diff",
    "--binary",
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--no-relative",
    "--src-prefix=a/",
    "--dst-prefix=b/",
    "--submodule=short",
];

/// Saves the uncommitted `changes` as a patch file. Untracked files go in
/// as new files: they are added "intent to add" to a copy of the index, so
/// the real one is never touched.
pub fn save_changes(changes: Changes, file: &Path) -> GitCmd {
    let file = file.to_path_buf();
    let mut cmd = GitCmd::empty(format!("Save {} as a patch", changes.label())).done(format!("Saved {}", file_name(&file)));
    cmd.steps.push(Step::internal(Internal::new(format!("diff into {}", file.display()), move |workdir| {
        let patch = diff_changes(workdir, changes)?;
        if patch.is_empty() {
            return Err(format!("There are no {} to save.", changes.label()));
        }
        std::fs::write(&file, patch).map_err(|e| format!("Could not write {}: {e}", file.display()))?;
        Ok(file.display().to_string())
    })));
    cmd
}

/// The patch [`save_changes`] writes.
pub fn diff_changes(workdir: &Path, changes: Changes) -> Result<Vec<u8>, String> {
    if changes == Changes::Staged {
        let mut args = DIFF.to_vec();
        args.push("--cached");
        return git_bytes(workdir, &args, None, &[]);
    }
    let untracked = git_bytes(workdir, &["ls-files", "-z", "--others", "--exclude-standard"], None, &[])?;
    let index = TempIndex::copy_of(workdir)?;
    let env = [("GIT_INDEX_FILE", index.path.as_os_str())];
    if !untracked.is_empty() {
        let args = ["add", "--intent-to-add", "--pathspec-from-file=-", "--pathspec-file-nul"];
        git_bytes(workdir, &args, Some(&untracked), &[env[0], ("GIT_LITERAL_PATHSPECS", OsStr::new("1"))])?;
    }
    let mut args = DIFF.to_vec();
    let base;
    if changes == Changes::All {
        // Against HEAD, or the empty tree before the first commit.
        base = match git_bytes(workdir, &["rev-parse", "-q", "--verify", "HEAD^{commit}"], None, &[]) {
            Ok(_) => "HEAD".to_owned(),
            Err(_) => String::from_utf8_lossy(&git_bytes(workdir, &["hash-object", "-t", "tree", "--stdin"], Some(b""), &[])?)
                .trim()
                .to_owned(),
        };
        args.push(&base);
    }
    git_bytes(workdir, &args, None, &env)
}

/// A copy of the repository's index in the temporary folder, removed when
/// dropped.
struct TempIndex {
    path: PathBuf,
}

impl TempIndex {
    fn copy_of(workdir: &Path) -> Result<TempIndex, String> {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNT: AtomicU32 = AtomicU32::new(0);
        let real = git_bytes(workdir, &["rev-parse", "--git-path", "index"], None, &[])?;
        let real = workdir.join(String::from_utf8_lossy(&real).trim());
        let n = COUNT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("gitkrill-index-{}-{n}", std::process::id()));
        if real.is_file() {
            std::fs::copy(&real, &path).map_err(|e| format!("Could not copy the index: {e}"))?;
        }
        Ok(TempIndex { path })
    }
}

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

// ---- checking ----------------------------------------------------------------

/// What a patch file holds and how it would apply now, read on the reader
/// thread when the file is picked ([`inspect`]).
#[derive(Clone, Debug)]
pub struct PatchInfo {
    pub path: PathBuf,
    /// The subjects of its mails, in order, when it is `git format-patch`
    /// output (or another mailbox): those can be committed with `git am`.
    pub mails: Vec<String>,
    /// The files it changes (both names of a rename), as git reads it.
    pub files: Vec<String>,
    /// Applying it to the working tree alone.
    pub tree: Check,
    /// Applying it to the working tree and the index (staging the changes,
    /// or committing the mails).
    pub index: Check,
    /// Why the mails cannot be committed now (staged changes), if so.
    pub commit_blocker: Option<String>,
}

/// How a patch applies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Check {
    /// As it is.
    Clean,
    /// Not as it is, but with a three-way merge: these files end up with
    /// conflicts to resolve (none when the merge sorts everything out).
    Merge { conflicts: Vec<String> },
    /// Not even so: what does not apply, and why.
    Fails(Vec<Failure>),
}

/// A file of a patch that does not apply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// `None` when git did not name one.
    pub path: Option<String>,
    pub why: String,
}

/// Reads the patch file at `path` and checks how it applies to the
/// repository at `workdir`. An error says why it is not a patch git can
/// read.
pub fn inspect(workdir: &Path, path: &Path) -> Result<PatchInfo, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("Could not read {}: {e}", path.display()))?;
    let mails = mail_subjects(&String::from_utf8_lossy(&bytes));
    let shown = path.display().to_string();
    let numstat = git_bytes(workdir, &["apply", "--numstat", "-z", &shown], None, &[])
        .map_err(|e| format!("{} is not a patch git can read: {}", file_name(path), first_error(&e)))?;
    let files = numstat_files(&numstat);
    if files.is_empty() {
        return Err(format!("{} changes no files.", file_name(path)));
    }
    let tree = check(workdir, &shown, false, &files);
    let index = check(workdir, &shown, true, &files);
    let commit_blocker = if mails.is_empty() {
        None
    } else {
        cli::run(workdir, &["diff".into(), "--cached".into(), "--quiet".into()], cli::Options::default())
            .err()
            .map(|_| "Commit or unstage the staged changes first: git am needs them out of the way.".to_owned())
    };
    Ok(PatchInfo { path: path.to_path_buf(), mails, files, tree, index, commit_blocker })
}

/// `git apply --check`, then with `--3way` when that fails.
fn check(workdir: &Path, patch: &str, index: bool, files: &[String]) -> Check {
    let run = |extra: &[&str]| {
        let mut args: Vec<String> = vec!["apply".into(), "--check".into()];
        args.extend(extra.iter().map(|a| (*a).to_owned()));
        args.push(patch.to_owned());
        cli::run(workdir, &args, cli::Options::default())
    };
    let plain = match run(if index { &["--index"] } else { &[] }) {
        Ok(_) => return Check::Clean,
        Err(e) => e,
    };
    // A three-way check succeeds even when it leaves conflicts, and says so
    // per file.
    match run(&["--3way"]) {
        Ok(out) => Check::Merge {
            conflicts: files.iter().filter(|f| out.contains(&format!("'{f}' with conflicts"))).cloned().collect(),
        },
        Err(_) => Check::Fails(failures(&plain, files)),
    }
}

/// The files `git apply`'s errors name, with the reason given for each;
/// one failure without a file when none of its errors names one.
pub fn failures(message: &str, files: &[String]) -> Vec<Failure> {
    let mut out: Vec<Failure> = Vec::new();
    for line in message.lines() {
        let Some(rest) = line.strip_prefix("error: ") else { continue };
        // "patch failed: a.txt:12" and "a.txt: patch does not apply": the
        // longest known file name that fits (one name may start another).
        let found = files
            .iter()
            .filter_map(|f| {
                if let Some(at) = rest.strip_prefix("patch failed: ").and_then(|r| r.strip_prefix(f.as_str()))
                    && at.starts_with(':')
                {
                    return Some((f, None));
                }
                rest.strip_prefix(f.as_str()).and_then(|r| r.strip_prefix(": ")).map(|why| (f, Some(why)))
            })
            .max_by_key(|(f, _)| f.len());
        let Some((path, why)) = found else { continue };
        match out.iter_mut().find(|known| known.path.as_deref() == Some(path)) {
            Some(known) => {
                if let Some(why) = why {
                    known.why = why.to_owned();
                }
            }
            None => out.push(Failure { path: Some(path.clone()), why: why.unwrap_or("patch does not apply").to_owned() }),
        }
    }
    if out.is_empty() {
        out.push(Failure { path: None, why: first_error(message).to_owned() });
    }
    out
}

/// The first line of git's message that says something ("error: ..." or
/// "fatal: ..." without the prefix).
fn first_error(message: &str) -> &str {
    let line = message
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("error:") || l.starts_with("fatal:"))
        .or_else(|| message.lines().map(str::trim).find(|l| !l.is_empty()))
        .unwrap_or("");
    line.strip_prefix("error:").or_else(|| line.strip_prefix("fatal:")).unwrap_or(line).trim()
}

/// The paths of `git apply --numstat -z` output, each once: a renamed
/// file's old and new names both.
pub fn numstat_files(out: &[u8]) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    let mut add = |path: &[u8]| {
        let path = String::from_utf8_lossy(path).into_owned();
        if !path.is_empty() && !files.contains(&path) {
            files.push(path);
        }
    };
    let mut fields = out.split(|b| *b == 0);
    while let Some(record) = fields.next() {
        if record.is_empty() {
            continue;
        }
        // "added\tdeleted\tpath", or "added\tdeleted\t" then the old and
        // the new name as fields of their own.
        let mut parts = record.splitn(3, |b| *b == b'\t');
        let (_, _, path) = (parts.next(), parts.next(), parts.next().unwrap_or_default());
        if path.is_empty() {
            if let Some(old) = fields.next() {
                add(old);
            }
            if let Some(new) = fields.next() {
                add(new);
            }
        } else {
            add(path);
        }
    }
    files
}

/// The subjects of the mails in `text`, without git's "[PATCH 2/3]"; empty
/// when it is a plain diff. A mail starts the text with headers, or after
/// the "From <sha> <date>" line `git format-patch` writes.
pub fn mail_subjects(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut starts: Vec<usize> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let separator = line.strip_prefix("From ").is_some_and(|rest| {
            let sha = rest.split(' ').next().unwrap_or("");
            (sha.len() == 40 || sha.len() == 64) && sha.bytes().all(|b| b.is_ascii_hexdigit())
        });
        if separator || (i == 0 && is_header(line)) {
            starts.push(i);
        }
    }
    starts
        .iter()
        .filter_map(|&start| {
            let mut subject: Option<String> = None;
            for line in lines[start..].iter().skip(usize::from(lines[start].starts_with("From "))) {
                if line.trim().is_empty() {
                    break;
                }
                match &mut subject {
                    // A long subject continues on lines that start with a space.
                    Some(text) if line.starts_with([' ', '\t']) => {
                        text.push(' ');
                        text.push_str(line.trim());
                    }
                    Some(_) => break,
                    None => {
                        if let Some(value) = line.strip_prefix("Subject:") {
                            subject = Some(value.trim().to_owned());
                        }
                    }
                }
            }
            subject.map(|s| strip_patch_prefix(&s))
        })
        .collect()
}

fn is_header(line: &str) -> bool {
    line.split_once(": ").is_some_and(|(name, _)| {
        !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

/// "[PATCH v2 1/3] Fix it" is "Fix it".
fn strip_patch_prefix(subject: &str) -> String {
    let mut rest = subject.trim();
    while let Some(after) = rest.strip_prefix('[').and_then(|r| r.split_once(']')).map(|(_, after)| after.trim_start()) {
        rest = after;
    }
    rest.to_owned()
}

// ---- applying ----------------------------------------------------------------

/// How [`apply`] applies a patch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum How {
    /// Its mails become commits, with their authors and messages (`git am
    /// --3way`). Undo puts the branch back where it was.
    Commit,
    /// Its changes go in the working tree only.
    Tree,
    /// In the working tree and staged.
    Stage,
}

/// Applies the patch `info` describes, the way its check says it can: as
/// it is, with a three-way merge (conflicts are left to resolve), or the
/// parts that apply, the rest saved next to each file as `<file>.rej`.
/// The working-tree ways are undoable: the files are put back as they were,
/// and so are their entries in the index.
pub fn apply(workdir: &Path, info: &PatchInfo, how: How) -> GitCmd {
    let name = file_name(&info.path);
    let patch = info.path.display().to_string();
    if how == How::Commit {
        let n = info.mails.len();
        return GitCmd::new(format!("Apply {}", patches(n)), &["am", "--3way", &patch])
            .done(format!("Committed {}", patches(n)))
            .undoable(UndoKind::HeadMove);
    }

    let check = if how == How::Stage { &info.index } else { &info.tree };
    let mut args: Vec<String> = vec!["apply".into(), "--whitespace=nowarn".into()];
    match check {
        Check::Clean => {}
        Check::Merge { .. } => args.push("--3way".into()),
        Check::Fails(_) => args.push("--reject".into()),
    }
    if how == How::Stage && !matches!(check, Check::Merge { .. }) {
        args.push("--index".into());
    }
    args.push(patch);
    // A three-way merge writes the index too, whatever was asked.
    let touches_index = how == How::Stage || matches!(check, Check::Merge { .. });

    let done = match check {
        Check::Merge { conflicts } if !conflicts.is_empty() => format!("Applied {name}, with conflicts"),
        Check::Fails(_) => format!("Applied part of {name}"),
        _ => format!("Applied {name}"),
    };
    let mut cmd = GitCmd::empty(format!("Apply {name}")).done(done);
    let mut undo_steps = Vec::new();
    if touches_index {
        let (note, restore) = index_entries(info.files.clone());
        cmd.steps.push(note);
        undo_steps.push(restore);
    }
    let files = info.files.clone();
    cmd.steps.push(Step::internal(Internal::new(format!("git {}", args.join(" ")), move |workdir| {
        match cli::run(workdir, &args, cli::Options::default()) {
            Ok(_) => Ok(String::new()),
            // Both leave the repository changed and say so; what is left
            // to do is the summary of the command.
            Err(e) if e.contains("with conflicts") || e.contains("Rejected hunk") => Ok(outcome(&e, &files)),
            Err(e) => Err(e),
        }
    })));

    let mut states: Vec<FileState> = info.files.iter().map(|f| FileState::new(workdir.join(f))).collect();
    if matches!(check, Check::Fails(_)) {
        states.extend(info.files.iter().map(|f| FileState::new(workdir.join(format!("{f}.rej")))));
    }
    cmd.undoable(UndoKind::Files(FileUndo { files: states, undo_steps }))
}

/// What a three-way or partial apply left to do, from git's output.
fn outcome(message: &str, files: &[String]) -> String {
    let conflicted: Vec<&str> =
        files.iter().filter(|f| message.contains(&format!("'{f}' with conflicts"))).map(String::as_str).collect();
    let rejected: Vec<&str> = files
        .iter()
        .filter(|f| message.contains(&format!("Applying patch {f} with ")) && !message.contains(&format!("Applying patch {f} with 0 reject")))
        .map(String::as_str)
        .collect();
    let list = |names: &[&str]| match names {
        [] => String::new(),
        [one] => (*one).to_owned(),
        many if many.len() <= 4 => many.join(", "),
        many => format!("{} and {} more", many[..3].join(", "), many.len() - 3),
    };
    match (conflicted.is_empty(), rejected.is_empty()) {
        (false, _) => format!("Conflicts in {}: resolve them, then stage the files.", list(&conflicted)),
        (true, false) => format!("Did not apply to {}: the rejected parts are in .rej files next to them.", list(&rejected)),
        (true, true) => "Parts of the patch did not apply: see the .rej files.".to_owned(),
    }
}

/// Two internal steps: one that notes the index entries of `files` (every
/// stage), and one that puts them back as noted, removing what was not
/// there. The first runs with the command (again on redo), the second on
/// undo; they share what was noted.
fn index_entries(files: Vec<String>) -> (Step, Step) {
    let noted: Arc<Mutex<Vec<u8>>> = Arc::default();
    let into = noted.clone();
    let wanted = files.clone();
    let note = Step::internal(Internal::new("note index entries", move |workdir| {
        let all = git_bytes(workdir, &["ls-files", "-s", "-z"], None, &[])?;
        let mut kept = Vec::new();
        for entry in all.split(|b| *b == 0).filter(|e| !e.is_empty()) {
            // "<mode> <sha> <stage>\t<path>"
            let path = entry.iter().position(|b| *b == b'\t').map(|tab| &entry[tab + 1..]).unwrap_or_default();
            if wanted.iter().any(|f| f.as_bytes() == path) {
                kept.extend_from_slice(entry);
                kept.push(0);
            }
        }
        *into.lock().map_err(|_| "index notes unavailable")? = kept;
        Ok(String::new())
    }));
    let restore = Step::internal(Internal::new("put index entries back", move |workdir| {
        let format = git_bytes(workdir, &["rev-parse", "--show-object-format"], None, &[]).unwrap_or_default();
        let zero = if String::from_utf8_lossy(&format).trim() == "sha256" { "0".repeat(64) } else { "0".repeat(40) };
        let mut input = Vec::new();
        // Mode 0 removes the path from the index, every stage of it.
        for file in &files {
            input.extend_from_slice(format!("0 {zero}\t").as_bytes());
            input.extend_from_slice(file.as_bytes());
            input.push(0);
        }
        input.extend_from_slice(&noted.lock().map_err(|_| "index notes unavailable")?);
        git_bytes(workdir, &["update-index", "-z", "--index-info"], Some(&input), &[])?;
        Ok(String::new())
    }));
    (note, restore)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::RepoState;
    use crate::tests::{Fixture, git};
    use crate::worker::Origin;

    #[test]
    fn mail_subjects_are_read_from_format_patch_output_only() {
        let mbox = "From 0123456789abcdef0123456789abcdef01234567 Mon Sep 17 00:00:00 2001\n\
                    From: A <a@example.com>\n\
                    Date: Thu, 1 Jan 2026 00:00:00 +0000\n\
                    Subject: [PATCH 1/2] Fix the parser so that\n it reads long lines\n\
                    \n\
                    From the start of the body: not a mail.\n\
                    ---\n\
                    diff --git a/x b/x\n\
                    From 89abcdef0123456789abcdef0123456789abcdef Mon Sep 17 00:00:00 2001\n\
                    From: B <b@example.com>\n\
                    Subject: [PATCH 2/2] Second\n\
                    \n";
        assert_eq!(mail_subjects(mbox), vec!["Fix the parser so that it reads long lines", "Second"]);
        assert_eq!(mail_subjects("diff --git a/x b/x\nindex 1..2\n"), Vec::<String>::new());
        assert_eq!(mail_subjects("Subject: [RFC] [PATCH] One\nFrom: c\n\nbody\n"), vec!["One"]);
    }

    #[test]
    fn numstat_names_both_sides_of_a_rename() {
        let out = b"1\t1\tplain.txt\x000\t0\t\x00old name.txt\x00new name.txt\x00-\t-\tpic.png\x00";
        assert_eq!(numstat_files(out), vec!["plain.txt", "old name.txt", "new name.txt", "pic.png"]);
    }

    #[test]
    fn failures_name_the_files_git_complains_about() {
        let files = vec!["a.txt".to_owned(), "a.txt.bak".to_owned(), "dir/b c.txt".to_owned()];
        let message = "error: patch failed: a.txt:3\nerror: a.txt: patch does not apply\n\
                       error: dir/b c.txt: No such file or directory";
        assert_eq!(
            failures(message, &files),
            vec![
                Failure { path: Some("a.txt".into()), why: "patch does not apply".into() },
                Failure { path: Some("dir/b c.txt".into()), why: "No such file or directory".into() },
            ]
        );
        assert_eq!(
            failures("error: corrupt patch at line 4", &files),
            vec![Failure { path: None, why: "corrupt patch at line 4".into() }]
        );
    }

    /// A repository with `a.txt` ("one two three") committed, and a patch
    /// file outside it that changes "two" to "TWO" (a commit made and then
    /// taken back).
    fn with_patch(name: &str) -> (Fixture, PathBuf, PathBuf) {
        let fx = Fixture::new(name);
        fx.write("a.txt", "one\ntwo\nthree\n");
        fx.write("dir/naïve file.txt", "keep\n");
        fx.commit_all("base");
        fx.write("a.txt", "one\nTWO\nthree\n");
        fx.write("dir/naïve file.txt", "keep\nmore\n");
        fx.commit_all("Change two");
        let out = fx.dir.with_extension("out");
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(&out).unwrap();
        let head = Oid::from_str(&git(&fx.dir, &["rev-parse", "HEAD"])).unwrap();
        fx.run(save_each(&[head], &out));
        let mail = std::fs::read_dir(&out).unwrap().next().unwrap().unwrap().path();
        let diff = out.join("plain diff.patch");
        std::fs::write(&diff, git_bytes(&fx.dir, &["diff", "HEAD~1", "HEAD"], None, &[]).unwrap()).unwrap();
        git(&fx.dir, &["reset", "-q", "--hard", "HEAD~1"]);
        (fx, mail, diff)
    }

    fn inspected(fx: &Fixture, path: &Path) -> PatchInfo {
        inspect(&fx.dir, path).expect("a patch")
    }

    #[test]
    fn a_saved_commit_is_a_mail_that_commits_again_and_undoes() {
        let (fx, mail, _) = with_patch("patch-am");
        assert!(mail.file_name().unwrap().to_string_lossy().starts_with("0001-Change-two"));
        let info = inspected(&fx, &mail);
        assert_eq!(info.mails, vec!["Change two"]);
        assert_eq!(info.files, vec!["a.txt", "dir/naïve file.txt"]);
        assert!(info.tree == Check::Clean && info.index == Check::Clean);
        assert_eq!(info.commit_blocker, None);

        let before = git(&fx.dir, &["rev-parse", "HEAD"]);
        let done = fx.run(apply(&fx.dir, &info, How::Commit));
        assert_eq!(git(&fx.dir, &["log", "-1", "--format=%s"]), "Change two");
        assert_eq!(fx.read("a.txt"), "one\nTWO\nthree\n");
        let entry = done.undo.expect("undoable");
        fx.run_as(entry.undo.clone(), Origin::Undo(entry)).result.expect("undo works");
        assert_eq!(git(&fx.dir, &["rev-parse", "HEAD"]), before);
    }

    #[test]
    fn staged_changes_block_committing_mails() {
        let (fx, mail, _) = with_patch("patch-am-dirty");
        fx.write("other.txt", "staged\n");
        git(&fx.dir, &["add", "other.txt"]);
        assert!(inspected(&fx, &mail).commit_blocker.is_some());
    }

    #[test]
    fn a_plain_diff_applies_to_the_working_tree_and_undoes() {
        let (fx, _, diff) = with_patch("patch-apply");
        let info = inspected(&fx, &diff);
        assert!(info.mails.is_empty());
        let done = fx.run(apply(&fx.dir, &info, How::Tree));
        assert_eq!(fx.read("a.txt"), "one\nTWO\nthree\n");
        assert_eq!(fx.read("dir/naïve file.txt"), "keep\nmore\n");
        assert!(done.snap.status.staged.is_empty(), "nothing staged");

        let entry = done.undo.expect("undoable");
        fx.run_as(entry.undo.clone(), Origin::Undo(entry.clone())).result.expect("undo works");
        assert_eq!(fx.read("a.txt"), "one\ntwo\nthree\n");
        fx.run_as(entry.redo.clone(), Origin::Redo(entry)).result.expect("redo works");
        assert_eq!(fx.read("a.txt"), "one\nTWO\nthree\n");
    }

    #[test]
    fn staging_a_patch_is_undone_in_the_index_too() {
        let (fx, _, diff) = with_patch("patch-stage");
        // Something else staged stays staged through apply and undo.
        fx.write("other.txt", "mine\n");
        git(&fx.dir, &["add", "other.txt"]);
        let info = inspected(&fx, &diff);
        let done = fx.run(apply(&fx.dir, &info, How::Stage));
        let staged: Vec<&str> = done.snap.status.staged.iter().map(|f| f.path.as_str()).collect();
        assert!(staged.contains(&"a.txt") && staged.contains(&"other.txt"), "{staged:?}");

        let entry = done.undo.expect("undoable");
        let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry));
        undone.result.expect("undo works");
        let staged: Vec<&str> = undone.snap.status.staged.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(staged, vec!["other.txt"]);
        assert_eq!(fx.read("a.txt"), "one\ntwo\nthree\n");
    }

    #[test]
    fn a_patch_that_conflicts_merges_three_ways_and_undoes() {
        let (fx, mail, _) = with_patch("patch-3way");
        fx.write("a.txt", "one\ntwo, changed here\nthree\n");
        fx.commit_all("Local change");
        let info = inspected(&fx, &mail);
        assert_eq!(info.tree, Check::Merge { conflicts: vec!["a.txt".to_owned()] });

        let done = fx.run(apply(&fx.dir, &info, How::Tree));
        assert!(done.result.as_deref().unwrap().contains("Conflicts in a.txt"), "{:?}", done.result);
        assert!(fx.read("a.txt").contains("<<<<<<<"));
        assert!(done.snap.status.unstaged.iter().any(|f| f.path == "a.txt" && f.kind == crate::git::ChangeKind::Conflicted));

        let entry = done.undo.expect("undoable");
        let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry));
        undone.result.expect("undo works");
        assert!(undone.snap.status.is_clean(), "back to a clean tree");
        assert_eq!(fx.read("a.txt"), "one\ntwo, changed here\nthree\n");
    }

    #[test]
    fn mails_that_conflict_stop_and_continue_like_a_merge() {
        let (fx, mail, _) = with_patch("patch-am-3way");
        fx.write("a.txt", "one\ntwo, changed here\nthree\n");
        fx.commit_all("Local change");
        let info = inspected(&fx, &mail);
        let stopped = fx.run_as(apply(&fx.dir, &info, How::Commit), Origin::User);
        assert!(stopped.result.unwrap_err().contains("CONFLICT"));
        assert_eq!(stopped.snap.state, RepoState::ApplyMailbox);

        fx.write("a.txt", "one\nTWO, changed here\nthree\n");
        git(&fx.dir, &["add", "a.txt"]);
        let done = fx.run(crate::git::ops::continue_op(RepoState::ApplyMailbox).unwrap());
        assert_eq!(done.snap.state, RepoState::Clean);
        assert_eq!(git(&fx.dir, &["log", "-1", "--format=%s"]), "Change two");

        // Undone as a whole once finished.
        let entry = done.undo.expect("undoable");
        assert_eq!(entry.label, "Apply 1 patch");
        fx.run_as(entry.undo.clone(), Origin::Undo(entry)).result.expect("undo works");
        assert_eq!(git(&fx.dir, &["log", "-1", "--format=%s"]), "Local change");
    }

    #[test]
    fn mails_finished_in_a_terminal_are_offered_for_undo() {
        let (fx, mail, _) = with_patch("patch-am-terminal");
        fx.write("a.txt", "one\ntwo, changed here\nthree\n");
        fx.commit_all("Local change");
        let info = inspected(&fx, &mail);
        let stop = || assert!(fx.run_as(apply(&fx.dir, &info, How::Commit), Origin::User).result.is_err());
        let resolve = || {
            fx.write("a.txt", "one\nTWO, changed here\nthree\n");
            git(&fx.dir, &["add", "a.txt"]);
        };
        // The operation thread looks before each command.
        let look = || fx.run(GitCmd::empty("Look").quiet()).finished;
        let subject = || git(&fx.dir, &["log", "-1", "--format=%s"]);

        stop();
        resolve();
        git(&fx.dir, &["am", "--continue"]);
        let entry = look().expect("offered");
        assert_eq!(entry.label, "Apply 1 patch");
        fx.run_as(entry.undo.clone(), Origin::Undo(entry)).result.expect("undo works");
        assert_eq!(subject(), "Local change");

        // Aborted, or finished and then committed on: nothing.
        stop();
        git(&fx.dir, &["am", "--abort"]);
        assert!(look().is_none());
        stop();
        resolve();
        git(&fx.dir, &["am", "--continue"]);
        fx.write("later.txt", "later\n");
        fx.commit_all("later");
        assert!(look().is_none());
        assert_eq!(subject(), "later");
    }

    #[test]
    fn what_does_not_apply_is_named_and_left_in_rej_files() {
        let (fx, _, diff) = with_patch("patch-reject");
        fx.write("a.txt", "something else entirely\n");
        fx.commit_all("Rewrite");
        // A patch made elsewhere: its blobs are not here, so no three-way
        // merge either.
        let text = std::fs::read_to_string(&diff).unwrap();
        let foreign: Vec<&str> =
            text.lines().map(|l| if l.starts_with("index ") { "index 1234567..89abcde 100644" } else { l }).collect();
        std::fs::write(&diff, foreign.join("\n") + "\n").unwrap();
        let info = inspected(&fx, &diff);
        let Check::Fails(failed) = &info.tree else { panic!("{:?}", info.tree) };
        assert_eq!(failed[0].path.as_deref(), Some("a.txt"));

        let done = fx.run(apply(&fx.dir, &info, How::Tree));
        assert!(done.result.as_deref().unwrap().contains("Did not apply to a.txt"), "{:?}", done.result);
        assert_eq!(fx.read("dir/naïve file.txt"), "keep\nmore\n", "the rest applied");
        assert!(fx.dir.join("a.txt.rej").is_file());

        let entry = done.undo.expect("undoable");
        fx.run_as(entry.undo.clone(), Origin::Undo(entry)).result.expect("undo works");
        assert!(!fx.dir.join("a.txt.rej").exists());
        assert_eq!(fx.read("dir/naïve file.txt"), "keep\n");
    }

    #[test]
    fn a_file_that_is_not_a_patch_is_refused() {
        let fx = Fixture::new("patch-not-a-patch");
        let file = fx.dir.with_extension("txt");
        std::fs::write(&file, "just some words\n").unwrap();
        let err = inspect(&fx.dir, &file).unwrap_err();
        assert!(err.contains("is not a patch"), "{err}");
    }

    #[test]
    fn several_commits_save_as_one_mailbox_in_order() {
        let fx = Fixture::new("patch-mailbox");
        fx.write("a.txt", "1\n");
        fx.commit_all("first");
        let mut oids = Vec::new();
        for (n, text) in [(2, "second"), (3, "third")] {
            fx.write("a.txt", &format!("{n}\n"));
            fx.commit_all(text);
            oids.push(Oid::from_str(&git(&fx.dir, &["rev-parse", "HEAD"])).unwrap());
        }
        let file = fx.dir.with_extension("mbox");
        fx.run(save_combined(&oids, &file));
        assert_eq!(mail_subjects(&std::fs::read_to_string(&file).unwrap()), vec!["second", "third"]);

        git(&fx.dir, &["reset", "-q", "--hard", "HEAD~2"]);
        let info = inspected(&fx, &file);
        assert!(info.tree == Check::Clean, "{:?}", info.tree);
        fx.run(apply(&fx.dir, &info, How::Commit));
        assert_eq!(git(&fx.dir, &["log", "--format=%s"]), "third\nsecond\nfirst");
    }

    #[test]
    fn uncommitted_changes_save_with_untracked_files_and_apply_back() {
        let fx = Fixture::new("patch-changes");
        fx.write("a.txt", "one\n");
        fx.write("bin.dat", "\0\x01\x02");
        fx.commit_all("base");
        fx.write("a.txt", "one\nstaged\n");
        git(&fx.dir, &["add", "a.txt"]);
        fx.write("a.txt", "one\nstaged\nunstaged\n");
        fx.write("new dir/ünï cödé.txt", "fresh\n");
        std::fs::write(fx.dir.join("bin.dat"), [0u8, 1, 2, 3, 255]).unwrap();

        let staged = String::from_utf8(diff_changes(&fx.dir, Changes::Staged).unwrap()).unwrap();
        assert!(staged.contains("+staged") && !staged.contains("+unstaged") && !staged.contains("fresh"));
        let unstaged = String::from_utf8_lossy(&diff_changes(&fx.dir, Changes::Unstaged).unwrap()).into_owned();
        assert!(unstaged.contains("+unstaged") && !unstaged.contains("+staged") && unstaged.contains("+fresh"));
        assert!(unstaged.contains("GIT binary patch"));
        let index_before = std::fs::read(fx.dir.join(".git/index")).unwrap();

        let file = fx.dir.with_extension("patch");
        fx.run(save_changes(Changes::All, &file));
        assert_eq!(std::fs::read(fx.dir.join(".git/index")).unwrap(), index_before, "the real index is untouched");

        // Back to HEAD, then the patch brings every change back.
        git(&fx.dir, &["reset", "-q", "--hard"]);
        git(&fx.dir, &["clean", "-fdq"]);
        let info = inspected(&fx, &file);
        assert!(info.files.contains(&"new dir/ünï cödé.txt".to_owned()), "{:?}", info.files);
        fx.run(apply(&fx.dir, &info, How::Tree));
        assert_eq!(fx.read("a.txt"), "one\nstaged\nunstaged\n");
        assert_eq!(fx.read("new dir/ünï cödé.txt"), "fresh\n");
        assert_eq!(std::fs::read(fx.dir.join("bin.dat")).unwrap(), vec![0u8, 1, 2, 3, 255]);
    }

    #[test]
    fn nothing_to_save_is_said() {
        let fx = Fixture::new("patch-nothing");
        fx.write("a.txt", "one\n");
        fx.commit_all("base");
        let done = fx.run_as(save_changes(Changes::Staged, &fx.dir.with_extension("patch")), Origin::User);
        assert_eq!(done.result.unwrap_err(), "There are no staged changes to save.");
    }

    #[test]
    fn changes_before_the_first_commit_save_against_nothing() {
        let fx = Fixture::new("patch-unborn");
        fx.write("a.txt", "one\n");
        let patch = String::from_utf8(diff_changes(&fx.dir, Changes::All).unwrap()).unwrap();
        assert!(patch.contains("new file mode") && patch.contains("+one"), "{patch}");
    }
}
