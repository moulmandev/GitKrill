//! Undo for an operation that stopped on conflicts and was finished later.
//!
//! A rebase, merge, cherry-pick, revert or `git am` that stops on conflicts
//! ends its command with an error, so the operation thread cannot build its
//! undo entry then. [`Carry`] keeps what the entry needs (the command, with
//! the refs it moves noted before it ran, and HEAD before it) while git
//! waits for the user, and builds the entry once the operation is finished:
//! undo then puts everything back as it was before the operation started,
//! however many stops it made on the way.
//!
//! Finished in GitKrill, by a command that carries the operation on
//! (Continue, Skip, or the commit that concludes a merge) and leaves the
//! repository clean, the entry is offered at once. Finished elsewhere (in a
//! terminal, in another tool, or while GitKrill was closed, which is why
//! what is held is also saved in the git directory, [`FILE`]), only the
//! repository can say so, and an abort, a `--quit` or a reset end the
//! operation too: when it stops, [`Stop`] notes what only finishing it
//! leaves behind ([`Finish`]), and [`Carry::check`] looks for that once the
//! operation is no longer in progress. When it is not there, or something
//! else moved the refs since, the entry is dropped: an undo must never be
//! offered for something other than what it would undo.
//!
//! The operation in progress is recognised by what git wrote down when it
//! began (the branch and commit a rebase started from, the commit being
//! merged, ...): see [`marker`].

use std::collections::HashSet;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use git2::{Commit, Repository};
use serde::{Deserialize, Serialize};

use super::cli;
use super::ops::{GitCmd, RefMove, UndoEntry, UndoKind, build_undo};
use super::repo::{lossy, read_head};
use super::{Head, Oid};

/// Where what is held waits for the next session, in the git directory
/// (each worktree's own, like the operation itself).
pub const FILE: &str = "gitkrill-carry.json";

/// What an undo entry needs from before the operation, kept while it waits.
struct Held {
    /// The command as it ran, refs of [`UndoKind::Refs`] noted before.
    cmd: GitCmd,
    before: Head,
    deleted_at: Option<Oid>,
    /// [`marker`] of the operation while it waits.
    marker: String,
    stop: Stop,
}

/// Owned by the operation thread; one per repository.
#[derive(Default)]
pub struct Carry {
    held: Option<Held>,
    /// Where [`FILE`] goes; without it, what is held lasts this session only.
    file: Option<PathBuf>,
}

impl Carry {
    /// The carry of the repository at `workdir`, holding what an earlier
    /// session left waiting there. Call [`Carry::check`] next: the
    /// operation may have been finished while GitKrill was closed.
    pub fn open(workdir: &Path) -> Self {
        let Ok(repo) = Repository::open(workdir) else { return Carry::default() };
        let file = repo.path().join(FILE);
        let held = std::fs::read(&file).ok().and_then(|text| match serde_json::from_slice::<Saved>(&text) {
            Ok(saved) => Some(Held::from(saved)),
            // Cut short, or written by another version: nothing to go on.
            Err(_) => {
                let _ = std::fs::remove_file(&file);
                None
            }
        });
        Carry { held, file: Some(file) }
    }

    /// Called before every command, and whenever the repository may have
    /// changed outside GitKrill. Keeps what is held while its operation is
    /// still the one in progress. Once it is not, returns the entry when
    /// the repository shows the operation was finished, and drops it
    /// otherwise (aborted, given up half way, replaced by another one, or
    /// followed by other changes that an undo would throw away).
    pub fn check(&mut self, workdir: &Path) -> Option<UndoEntry> {
        let held = self.held.as_ref()?;
        let repo = Repository::open(workdir).ok()?;
        let now = marker(&repo);
        if now.as_deref() == Some(held.marker.as_str()) {
            return None;
        }
        let finished = now.is_none() && held.stop.finished(&repo);
        let held = self.release()?;
        if finished { finish(&repo, held) } else { None }
    }

    /// Called after a command from the user ran. `before` and `deleted_at`
    /// are what the operation thread noted for the command's own undo
    /// (`before` is `None` when it has none).
    ///
    /// Returns the undo entry of a held operation that this command
    /// finished, which takes the place of the command's own.
    pub fn end(
        &mut self,
        workdir: &Path,
        cmd: &GitCmd,
        succeeded: bool,
        before: Option<&Head>,
        deleted_at: Option<Oid>,
    ) -> Option<UndoEntry> {
        let repo = Repository::open(workdir).ok()?;
        let now = marker(&repo);

        if let Some(held) = &self.held {
            // Still waiting (a file was staged, or the next commit stopped
            // as well).
            if now.as_deref() == Some(held.marker.as_str()) {
                return None;
            }
            let finished =
                now.is_none() && ((succeeded && carries_on(cmd)) || held.stop.finished(&repo));
            let held = self.release()?;
            if finished {
                return finish(&repo, held);
            }
            // Aborted, or replaced by something else.
        }

        // The command stopped half way, with its own undo still to build.
        if !succeeded
            && let (Some(marker), Some(before)) = (now, before)
            && !matches!(cmd.undo, UndoKind::None)
        {
            let stop = Stop::read(&repo, workdir, &cmd.undo);
            self.hold(Held { cmd: cmd.clone(), before: before.clone(), deleted_at, marker, stop });
        }
        None
    }

    /// Whether an entry is waiting for its operation to finish.
    pub fn is_holding(&self) -> bool {
        self.held.is_some()
    }

    fn hold(&mut self, held: Held) {
        if let Some(file) = &self.file {
            match held.saved().and_then(|saved| serde_json::to_vec_pretty(&saved).ok()) {
                Some(text) => {
                    let _ = std::fs::write(file, text);
                }
                None => {
                    let _ = std::fs::remove_file(file);
                }
            }
        }
        self.held = Some(held);
    }

    /// Takes what is held, its file with it.
    fn release(&mut self) -> Option<Held> {
        let held = self.held.take()?;
        if let Some(file) = &self.file {
            let _ = std::fs::remove_file(file);
        }
        Some(held)
    }
}

/// The entry for a held operation that has just finished.
fn finish(repo: &Repository, mut held: Held) -> Option<UndoEntry> {
    if let UndoKind::Refs(moves) = &mut held.cmd.undo {
        for m in moves {
            m.after = repo.refname_to_id(&m.name).ok();
        }
    }
    build_undo(&held.cmd, &held.before, &read_head(repo), held.deleted_at)
}

/// Whether `cmd` carries a stopped operation on, rather than abandoning it:
/// `--continue`, `--skip`, or a commit (which concludes a merge or a single
/// cherry-pick). `--abort`, `--quit` and resets do not.
fn carries_on(cmd: &GitCmd) -> bool {
    cmd.steps.iter().any(|step| {
        let args = &step.args;
        // Past `-c name=value` pairs to the subcommand.
        let mut at = 0;
        while args.get(at).is_some_and(|arg| arg == "-c") {
            at += 2;
        }
        args.get(at).is_some_and(|sub| sub == "commit")
            || args[at.min(args.len())..].iter().any(|arg| arg == "--continue" || arg == "--skip")
    })
}

/// What identifies the operation in progress, or `None` when there is none.
///
/// Each is written by git when the operation begins and stays the same until
/// it ends: the branch and commit a rebase started from, the number of mails
/// `git am` was given and the first one's opening line, the commit a
/// sequence of cherry-picks or reverts started on, the commit being merged
/// or picked.
pub fn marker(repo: &Repository) -> Option<String> {
    let dir = repo.path();
    let read = |name: &str| read_text(dir, name);
    for state in ["rebase-merge", "rebase-apply"] {
        if let Some(orig) = read(&format!("{state}/orig-head")) {
            let name = read(&format!("{state}/head-name")).unwrap_or_default();
            return Some(format!("rebase {name} {orig}"));
        }
    }
    if dir.join("rebase-apply/applying").is_file() {
        let last = read("rebase-apply/last").unwrap_or_default();
        let first = std::fs::File::open(dir.join("rebase-apply/0001"))
            .ok()
            .and_then(|file| {
                let mut line = Vec::new();
                std::io::BufReader::new(file).read_until(b'\n', &mut line).ok()?;
                Some(lossy(&line).trim().to_owned())
            })
            .unwrap_or_default();
        return Some(format!("am {last} {first}"));
    }
    if let Some(head) = read("sequencer/head") {
        return Some(format!("sequence {head}"));
    }
    ["MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD"]
        .into_iter()
        .find_map(|name| read(name).map(|oid| format!("{name} {oid}")))
}

/// A file git keeps in `dir`, trimmed; `None` when it is missing or empty.
fn read_text(dir: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(name)).ok().map(|text| text.trim().to_owned()).filter(|text| !text.is_empty())
}

/// The repository as the operation stopped, and what finishing it leaves.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Stop {
    head: SavedHead,
    /// Where each ref of an [`UndoKind::Refs`] command pointed.
    refs: Vec<(String, Option<Sha>)>,
    finish: Finish,
}

impl Stop {
    fn read(repo: &Repository, workdir: &Path, undo: &UndoKind) -> Stop {
        let refs = match undo {
            UndoKind::Refs(moves) => {
                moves.iter().map(|m| (m.name.clone(), repo.refname_to_id(&m.name).ok().map(Sha))).collect()
            }
            _ => Vec::new(),
        };
        Stop { head: SavedHead::from(&read_head(repo)), refs, finish: Finish::read(repo, workdir) }
    }

    /// Whether the repository is as finishing the operation left it: what
    /// [`Finish`] describes is there, HEAD has not moved on from it, and
    /// the other refs the command moves are where they were when it
    /// stopped.
    fn finished(&self, repo: &Repository) -> bool {
        let head = read_head(repo);
        let stopped = self.head.oid.map(|sha| sha.0);
        let in_place = head.branch == self.head.branch && head.detached == self.head.detached;
        // The branch the operation itself moves.
        let moving = match &self.finish {
            Finish::Rebase { branch, .. } => Some(branch.clone()),
            _ => head.branch.as_ref().map(|name| format!("refs/heads/{name}")),
        };
        let others_stayed = self
            .refs
            .iter()
            .filter(|(name, _)| Some(name) != moving.as_ref())
            .all(|(name, at)| repo.refname_to_id(name).ok() == at.map(|sha| sha.0));
        if !others_stayed {
            return false;
        }
        match &self.finish {
            Finish::Rebase { branch, orig, onto } => rebased(repo, &head, branch, orig.0, onto.0),
            Finish::Merge { heads } => in_place && merged(repo, head.oid, stopped, heads),
            Finish::Sequence { items } => {
                in_place && made_since(repo, head.oid, stopped, items.len(), |commit, i| items[i].made(repo, commit))
            }
            Finish::Mails { left, subject } => {
                let by_am = made_by_am(repo);
                in_place
                    && made_since(repo, head.oid, stopped, *left, |commit, i| {
                        by_am.contains(&commit.id())
                            && (i + 1 < *left || commit.summary_bytes() == Some(subject.as_bytes()))
                    })
            }
            Finish::Unknown => false,
        }
    }
}

/// What only finishing the operation leaves behind. An abort puts HEAD
/// back where the operation started and leaves the branches alone; a
/// `--quit` leaves HEAD where the operation had got to.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Finish {
    /// A rebase of `branch` (a full ref name) that started from `orig`. Its
    /// last step moves the branch from `orig` to the result, writing
    /// "rebase (finish): <branch> onto <onto>" in the branch's reflog, and
    /// checks the branch out.
    Rebase { branch: String, orig: Sha, onto: Sha },
    /// HEAD is the commit that concludes the merge: its parents are HEAD as
    /// the merge stopped, then these.
    Merge { heads: Vec<Sha> },
    /// The commits still to cherry-pick or revert, the one that stopped
    /// first. HEAD is made from the last of them, with nothing but commits
    /// made from the others, in order, down to HEAD as it stopped (one that
    /// was skipped has none).
    Sequence { items: Vec<Item> },
    /// `git am`: how many mails were still to apply, the one that stopped
    /// included, and the subject of the last. HEAD is the last one's commit,
    /// with nothing but commits `git am` made down to HEAD as it stopped.
    Mails { left: usize, subject: String },
    /// Nothing tells: a rebase of a detached HEAD writes nothing of its own
    /// when it finishes, so it looks like one given up with `--quit`.
    Unknown,
}

impl Finish {
    /// For the operation in progress, read in the same order as [`marker`].
    fn read(repo: &Repository, workdir: &Path) -> Finish {
        let dir = repo.path();
        let oid = |name: &str| read_text(dir, name).and_then(|text| Oid::from_str(&text).ok()).map(Sha);
        for state in ["rebase-merge", "rebase-apply"] {
            if let Some(orig) = oid(&format!("{state}/orig-head")) {
                let branch = read_text(dir, &format!("{state}/head-name")).unwrap_or_default();
                return match oid(&format!("{state}/onto")) {
                    Some(onto) if branch.starts_with("refs/heads/") => Finish::Rebase { branch, orig, onto },
                    _ => Finish::Unknown,
                };
            }
        }
        if dir.join("rebase-apply/applying").is_file() {
            return mails(repo, workdir).unwrap_or(Finish::Unknown);
        }
        if let Some(todo) = read_text(dir, "sequencer/todo") {
            return sequence(repo, &todo).unwrap_or(Finish::Unknown);
        }
        if let Some(text) = read_text(dir, "MERGE_HEAD") {
            let heads: Option<Vec<Sha>> = text.lines().map(|line| Oid::from_str(line.trim()).ok().map(Sha)).collect();
            return heads.map_or(Finish::Unknown, |heads| Finish::Merge { heads });
        }
        if let Some(pick) = oid("CHERRY_PICK_HEAD") {
            return Finish::Sequence { items: vec![Item::Pick(pick)] };
        }
        if let Some(revert) = oid("REVERT_HEAD") {
            return Finish::Sequence { items: vec![Item::Revert(revert)] };
        }
        Finish::Unknown
    }
}

/// One commit of a sequence of cherry-picks or reverts.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Item {
    Pick(Sha),
    Revert(Sha),
}

impl Item {
    /// Whether `commit` was made by picking or reverting this one.
    fn made(self, repo: &Repository, commit: &Commit) -> bool {
        match self {
            // A pick keeps the author, the date included, and the message.
            Item::Pick(sha) => repo.find_commit(sha.0).is_ok_and(|original| {
                let (made, was) = (commit.author(), original.author());
                made.name_bytes() == was.name_bytes()
                    && made.email_bytes() == was.email_bytes()
                    && made.when().seconds() == was.when().seconds()
                    && made.when().offset_minutes() == was.when().offset_minutes()
                    && commit.summary_bytes() == original.summary_bytes()
            }),
            // A revert names the commit in its message.
            Item::Revert(sha) => lossy(commit.message_bytes()).contains(&format!("This reverts commit {}", sha.0)),
        }
    }
}

/// The commits a sequence of cherry-picks or reverts still has to go
/// through, from its todo list (which names them abbreviated).
fn sequence(repo: &Repository, todo: &str) -> Option<Finish> {
    let mut items = Vec::new();
    for line in todo.lines().map(str::trim).filter(|line| !line.is_empty() && !line.starts_with('#')) {
        let mut words = line.split_whitespace();
        let (verb, name) = (words.next()?, words.next()?);
        let sha = Sha(repo.revparse_single(&format!("{name}^{{commit}}")).ok()?.id());
        items.push(match verb {
            "pick" | "p" => Item::Pick(sha),
            "revert" => Item::Revert(sha),
            _ => return None,
        });
    }
    (!items.is_empty()).then_some(Finish::Sequence { items })
}

/// The mails `git am` still has to apply, and the subject the last one's
/// commit gets, read the way `git am` reads it (`git mailinfo`).
fn mails(repo: &Repository, workdir: &Path) -> Option<Finish> {
    let dir = repo.path().join("rebase-apply");
    let number = |name: &str| read_text(&dir, name)?.parse::<usize>().ok();
    let (next, last) = (number("next")?, number("last")?);
    let left = (last + 1).checked_sub(next).filter(|&left| left > 0)?;
    // Numbered with four digits at least.
    let mail = std::fs::read(dir.join(format!("{last:04}"))).ok()?;
    // Git for Windows takes `/dev/null` for the null device too.
    let args = ["mailinfo", "/dev/null", "/dev/null"].map(str::to_owned);
    let options = cli::Options { stdin: Some(&mail), background: true, ..Default::default() };
    let info = cli::run(workdir, &args, options).ok()?;
    let subject = info.lines().find_map(|line| line.strip_prefix("Subject: "))?.trim().to_owned();
    Some(Finish::Mails { left, subject })
}

/// Whether the rebase of `branch` finished: its last step moved the branch
/// from where the rebase started and checked it out, and nothing moved it
/// since.
fn rebased(repo: &Repository, head: &Head, branch: &str, orig: Oid, onto: Oid) -> bool {
    let checked_out =
        !head.detached && head.branch.as_ref().is_some_and(|name| format!("refs/heads/{name}") == branch);
    let Ok(log) = repo.reflog(branch) else { return false };
    let Some(last) = log.get(0) else { return false };
    let message = last.message_bytes().map(lossy).unwrap_or_default();
    checked_out
        && head.oid == Some(last.id_new())
        && last.id_old() == orig
        && last.id_new() != orig
        // "rebase (finish): refs/heads/topic onto <onto>"; older versions
        // of git wrote "rebase finished: ...".
        && message.contains("finish")
        && message.trim_end().ends_with(&format!("{branch} onto {onto}"))
}

/// Whether `head` concludes the merge: HEAD as the merge stopped, then the
/// commits being merged, are its parents.
fn merged(repo: &Repository, head: Option<Oid>, stopped: Option<Oid>, heads: &[Sha]) -> bool {
    let Some(commit) = head.and_then(|oid| repo.find_commit(oid).ok()) else { return false };
    let mut parents = commit.parent_ids();
    stopped.is_some() && !heads.is_empty() && parents.next() == stopped && parents.eq(heads.iter().map(|sha| sha.0))
}

/// Whether the first-parent line from `head` down to `stopped` holds one
/// or more commits and nothing but commits made from the `n` things the
/// operation still had to apply when it stopped, in order, the newest made
/// from the last: `made(commit, i)` says whether `commit` was made from the
/// `i`-th. One that was skipped has no commit.
fn made_since(
    repo: &Repository,
    head: Option<Oid>,
    stopped: Option<Oid>,
    n: usize,
    made: impl Fn(&Commit, usize) -> bool,
) -> bool {
    let (Some(mut at), Some(stopped)) = (head, stopped) else { return false };
    // Each commit is made from one of the first `left`; the walk goes
    // newest first, so `left` only goes down.
    let mut left = n;
    while at != stopped {
        let Ok(commit) = repo.find_commit(at) else { return false };
        let Some(i) = (0..left).rev().find(|&i| made(&commit, i)) else { return false };
        if left == n && i + 1 != n {
            return false;
        }
        left = i;
        let Ok(parent) = commit.parent_id(0) else { return false };
        at = parent;
    }
    left < n
}

/// The commits `git am` made, from HEAD's reflog ("am: <subject>").
fn made_by_am(repo: &Repository) -> HashSet<Oid> {
    let Ok(log) = repo.reflog("HEAD") else { return HashSet::new() };
    log.iter()
        .filter(|entry| entry.message_bytes().is_some_and(|message| message.starts_with(b"am: ")))
        .map(|entry| entry.id_new())
        .collect()
}

/// An [`Oid`] saved as its hex text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
struct Sha(Oid);

impl TryFrom<String> for Sha {
    type Error = git2::Error;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Oid::from_str(&text).map(Sha)
    }
}

impl From<Sha> for String {
    fn from(sha: Sha) -> String {
        sha.0.to_string()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct SavedHead {
    branch: Option<String>,
    oid: Option<Sha>,
    detached: bool,
}

impl From<&Head> for SavedHead {
    fn from(head: &Head) -> Self {
        SavedHead { branch: head.branch.clone(), oid: head.oid.map(Sha), detached: head.detached }
    }
}

impl From<SavedHead> for Head {
    fn from(head: SavedHead) -> Self {
        Head { branch: head.branch, oid: head.oid.map(|sha| sha.0), detached: head.detached }
    }
}

/// What [`FILE`] holds: a [`Held`] whose command can be built again from
/// its label and its refs, which is all [`build_undo`] reads of the kinds
/// that stop on conflicts.
#[derive(Serialize, Deserialize)]
struct Saved {
    label: String,
    /// The refs of an [`UndoKind::Refs`] command and where each pointed
    /// before it ran; `None` for [`UndoKind::HeadMove`].
    refs: Option<Vec<(String, Option<Sha>)>>,
    before: SavedHead,
    marker: String,
    stop: Stop,
}

impl Held {
    /// `None` for kinds that need more of the command to be undone.
    fn saved(&self) -> Option<Saved> {
        let refs = match &self.cmd.undo {
            UndoKind::HeadMove => None,
            UndoKind::Refs(moves) => Some(moves.iter().map(|m| (m.name.clone(), m.before.map(Sha))).collect()),
            _ => return None,
        };
        Some(Saved {
            label: self.cmd.label.clone(),
            refs,
            before: SavedHead::from(&self.before),
            marker: self.marker.clone(),
            stop: self.stop.clone(),
        })
    }
}

impl From<Saved> for Held {
    fn from(saved: Saved) -> Self {
        let undo = match saved.refs {
            None => UndoKind::HeadMove,
            Some(refs) => UndoKind::Refs(
                refs.into_iter()
                    .map(|(name, before)| RefMove { before: before.map(|sha| sha.0), ..RefMove::new(name) })
                    .collect(),
            ),
        };
        Held {
            cmd: GitCmd::empty(saved.label).undoable(undo),
            before: saved.before.into(),
            deleted_at: None,
            marker: saved.marker,
            stop: saved.stop,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::RepoState;
    use crate::git::ops;
    use crate::tests::{Fixture, TIMEOUT, git};
    use crate::worker::{Origin, RepoHandle, Response};

    /// main and topic both change `f.txt` after `base`, so merging or
    /// rebasing one onto the other stops on a conflict.
    fn clash(name: &str) -> Fixture {
        let fx = Fixture::new(name);
        fx.write("f.txt", "base\n");
        fx.write("other file.txt", "other\n");
        fx.commit_all("base");
        git(&fx.dir, &["checkout", "-q", "-b", "topic"]);
        fx.write("f.txt", "topic\n");
        fx.write("tópico.txt", "topic\n");
        fx.commit_all("topic one");
        fx.write("topic two.txt", "two\n");
        fx.commit_all("topic two");
        git(&fx.dir, &["checkout", "-q", "main"]);
        fx.write("f.txt", "main\n");
        fx.commit_all("main work");
        fx
    }

    fn rev(fx: &Fixture, name: &str) -> Oid {
        Oid::from_str(&git(&fx.dir, &["rev-parse", name])).unwrap()
    }

    fn resolve(fx: &Fixture) {
        fx.write("f.txt", "resolved\n");
        git(&fx.dir, &["add", "f.txt"]);
    }

    fn continue_rebase() -> GitCmd {
        ops::continue_op(RepoState::Rebase).unwrap()
    }

    fn saved(fx: &Fixture) -> PathBuf {
        fx.dir.join(".git").join(FILE)
    }

    /// What GitKrill offers once it notices that the repository changed
    /// (the refresh the file watcher asks for): the entry of an operation
    /// finished outside it.
    fn offered(fx: &Fixture) -> UndoEntry {
        fx.handle.refresh();
        loop {
            if let Response::Finished(entry) = fx.handle.rx.recv_timeout(TIMEOUT).expect("an entry is offered") {
                return entry;
            }
        }
    }

    /// What is offered before the next command runs (the operation thread
    /// looks first), once a command that does nothing has run.
    fn look(fx: &Fixture) -> Option<UndoEntry> {
        fx.run(GitCmd::empty("Look").quiet()).finished
    }

    /// Undoes `entry` and returns HEAD then.
    fn undo(fx: &Fixture, entry: UndoEntry) -> Head {
        let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry));
        undone.result.expect("undo works");
        assert!(undone.snap.status.is_clean());
        undone.snap.head.clone()
    }

    /// Closes the repository and opens it again, as quitting GitKrill and
    /// starting it again does.
    fn reopen(fx: &mut Fixture) {
        fx.handle = RepoHandle::spawn(fx.dir.clone(), eframe::egui::Context::default(), 1000);
    }

    #[test]
    fn a_rebase_continued_after_conflicts_can_be_undone_and_redone() {
        let fx = clash("carry-rebase");
        git(&fx.dir, &["checkout", "-q", "topic"]);
        let tip = rev(&fx, "topic");

        let stopped = fx.run_as(ops::rebase("main"), Origin::User);
        assert!(stopped.result.unwrap_err().contains("CONFLICT"));
        assert!(stopped.undo.is_none());
        // Staging the resolution keeps the entry waiting.
        let staged = fx.run(ops::stage(&["f.txt".to_owned()]));
        assert!(staged.undo.is_none());
        fx.write("f.txt", "resolved\n");
        fx.run(ops::stage(&["f.txt".to_owned()]));

        let done = fx.run(continue_rebase());
        assert_eq!(done.snap.state, RepoState::Clean);
        let rebased = done.snap.head.oid.unwrap();
        assert_ne!(rebased, tip);
        let entry = done.undo.expect("the finished rebase is undoable");
        assert_eq!(entry.label, "Rebase onto main");
        assert!(!saved(&fx).exists());

        let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry.clone()));
        undone.result.expect("undo works");
        assert_eq!(undone.snap.head.oid, Some(tip));
        assert_eq!(undone.snap.head.branch.as_deref(), Some("topic"));
        assert_eq!(fx.read("f.txt"), "topic\n");
        assert!(undone.snap.status.is_clean());

        let redone = fx.run_as(entry.redo.clone(), Origin::Redo(entry));
        redone.result.expect("redo works");
        assert_eq!(redone.snap.head.oid, Some(rebased));
    }

    #[test]
    fn a_rebase_that_stops_twice_is_undone_to_where_it_started() {
        let fx = clash("carry-rebase-twice");
        git(&fx.dir, &["checkout", "-q", "topic"]);
        // The second topic commit now clashes with main as well.
        fx.write("f.txt", "topic again\n");
        fx.commit_all("topic three");
        let tip = rev(&fx, "topic");

        assert!(fx.run_as(ops::rebase("main"), Origin::User).result.is_err());
        resolve(&fx);
        let again = fx.run_as(continue_rebase(), Origin::User);
        assert!(again.result.is_err(), "the next commit stops too");
        assert!(again.undo.is_none());
        resolve(&fx);
        let done = fx.run(continue_rebase());
        let entry = done.undo.expect("undoable once finished");
        assert_eq!(undo(&fx, entry).oid, Some(tip));
    }

    #[test]
    fn skipping_the_last_commit_finishes_the_rebase_too() {
        let fx = clash("carry-skip");
        git(&fx.dir, &["checkout", "-q", "topic"]);
        let tip = rev(&fx, "topic");
        // Only the first topic commit clashes; skip it.
        assert!(fx.run_as(ops::rebase("main"), Origin::User).result.is_err());
        let done = fx.run(GitCmd::new("Skip", &["rebase", "--skip"]));
        assert_eq!(done.snap.state, RepoState::Clean);
        let entry = done.undo.expect("undoable");
        assert_eq!(undo(&fx, entry).oid, Some(tip));
    }

    #[test]
    fn an_aborted_operation_leaves_nothing_to_undo() {
        let fx = clash("carry-abort");
        git(&fx.dir, &["checkout", "-q", "topic"]);
        assert!(fx.run_as(ops::rebase("main"), Origin::User).result.is_err());
        let aborted = fx.run(ops::abort(RepoState::Rebase).unwrap());
        assert!(aborted.undo.is_none());
        assert_eq!(aborted.snap.state, RepoState::Clean);
        assert!(!saved(&fx).exists());

        // Aborted in a terminal, then a commit made in the app: that commit
        // gets its own undo, not the rebase's.
        assert!(fx.run_as(ops::rebase("main"), Origin::User).result.is_err());
        git(&fx.dir, &["rebase", "--abort"]);
        fx.write("new file.txt", "new\n");
        let commit = fx.run(ops::commit_everything("after the abort"));
        assert!(commit.finished.is_none());
        assert_eq!(commit.undo.expect("the commit is undoable").label, "Commit");
    }

    #[test]
    fn a_rebase_finished_in_a_terminal_is_offered_for_undo() {
        let fx = clash("carry-rebase-terminal");
        git(&fx.dir, &["checkout", "-q", "topic"]);
        let tip = rev(&fx, "topic");
        assert!(fx.run_as(ops::rebase("main"), Origin::User).result.is_err());
        assert!(saved(&fx).is_file(), "kept for the next session too");
        resolve(&fx);
        git(&fx.dir, &["-c", "core.editor=true", "rebase", "--continue"]);
        let rebased = rev(&fx, "topic");

        let entry = offered(&fx);
        assert_eq!(entry.label, "Rebase onto main");
        assert!(!saved(&fx).exists());
        let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry.clone()));
        undone.result.expect("undo works");
        assert_eq!(undone.snap.head.oid, Some(tip));
        assert_eq!(undone.snap.head.branch.as_deref(), Some("topic"));
        let redone = fx.run_as(entry.redo.clone(), Origin::Redo(entry));
        redone.result.expect("redo works");
        assert_eq!(redone.snap.head.oid, Some(rebased));

        // Skipping the commit that stopped finishes it as well.
        git(&fx.dir, &["reset", "-q", "--hard", &tip.to_string()]);
        assert!(fx.run_as(ops::rebase("main"), Origin::User).result.is_err());
        git(&fx.dir, &["rebase", "--skip"]);
        assert_eq!(undo(&fx, offered(&fx)).oid, Some(tip));
    }

    #[test]
    fn a_rebase_given_up_in_a_terminal_offers_nothing() {
        let fx = clash("carry-rebase-given-up");
        git(&fx.dir, &["checkout", "-q", "-b", "other", "main~1"]);
        fx.write("other.txt", "other\n");
        fx.commit_all("other work");
        git(&fx.dir, &["checkout", "-q", "topic"]);
        let tip = rev(&fx, "topic");

        // Aborted.
        assert!(fx.run_as(ops::rebase("main"), Origin::User).result.is_err());
        git(&fx.dir, &["rebase", "--abort"]);
        assert!(look(&fx).is_none());
        assert!(!saved(&fx).exists());

        // Aborted, then rebased onto something else: not this rebase.
        assert!(fx.run_as(ops::rebase("main"), Origin::User).result.is_err());
        git(&fx.dir, &["rebase", "--abort"]);
        git(&fx.dir, &["rebase", "-q", "other"]);
        assert_ne!(rev(&fx, "topic"), tip);
        assert!(look(&fx).is_none());
        git(&fx.dir, &["reset", "-q", "--hard", &tip.to_string()]);

        // Given up half way: the branch never moved.
        assert!(fx.run_as(ops::rebase("main"), Origin::User).result.is_err());
        git(&fx.dir, &["rebase", "--quit"]);
        assert!(look(&fx).is_none());
        git(&fx.dir, &["reset", "-q", "--hard"]);
        git(&fx.dir, &["checkout", "-q", "topic"]);

        // Finished, then committed on before GitKrill looked: undoing the
        // rebase would take that commit away too.
        assert!(fx.run_as(ops::rebase("main"), Origin::User).result.is_err());
        resolve(&fx);
        git(&fx.dir, &["-c", "core.editor=true", "rebase", "--continue"]);
        fx.write("later.txt", "later\n");
        fx.commit_all("later");
        assert!(look(&fx).is_none());

        // A rebase of a detached HEAD leaves no sign that it finished.
        git(&fx.dir, &["checkout", "-q", "--detach", &tip.to_string()]);
        assert!(fx.run_as(ops::rebase("main"), Origin::User).result.is_err());
        resolve(&fx);
        git(&fx.dir, &["-c", "core.editor=true", "rebase", "--continue"]);
        assert!(look(&fx).is_none());
    }

    #[test]
    fn a_merge_concluded_by_a_commit_is_undone_as_the_merge() {
        let fx = clash("carry-merge");
        let main = rev(&fx, "main");
        let stopped = fx.run_as(ops::merge("topic"), Origin::User);
        assert!(stopped.result.unwrap_err().contains("CONFLICT"));
        resolve(&fx);
        let done = fx.run(ops::commit("Merge topic, by hand", false));
        let merge = done.snap.head.oid.unwrap();
        let entry = done.undo.expect("the merge is undoable");
        assert_eq!(entry.label, "Merge topic");
        let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry.clone()));
        undone.result.expect("undo works");
        assert_eq!(undone.snap.head.oid, Some(main));
        assert!(undone.snap.status.is_clean());
        let redone = fx.run_as(entry.redo.clone(), Origin::Redo(entry));
        redone.result.expect("redo works");
        assert_eq!(redone.snap.head.oid, Some(merge));
    }

    #[test]
    fn a_merge_concluded_in_a_terminal_is_offered_for_undo() {
        let fx = clash("carry-merge-terminal");
        let main = rev(&fx, "main");
        assert!(fx.run_as(ops::merge("topic"), Origin::User).result.is_err());
        resolve(&fx);
        git(&fx.dir, &["commit", "-q", "--no-edit"]);
        let entry = offered(&fx);
        assert_eq!(entry.label, "Merge topic");
        assert_eq!(undo(&fx, entry).oid, Some(main));

        // `git merge --continue` concludes it the same way.
        assert!(fx.run_as(ops::merge("topic"), Origin::User).result.is_err());
        resolve(&fx);
        git(&fx.dir, &["-c", "core.editor=true", "merge", "--continue"]);
        assert_eq!(undo(&fx, look(&fx).expect("offered")).oid, Some(main));
    }

    #[test]
    fn a_merge_given_up_in_a_terminal_offers_nothing() {
        let fx = clash("carry-merge-given-up");
        assert!(fx.run_as(ops::merge("topic"), Origin::User).result.is_err());
        git(&fx.dir, &["merge", "--abort"]);
        assert!(look(&fx).is_none());

        // Reset, then a commit of its own: not a merge.
        assert!(fx.run_as(ops::merge("topic"), Origin::User).result.is_err());
        git(&fx.dir, &["reset", "-q", "--hard"]);
        fx.write("own.txt", "own\n");
        fx.commit_all("own work");
        assert!(look(&fx).is_none());

        // Concluded, then committed on before GitKrill looked.
        assert!(fx.run_as(ops::merge("topic"), Origin::User).result.is_err());
        resolve(&fx);
        git(&fx.dir, &["commit", "-q", "--no-edit"]);
        fx.write("later.txt", "later\n");
        fx.commit_all("later");
        assert!(look(&fx).is_none());
    }

    #[test]
    fn a_checkout_and_merge_that_stopped_is_undone_on_both_branches() {
        let fx = clash("carry-refs");
        git(&fx.dir, &["checkout", "-q", "topic"]);
        let (main, topic) = (rev(&fx, "main"), rev(&fx, "topic"));
        // What dragging topic onto main and choosing Merge runs.
        let cmd = || {
            GitCmd::new("Checkout main and merge topic", &["checkout", "main"])
                .step(&["merge", "--no-edit", "topic"])
                .undoable(UndoKind::Refs(vec![RefMove::new("refs/heads/main")]))
        };
        assert!(fx.run_as(cmd(), Origin::User).result.is_err());
        resolve(&fx);
        let done = fx.run(GitCmd::new("Continue merge", &["merge", "--continue"]));
        assert_eq!(done.snap.head.branch.as_deref(), Some("main"));
        let entry = done.undo.expect("undoable");
        assert_eq!(undo(&fx, entry).branch.as_deref(), Some("topic"));
        assert_eq!(rev(&fx, "main"), main);
        assert_eq!(rev(&fx, "topic"), topic);

        // Concluded in a terminal: the same.
        assert!(fx.run_as(cmd(), Origin::User).result.is_err());
        resolve(&fx);
        git(&fx.dir, &["commit", "-q", "--no-edit"]);
        assert_eq!(undo(&fx, offered(&fx)).branch.as_deref(), Some("topic"));
        assert_eq!(rev(&fx, "main"), main);
        assert_eq!(rev(&fx, "topic"), topic);

        // Another ref of the command moved since it stopped: unclear.
        let both = GitCmd::new("Checkout main and merge topic", &["checkout", "main"])
            .step(&["merge", "--no-edit", "topic"])
            .undoable(UndoKind::Refs(vec![RefMove::new("refs/heads/main"), RefMove::new("refs/heads/topic")]));
        assert!(fx.run_as(both, Origin::User).result.is_err());
        resolve(&fx);
        git(&fx.dir, &["commit", "-q", "--no-edit"]);
        git(&fx.dir, &["branch", "-f", "topic", "topic~1"]);
        assert!(look(&fx).is_none());
    }

    #[test]
    fn a_cherry_pick_finished_in_a_terminal_is_offered_for_undo() {
        let fx = clash("carry-pick-terminal");
        let main = rev(&fx, "main");
        let pick = || ops::cherry_pick(rev(&fx, "topic~1"), false);
        assert!(fx.run_as(pick(), Origin::User).result.is_err());
        resolve(&fx);
        git(&fx.dir, &["-c", "core.editor=true", "cherry-pick", "--continue"]);
        let entry = offered(&fx);
        assert!(entry.label.starts_with("Cherry-pick "), "{}", entry.label);
        assert_eq!(undo(&fx, entry).oid, Some(main));

        // Concluded by a commit instead.
        assert!(fx.run_as(pick(), Origin::User).result.is_err());
        resolve(&fx);
        git(&fx.dir, &["commit", "-q", "--no-edit"]);
        assert_eq!(undo(&fx, look(&fx).expect("offered")).oid, Some(main));

        // Several at once, the first stopping: picked to the end in a
        // terminal.
        let picks = [rev(&fx, "topic~1"), rev(&fx, "topic")];
        assert!(fx.run_as(crate::git::multi::cherry_pick(&picks, false), Origin::User).result.is_err());
        resolve(&fx);
        git(&fx.dir, &["-c", "core.editor=true", "cherry-pick", "--continue"]);
        let entry = offered(&fx);
        assert_eq!(entry.label, "Cherry-pick 2 commits");
        assert_eq!(undo(&fx, entry).oid, Some(main));
    }

    #[test]
    fn a_revert_finished_in_a_terminal_is_offered_for_undo() {
        let fx = clash("carry-revert-terminal");
        fx.write("f.txt", "main again\n");
        fx.commit_all("main again");
        let head = rev(&fx, "main");
        // Taking "main work" back clashes with "main again".
        assert!(fx.run_as(ops::revert(rev(&fx, "main~1"), false), Origin::User).result.is_err());
        resolve(&fx);
        git(&fx.dir, &["-c", "core.editor=true", "revert", "--continue"]);
        let entry = offered(&fx);
        assert!(entry.label.starts_with("Revert "), "{}", entry.label);
        assert_eq!(undo(&fx, entry).oid, Some(head));
    }

    #[test]
    fn a_cherry_pick_given_up_in_a_terminal_offers_nothing() {
        let fx = clash("carry-pick-given-up");
        let one = rev(&fx, "topic~1");
        assert!(fx.run_as(ops::cherry_pick(one, false), Origin::User).result.is_err());
        git(&fx.dir, &["cherry-pick", "--abort"]);
        assert!(look(&fx).is_none());

        // Aborted, then a commit of its own in the same place: not the pick.
        assert!(fx.run_as(ops::cherry_pick(one, false), Origin::User).result.is_err());
        git(&fx.dir, &["cherry-pick", "--abort"]);
        fx.write("f.txt", "own\n");
        fx.commit_all("own work");
        assert!(look(&fx).is_none());

        // Several, given up after the first: `--quit` keeps what was made.
        let picks = [one, rev(&fx, "topic")];
        assert!(fx.run_as(crate::git::multi::cherry_pick(&picks, false), Origin::User).result.is_err());
        resolve(&fx);
        git(&fx.dir, &["commit", "-q", "--no-edit"]);
        git(&fx.dir, &["cherry-pick", "--quit"]);
        assert!(look(&fx).is_none());
    }

    #[test]
    fn an_operation_finished_while_gitkrill_was_closed_is_offered_when_it_opens_again() {
        let mut fx = clash("carry-closed");
        let main = rev(&fx, "main");
        assert!(fx.run_as(ops::merge("topic"), Origin::User).result.is_err());
        // Concluded in a terminal while GitKrill is closed.
        resolve(&fx);
        git(&fx.dir, &["commit", "-q", "--no-edit"]);
        reopen(&mut fx);
        let entry = offered(&fx);
        assert_eq!(entry.label, "Merge topic");
        assert!(!saved(&fx).exists());
        assert_eq!(undo(&fx, entry).oid, Some(main));

        // Still waiting when it opens again, then concluded in GitKrill.
        assert!(fx.run_as(ops::merge("topic"), Origin::User).result.is_err());
        reopen(&mut fx);
        resolve(&fx);
        let done = fx.run(ops::commit("Merge topic", false));
        assert_eq!(undo(&fx, done.undo.expect("undoable as the merge")).oid, Some(main));

        // Aborted while closed: forgotten.
        assert!(fx.run_as(ops::merge("topic"), Origin::User).result.is_err());
        git(&fx.dir, &["merge", "--abort"]);
        reopen(&mut fx);
        assert!(look(&fx).is_none());
        assert!(!saved(&fx).exists());

        // A file that cannot be read is thrown away.
        std::fs::write(saved(&fx), "{ not what GitKrill writes").unwrap();
        let carry = Carry::open(&fx.dir);
        assert!(!carry.is_holding());
        assert!(!saved(&fx).exists());
    }

    #[test]
    fn what_is_saved_reads_back_the_same() {
        let fx = clash("carry-saved");
        git(&fx.dir, &["checkout", "-q", "topic"]);
        let cmd = GitCmd::new("Checkout main and merge topic", &["checkout", "main"])
            .step(&["merge", "--no-edit", "topic"])
            .undoable(UndoKind::Refs(vec![RefMove::new("refs/heads/main")]));
        assert!(fx.run_as(cmd, Origin::User).result.is_err());
        let text = std::fs::read(saved(&fx)).unwrap();
        let saved: Saved = serde_json::from_slice(&text).unwrap();
        assert_eq!(saved.label, "Checkout main and merge topic");
        assert_eq!(saved.refs, Some(vec![("refs/heads/main".to_owned(), Some(Sha(rev(&fx, "main"))))]));
        assert_eq!(Head::from(saved.before.clone()).branch.as_deref(), Some("topic"));
        assert_eq!(saved.stop.head.branch.as_deref(), Some("main"));
        assert_eq!(saved.stop.finish, Finish::Merge { heads: vec![Sha(rev(&fx, "topic"))] });
        let held = Held::from(saved);
        let again = held.saved().unwrap();
        assert_eq!(serde_json::to_vec_pretty(&again).unwrap(), text);
    }

    #[test]
    fn the_marker_names_the_operation_in_progress() {
        let fx = clash("carry-marker");
        let repo = || Repository::open(&fx.dir).unwrap();
        assert_eq!(marker(&repo()), None);
        let _ = std::process::Command::new("git").current_dir(&fx.dir).args(["merge", "topic"]).output();
        let merge = marker(&repo()).unwrap();
        assert!(merge.starts_with("MERGE_HEAD "), "{merge}");
        git(&fx.dir, &["merge", "--abort"]);
        git(&fx.dir, &["checkout", "-q", "topic"]);
        let _ = std::process::Command::new("git").current_dir(&fx.dir).args(["rebase", "main"]).output();
        let rebase = marker(&repo()).unwrap();
        assert!(rebase.starts_with("rebase refs/heads/topic "), "{rebase}");

        let mut carry = Carry::default();
        assert!(carry.check(&fx.dir).is_none());
        assert!(!carry.is_holding());
    }

    #[test]
    fn commands_that_carry_an_operation_on() {
        assert!(carries_on(&continue_rebase()));
        assert!(carries_on(&GitCmd::new("Skip", &["rebase", "--skip"])));
        assert!(carries_on(&ops::commit("m", false)));
        assert!(carries_on(&GitCmd::new("x", &["-c", "core.editor=true", "commit"])));
        assert!(!carries_on(&ops::abort(RepoState::Merge).unwrap()));
        assert!(!carries_on(&ops::discard_all()));
        assert!(!carries_on(&GitCmd::new("x", &["-c"])));
    }
}
