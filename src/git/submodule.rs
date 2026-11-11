//! Submodules: what the sidebar shows about each one (read with every
//! snapshot, through libgit2), what a change of one looks like in a diff,
//! and the git commands that add, update, sync and remove them.
//!
//! The superproject records a commit for each submodule (a "gitlink" in its
//! tree and index); the submodule's own clone may be checked out there, at
//! another commit, or not at all.

use std::path::Path;

use git2::{Repository, SubmoduleIgnore, SubmoduleStatus};

use super::ops::{GitCmd, Step};
use super::repo::lossy;
use super::{Oid, WorkingStatus};

/// Where a submodule's clone is, compared with what the superproject records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Listed, but not cloned and checked out here (`git submodule update
    /// --init` does that).
    NotInitialized,
    /// Checked out at the commit the superproject records.
    Recorded,
    /// Checked out at another commit.
    Moved,
}

#[derive(Clone, Debug)]
pub struct SubmoduleInfo {
    pub name: String,
    /// Relative to the working tree, with `/`.
    pub path: String,
    /// As `.gitmodules` (or the repository's configuration) has it.
    pub url: Option<String>,
    /// The branch `.gitmodules` names for `git submodule update --remote`.
    pub branch: Option<String>,
    /// The commit the superproject records: the index's, which is HEAD's
    /// unless a move was staged. `None` for one added to `.gitmodules` only.
    pub recorded: Option<Oid>,
    /// Where the submodule's own HEAD is; `None` when it is not checked out.
    pub checked_out: Option<Oid>,
    pub state: State,
    /// It has modified or untracked files of its own.
    pub modified: bool,
}

impl SubmoduleInfo {
    /// A few words for the sidebar: "not initialized", "abc1234",
    /// "abc1234, modified".
    pub fn summary(&self) -> String {
        let at = match (self.state, self.checked_out) {
            (State::NotInitialized, _) | (_, None) => return "not initialized".to_owned(),
            (_, Some(oid)) => super::short(oid),
        };
        if self.modified { format!("{at}, modified") } else { at }
    }

    /// One sentence on its state, for a tooltip.
    pub fn describe(&self) -> String {
        let recorded = self.recorded.map(super::short);
        let mut text = match (self.state, self.checked_out, recorded) {
            (State::NotInitialized, ..) | (_, None, _) => "Not initialized: its files are not checked out here.".to_owned(),
            (State::Recorded, Some(at), _) => format!("At {}, the commit this repository records.", super::short(at)),
            (State::Moved, Some(at), Some(recorded)) => {
                format!("Checked out at {}; this repository records {recorded}.", super::short(at))
            }
            (State::Moved, Some(at), None) => format!("Checked out at {}; no commit recorded yet.", super::short(at)),
        };
        if self.modified {
            text.push_str(" Has modified or untracked files.");
        }
        text
    }
}

/// The submodules of `repo`, with the state of each. `status` is the
/// working tree status just read: a submodule whose files changed is listed
/// there as modified, which saves looking inside it in the common case.
pub fn read(repo: &Repository, status: &WorkingStatus) -> Vec<SubmoduleInfo> {
    let Ok(subs) = repo.submodules() else { return Vec::new() };
    let mut found: Vec<SubmoduleInfo> = subs
        .iter()
        // One whose removal is staged is still in HEAD's tree, but neither
        // in the index nor in `.gitmodules`: it is gone.
        .filter(|sub| sub.index_id().is_some() || sub.opt_url_bytes().is_some())
        .map(|sub| {
            let name = lossy(sub.name_bytes());
            let path = sub.path().to_string_lossy().replace('\\', "/");
            let recorded = sub.index_id().or_else(|| sub.head_id());
            let checked_out = sub.workdir_id();
            let state = match checked_out {
                None => State::NotInitialized,
                Some(at) if Some(at) == recorded => State::Recorded,
                Some(_) => State::Moved,
            };
            let listed = status.unstaged.iter().any(|f| f.path == path);
            let modified = listed
                && match state {
                    State::NotInitialized => false,
                    // Listed although at the recorded commit: its files changed.
                    State::Recorded => true,
                    // Listed for the move anyway; look inside.
                    State::Moved => repo.submodule_status(&name, SubmoduleIgnore::None).is_ok_and(|st| {
                        st.intersects(
                            SubmoduleStatus::WD_INDEX_MODIFIED
                                | SubmoduleStatus::WD_WD_MODIFIED
                                | SubmoduleStatus::WD_UNTRACKED,
                        )
                    }),
                };
            SubmoduleInfo {
                url: sub.opt_url_bytes().map(lossy),
                branch: sub.branch_bytes().map(lossy),
                name,
                path,
                recorded,
                checked_out,
                state,
                modified,
            }
        })
        .collect();
    found.sort_by_cached_key(|sub| sub.path.to_lowercase());
    found
}

// ---- in a diff ------------------------------------------------------------

/// A submodule's change in a diff: the commit it moved from and to, with
/// what can be found out about them in its clone.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Change {
    /// `None` when the submodule was added.
    pub old: Option<Oid>,
    /// `None` when it was removed.
    pub new: Option<Oid>,
    /// The summaries of `old` and `new`, when its clone has them.
    pub old_summary: Option<String>,
    pub new_summary: Option<String>,
    /// Commits `new` has that `old` has not, and the other way round.
    pub ahead: usize,
    pub behind: usize,
    /// The first of those commits, newest first: the ones `new` adds, or
    /// those it leaves behind when it only goes back.
    pub commits: Vec<(Oid, String)>,
    /// Its clone was found, so the above could be looked up.
    pub checked_out: bool,
    /// Working tree side: its own files have changes too.
    pub dirty: bool,
}

/// How many commits of a move are listed.
const LISTED: usize = 12;

impl Change {
    /// A change between the commits git's diff reports (zero for none).
    pub fn between(old: Oid, new: Oid) -> Self {
        let some = |oid: Oid| (!oid.is_zero()).then_some(oid);
        Change { old: some(old), new: some(new), ..Change::default() }
    }

    /// "moved from abc1234 to def5678", "added at def5678", ...
    pub fn headline(&self) -> String {
        match (self.old, self.new) {
            (Some(old), Some(new)) if old == new => format!("at {}", super::short(new)),
            (Some(old), Some(new)) => format!("moved from {} to {}", super::short(old), super::short(new)),
            (None, Some(new)) => format!("added at {}", super::short(new)),
            (Some(old), None) => format!("removed (was at {})", super::short(old)),
            (None, None) => "not checked out".to_owned(),
        }
    }

    /// Looks the commits up in the submodule's clone, if it has one here.
    /// `path` is the submodule's path in the superproject `repo`.
    pub fn describe(&mut self, repo: &Repository, path: &str) {
        let Some(sub) = open(repo, path) else { return };
        self.checked_out = true;
        let summary = |oid: Option<Oid>| {
            let commit = sub.find_commit(oid?).ok()?;
            Some(lossy(commit.summary_bytes().unwrap_or_default()))
        };
        self.old_summary = summary(self.old);
        self.new_summary = summary(self.new);
        let (Some(old), Some(new)) = (self.old, self.new) else { return };
        if old == new || self.old_summary.is_none() || self.new_summary.is_none() {
            return;
        }
        (self.ahead, self.behind) = sub.graph_ahead_behind(new, old).unwrap_or((0, 0));
        let (from, hide) = if self.ahead > 0 { (new, old) } else { (old, new) };
        let Ok(mut walk) = sub.revwalk() else { return };
        if walk.push(from).is_err() || walk.hide(hide).is_err() {
            return;
        }
        let _ = walk.set_sorting(git2::Sort::TOPOLOGICAL);
        self.commits = walk
            .flatten()
            .take(LISTED)
            .filter_map(|oid| Some((oid, lossy(sub.find_commit(oid).ok()?.summary_bytes().unwrap_or_default()))))
            .collect();
    }
}

/// The submodule at `path` of `repo`, opened, when it is cloned here: the
/// one the superproject knows of, or else a repository sitting at that path
/// (a submodule since removed from `.gitmodules`).
fn open(repo: &Repository, path: &str) -> Option<Repository> {
    if let Ok(sub) = repo.find_submodule(path).and_then(|sub| sub.open()) {
        return Some(sub);
    }
    let dir = repo.workdir()?.join(path);
    if !dir.join(".git").exists() {
        return None;
    }
    Repository::open(dir).ok()
}

// ---- commands ---------------------------------------------------------------

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|arg| (*arg).to_owned()).collect()
}

/// `git <list> -- <paths>` with the paths taken literally.
fn with_paths(list: &[&str], paths: &[&str]) -> Step {
    let mut args = args(list);
    args.push("--".to_owned());
    args.extend(paths.iter().map(|path| (*path).to_owned()));
    Step::with_paths(args, None)
}

fn one(label: String, step: Step) -> GitCmd {
    let mut cmd = GitCmd::empty(label);
    cmd.steps.push(step);
    cmd
}

/// Clones what is missing and checks out the recorded commits, submodules
/// inside included: of the submodule at `path`, or of all with `None`.
pub fn init_update(path: Option<&str>) -> GitCmd {
    let list = ["submodule", "update", "--init", "--recursive"];
    match path {
        Some(path) => one(format!("Update submodule {path}"), with_paths(&list, &[path]))
            .done(format!("Updated submodule {path}")),
        None => GitCmd::new("Update submodules", &list).done("Updated the submodules"),
    }
}

/// Checks out the commit the superproject records in the submodule at
/// `path`, and in the submodules inside it, cloning only what is missing.
/// Files changed inside it are kept unless they are in the way.
pub fn to_recorded(path: &str) -> GitCmd {
    let list = ["submodule", "update", "--init", "--recursive"];
    one(format!("Check out the recorded commit of {path}"), with_paths(&list, &[path]))
        .done(format!("{path} is at the recorded commit"))
}

/// Copies the URLs of `.gitmodules` to the repository's configuration and
/// the submodules' own remotes (after a URL changed upstream).
pub fn sync(path: Option<&str>) -> GitCmd {
    let list = ["submodule", "sync", "--recursive"];
    match path {
        Some(path) => {
            one(format!("Sync URL of {path}"), with_paths(&list, &[path])).done(format!("Synced the URL of {path}"))
        }
        None => GitCmd::new("Sync submodule URLs", &list).done("Synced the submodule URLs"),
    }
}

/// Adds the repository at `url` as a submodule at `path`, following
/// `branch` when one is given. The new `.gitmodules` and the submodule
/// come staged, ready to commit.
pub fn add(url: &str, path: &str, branch: &str) -> GitCmd {
    let mut list = vec!["submodule", "add"];
    if !branch.is_empty() {
        list.extend(["-b", branch]);
    }
    list.extend(["--", url, path]);
    GitCmd::new(format!("Add submodule {path}"), &list).done(format!("Added submodule {path}"))
}

/// Takes the submodule at `path` out of the repository: its files and
/// configuration go (`deinit -f`, which drops changes made inside it), and
/// its removal from the index and `.gitmodules` is staged (`git rm`). Its
/// clone stays in `.git/modules`, as git leaves it, so adding it back is quick.
pub fn remove(path: &str) -> GitCmd {
    let mut cmd = GitCmd::empty(format!("Remove submodule {path}"));
    cmd.steps.push(with_paths(&["submodule", "deinit", "-f"], &[path]));
    cmd.steps.push(with_paths(&["rm", "-f", "-q"], &[path]));
    cmd.done(format!("Removed submodule {path}"))
}

/// Where a new submodule goes by default: the last part of its URL,
/// without `.git` (`https://host/team/lib.git` gives `lib`).
pub fn path_from_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches(['/', '\\']);
    let last = trimmed.rsplit(['/', '\\', ':']).next().unwrap_or_default();
    last.strip_suffix(".git").unwrap_or(last).to_owned()
}

/// What is wrong with `path` as the place of a new submodule, if anything
/// (an empty message: nothing typed yet). `existing` are the paths of the
/// submodules there already. Whether the folder is free is left to git,
/// which says so.
pub fn check_path<'a>(path: &str, mut existing: impl Iterator<Item = &'a str>) -> Result<(), String> {
    let path = path.trim();
    if path.is_empty() {
        return Err(String::new());
    }
    let normal = path.replace('\\', "/");
    let normal = normal.trim_end_matches('/');
    let absolute = Path::new(path).is_absolute() || normal.starts_with('/') || normal.get(1..2) == Some(":");
    if absolute || normal.split('/').any(|part| part == ".." || part == ".git") {
        return Err("Give a folder inside this repository.".to_owned());
    }
    if existing.any(|other| other == normal) {
        return Err(format!("A submodule is already at {normal}."));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::DiffSource;
    use crate::tests::{Fixture, git};

    /// Lets git clone a local folder for the commands of `cmd` (git refuses
    /// the file protocol for submodules unless told otherwise), without
    /// touching any configuration file.
    fn allow_file(mut cmd: GitCmd) -> GitCmd {
        for step in &mut cmd.steps {
            step.env.push(("GIT_CONFIG_COUNT".to_owned(), "1".to_owned()));
            step.env.push(("GIT_CONFIG_KEY_0".to_owned(), "protocol.file.allow".to_owned()));
            step.env.push(("GIT_CONFIG_VALUE_0".to_owned(), "always".to_owned()));
        }
        cmd
    }

    /// A library repository with two commits, and its URL.
    fn library(name: &str) -> (Fixture, String, Oid, Oid) {
        let lib = Fixture::new(name);
        lib.write("lib.txt", "one\n");
        lib.commit_all("lib one");
        let first = Oid::from_str(&git(&lib.dir, &["rev-parse", "HEAD"])).unwrap();
        lib.write("lib.txt", "two\n");
        lib.commit_all("lib two");
        let second = Oid::from_str(&git(&lib.dir, &["rev-parse", "HEAD"])).unwrap();
        let url = format!("file:///{}", lib.dir.to_string_lossy().replace('\\', "/").trim_start_matches('/'));
        (lib, url, first, second)
    }

    fn state_of(fx: &Fixture, path: &str) -> SubmoduleInfo {
        let snap = fx.snapshot();
        snap.submodules.iter().find(|s| s.path == path).cloned().unwrap_or_else(|| panic!("{path} is listed"))
    }

    #[test]
    fn paths_come_from_urls_and_are_checked() {
        assert_eq!(path_from_url("https://example.com/team/lib.git"), "lib");
        assert_eq!(path_from_url("git@example.com:team/my-lib.git/"), "my-lib");
        assert_eq!(path_from_url("git@example.com:lib"), "lib");
        assert_eq!(path_from_url("C:\\code\\lib\\"), "lib");
        assert_eq!(path_from_url(""), "");

        let none = std::iter::empty::<&str>;
        assert!(check_path("libs/new one", none()).is_ok());
        assert!(check_path("libs/new one/", none()).is_ok());
        assert!(check_path("../outside", none()).is_err());
        assert!(check_path("/abs", none()).is_err());
        assert!(check_path("C:/abs", none()).is_err());
        assert!(check_path("a/.git/b", none()).is_err());
        assert!(check_path("lib/", ["lib"].into_iter()).is_err());
        assert_eq!(check_path("  ", none()), Err(String::new()));
    }

    #[test]
    fn commands_take_paths_literally() {
        let cmd = init_update(Some("libs/my lib"));
        assert_eq!(cmd.steps[0].args, ["submodule", "update", "--init", "--recursive", "--", "libs/my lib"]);
        assert!(cmd.steps[0].literal_paths);
        assert_eq!(init_update(None).steps[0].args, ["submodule", "update", "--init", "--recursive"]);
        assert_eq!(sync(Some("lib")).steps[0].args, ["submodule", "sync", "--recursive", "--", "lib"]);
        assert_eq!(add("../lib.git", "lib", "").steps[0].args, ["submodule", "add", "--", "../lib.git", "lib"]);
        assert_eq!(add("u", "p", "main").steps[0].args, ["submodule", "add", "-b", "main", "--", "u", "p"]);
        let remove = remove("my lib");
        assert_eq!(remove.steps[0].args, ["submodule", "deinit", "-f", "--", "my lib"]);
        assert_eq!(remove.steps[1].args, ["rm", "-f", "-q", "--", "my lib"]);
    }

    #[test]
    fn a_submodule_is_added_updated_moved_and_removed() {
        let (_lib, url, first, second) = library("submodule-life-lib");
        let fx = Fixture::new("submodule-life");
        fx.write("app.txt", "app\n");
        fx.commit_all("app");
        assert!(fx.snapshot().submodules.is_empty());

        // A name with a space, following a branch.
        fx.run(allow_file(add(&url, "libs/my lib", "main")));
        let added = state_of(&fx, "libs/my lib");
        assert_eq!(added.state, State::Recorded);
        assert_eq!(added.branch.as_deref(), Some("main"));
        assert_eq!(added.checked_out, Some(second));
        assert!(!added.modified);
        assert!(fx.read(".gitmodules").contains("path = libs/my lib"));
        fx.commit_all("add lib");

        // Checked out at another commit: listed as a change, and moved back.
        let inside = fx.dir.join("libs/my lib");
        git(&inside, &["checkout", "-q", &first.to_string()]);
        let moved = state_of(&fx, "libs/my lib");
        assert_eq!((moved.state, moved.checked_out, moved.recorded), (State::Moved, Some(first), Some(second)));
        assert!(!moved.modified);
        assert_eq!(moved.summary(), crate::git::short(first));
        fx.run(to_recorded("libs/my lib"));
        assert_eq!(state_of(&fx, "libs/my lib").state, State::Recorded);

        // Files changed inside it.
        std::fs::write(inside.join("lib.txt"), "changed\n").unwrap();
        let modified = state_of(&fx, "libs/my lib");
        assert_eq!(modified.state, State::Recorded);
        assert!(modified.modified);
        assert_eq!(modified.summary(), format!("{}, modified", crate::git::short(second)));
        git(&inside, &["checkout", "-q", "--", "lib.txt"]);
        std::fs::write(inside.join("notes.txt"), "untracked\n").unwrap();
        git(&inside, &["checkout", "-q", &first.to_string()]);
        let both = state_of(&fx, "libs/my lib");
        assert_eq!(both.state, State::Moved);
        assert!(both.modified, "a move does not hide an untracked file");

        // Removing it stages its removal; its folder is gone.
        fx.run(remove("libs/my lib"));
        let snap = fx.snapshot();
        assert!(snap.submodules.is_empty(), "{:?}", snap.submodules);
        assert!(!inside.join("lib.txt").exists());
        assert!(snap.status.staged.iter().any(|f| f.path == "libs/my lib"), "{:?}", snap.status);
    }

    #[test]
    fn a_fresh_clone_has_its_submodules_uninitialized_until_updated() {
        let (_lib, url, _, second) = library("submodule-clone-lib");
        let origin = Fixture::new("submodule-clone-origin");
        origin.write("app.txt", "app\n");
        origin.commit_all("app");
        origin.run(allow_file(add(&url, "lib", "")));
        origin.commit_all("add lib");

        let fx = Fixture::new("submodule-clone");
        let from = origin.dir.to_string_lossy().replace('\\', "/");
        git(&fx.dir, &["-c", "protocol.file.allow=always", "pull", "-q", &from, "main"]);
        let sub = state_of(&fx, "lib");
        assert_eq!(sub.state, State::NotInitialized);
        assert_eq!(sub.recorded, Some(second));
        assert_eq!(sub.summary(), "not initialized");

        // Sync has nothing to do before the submodule is initialized; the
        // update registers and clones it.
        fx.run(sync(None));
        fx.run(allow_file(init_update(None)));
        let sub = state_of(&fx, "lib");
        assert_eq!((sub.state, sub.checked_out), (State::Recorded, Some(second)));
        // Nothing to do: still fine.
        fx.run(allow_file(init_update(Some("lib"))));
    }

    #[test]
    fn a_moved_submodule_diffs_as_a_move_and_stages() {
        let (_lib, url, first, second) = library("submodule-diff-lib");
        let fx = Fixture::new("submodule-diff");
        fx.write("app.txt", "app\n");
        fx.commit_all("app");
        fx.run(allow_file(add(&url, "lib", "")));
        fx.commit_all("add lib");
        git(&fx.dir.join("lib"), &["checkout", "-q", &first.to_string()]);

        let snap = fx.snapshot();
        assert!(snap.status.unstaged.iter().any(|f| f.path == "lib"));
        let diff = fx.diff(DiffSource::Unstaged, "lib");
        let change = diff.submodule.as_ref().expect("the diff knows it is a submodule");
        assert_eq!((change.old, change.new), (Some(second), Some(first)));
        assert_eq!(change.headline(), format!("moved from {} to {}", crate::git::short(second), crate::git::short(first)));
        assert_eq!(change.old_summary.as_deref(), Some("lib two"));
        assert_eq!(change.new_summary.as_deref(), Some("lib one"));
        assert_eq!((change.ahead, change.behind), (0, 1));
        assert_eq!(change.commits, [(second, "lib two".to_owned())], "the commit it goes back over");
        assert!(change.checked_out && !change.dirty);

        // Staging records the new commit.
        fx.run(crate::git::ops::stage(&["lib".to_owned()]));
        let snap = fx.snapshot();
        assert!(snap.status.staged.iter().any(|f| f.path == "lib"), "{:?}", snap.status);
        assert!(!snap.status.unstaged.iter().any(|f| f.path == "lib"));
        assert_eq!(git(&fx.dir, &["rev-parse", ":lib"]), first.to_string());
        let staged = fx.diff(DiffSource::Staged, "lib");
        assert_eq!(staged.submodule.as_ref().map(|c| (c.old, c.new)), Some((Some(second), Some(first))));
        // And the moved state follows the index.
        assert_eq!(state_of(&fx, "lib").state, State::Recorded);
        fx.run(crate::git::ops::unstage(&["lib".to_owned()], false));
        assert_eq!(state_of(&fx, "lib").state, State::Moved);

        // Changed files inside show as dirty, and the commit of the move
        // in history reads the same way.
        std::fs::write(fx.dir.join("lib/lib.txt"), "dirty\n").unwrap();
        let dirty = fx.diff(DiffSource::Unstaged, "lib");
        assert!(dirty.submodule.as_ref().is_some_and(|c| c.dirty), "{:?}", dirty.submodule);
        git(&fx.dir.join("lib"), &["checkout", "-q", "--", "lib.txt"]);
        fx.commit_all("lib back to one");
        let head = Oid::from_str(&git(&fx.dir, &["rev-parse", "HEAD"])).unwrap();
        let committed = fx.diff(DiffSource::Commit(head), "lib");
        let change = committed.submodule.as_ref().expect("a submodule in history");
        assert_eq!((change.old, change.new), (Some(second), Some(first)));
        assert!(!change.dirty);
        // The commit that added it.
        let parent = Oid::from_str(&git(&fx.dir, &["rev-parse", "HEAD~1"])).unwrap();
        let added = fx.diff(DiffSource::Commit(parent), "lib");
        assert_eq!(added.submodule.as_ref().map(|c| c.headline()), Some(format!("added at {}", crate::git::short(second))));
    }

    #[test]
    fn a_move_forward_lists_the_new_commits() {
        let mut change = Change::between(Oid::ZERO_SHA1, Oid::ZERO_SHA1);
        assert_eq!(change, Change::default());
        assert_eq!(change.headline(), "not checked out");

        let (lib, url, first, second) = library("submodule-forward-lib");
        let fx = Fixture::new("submodule-forward");
        fx.write("app.txt", "app\n");
        fx.commit_all("app");
        fx.run(allow_file(add(&url, "lib", "")));
        let repo = Repository::open(&fx.dir).unwrap();
        change = Change::between(first, second);
        change.describe(&repo, "lib");
        assert_eq!((change.ahead, change.behind), (1, 0));
        assert_eq!(change.commits, [(second, "lib two".to_owned())]);
        // A commit its clone does not have yet: no summary, nothing listed.
        lib.write("lib.txt", "three\n");
        lib.commit_all("lib three");
        let third = Oid::from_str(&git(&lib.dir, &["rev-parse", "HEAD"])).unwrap();
        let mut unknown = Change::between(second, third);
        unknown.describe(&repo, "lib");
        assert!(unknown.checked_out);
        assert_eq!((unknown.old_summary.as_deref(), unknown.new_summary.as_deref()), (Some("lib two"), None));
        assert!(unknown.commits.is_empty());
    }
}
