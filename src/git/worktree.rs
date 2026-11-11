//! Worktrees: the working trees of a repository (`git worktree`), read
//! with every snapshot through libgit2 and its files (no git process), and
//! the git commands that add, lock, unlock, remove and prune them.
//!
//! A branch can be checked out in one worktree at a time: git refuses to
//! check it out anywhere else, which [`in_use`] recognises in its message.

use std::path::{Path, PathBuf};

use git2::{Repository, WorktreeLockStatus};

use super::Oid;
use super::ops::{GitCmd, Step};

/// One working tree of the repository, as `git worktree list` has it.
#[derive(Clone, Debug, PartialEq)]
pub struct WorktreeInfo {
    /// The name git keeps it under (`.git/worktrees/<name>`); empty for the
    /// main one.
    pub name: String,
    pub path: PathBuf,
    /// The branch checked out there (short name); `None` when detached.
    pub branch: Option<String>,
    pub head: Option<Oid>,
    /// The repository's own working tree, which cannot be removed.
    pub main: bool,
    /// The one this snapshot was read from.
    pub current: bool,
    /// Locked (`git worktree lock`), with the reason given, which may be empty.
    pub locked: Option<String>,
    /// Its folder is gone; `git worktree prune` forgets it.
    pub prunable: bool,
}

impl WorktreeInfo {
    /// The folder's name, what the sidebar shows.
    pub fn title(&self) -> String {
        self.path.file_name().map_or_else(|| self.path.display().to_string(), |n| n.to_string_lossy().into_owned())
    }

    /// The path as git prints it, with `/`.
    pub fn display(&self) -> String {
        self.path.to_string_lossy().replace('\\', "/")
    }
}

/// Every working tree of `repo`: the main one first, then the linked ones
/// by path. Reads a few small files per worktree.
pub fn read(repo: &Repository) -> Vec<WorktreeInfo> {
    let common = repo.commondir().to_path_buf();
    let linked: Vec<String> = match repo.worktrees() {
        Ok(names) => names.iter().flatten().flatten().map(str::to_owned).collect(),
        Err(_) => Vec::new(),
    };
    // The current one, when it is linked: its git directory is
    // `.git/worktrees/<name>`.
    let current = repo
        .is_worktree()
        .then(|| repo.path().components().next_back().map(|c| c.as_os_str().to_string_lossy().into_owned()))
        .flatten();

    let mut found = Vec::with_capacity(linked.len() + 1);
    if let Some(path) = main_path(repo, &common) {
        let (branch, head) = read_head(repo, &common.join("HEAD"));
        found.push(WorktreeInfo {
            name: String::new(),
            path,
            branch,
            head,
            main: true,
            current: current.is_none(),
            locked: None,
            prunable: false,
        });
    }
    let mut others: Vec<WorktreeInfo> = linked
        .iter()
        .filter_map(|name| {
            let worktree = repo.find_worktree(name).ok()?;
            let (branch, head) = read_head(repo, &common.join("worktrees").join(name).join("HEAD"));
            let locked = match worktree.is_locked() {
                Ok(WorktreeLockStatus::Locked(reason)) => Some(reason.unwrap_or_default().trim().to_owned()),
                _ => None,
            };
            Some(WorktreeInfo {
                name: name.clone(),
                path: clean(worktree.path()),
                branch,
                head,
                main: false,
                current: current.as_deref() == Some(name.as_str()),
                locked,
                prunable: worktree.validate().is_err() || !worktree.path().exists(),
            })
        })
        .collect();
    others.sort_by_cached_key(|w| w.display().to_lowercase());
    found.extend(others);
    found
}

/// The main working tree: this one, or the folder holding the common git
/// directory when this is a linked worktree.
fn main_path(repo: &Repository, common: &Path) -> Option<PathBuf> {
    if !repo.is_worktree() {
        return repo.workdir().map(clean);
    }
    if common.file_name().is_some_and(|name| name == ".git") {
        return common.parent().map(clean);
    }
    Repository::open(common).ok()?.workdir().map(clean)
}

/// A path without the trailing separator libgit2 leaves on folders.
fn clean(path: &Path) -> PathBuf {
    path.components().collect()
}

/// The branch and commit a `HEAD` file names.
fn read_head(repo: &Repository, file: &Path) -> (Option<String>, Option<Oid>) {
    let Ok(text) = std::fs::read_to_string(file) else { return (None, None) };
    let text = text.trim();
    match text.strip_prefix("ref:") {
        Some(name) => {
            let name = name.trim();
            let oid = repo.refname_to_id(name).ok();
            (Some(name.strip_prefix("refs/heads/").unwrap_or(name).to_owned()), oid)
        }
        None => (None, Oid::from_str(text).ok()),
    }
}

/// The worktree other than the current one where `branch` is checked out.
pub fn elsewhere<'a>(worktrees: &'a [WorktreeInfo], branch: &str) -> Option<&'a WorktreeInfo> {
    worktrees.iter().find(|w| !w.current && !w.prunable && w.branch.as_deref() == Some(branch))
}

/// Reads git's refusal to check out a branch that another worktree has:
/// the branch and that worktree's path. Git 2.42 and later say "'x' is
/// already used by worktree at '/path'", earlier ones "'x' is already
/// checked out at '/path'".
pub fn in_use(message: &str) -> Option<(String, PathBuf)> {
    for line in message.lines() {
        let line = line.trim().trim_start_matches("fatal:").trim();
        for middle in ["' is already used by worktree at '", "' is already checked out at '"] {
            if let Some((branch, path)) = line.split_once(middle) {
                let branch = branch.strip_prefix('\'').unwrap_or(branch);
                let path = path.strip_suffix('\'').unwrap_or(path);
                return Some((branch.to_owned(), PathBuf::from(path)));
            }
        }
    }
    None
}

/// Where a worktree for `branch` goes by default: next to the main working
/// tree, named after it and the branch (`app` and `feature/login` give
/// `app-feature-login`).
pub fn default_path(main: &Path, branch: &str) -> PathBuf {
    let name = main.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "worktree".to_owned());
    let branch: String = branch
        .trim()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' })
        .collect();
    let branch = branch.trim_matches('-');
    let folder = if branch.is_empty() { format!("{name}-worktree") } else { format!("{name}-{branch}") };
    main.parent().map_or_else(|| PathBuf::from(&folder), |parent| parent.join(&folder))
}

/// What is wrong with `path` as the folder of a new worktree, if anything
/// (an empty message: nothing typed yet). Whether the folder is free is
/// left to git, which says so.
pub fn check_path(path: &str) -> Result<(), String> {
    let path = path.trim();
    if path.is_empty() {
        return Err(String::new());
    }
    if !Path::new(path).is_absolute() {
        return Err("Give the full path of a folder.".to_owned());
    }
    Ok(())
}

// ---- commands ---------------------------------------------------------------

fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// `git worktree <args>` as one step.
fn step(args: &[&str]) -> Step {
    Step::new(args.iter().map(|a| (*a).to_owned()).collect(), None)
}

/// A new worktree at `path` on a new branch `branch` starting at `start`
/// (the current commit when empty).
pub fn add_new(path: &Path, branch: &str, start: &str) -> GitCmd {
    let path = text(path);
    let mut args = vec!["worktree", "add", "-q", "-b", branch, "--", &path];
    if !start.is_empty() {
        args.push(start);
    }
    GitCmd::new(format!("Add worktree {branch}"), &args).done(format!("Added a worktree for {branch}"))
}

/// A new worktree at `path` with the existing local branch `branch`.
pub fn add_existing(path: &Path, branch: &str) -> GitCmd {
    let path = text(path);
    GitCmd::new(format!("Add worktree {branch}"), &["worktree", "add", "-q", "--", &path, branch])
        .done(format!("Added a worktree for {branch}"))
}

/// Removes the worktree at `path` and its folder. Git refuses when it has
/// modified or untracked files unless `force`, which deletes them.
pub fn remove(path: &Path, force: bool) -> GitCmd {
    let path = text(path);
    let mut cmd = GitCmd::empty(format!("Remove worktree {}", file_name(&path)));
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.extend(["--", &path]);
    cmd.steps.push(step(&args));
    cmd.done(format!("Removed worktree {}", file_name(&path)))
}

pub fn lock(path: &Path) -> GitCmd {
    let path = text(path);
    GitCmd::new(format!("Lock worktree {}", file_name(&path)), &["worktree", "lock", "--", &path])
        .done(format!("Locked worktree {}", file_name(&path)))
}

pub fn unlock(path: &Path) -> GitCmd {
    let path = text(path);
    GitCmd::new(format!("Unlock worktree {}", file_name(&path)), &["worktree", "unlock", "--", &path])
        .done(format!("Unlocked worktree {}", file_name(&path)))
}

/// Forgets the worktrees whose folders are gone.
pub fn prune() -> GitCmd {
    GitCmd::new("Prune worktrees", &["worktree", "prune", "-v"]).done("Pruned worktrees")
}

fn file_name(path: &str) -> &str {
    path.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or(path)
}

/// Whether the worktree at `path` has modified or untracked files, which
/// `git worktree remove` refuses to delete. Assumed when it cannot be told.
pub fn has_changes(path: &Path) -> bool {
    super::branch_folder::has_changes(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{Fixture, git};

    fn same(a: &Path, b: &Path) -> bool {
        a.canonicalize().ok() == b.canonicalize().ok()
    }

    #[test]
    fn refusals_name_the_branch_and_the_worktree() {
        let new = "fatal: 'feature/a b' is already used by worktree at 'C:/work/app-a b'";
        assert_eq!(in_use(new), Some(("feature/a b".to_owned(), PathBuf::from("C:/work/app-a b"))));
        let old = "warning: something\nfatal: 'main' is already checked out at '/home/me/app'\n";
        assert_eq!(in_use(old), Some(("main".to_owned(), PathBuf::from("/home/me/app"))));
        assert_eq!(in_use("error: pathspec 'x' did not match any file(s) known to git"), None);
    }

    #[test]
    fn default_paths_sit_next_to_the_main_working_tree() {
        let main = Path::new("/work/app");
        assert_eq!(default_path(main, "feature/login"), Path::new("/work/app-feature-login"));
        assert_eq!(default_path(main, "fix: it!"), Path::new("/work/app-fix--it"));
        assert_eq!(default_path(main, ""), Path::new("/work/app-worktree"));
        assert_eq!(default_path(main, "été"), Path::new("/work/app-été"));
    }

    #[test]
    fn worktrees_are_listed_added_locked_and_removed() {
        let fx = Fixture::new("worktree-life");
        fx.write("a.txt", "a\n");
        fx.commit_all("one");
        let listed = fx.snapshot().worktrees.clone();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].main && listed[0].current && !listed[0].prunable);
        assert_eq!(listed[0].branch.as_deref(), Some("main"));
        assert!(same(&listed[0].path, &fx.dir));

        // A new branch in a folder with a space and an accent.
        let base = fx.dir.with_file_name(format!("worktree life-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let side = base.join("côté");
        fx.run(add_new(&side, "side", ""));
        // An existing branch, detached HEAD in a third.
        git(&fx.dir, &["branch", "topic"]);
        let topic = base.join("topic");
        fx.run(add_existing(&topic, "topic"));
        let detached = base.join("detached");
        git(&fx.dir, &["worktree", "add", "-q", "--detach", detached.to_str().unwrap()]);

        let snap = fx.snapshot();
        let names: Vec<_> = snap.worktrees.iter().map(|w| (w.title(), w.branch.clone(), w.main)).collect();
        assert_eq!(
            names,
            [
                (fx.dir.file_name().unwrap().to_string_lossy().into_owned(), Some("main".to_owned()), true),
                ("côté".to_owned(), Some("side".to_owned()), false),
                ("detached".to_owned(), None, false),
                ("topic".to_owned(), Some("topic".to_owned()), false),
            ]
        );
        assert!(same(&snap.worktrees[1].path, &side));
        assert_eq!(snap.worktrees[1].head, snap.head.oid);
        assert_eq!(snap.worktrees[2].head, snap.head.oid);
        assert!(snap.worktrees[1..].iter().all(|w| !w.current));
        assert_eq!(elsewhere(&snap.worktrees, "side").map(|w| w.title()), Some("côté".to_owned()));
        assert!(elsewhere(&snap.worktrees, "main").is_none(), "main is checked out here");

        // Git refuses to check out a branch that is in use elsewhere.
        let refused = fx.run_as(crate::git::ops::checkout("side"), crate::worker::Origin::User);
        let (branch, path) = in_use(&refused.result.unwrap_err()).expect("git names the worktree");
        assert_eq!(branch, "side");
        assert!(same(&path, &side));

        // Locking, with and without a reason.
        fx.run(lock(&side));
        git(&fx.dir, &["worktree", "lock", "--reason", "agent at work", topic.to_str().unwrap()]);
        let snap = fx.snapshot();
        assert_eq!(snap.worktrees[1].locked.as_deref(), Some(""));
        assert_eq!(snap.worktrees[3].locked.as_deref(), Some("agent at work"));
        let refused = fx.run_as(remove(&side, false), crate::worker::Origin::User);
        assert!(refused.result.unwrap_err().contains("locked"));
        fx.run(unlock(&side));
        assert_eq!(fx.snapshot().worktrees[1].locked, None);

        // Changes make git refuse, unless forced.
        std::fs::write(side.join("new file.txt"), "x").unwrap();
        assert!(has_changes(&side));
        assert!(!has_changes(&detached));
        let refused = fx.run_as(remove(&side, false), crate::worker::Origin::User);
        assert!(refused.result.unwrap_err().contains("modified or untracked"));
        fx.run(remove(&side, true));
        assert!(!side.exists());
        fx.run(remove(&detached, false));
        assert!(!detached.exists());

        // A folder deleted by hand is prunable until pruned.
        fx.run(unlock(&topic));
        std::fs::remove_dir_all(&topic).unwrap();
        let snap = fx.snapshot();
        assert_eq!(snap.worktrees.len(), 2);
        assert!(snap.worktrees[1].prunable);
        assert!(elsewhere(&snap.worktrees, "topic").is_none(), "a missing worktree does not hold its branch");
        fx.run(prune());
        assert_eq!(fx.snapshot().worktrees.len(), 1);
        // The branches stay.
        assert!(git(&fx.dir, &["branch", "--list"]).contains("side"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_linked_worktree_sees_the_main_one_and_itself() {
        let fx = Fixture::new("worktree-linked-main");
        fx.write("a.txt", "a\n");
        fx.commit_all("one");
        let dir = fx.dir.with_file_name(format!("worktree linked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        git(&fx.dir, &["worktree", "add", "-q", "-b", "side", dir.to_str().unwrap()]);

        let repo = Repository::open(&dir).unwrap();
        let listed = read(&repo);
        assert_eq!(listed.len(), 2);
        assert!(listed[0].main && !listed[0].current);
        assert!(same(&listed[0].path, &fx.dir));
        assert!(!listed[1].main && listed[1].current);
        assert_eq!(listed[1].branch.as_deref(), Some("side"));
        assert_eq!(elsewhere(&listed, "main").map(|w| w.main), Some(true));
        drop(repo);
        git(&fx.dir, &["worktree", "remove", "--force", dir.to_str().unwrap()]);
    }

    #[test]
    fn new_worktree_folders_are_checked() {
        let fx = Fixture::new("worktree-check-path");
        assert_eq!(check_path(" "), Err(String::new()));
        assert!(check_path("relative/folder").is_err());
        assert!(check_path(&fx.dir.join("new one").to_string_lossy()).is_ok());
        // A folder that is not empty: git says so.
        fx.write("a.txt", "a\n");
        fx.commit_all("one");
        let taken = fx.dir.join("taken");
        std::fs::create_dir_all(&taken).unwrap();
        std::fs::write(taken.join("x"), "x").unwrap();
        let refused = fx.run_as(add_new(&taken, "other", ""), crate::worker::Origin::User);
        assert!(refused.result.unwrap_err().contains("already exists"));
    }
}
