//! Deleting a folder of branches (everything under `t3/`, say) at once,
//! including the worktrees they are checked out in.
//!
//! Git will not delete a branch that a worktree has checked out, even one
//! whose folder is gone, so the plan looks at every worktree first: missing
//! ones are pruned, clean ones can be removed with their branch, and a
//! branch is kept when its worktree is the main one, is locked, or has
//! uncommitted changes. Nothing is ever forced.

use std::path::{Path, PathBuf};

use super::cli;
use super::ops::{GitCmd, RefMove, UndoKind};

/// One worktree, as `git worktree list --porcelain` describes it.
#[derive(Clone, Debug, PartialEq)]
pub struct Worktree {
    pub path: PathBuf,
    /// The branch checked out there (short name); `None` when detached.
    pub branch: Option<String>,
    /// The repository's own working tree, which cannot be removed.
    pub main: bool,
    pub locked: bool,
    /// Its folder is gone; `git worktree prune` forgets it.
    pub prunable: bool,
}

/// Reads `git worktree list --porcelain`: one block per worktree, the main
/// one first.
pub fn parse_worktrees(text: &str) -> Vec<Worktree> {
    let mut found: Vec<Worktree> = Vec::new();
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            found.push(Worktree {
                path: PathBuf::from(path),
                branch: None,
                main: found.is_empty(),
                locked: false,
                prunable: false,
            });
            continue;
        }
        let Some(current) = found.last_mut() else { continue };
        if let Some(name) = line.strip_prefix("branch refs/heads/") {
            current.branch = Some(name.to_owned());
        } else if line == "locked" || line.starts_with("locked ") {
            current.locked = true;
        } else if line == "prunable" || line.starts_with("prunable ") {
            current.prunable = true;
        }
    }
    found
}

/// What deleting a folder of local branches will do.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FolderPlan {
    /// Branches that can be deleted, each with the worktree it is checked
    /// out in, which goes first.
    pub branches: Vec<(String, Option<PathBuf>)>,
    /// Branches that stay, and why.
    pub kept: Vec<(String, &'static str)>,
    /// A worktree's folder is gone already: forget it first.
    pub prune: bool,
}

impl FolderPlan {
    /// The branches deleted: all of [`FolderPlan::branches`], or only those
    /// without a worktree when worktrees are to stay.
    pub fn deleted(&self, remove_worktrees: bool) -> impl Iterator<Item = &(String, Option<PathBuf>)> {
        self.branches.iter().filter(move |(_, worktree)| remove_worktrees || worktree.is_none())
    }

    /// How many of the deletable branches have a worktree.
    pub fn worktrees(&self) -> usize {
        self.branches.iter().filter(|(_, worktree)| worktree.is_some()).count()
    }
}

/// Plans deleting the local branches `names` from the repository at
/// `workdir`, where `head` is checked out. Runs git and reads every worktree
/// involved: call it on the reader thread.
pub fn plan(workdir: &Path, names: &[String], head: Option<&str>) -> Result<FolderPlan, String> {
    let args = ["worktree", "list", "--porcelain"].map(str::to_owned);
    let worktrees = parse_worktrees(&cli::run(workdir, &args, cli::Options::default())?);
    Ok(plan_with(names, head, &worktrees, has_changes))
}

fn plan_with(
    names: &[String],
    head: Option<&str>,
    worktrees: &[Worktree],
    has_changes: impl Fn(&Path) -> bool,
) -> FolderPlan {
    let mut plan = FolderPlan::default();
    for name in names {
        if head == Some(name.as_str()) {
            plan.kept.push((name.clone(), "the current branch"));
            continue;
        }
        match worktrees.iter().find(|w| w.branch.as_deref() == Some(name.as_str())) {
            None => plan.branches.push((name.clone(), None)),
            Some(w) if w.locked => plan.kept.push((name.clone(), "its worktree is locked")),
            Some(w) if w.main => plan.kept.push((name.clone(), "checked out in the main working tree")),
            Some(w) if w.prunable => {
                plan.prune = true;
                plan.branches.push((name.clone(), None));
            }
            Some(w) if has_changes(&w.path) => {
                plan.kept.push((name.clone(), "its worktree has uncommitted changes"));
            }
            Some(w) => plan.branches.push((name.clone(), Some(w.path.clone()))),
        }
    }
    plan
}

/// Whether `git worktree remove` would refuse the worktree at `path` for
/// modified or untracked files. When that cannot be told, it is assumed.
pub fn has_changes(path: &Path) -> bool {
    let Ok(repo) = git2::Repository::open(path) else { return true };
    let mut options = git2::StatusOptions::new();
    options.include_untracked(true).include_ignored(false).recurse_untracked_dirs(false).exclude_submodules(false);
    repo.statuses(Some(&mut options)).map(|statuses| !statuses.is_empty()).unwrap_or(true)
}

/// Branch names per `git` invocation, well under Windows' command line limit.
const CHUNK: usize = 50;

/// Deletes the local branches of `plan` in `folder` (and their worktrees,
/// when `remove_worktrees`). Worktrees go first, so a refusal stops the
/// command before any branch is deleted; undo puts the branches back where
/// they were, though not the worktrees.
pub fn delete_local(folder: &str, plan: &FolderPlan, remove_worktrees: bool) -> GitCmd {
    let deleted: Vec<&(String, Option<PathBuf>)> = plan.deleted(remove_worktrees).collect();
    let mut cmd = GitCmd::empty(format!("Delete folder {folder}"));
    if plan.prune {
        cmd = cmd.step(&["worktree", "prune"]);
    }
    for (_, worktree) in &deleted {
        if let Some(path) = worktree {
            cmd = cmd.step(&["worktree", "remove", &path.to_string_lossy()]);
        }
    }
    let names: Vec<&str> = deleted.iter().map(|(name, _)| name.as_str()).collect();
    for chunk in names.chunks(CHUNK) {
        let mut args = vec!["branch", "-D"];
        args.extend(chunk);
        cmd = cmd.step(&args);
    }
    let moves = names.iter().map(|name| RefMove::new(format!("refs/heads/{name}"))).collect();
    cmd.done(format!("Deleted {} in {folder}", count(names.len(), "branch", "branches")))
        .undoable(UndoKind::Refs(moves))
}

/// Deletes the branches `names` (without the remote's name) on `remote`.
pub fn delete_remote(remote: &str, folder: &str, names: &[String]) -> GitCmd {
    let mut cmd = GitCmd::empty(format!("Delete folder {remote}/{folder}"));
    for chunk in names.chunks(CHUNK) {
        let mut args = vec!["push", remote, "--delete"];
        args.extend(chunk.iter().map(String::as_str));
        cmd = cmd.step(&args);
    }
    cmd.done(format!("Deleted {} in {remote}/{folder}", count(names.len(), "branch", "branches")))
}

/// "1 branch", "3 branches".
pub fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{Fixture, git};
    use crate::worker::Origin;

    #[test]
    fn reads_the_porcelain_list() {
        let text = "worktree C:/repo\nHEAD 1111\nbranch refs/heads/main\n\n\
                    worktree C:/wt/a\nHEAD 2222\nbranch refs/heads/t3/a\nlocked agent busy\n\n\
                    worktree C:/wt/b\nHEAD 3333\ndetached\nprunable gitdir file points to non-existent location\n\n";
        let found = parse_worktrees(text);
        assert_eq!(found.len(), 3);
        assert!(found[0].main && found[0].branch.as_deref() == Some("main"));
        assert!(!found[1].main && found[1].locked && found[1].branch.as_deref() == Some("t3/a"));
        assert_eq!(found[1].path, PathBuf::from("C:/wt/a"));
        assert!(found[2].prunable && found[2].branch.is_none());
    }

    #[test]
    fn keeps_what_cannot_go_without_force() {
        let wt = |path: &str, branch: &str, main: bool, locked: bool, prunable: bool| Worktree {
            path: PathBuf::from(path),
            branch: Some(branch.to_owned()),
            main,
            locked,
            prunable,
        };
        let worktrees = [
            wt("/repo", "t3/main-here", true, false, false),
            wt("/wt/clean", "t3/clean", false, false, false),
            wt("/wt/dirty", "t3/dirty", false, false, false),
            wt("/wt/locked", "t3/locked", false, true, true),
            wt("/wt/gone", "t3/gone", false, false, true),
        ];
        let names: Vec<String> =
            ["t3/plain", "t3/current", "t3/main-here", "t3/clean", "t3/dirty", "t3/locked", "t3/gone"]
                .map(str::to_owned)
                .to_vec();
        let plan = plan_with(&names, Some("t3/current"), &worktrees, |path| path == Path::new("/wt/dirty"));
        assert_eq!(
            plan.branches,
            [
                ("t3/plain".to_owned(), None),
                ("t3/clean".to_owned(), Some(PathBuf::from("/wt/clean"))),
                ("t3/gone".to_owned(), None),
            ]
        );
        let kept: Vec<&str> = plan.kept.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(kept, ["t3/current", "t3/main-here", "t3/dirty", "t3/locked"]);
        assert!(plan.prune);
        assert_eq!(plan.worktrees(), 1);
        assert_eq!(plan.deleted(false).count(), 2);
    }

    #[test]
    fn deletes_a_folder_with_its_worktrees_and_undoes_it() {
        let fx = Fixture::new("branch-folder");
        fx.write("a.txt", "base\n");
        fx.commit_all("base");
        let place = |name: &str| fx.dir.with_file_name(format!("branch-folder {name}-{}", std::process::id()));
        let add = |name: &str| {
            let dir = place(name);
            let _ = std::fs::remove_dir_all(&dir);
            git(&fx.dir, &["worktree", "add", "-q", "-b", &format!("t3/{name}"), dir.to_str().unwrap()]);
            dir
        };
        git(&fx.dir, &["branch", "t3/plain"]);
        let clean = add("clean");
        let dirty = add("dirty");
        std::fs::write(dirty.join("new.txt"), "work in progress\n").unwrap();
        let gone = add("gone");
        std::fs::remove_dir_all(&gone).unwrap();
        git(&fx.dir, &["branch", "keep/other"]);

        let snap = fx.snapshot();
        let names: Vec<String> =
            snap.branches.iter().map(|b| b.name.clone()).filter(|name| name.starts_with("t3/")).collect();
        let plan = plan(&fx.dir, &names, snap.head.branch.as_deref()).unwrap();
        assert_eq!(plan.kept, [("t3/dirty".to_owned(), "its worktree has uncommitted changes")]);
        assert!(plan.prune);

        let done = fx.run(delete_local("t3", &plan, true));
        done.result.expect("the folder is deleted");
        let mut left: Vec<&str> = done.snap.branches.iter().map(|b| b.name.as_str()).collect();
        left.sort();
        assert_eq!(left, ["keep/other", "main", "t3/dirty"]);
        assert!(!clean.exists(), "the clean worktree is removed");
        assert!(dirty.join("new.txt").exists(), "the one with changes is left alone");

        let entry = done.undo.expect("deleting a folder is undoable");
        let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry));
        undone.result.expect("undo works");
        let mut back: Vec<&str> = undone.snap.branches.iter().map(|b| b.name.as_str()).collect();
        back.sort();
        assert_eq!(back, ["keep/other", "main", "t3/clean", "t3/dirty", "t3/gone", "t3/plain"]);

        git(&fx.dir, &["worktree", "remove", "--force", dirty.to_str().unwrap()]);
    }
}
