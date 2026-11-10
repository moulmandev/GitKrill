//! Every mutating operation, expressed as one or more `git` invocations.

use super::{ChangeKind, FileChange, Head, Oid, RepoState};

#[derive(Clone, Debug)]
pub struct Step {
    pub args: Vec<String>,
    pub stdin: Option<Vec<u8>>,
    /// The step names files; treat them as exact paths, never as globs.
    pub literal_paths: bool,
    /// Extra environment variables for this step only.
    pub env: Vec<(String, String)>,
}

impl Step {
    pub fn new(args: Vec<String>, stdin: Option<Vec<u8>>) -> Self {
        Step { args, stdin, literal_paths: false, env: Vec::new() }
    }

    /// A step whose path arguments must be taken literally.
    pub fn with_paths(args: Vec<String>, stdin: Option<Vec<u8>>) -> Self {
        Step { args, stdin, literal_paths: true, env: Vec::new() }
    }

    pub fn env(mut self, key: &str, value: impl Into<String>) -> Self {
        self.env.push((key.to_owned(), value.into()));
        self
    }
}

/// How (and whether) an operation can be undone. The operation thread turns
/// this into a concrete [`UndoEntry`] from HEAD before and after the command.
#[derive(Clone, Debug)]
pub enum UndoKind {
    None,
    /// Undone with a soft reset so the changes stay staged.
    Commit,
    /// HEAD moved on the same branch (merge, pull, cherry-pick, revert...).
    HeadMove,
    /// `git reset` with the given mode flag.
    Reset(&'static str),
    Checkout,
    BranchCreate { name: String, checkout: bool },
    BranchDelete(String),
    /// Several branches and tags change at once (a Gitflow finish merges
    /// into two branches, tags and deletes one): the operation thread notes
    /// where each of these refs pointed before and after, and undo puts
    /// every one of them back, HEAD included.
    Refs(Vec<RefMove>),
}

/// One ref of [`UndoKind::Refs`].
#[derive(Clone, Debug)]
pub struct RefMove {
    /// Full name (`refs/heads/develop`, `refs/tags/1.2`).
    pub name: String,
    /// Filled in by the operation thread; `None` when the ref does not exist.
    pub before: Option<Oid>,
    pub after: Option<Oid>,
}

impl RefMove {
    pub fn new(name: impl Into<String>) -> Self {
        RefMove { name: name.into(), before: None, after: None }
    }
}

#[derive(Clone, Debug)]
pub struct GitCmd {
    pub label: String,
    pub steps: Vec<Step>,
    pub undo: UndoKind,
    /// Routine operations (staging and the like) that need no success toast.
    pub quiet: bool,
    /// When set, the command only runs if HEAD is still exactly here. Undo and
    /// redo use this so they never rewrite a repository that moved on.
    pub expect_head: Option<Head>,
    /// Title of the success toast when the label ("Push") reads better in the
    /// past tense ("Pushed main to origin").
    pub success: Option<String>,
    /// Started by the app rather than the user (auto-fetch): never raises a
    /// toast or a credential prompt, whatever the outcome.
    pub background: bool,
    /// Said after "resolve, then Continue" when the command stops on merge
    /// conflicts: what is left to do once the merge is committed.
    pub conflict_note: Option<String>,
}

#[derive(Clone, Debug)]
pub struct UndoEntry {
    pub label: String,
    pub undo: GitCmd,
    pub redo: GitCmd,
}

fn s(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| (*a).to_owned()).collect()
}

impl GitCmd {
    pub fn new(label: impl Into<String>, args: &[&str]) -> Self {
        GitCmd {
            label: label.into(),
            steps: vec![Step::new(s(args), None)],
            undo: UndoKind::None,
            quiet: false,
            expect_head: None,
            success: None,
            background: false,
            conflict_note: None,
        }
    }

    /// A command with no steps yet; push [`Step`]s onto `steps`.
    pub fn empty(label: impl Into<String>) -> Self {
        GitCmd {
            label: label.into(),
            steps: Vec::new(),
            undo: UndoKind::None,
            quiet: false,
            expect_head: None,
            success: None,
            background: false,
            conflict_note: None,
        }
    }

    /// Sets the success toast title.
    pub fn done(mut self, message: impl Into<String>) -> Self {
        self.success = Some(message.into());
        self
    }

    /// What is left to do if the command stops on merge conflicts; see
    /// [`GitCmd::conflict_note`].
    pub fn after_conflict(mut self, note: impl Into<String>) -> Self {
        self.conflict_note = Some(note.into());
        self
    }

    /// No success toast: for routine operations such as staging.
    pub fn quiet(mut self) -> Self {
        self.quiet = true;
        self
    }

    fn expecting(mut self, head: &Head) -> Self {
        self.expect_head = Some(head.clone());
        self
    }

    /// Appends another git invocation, run only if the previous ones succeeded.
    pub fn step(mut self, args: &[&str]) -> Self {
        self.steps.push(Step::new(s(args), None));
        self
    }

    /// A step taking its paths NUL-separated on stdin, which sidesteps both
    /// command line length limits and any quoting concerns.
    fn step_with_paths<'a>(mut self, args: &[&str], paths: impl Iterator<Item = &'a str>) -> Self {
        let mut args = s(args);
        args.push("--pathspec-from-file=-".to_owned());
        args.push("--pathspec-file-nul".to_owned());
        let mut stdin = Vec::new();
        for p in paths {
            stdin.extend_from_slice(p.as_bytes());
            stdin.push(0);
        }
        self.steps.push(Step::with_paths(args, Some(stdin)));
        self
    }

    pub fn undoable(mut self, undo: UndoKind) -> Self {
        self.undo = undo;
        self
    }
}

fn label_for(verb: &str, paths: &[String]) -> String {
    match paths {
        [one] => format!("{verb} {}", file_name(one)),
        many => format!("{verb} {} files", many.len()),
    }
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

// ---- index / working tree -------------------------------------------------

pub fn stage(paths: &[String]) -> GitCmd {
    GitCmd::empty(label_for("Stage", paths))
        .step_with_paths(&["add", "-A"], paths.iter().map(String::as_str))
        .quiet()
}

pub fn stage_all() -> GitCmd {
    GitCmd::new("Stage all changes", &["add", "-A"]).quiet()
}

pub fn unstage(paths: &[String], unborn: bool) -> GitCmd {
    let cmd = GitCmd::empty(label_for("Unstage", paths)).quiet();
    let paths = paths.iter().map(String::as_str);
    if unborn {
        cmd.step_with_paths(&["rm", "--cached", "-r", "-q"], paths)
    } else {
        cmd.step_with_paths(&["reset", "-q", "HEAD"], paths)
    }
}

pub fn unstage_all(unborn: bool) -> GitCmd {
    if unborn {
        GitCmd::new("Unstage all changes", &["rm", "--cached", "-r", "-q", "--", "."]).quiet()
    } else {
        GitCmd::new("Unstage all changes", &["reset", "-q"]).quiet()
    }
}

/// Throws away unstaged changes to the given files. Untracked files are
/// deleted; tracked files are restored from the index.
pub fn discard(files: &[FileChange]) -> GitCmd {
    let paths: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
    let mut cmd = GitCmd::empty(label_for("Discard", &paths));

    let tracked: Vec<&str> = files
        .iter()
        .filter(|f| f.kind != ChangeKind::Untracked)
        .map(|f| f.path.as_str())
        .collect();
    if !tracked.is_empty() {
        cmd = cmd.step_with_paths(&["checkout"], tracked.into_iter());
    }

    let untracked: Vec<&str> = files
        .iter()
        .filter(|f| f.kind == ChangeKind::Untracked)
        .map(|f| f.path.as_str())
        .collect();
    // `git clean` has no --pathspec-from-file, so batch the arguments.
    for chunk in untracked.chunks(64) {
        let mut args = s(&["clean", "-f", "-d", "-q", "--"]);
        args.extend(chunk.iter().map(|p| (*p).to_owned()));
        cmd.steps.push(Step::with_paths(args, None));
    }
    cmd
}

pub fn discard_all() -> GitCmd {
    GitCmd::new("Discard all changes", &["reset", "--hard", "-q"]).step(&["clean", "-f", "-d", "-q"])
}

/// Applies a patch to the index (`cached`) or the working tree, optionally in
/// reverse. This is what stages, unstages and discards single hunks.
pub fn apply_patch(label: &str, patch: Vec<u8>, cached: bool, reverse: bool) -> GitCmd {
    let mut args = s(&["apply", "--whitespace=nowarn"]);
    if cached {
        args.push("--cached".to_owned());
    }
    if reverse {
        args.push("--reverse".to_owned());
    }
    args.push("-".to_owned());
    let mut cmd = GitCmd::empty(label).quiet();
    cmd.steps.push(Step::new(args, Some(patch)));
    cmd
}

pub fn commit(message: &str, amend: bool) -> GitCmd {
    let mut args = s(&["commit", "-F", "-"]);
    if amend {
        args.push("--amend".to_owned());
    }
    let mut cmd = GitCmd::empty(if amend { "Amend commit" } else { "Commit" });
    cmd.steps.push(Step::new(args, Some(message.as_bytes().to_vec())));
    cmd.undoable(UndoKind::Commit)
}

/// Stages everything, then commits: the one-click path when nothing is staged.
pub fn commit_everything(message: &str) -> GitCmd {
    let mut cmd = GitCmd::empty("Commit");
    cmd.steps.push(Step::new(s(&["add", "-A"]), None));
    cmd.steps.push(Step::new(s(&["commit", "-F", "-"]), Some(message.as_bytes().to_vec())));
    cmd.undoable(UndoKind::Commit)
}

// ---- branches -------------------------------------------------------------

pub fn checkout(target: &str) -> GitCmd {
    GitCmd::new(format!("Checkout {}", display_ref(target)), &["checkout", target])
        .undoable(UndoKind::Checkout)
}

/// Checks out a remote branch, creating the local tracking branch on first use.
pub fn checkout_remote(full: &str, local: &str, local_exists: bool) -> GitCmd {
    if local_exists {
        checkout(local)
    } else {
        GitCmd::new(format!("Checkout {local}"), &["checkout", "-b", local, "--track", full])
            .undoable(UndoKind::BranchCreate { name: local.to_owned(), checkout: true })
    }
}

pub fn create_branch(name: &str, at: Option<&str>, checkout: bool) -> GitCmd {
    let mut args: Vec<&str> = if checkout { vec!["checkout", "-b", name] } else { vec!["branch", name] };
    if let Some(at) = at {
        args.push(at);
    }
    GitCmd::new(format!("Create branch {name}"), &args)
        .undoable(UndoKind::BranchCreate { name: name.to_owned(), checkout })
}

pub fn delete_branch(name: &str, force: bool) -> GitCmd {
    GitCmd::new(format!("Delete branch {name}"), &["branch", if force { "-D" } else { "-d" }, name])
        .undoable(UndoKind::BranchDelete(name.to_owned()))
}

pub fn rename_branch(old: &str, new: &str) -> GitCmd {
    GitCmd::new(format!("Rename {old} to {new}"), &["branch", "-m", old, new])
}

pub fn delete_remote_branch(remote: &str, name: &str) -> GitCmd {
    GitCmd::new(format!("Delete {remote}/{name}"), &["push", remote, "--delete", name])
}

pub fn set_upstream(branch: &str, upstream: &str) -> GitCmd {
    GitCmd::new(
        format!("Track {upstream}"),
        &["branch", &format!("--set-upstream-to={upstream}"), branch],
    )
}

// ---- history --------------------------------------------------------------

pub fn merge(target: &str) -> GitCmd {
    GitCmd::new(format!("Merge {}", display_ref(target)), &["merge", "--no-edit", target])
        .undoable(UndoKind::HeadMove)
}

pub fn rebase(onto: &str) -> GitCmd {
    GitCmd::new(format!("Rebase onto {}", display_ref(onto)), &["rebase", onto])
        .undoable(UndoKind::HeadMove)
}

pub fn cherry_pick(oid: Oid, is_merge: bool) -> GitCmd {
    let sha = oid.to_string();
    let mut args = vec!["cherry-pick"];
    if is_merge {
        args.extend(["-m", "1"]);
    }
    args.push(&sha);
    GitCmd::new(format!("Cherry-pick {}", super::short(oid)), &args).undoable(UndoKind::HeadMove)
}

pub fn revert(oid: Oid, is_merge: bool) -> GitCmd {
    let sha = oid.to_string();
    let mut args = vec!["revert", "--no-edit"];
    if is_merge {
        args.extend(["-m", "1"]);
    }
    args.push(&sha);
    GitCmd::new(format!("Revert {}", super::short(oid)), &args).undoable(UndoKind::HeadMove)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetMode {
    Soft,
    Mixed,
    Hard,
}

impl ResetMode {
    pub fn flag(self) -> &'static str {
        match self {
            ResetMode::Soft => "--soft",
            ResetMode::Mixed => "--mixed",
            ResetMode::Hard => "--hard",
        }
    }

    pub fn name(self) -> &'static str {
        &self.flag()[2..]
    }
}

pub fn reset(target: &str, mode: ResetMode) -> GitCmd {
    GitCmd::new(
        format!("Reset ({}) to {}", mode.name(), display_ref(target)),
        &["reset", mode.flag(), target],
    )
    .undoable(UndoKind::Reset(mode.flag()))
}

// ---- tags -----------------------------------------------------------------

pub fn create_tag(name: &str, target: &str, message: &str) -> GitCmd {
    let label = format!("Create tag {name}");
    if message.trim().is_empty() {
        GitCmd::new(label, &["tag", name, target])
    } else {
        GitCmd::new(label, &["tag", "-a", name, "-m", message.trim(), target])
    }
}

pub fn delete_tag(name: &str) -> GitCmd {
    GitCmd::new(format!("Delete tag {name}"), &["tag", "-d", name])
}

pub fn push_tag(remote: &str, name: &str) -> GitCmd {
    GitCmd::new(format!("Push tag {name}"), &["push", remote, &format!("refs/tags/{name}")])
}

// ---- stashes --------------------------------------------------------------

pub fn stash_push(message: &str) -> GitCmd {
    let mut args = vec!["stash", "push", "--include-untracked"];
    let message = message.trim();
    if !message.is_empty() {
        args.extend(["-m", message]);
    }
    GitCmd::new("Stash changes", &args)
}

fn stash_ref(index: usize) -> String {
    format!("stash@{{{index}}}")
}

pub fn stash_pop(index: usize) -> GitCmd {
    GitCmd::new("Pop stash", &["stash", "pop", &stash_ref(index)])
}

pub fn stash_apply(index: usize) -> GitCmd {
    GitCmd::new("Apply stash", &["stash", "apply", &stash_ref(index)])
}

pub fn stash_drop(index: usize) -> GitCmd {
    GitCmd::new("Delete stash", &["stash", "drop", &stash_ref(index)])
}

// ---- remotes --------------------------------------------------------------

/// `git fetch <args>`, with `--prune` unless Auto-Prune (Preferences >
/// General) is off.
pub fn fetch<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut all = vec!["fetch"];
    if crate::prefs::general::auto_prune() {
        all.push("--prune");
    }
    all.extend_from_slice(args);
    all
}

pub fn fetch_all() -> GitCmd {
    GitCmd::new("Fetch", &fetch(&["--all"])).done("Fetched all remotes")
}

/// The periodic fetch that keeps remote branches current without being asked.
pub fn auto_fetch() -> GitCmd {
    let mut cmd = GitCmd::new("Auto-fetch", &fetch(&["--all"])).quiet();
    cmd.background = true;
    cmd
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PullMode {
    Default,
    FastForwardOnly,
    Rebase,
}

pub fn pull(mode: PullMode) -> GitCmd {
    let args: &[&str] = match mode {
        PullMode::Default => &["pull"],
        PullMode::FastForwardOnly => &["pull", "--ff-only"],
        PullMode::Rebase => &["pull", "--rebase"],
    };
    GitCmd::new("Pull", args).undoable(UndoKind::HeadMove).done("Pulled")
}

/// `upstream` is `(remote, branch)` when the branch has no upstream yet and
/// one should be set while pushing.
pub fn push(upstream: Option<(&str, &str)>, force: bool) -> GitCmd {
    let mut args = vec!["push"];
    if force {
        args.push("--force-with-lease");
    }
    if let Some((remote, branch)) = upstream {
        args.extend(["-u", remote, branch]);
    }
    let done = match upstream {
        Some((remote, branch)) => format!("Pushed {branch} to {remote}"),
        None => "Pushed".to_owned(),
    };
    GitCmd::new(if force { "Force push" } else { "Push" }, &args).done(done)
}

pub fn push_branch(remote: &str, branch: &str) -> GitCmd {
    GitCmd::new(format!("Push {branch}"), &["push", "-u", remote, branch])
        .done(format!("Pushed {branch} to {remote}"))
}

// ---- in-progress operations ----------------------------------------------

fn state_verb(state: RepoState) -> Option<&'static str> {
    match state {
        RepoState::Merge => Some("merge"),
        RepoState::Rebase => Some("rebase"),
        RepoState::CherryPick => Some("cherry-pick"),
        RepoState::Revert => Some("revert"),
        _ => None,
    }
}

pub fn abort(state: RepoState) -> Option<GitCmd> {
    let verb = state_verb(state)?;
    Some(GitCmd::new(format!("Abort {verb}"), &[verb, "--abort"]))
}

pub fn continue_op(state: RepoState) -> Option<GitCmd> {
    let verb = state_verb(state)?;
    Some(GitCmd::new(format!("Continue {verb}"), &[verb, "--continue"]))
}

// ---- undo -----------------------------------------------------------------

fn display_ref(r: &str) -> String {
    if r.len() == 40 && r.bytes().all(|b| b.is_ascii_hexdigit()) {
        r[..7].to_owned()
    } else {
        r.to_owned()
    }
}

/// Builds the inverse of a finished operation, if it has one.
///
/// `deleted_at` is where a deleted branch pointed before it was removed.
pub fn build_undo(
    cmd: &GitCmd,
    before: &Head,
    after: &Head,
    deleted_at: Option<Oid>,
) -> Option<UndoEntry> {
    let label = cmd.label.clone();
    // Undo expects the repository exactly as the command left it, redo
    // exactly as it was beforehand.
    let entry = |undo: GitCmd, redo: GitCmd| {
        Some(UndoEntry {
            label: label.clone(),
            undo: undo.expecting(after),
            redo: redo.expecting(before),
        })
    };
    let detached = |undo: GitCmd, redo: GitCmd| Some(UndoEntry { label: label.clone(), undo, redo });
    let plain = |l: String, args: &[&str]| GitCmd::new(l, args);

    match &cmd.undo {
        UndoKind::None => None,

        UndoKind::Commit => {
            let new = after.oid?.to_string();
            if before.branch != after.branch || before.oid == after.oid {
                return None;
            }
            let undo = match before.oid {
                Some(old) => plain(format!("Undo {label}"), &["reset", "--soft", &old.to_string()]),
                // Undoing the very first commit: make the branch unborn again.
                None => plain(format!("Undo {label}"), &["update-ref", "-d", "HEAD"]),
            };
            entry(undo, plain(format!("Redo {label}"), &["reset", "--soft", &new]))
        }

        UndoKind::HeadMove => {
            let (old, new) = (before.oid?.to_string(), after.oid?.to_string());
            if before.branch != after.branch || old == new {
                return None;
            }
            // --keep refuses to clobber local changes instead of destroying them.
            entry(
                plain(format!("Undo {label}"), &["reset", "--keep", &old]),
                plain(format!("Redo {label}"), &["reset", "--keep", &new]),
            )
        }

        UndoKind::Reset(flag) => {
            let (old, new) = (before.oid?.to_string(), after.oid?.to_string());
            if old == new {
                return None;
            }
            entry(
                plain(format!("Undo {label}"), &["reset", flag, &old]),
                plain(format!("Redo {label}"), &["reset", flag, &new]),
            )
        }

        UndoKind::Checkout => {
            let (old, new) = (before.refname()?, after.refname()?);
            if old == new {
                return None;
            }
            entry(
                plain(format!("Undo {label}"), &["checkout", &old]),
                plain(format!("Redo {label}"), &["checkout", &new]),
            )
        }

        UndoKind::BranchCreate { name, checkout } => {
            let at = after.oid.map(|o| o.to_string());
            if *checkout {
                let old = before.refname()?;
                let undo = plain(format!("Undo {label}"), &["checkout", &old])
                    .step(&["branch", "-D", name]);
                let mut args = vec!["checkout", "-b", name.as_str()];
                if let Some(at) = &at {
                    args.push(at);
                }
                entry(undo, plain(format!("Redo {label}"), &args))
            } else {
                // The branch may have been created somewhere other than HEAD,
                // so redo replays the original command rather than guessing.
                let undo = plain(format!("Undo {label}"), &["branch", "-D", name]);
                let mut redo = GitCmd::empty(format!("Redo {label}"));
                redo.steps = cmd.steps.clone();
                // Independent of where HEAD is.
                detached(undo, redo)
            }
        }

        UndoKind::BranchDelete(name) => {
            let at = deleted_at?.to_string();
            detached(
                plain(format!("Undo {label}"), &["branch", name, &at]),
                plain(format!("Redo {label}"), &["branch", "-D", name]),
            )
        }

        UndoKind::Refs(moves) => {
            if before == after && moves.iter().all(|m| m.before == m.after) {
                return None;
            }
            let back: Vec<(&str, Option<Oid>, Option<Oid>)> =
                moves.iter().map(|m| (m.name.as_str(), m.after, m.before)).collect();
            let forth: Vec<(&str, Option<Oid>, Option<Oid>)> =
                moves.iter().map(|m| (m.name.as_str(), m.before, m.after)).collect();
            entry(
                restore_refs(format!("Undo {label}"), &back, after, before)?,
                restore_refs(format!("Redo {label}"), &forth, before, after)?,
            )
        }
    }
}

/// Moves every ref in `refs` (name, where it points now, where it should
/// point; `None` is "does not exist") and HEAD from `now` to `goal`. The
/// branch HEAD ends up on moves with the working tree (`reset --keep`,
/// which refuses to clobber local changes); every other ref is moved with
/// `update-ref`, which checks it still points where it did.
fn restore_refs(label: String, refs: &[(&str, Option<Oid>, Option<Oid>)], now: &Head, goal: &Head) -> Option<GitCmd> {
    let absent = Oid::ZERO_SHA1.to_string();
    let goal_ref = goal.branch.as_ref().map(|b| format!("refs/heads/{b}"));
    let goal_move = refs.iter().find(|(name, _, _)| Some(*name) == goal_ref.as_deref());
    let mut cmd = GitCmd::empty(label);

    // The branch to end up on must exist before it can be checked out.
    if let (Some(goal_ref), Some((_, None, Some(want)))) = (&goal_ref, goal_move) {
        cmd = cmd.step(&["update-ref", goal_ref, &want.to_string(), &absent]);
    }
    if now.branch != goal.branch || now.detached != goal.detached {
        cmd = match &goal.branch {
            Some(branch) => cmd.step(&["checkout", branch]),
            None => cmd.step(&["checkout", "--detach", &goal.oid?.to_string()]),
        };
    }
    if let Some((_, Some(current), Some(want))) = goal_move
        && current != want
    {
        cmd = cmd.step(&["reset", "--keep", &want.to_string()]);
    }
    for (name, current, want) in refs {
        if Some(*name) == goal_ref.as_deref() || current == want {
            continue;
        }
        cmd = match (current, want) {
            (Some(current), None) => cmd.step(&["update-ref", "-d", name, &current.to_string()]),
            (None, Some(want)) => cmd.step(&["update-ref", name, &want.to_string(), &absent]),
            (Some(current), Some(want)) => cmd.step(&["update-ref", name, &want.to_string(), &current.to_string()]),
            (None, None) => cmd,
        };
    }
    Some(cmd)
}
