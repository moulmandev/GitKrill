//! Steps GitKrill runs itself, for what git has no command for (adding a
//! line to `.gitignore`), and how they are undone.
//!
//! An internal step ([`Step::internal`]) is part of a [`GitCmd`] like any
//! git invocation: it runs on the operator thread in its turn, a failure
//! stops the command and is shown as its error, and the refresh afterwards
//! follows as usual. A command whose internal steps write files can be made
//! undoable with [`UndoKind::Files`]: the operator thread reads each file
//! before and after the command, and undo writes back what was there before.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::ops::{GitCmd, Step, UndoEntry, UndoKind};

/// The code of an [`Internal`] step.
type Run = Arc<dyn Fn(&Path) -> Result<String, String> + Send + Sync>;

/// GitKrill's own code as one step of a command. It gets the working
/// directory; its `Ok` text joins the output of the git steps.
#[derive(Clone)]
pub struct Internal {
    /// What it does, for `Debug` and test failures ("add /a.txt to .gitignore").
    what: String,
    run: Run,
}

impl Internal {
    pub fn new(what: impl Into<String>, run: impl Fn(&Path) -> Result<String, String> + Send + Sync + 'static) -> Self {
        Internal { what: what.into(), run: Arc::new(run) }
    }

    pub fn run(&self, workdir: &Path) -> Result<String, String> {
        (self.run)(workdir)
    }
}

impl fmt::Debug for Internal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Internal({})", self.what)
    }
}

/// The files of [`UndoKind::Files`], and the git steps that undo the rest
/// of the command.
#[derive(Clone, Debug, Default)]
pub struct FileUndo {
    pub files: Vec<FileState>,
    /// Run after the files are put back on undo (putting files back in the
    /// index that the command took out). Redo replays the whole command.
    pub undo_steps: Vec<Step>,
}

/// One file a command writes, and what it held before and after the
/// command (filled in by the operator thread; `None` while it did not exist).
#[derive(Clone, Debug)]
pub struct FileState {
    pub path: PathBuf,
    pub before: Option<Vec<u8>>,
    pub after: Option<Vec<u8>>,
}

impl FileState {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        FileState { path: path.into(), before: None, after: None }
    }
}

/// Reads the files of an [`UndoKind::Files`] command, before (`after`
/// false) or after it ran.
pub fn note_files(undo: &mut UndoKind, after: bool) {
    if let UndoKind::Files(edit) = undo {
        for file in &mut edit.files {
            let content = std::fs::read(&file.path).ok();
            if after { file.after = content } else { file.before = content }
        }
    }
}

/// Undo and redo of an [`UndoKind::Files`] command; `None` when it changed
/// nothing (the line was there already).
pub fn undo_entry(label: &str, edit: &FileUndo, cmd: &GitCmd) -> Option<UndoEntry> {
    let changed: Vec<&FileState> = edit.files.iter().filter(|f| f.before != f.after).collect();
    if changed.is_empty() && edit.undo_steps.is_empty() {
        return None;
    }
    let mut undo = GitCmd::empty(format!("Undo {label}"));
    for file in changed {
        undo.steps.push(put_back(&file.path, file.after.clone(), file.before.clone()));
    }
    undo.steps.extend(edit.undo_steps.iter().cloned());
    let mut redo = GitCmd::empty(format!("Redo {label}"));
    redo.steps = cmd.steps.clone();
    // Files are not HEAD: neither depends on where it is.
    Some(UndoEntry { label: label.to_owned(), undo, redo })
}

/// A step that makes `path` hold `content` (or removes it, for `None`),
/// refusing when it does not hold `expect` now: it was changed since, and
/// writing would lose that.
pub fn put_back(path: &Path, expect: Option<Vec<u8>>, content: Option<Vec<u8>>) -> Step {
    let path = path.to_path_buf();
    let shown = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    Step::internal(Internal::new(format!("restore {}", path.display()), move |_| {
        let now = std::fs::read(&path).ok();
        if now != expect {
            return Err(format!("{shown} has changed since, so it is left as it is."));
        }
        let written = match &content {
            Some(bytes) => std::fs::write(&path, bytes),
            None => match std::fs::remove_file(&path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                other => other,
            },
        };
        written.map(|()| String::new()).map_err(|e| format!("Could not write {}: {e}", path.display()))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::Fixture;
    use crate::worker::Origin;

    fn write_line(path: PathBuf, line: &'static str) -> GitCmd {
        let mut cmd = GitCmd::empty("Write a line");
        let target = path.clone();
        cmd.steps.push(Step::internal(Internal::new("append", move |_| {
            let mut content = std::fs::read(&target).unwrap_or_default();
            content.extend_from_slice(line.as_bytes());
            std::fs::write(&target, content).map(|()| "wrote it".to_owned()).map_err(|e| e.to_string())
        })));
        cmd.undoable(UndoKind::Files(FileUndo { files: vec![FileState::new(path)], undo_steps: Vec::new() }))
    }

    #[test]
    fn internal_steps_run_in_turn_and_their_files_can_be_undone() {
        let fx = Fixture::new("internal-steps");
        fx.write("notes.txt", "one\n");
        let path = fx.dir.join("notes.txt");

        let done = fx.run(write_line(path.clone(), "two\n"));
        assert_eq!(done.result.as_deref(), Ok("wrote it"));
        assert_eq!(fx.read("notes.txt"), "one\ntwo\n");

        let entry = done.undo.expect("undoable");
        let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry.clone()));
        undone.result.expect("undo works");
        assert_eq!(fx.read("notes.txt"), "one\n");

        let redone = fx.run_as(entry.redo.clone(), Origin::Redo(entry.clone()));
        redone.result.expect("redo works");
        assert_eq!(fx.read("notes.txt"), "one\ntwo\n");

        // Changed by hand since: undo leaves it alone and says so.
        fx.write("notes.txt", "one\ntwo\nthree\n");
        let refused = fx.run_as(entry.undo.clone(), Origin::Undo(entry));
        assert!(refused.result.unwrap_err().contains("notes.txt has changed since"));
        assert_eq!(fx.read("notes.txt"), "one\ntwo\nthree\n");
    }

    #[test]
    fn undo_removes_a_file_the_command_created() {
        let fx = Fixture::new("internal-new-file");
        let path = fx.dir.join("made.txt");
        let done = fx.run(write_line(path.clone(), "hello\n"));
        let entry = done.undo.expect("undoable");
        fx.run_as(entry.undo.clone(), Origin::Undo(entry)).result.expect("undo works");
        assert!(!path.exists());
    }

    #[test]
    fn a_failing_internal_step_stops_the_command() {
        let fx = Fixture::new("internal-fails");
        let mut cmd = GitCmd::empty("Fail");
        cmd.steps.push(Step::internal(Internal::new("fail", |_| Err("no room".to_owned()))));
        cmd = cmd.step(&["commit", "--allow-empty", "-m", "never"]);
        let done = fx.run_as(cmd, Origin::User);
        assert_eq!(done.result, Err("no room".to_owned()));
        assert!(done.snap.head.oid.is_none(), "the git step after it did not run");
    }
}
