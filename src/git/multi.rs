//! Several commits at once.
//!
//! * [`cherry_pick`] and [`revert`] hand every commit to one `git
//!   cherry-pick` or `git revert`, so a conflict stops at the commit that
//!   causes it and Continue (`--continue`) carries on with the rest, exactly
//!   as for a single commit.
//! * [`check_squash`] makes sure a run of commits can be squashed together by
//!   the interactive rebase (`git::rebase`), and [`squash_plan`] sets a plan
//!   up to do it.
//! * [`facts`] tells, from the loaded history alone, what a set of selected
//!   commits is: their graph order, whether they are one line of history,
//!   how many the checked-out branch has. It is cheap enough for menus.

use std::collections::HashSet;

use git2::{BranchType, Repository};

use super::ops::{GitCmd, Step, UndoKind};
use super::rebase::{Action, MAX_COMMITS, Plan};
use super::repo::{lossy, read_head};
use super::{Head, History, Oid, RowKind, short};

fn commits(n: usize) -> String {
    if n == 1 { "1 commit".to_owned() } else { format!("{n} commits") }
}

fn err(e: git2::Error) -> String {
    e.message().to_owned()
}

/// One `git <verb> --stdin` over `oids`, in that order. The commits travel on
/// standard input, since no command line is long enough for a few thousand
/// SHAs, and git applies commits it is given one by one in the order given,
/// without walking their history.
fn sequence(label: String, verb: &[&str], oids: &[Oid], any_merge: bool) -> GitCmd {
    let mut args: Vec<String> = verb.iter().map(|arg| (*arg).to_owned()).collect();
    // Git accepts `-m 1` for ordinary commits too (since 2.21), so one merge
    // among them does not need a command of its own.
    if any_merge {
        args.extend(["-m".to_owned(), "1".to_owned()]);
    }
    args.push("--stdin".to_owned());
    let stdin: String = oids.iter().map(|oid| format!("{oid}\n")).collect();
    let mut cmd = GitCmd::empty(label).undoable(UndoKind::HeadMove);
    cmd.steps.push(Step::new(args, Some(stdin.into_bytes())));
    cmd
}

/// Cherry-picks `oldest_first` onto HEAD in one `git cherry-pick`.
/// `any_merge`: one of them is a merge commit, picked against its first
/// parent. Undone like a single cherry-pick, by moving the branch back.
pub fn cherry_pick(oldest_first: &[Oid], any_merge: bool) -> GitCmd {
    let n = oldest_first.len();
    sequence(format!("Cherry-pick {}", commits(n)), &["cherry-pick"], oldest_first, any_merge)
        .done(format!("Cherry-picked {}", commits(n)))
        .after_conflict("Continue then picks the commits after it.")
}

/// Reverts `newest_first` in one `git revert`, one new commit each: newest
/// first, so every revert applies to the code as the later commits left it.
pub fn revert(newest_first: &[Oid], any_merge: bool) -> GitCmd {
    let n = newest_first.len();
    sequence(format!("Revert {}", commits(n)), &["revert", "--no-edit"], newest_first, any_merge)
        .done(format!("Reverted {}", commits(n)))
        .after_conflict("Continue then reverts the older ones.")
}

// ---- squashing ----------------------------------------------------------------

/// Commits that can be squashed together, as checked by [`check_squash`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Squash {
    /// The commit below the oldest of them: where the interactive rebase starts.
    pub base: Oid,
    /// The commits to squash, oldest first.
    pub run: Vec<Oid>,
    /// The checked-out branch.
    pub branch: String,
    /// The branch's upstream and how many of the commits it already has,
    /// when it has any.
    pub pushed: Option<(String, usize)>,
}

impl Squash {
    /// What to say before rewriting commits that were already pushed.
    pub fn warning(&self) -> Option<String> {
        let (upstream, count) = self.pushed.as_ref()?;
        let which = if *count == self.run.len() {
            "These commits are".to_owned()
        } else {
            format!("{count} of these commits {}", if *count == 1 { "is" } else { "are" })
        };
        Some(format!(
            "{which} already on {upstream}. Squashing rewrites them: {} will need a force push \
             afterwards, and anyone who has them will have to move onto the new commit.",
            self.branch
        ))
    }
}

fn describe(repo: &Repository, oid: Oid) -> String {
    let summary = repo.find_commit(oid).ok().and_then(|c| c.summary_bytes().map(lossy)).unwrap_or_default();
    format!("{} \"{summary}\"", short(oid))
}

/// Checks against the repository that `oids` can be squashed into one
/// commit: they are an unbroken line of commits without merges on the
/// checked-out branch's own line of history (its first parents), which is
/// what the interactive rebase can rewrite. Merges between them and the tip
/// are made again by the rebase.
///
/// Touches the disk: call it from a reader-thread job.
pub fn check_squash(repo: &Repository, oids: &[Oid]) -> Result<Squash, String> {
    let wanted: HashSet<Oid> = oids.iter().copied().collect();
    if wanted.len() < 2 {
        return Err("Select at least two commits to squash them.".to_owned());
    }
    let head = read_head(repo);
    let branch = match &head.branch {
        Some(branch) if !head.detached => branch.clone(),
        _ => return Err(DETACHED.to_owned()),
    };
    let tip = head.oid.ok_or("This branch has no commits yet.")?;
    let reachable = || {
        wanted.iter().all(|oid| *oid == tip || repo.graph_descendant_of(tip, *oid).unwrap_or(false))
    };
    let elsewhere = || format!("Not all of these commits are on {branch}. Check out the branch they are on to squash them.");

    // Down the first-parent line from the tip, until every one is found.
    let mut run = Vec::new();
    let mut at = tip;
    let mut steps = 0;
    let mut past_merge = false;
    let merged_in = || {
        format!("These commits came into {branch} through a merge, so they are not on its own line of history. Squash them on the branch they were made on.")
    };
    loop {
        let commit = repo.find_commit(at).map_err(err)?;
        let selected = wanted.contains(&at);
        if !selected && !run.is_empty() {
            return Err(format!(
                "These commits are not one unbroken line of history: {} sits between them. Select \
                 it too, or squash fewer commits.",
                describe(repo, at)
            ));
        }
        if commit.parent_count() > 1 {
            if selected {
                return Err(format!("{} is a merge commit. {MERGE}", describe(repo, at)));
            }
            if !past_merge && !reachable() {
                return Err(elsewhere());
            }
            past_merge = true;
        }
        if selected {
            run.push(at);
            if run.len() == wanted.len() {
                break;
            }
        }
        steps += 1;
        if steps > MAX_COMMITS {
            if past_merge && reachable() {
                return Err(merged_in());
            }
            return Err(format!(
                "There are more than {MAX_COMMITS} commits between these and the tip of {branch}, \
                 more than an interactive rebase takes."
            ));
        }
        at = match commit.parent_id(0) {
            Ok(parent) => parent,
            Err(_) if past_merge && reachable() => return Err(merged_in()),
            Err(_) => return Err(elsewhere()),
        };
    }

    let oldest = *run.last().expect("the run is complete");
    let base = repo
        .find_commit(oldest)
        .map_err(err)?
        .parent_id(0)
        .map_err(|_| format!("{} is the first commit of the repository. {ROOT}", describe(repo, oldest)))?;
    run.reverse();

    // The upstream has an unbroken prefix of the run (the oldest ones), if
    // anything: count it by halving.
    let pushed = upstream(repo, &branch).and_then(|(name, up)| {
        let has = |oid: Oid| oid == up || repo.graph_descendant_of(up, oid).unwrap_or(false);
        let count = run.partition_point(|oid| has(*oid));
        (count > 0).then_some((name, count))
    });
    Ok(Squash { base, run, branch, pushed })
}

fn upstream(repo: &Repository, branch: &str) -> Option<(String, Oid)> {
    let upstream = repo.find_branch(branch, BranchType::Local).ok()?.upstream().ok()?;
    let name = upstream.name_bytes().ok().map(lossy)?;
    Some((name, upstream.get().target()?))
}

const DETACHED: &str = "HEAD is detached. Squashing rewrites a branch: check out the branch these \
                        commits are on first.";
const MERGE: &str = "Squashing replays the commits one by one in a straight line, which a merge \
                     does not fit into.";
const ROOT: &str = "The interactive rebase starts from the commit below the oldest one squashed, \
                    and this one has none.";

/// Sets `plan` up to squash `run` (oldest first, as [`check_squash`] gives
/// it) into the oldest of them; every other commit keeps its action. The
/// combined message is the one git proposes until the user edits it.
/// Returns the plan position of the commit they fold into, or `None` (and
/// the plan untouched) when one of them is not in the plan.
pub fn squash_plan(plan: &mut Plan, run: &[Oid]) -> Option<usize> {
    let positions: Vec<usize> = run
        .iter()
        .map(|oid| plan.items.iter().position(|item| plan.range.commits[item.source].oid == *oid))
        .collect::<Option<_>>()?;
    for (i, pos) in positions.iter().enumerate() {
        plan.items[*pos].action = if i == 0 { Action::Pick } else { Action::Squash };
    }
    positions.first().copied()
}

// ---- what the loaded history says ---------------------------------------------

/// What the loaded history says about a set of selected commits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Facts {
    /// Those in the loaded history, newest first, as the graph lists them.
    pub commits: Vec<Oid>,
    /// Each one's first parent is the next in `commits`: one unbroken line
    /// of history.
    pub line: bool,
    /// The first parent of the oldest one, where their combined change
    /// starts. `None` for the repository's first commit.
    pub below: Option<Oid>,
    /// Merge commits among them.
    pub merges: usize,
    /// How many of them the checked-out commit has (itself included).
    pub on_head: usize,
    /// One of them is the checked-out commit.
    pub has_head: bool,
    /// Where an interactive rebase that squashes them starts, or why they
    /// cannot be squashed. Confirmed against the repository by
    /// [`check_squash`] before anything runs.
    pub squash: Result<Oid, String>,
}

impl Facts {
    /// The two sides of their combined change: from what came before the
    /// oldest to the newest. Two commits are compared with each other
    /// instead; see the panel.
    pub fn combined(&self) -> Option<(Oid, Oid)> {
        Some((self.below.filter(|_| self.line)?, *self.commits.first()?))
    }
}

/// Works out [`Facts`] for `selected` from the loaded `history`. Commits
/// that are not loaded rows, the WIP row and stashes are left out.
pub fn facts(history: &History, head: &Head, selected: &HashSet<Oid>) -> Facts {
    let row = |oid: &Oid| history.index_of.get(oid).map(|i| *i as usize);
    let mut rows: Vec<usize> = selected
        .iter()
        .filter_map(row)
        .filter(|r| history.commits[*r].kind == RowKind::Commit)
        .collect();
    rows.sort_unstable();
    let commits: Vec<Oid> = rows.iter().map(|r| history.commits[*r].oid).collect();
    let parents = |r: usize| &history.commits[r].parents;

    let line = rows.windows(2).all(|pair| parents(pair[0]).first() == Some(&history.commits[pair[1]].oid));
    let below = rows.last().and_then(|r| parents(*r).first().copied());
    let merges = rows.iter().filter(|r| parents(**r).len() > 1).count();
    let head_row = head.oid.as_ref().and_then(row);
    let has_head = head.oid.is_some_and(|oid| commits.contains(&oid));

    // Everything the checked-out commit reaches, down to the oldest selected
    // row: rows are ordered children first, so nothing below can lead back up.
    let mut on_head = 0;
    if let (Some(start), Some(&last)) = (head_row, rows.last())
        && start <= last
    {
        let mut seen = vec![false; last - start + 1];
        let mut stack = vec![start];
        seen[0] = true;
        while let Some(r) = stack.pop() {
            for parent in parents(r) {
                if let Some(p) = row(parent).filter(|p| *p <= last && !seen[*p - start]) {
                    seen[p - start] = true;
                    stack.push(p);
                }
            }
        }
        // Rows above the checked-out commit are newer: it cannot have them.
        on_head = rows.iter().filter(|r| **r >= start && seen[**r - start]).count();
    }

    let squash = squash_from(history, head, &rows, line, on_head, below);
    Facts { commits, line, below, merges, on_head, has_head, squash }
}

fn squash_from(
    history: &History,
    head: &Head,
    rows: &[usize],
    line: bool,
    on_head: usize,
    below: Option<Oid>,
) -> Result<Oid, String> {
    let info = |r: usize| {
        let commit = &history.commits[r];
        format!("{} \"{}\"", short(commit.oid), commit.summary)
    };
    if rows.len() < 2 {
        return Err("Select at least two commits to squash them.".to_owned());
    }
    let branch = match &head.branch {
        Some(branch) if !head.detached => branch,
        _ => return Err(DETACHED.to_owned()),
    };
    if on_head < rows.len() {
        return Err(format!("Not all of these commits are on {branch}. Check out the branch they are on to squash them."));
    }
    if let Some(merge) = rows.iter().find(|r| history.commits[**r].parents.len() > 1) {
        return Err(format!("{} is a merge commit. {MERGE}", info(*merge)));
    }
    if !line {
        return Err("These commits are not one unbroken line of history. Select the commits in \
                    between too, or squash fewer."
            .to_owned());
    }
    // From the tip down the first-parent line to the newest of them. Merges
    // on the way are made again by the interactive rebase.
    let newest = rows[0];
    let mut at = head.oid.and_then(|oid| history.index_of.get(&oid)).map(|i| *i as usize);
    let mut past_merge = false;
    while let Some(r) = at.filter(|r| *r < newest) {
        let parents = &history.commits[r].parents;
        past_merge |= parents.len() > 1;
        at = parents.first().and_then(|p| history.index_of.get(p)).map(|i| *i as usize);
    }
    if at != Some(newest) && past_merge {
        return Err(format!("These commits came into {branch} through a merge, so they are not on its own line of history. Squash them on the branch they were made on."));
    }
    if at != Some(newest) {
        return Err(format!("Not all of these commits are on {branch}. Check out the branch they are on to squash them."));
    }
    if newest + rows.len() > MAX_COMMITS {
        return Err(format!("There are more than {MAX_COMMITS} commits from these to the tip of {branch}, more than an interactive rebase takes."));
    }
    below.ok_or_else(|| format!("{} is the first commit of the repository. {ROOT}", info(rows[rows.len() - 1])))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::git::rebase::{self, Range};
    use crate::git::{CommitInfo, RepoState, graph::Graph};
    use crate::tests::{Fixture, git};
    use crate::worker::Origin;

    fn subjects(fx: &Fixture) -> Vec<String> {
        git(&fx.dir, &["log", "--format=%s"]).lines().map(str::to_owned).collect()
    }

    fn sha(fx: &Fixture, rev: &str) -> Oid {
        Oid::from_str(&git(&fx.dir, &["rev-parse", rev])).unwrap()
    }

    /// main: base, then "main work"; feature (from base): one, two, three,
    /// each adding its own file.
    fn two_branches(name: &str) -> Fixture {
        let fx = Fixture::new(name);
        fx.write("shared.txt", "line\n");
        fx.commit_all("base");
        git(&fx.dir, &["checkout", "-q", "-b", "feature"]);
        for (file, message) in [("one file.txt", "one"), ("zwei-ü.txt", "two"), ("three.txt", "three")] {
            fx.write(file, &format!("{message}\n"));
            fx.commit_all(message);
        }
        git(&fx.dir, &["checkout", "-q", "main"]);
        fx.write("main.txt", "main\n");
        fx.commit_all("main work");
        fx
    }

    // ---- cherry-pick and revert ------------------------------------------------

    #[test]
    fn several_commits_are_cherry_picked_oldest_first_and_undone_at_once() {
        let fx = two_branches("multi-pick");
        let picks = [sha(&fx, "feature~2"), sha(&fx, "feature~1"), sha(&fx, "feature")];
        let done = fx.run(cherry_pick(&picks, false));
        assert_eq!(subjects(&fx), ["three", "two", "one", "main work", "base"]);
        assert_eq!(fx.read("zwei-ü.txt"), "two\n");
        assert_eq!(done.snap.state, RepoState::Clean);

        let entry = done.undo.expect("undoable");
        assert_eq!(entry.label, "Cherry-pick 3 commits");
        let undone = fx.run_as(entry.undo.clone(), Origin::Undo(entry));
        undone.result.expect("undo works");
        assert_eq!(subjects(&fx), ["main work", "base"]);
        assert!(!fx.dir.join("one file.txt").exists());
    }

    #[test]
    fn a_merge_among_the_picks_is_taken_against_its_first_parent() {
        let fx = two_branches("multi-pick-merge");
        git(&fx.dir, &["checkout", "-q", "-b", "side", "main~1"]);
        git(&fx.dir, &["merge", "-q", "--no-ff", "-m", "merge feature", "feature"]);
        fx.write("after.txt", "after\n");
        fx.commit_all("after merge");
        git(&fx.dir, &["checkout", "-q", "main"]);
        let picks = [sha(&fx, "side~1"), sha(&fx, "side")];
        fx.run(cherry_pick(&picks, true));
        assert_eq!(subjects(&fx), ["after merge", "merge feature", "main work", "base"]);
        assert_eq!(fx.read("three.txt"), "three\n");
        assert_eq!(fx.read("after.txt"), "after\n");
    }

    /// feature: one (own file), clash (edits shared.txt), three; main edits
    /// shared.txt too.
    fn clashing(name: &str) -> (Fixture, [Oid; 3]) {
        let fx = Fixture::new(name);
        fx.write("shared.txt", "line\n");
        fx.commit_all("base");
        git(&fx.dir, &["checkout", "-q", "-b", "feature"]);
        fx.write("one.txt", "one\n");
        fx.commit_all("one");
        fx.write("shared.txt", "feature\n");
        fx.commit_all("clash");
        fx.write("three.txt", "three\n");
        fx.commit_all("three");
        git(&fx.dir, &["checkout", "-q", "main"]);
        fx.write("shared.txt", "main\n");
        fx.commit_all("main work");
        let picks = [sha(&fx, "feature~2"), sha(&fx, "feature~1"), sha(&fx, "feature")];
        (fx, picks)
    }

    #[test]
    fn a_conflict_stops_the_picks_where_it_happens_and_continue_finishes_them() {
        let (fx, picks) = clashing("multi-pick-continue");
        let stopped = fx.run_as(cherry_pick(&picks, false), Origin::User);
        assert!(stopped.result.is_err(), "the second pick conflicts");
        assert!(stopped.undo.is_none());
        assert_eq!(stopped.snap.state, RepoState::CherryPick);
        // The first one went in; the third is still to come.
        assert_eq!(subjects(&fx), ["one", "main work", "base"]);
        assert!(!fx.dir.join("three.txt").exists());

        fx.write("shared.txt", "both\n");
        git(&fx.dir, &["add", "shared.txt"]);
        let done = fx.run(crate::git::ops::continue_op(RepoState::CherryPick).unwrap());
        assert_eq!(done.snap.state, RepoState::Clean);
        assert_eq!(subjects(&fx), ["three", "clash", "one", "main work", "base"]);
        assert_eq!(fx.read("shared.txt"), "both\n");
    }

    #[test]
    fn abort_after_a_conflict_puts_the_branch_back_before_the_first_pick() {
        let (fx, picks) = clashing("multi-pick-abort");
        let before = sha(&fx, "HEAD");
        assert!(fx.run_as(cherry_pick(&picks, false), Origin::User).result.is_err());
        let done = fx.run(crate::git::ops::abort(RepoState::CherryPick).unwrap());
        assert_eq!(done.snap.state, RepoState::Clean);
        assert_eq!(sha(&fx, "HEAD"), before);
        assert_eq!(fx.read("shared.txt"), "main\n");
        assert!(!fx.dir.join("one.txt").exists());
    }

    #[test]
    fn several_commits_are_reverted_newest_first() {
        let fx = Fixture::new("multi-revert");
        fx.write("a.txt", "1\n");
        fx.commit_all("base");
        fx.write("a.txt", "1\n2\n");
        fx.commit_all("add two");
        fx.write("a.txt", "1\n2\n3\n");
        fx.commit_all("add three");
        // Oldest first would not apply: "add two" is undone before "add three".
        let reverts = [sha(&fx, "HEAD"), sha(&fx, "HEAD~1")];
        let done = fx.run(revert(&reverts, false));
        assert_eq!(fx.read("a.txt"), "1\n");
        let log = subjects(&fx);
        assert_eq!(log[..2], ["Revert \"add two\"", "Revert \"add three\""]);
        assert_eq!(done.undo.expect("undoable").label, "Revert 2 commits");
    }

    #[test]
    fn a_revert_that_conflicts_stops_and_abort_restores_the_branch() {
        let fx = Fixture::new("multi-revert-conflict");
        fx.write("a.txt", "1\n");
        fx.commit_all("base");
        fx.write("b.txt", "b\n");
        fx.commit_all("add b");
        fx.write("a.txt", "2\n");
        fx.commit_all("two");
        fx.write("a.txt", "3\n");
        fx.commit_all("three");
        let before = sha(&fx, "HEAD");
        // "add b" reverts cleanly; "two" no longer matches what "three" left.
        let stopped = fx.run_as(revert(&[sha(&fx, "HEAD~2"), sha(&fx, "HEAD~1")], false), Origin::User);
        assert!(stopped.result.is_err());
        assert_eq!(stopped.snap.state, RepoState::Revert);
        assert!(!fx.dir.join("b.txt").exists(), "the first revert went in");
        fx.run(crate::git::ops::abort(RepoState::Revert).unwrap());
        assert_eq!(sha(&fx, "HEAD"), before);
        assert_eq!(fx.read("b.txt"), "b\n");
    }

    // ---- squashing ----------------------------------------------------------------

    /// main: base, then a, b, c and d, each adding its own file.
    fn run_of_four(name: &str) -> Fixture {
        let fx = Fixture::new(name);
        fx.write("base.txt", "base\n");
        fx.commit_all("base");
        for name in ["a", "b", "c", "d"] {
            fx.write(&format!("{name} file.txt"), &format!("{name}\n"));
            fx.commit_all(&format!("{name}\n\nbody of {name}"));
        }
        fx
    }

    /// Pushes `upto` as the remote's main and makes it main's upstream.
    /// Returns the bare remote, for the test to remove.
    fn push_upstream(fx: &Fixture, upto: &str) -> std::path::PathBuf {
        let bare = fx.dir.with_extension("remote.git");
        let _ = std::fs::remove_dir_all(&bare);
        git(&fx.dir, &["init", "-q", "--bare", bare.to_str().unwrap()]);
        git(&fx.dir, &["remote", "add", "origin", bare.to_str().unwrap()]);
        git(&fx.dir, &["push", "-q", "origin", &format!("{upto}:refs/heads/main")]);
        git(&fx.dir, &["fetch", "-q", "origin"]);
        git(&fx.dir, &["branch", "-q", "--set-upstream-to=origin/main", "main"]);
        bare
    }

    fn check(fx: &Fixture, revs: &[&str]) -> Result<Squash, String> {
        let oids: Vec<Oid> = revs.iter().map(|rev| sha(fx, rev)).collect();
        fx.job(move |loader| check_squash(loader.repo(), &oids))
    }

    #[test]
    fn a_run_squashes_into_one_commit_with_the_combined_message() {
        let fx = run_of_four("multi-squash");
        let squash = check(&fx, &["HEAD~1", "HEAD~3", "HEAD~2"]).expect("a, b and c squash");
        assert_eq!(squash.run, [sha(&fx, "HEAD~3"), sha(&fx, "HEAD~2"), sha(&fx, "HEAD~1")]);
        assert_eq!(squash.base, sha(&fx, "HEAD~4"));
        assert_eq!(squash.pushed, None);
        assert_eq!(squash.warning(), None);

        let base = squash.base;
        let range: Range = fx.job(move |loader| rebase::load_range(loader.repo(), base)).unwrap();
        let mut plan = Plan::new(Arc::new(range));
        assert_eq!(squash_plan(&mut plan, &squash.run), Some(0));
        assert_eq!(squash_plan(&mut plan.clone(), &[sha(&fx, "HEAD~4")]), None, "the base is not in the plan");
        let group = plan.groups().into_iter().find(|g| !g.members.is_empty()).unwrap();
        assert_eq!(plan.default_combined(&group), "a\n\nbody of a\n\nb\n\nbody of b\n\nc\n\nbody of c\n");
        plan.items[group.leader].combined = Some("a, b and c\n\ntogether".to_owned());
        let cmd = fx.job(move |loader| plan.prepare(loader.repo())).unwrap();
        fx.run(cmd);

        assert_eq!(subjects(&fx), ["d", "a, b and c", "base"]);
        assert_eq!(git(&fx.dir, &["log", "-1", "--format=%B", "HEAD~1"]), "a, b and c\n\ntogether");
        for name in ["a", "b", "c", "d"] {
            assert_eq!(fx.read(&format!("{name} file.txt")), format!("{name}\n"));
        }
    }

    #[test]
    fn squashing_warns_about_commits_the_upstream_has() {
        let fx = run_of_four("multi-squash-pushed");
        let bare = push_upstream(&fx, "HEAD~2");
        let squash = check(&fx, &["HEAD~3", "HEAD~2", "HEAD~1"]).unwrap();
        assert_eq!(squash.pushed, Some(("origin/main".to_owned(), 2)));
        let warning = squash.warning().unwrap();
        assert!(warning.starts_with("2 of these commits are already on origin/main"), "{warning}");
        assert!(warning.contains("main will need a force push"), "{warning}");
        let all = check(&fx, &["HEAD~3", "HEAD~2"]).unwrap();
        assert!(all.warning().unwrap().starts_with("These commits are already on origin/main"));
        // Nothing of the newest two is on the remote yet.
        assert_eq!(check(&fx, &["HEAD", "HEAD~1"]).unwrap().pushed, None);
        let _ = std::fs::remove_dir_all(bare);
    }

    #[test]
    fn squashing_refuses_gaps_merges_other_branches_and_the_first_commit() {
        let fx = run_of_four("multi-squash-refusals");
        let gap = check(&fx, &["HEAD", "HEAD~2"]).unwrap_err();
        assert!(gap.contains("not one unbroken line") && gap.contains("\"c\""), "{gap}");
        let root = check(&fx, &["HEAD~4", "HEAD~3"]).unwrap_err();
        assert!(root.contains("first commit of the repository"), "{root}");
        assert!(check(&fx, &["HEAD"]).unwrap_err().contains("at least two"));

        // A merge in the run, then one between the run and the tip.
        git(&fx.dir, &["checkout", "-q", "-b", "topic", "HEAD~1"]);
        fx.write("topic.txt", "topic\n");
        fx.commit_all("topic");
        git(&fx.dir, &["checkout", "-q", "main"]);
        git(&fx.dir, &["merge", "-q", "--no-ff", "-m", "merge topic", "topic"]);
        let merge = check(&fx, &["HEAD~1", "HEAD"]).unwrap_err();
        assert!(merge.contains("\"merge topic\" is a merge commit"), "{merge}");
        fx.write("e.txt", "e\n");
        fx.commit_all("e");
        // Below a merge on main's own line: the rebase makes the merge again.
        let below = check(&fx, &["HEAD~3", "HEAD~2"]).expect("a merge above is no obstacle");
        assert_eq!(below.run, [sha(&fx, "HEAD~3"), sha(&fx, "HEAD~2")]);
        // ... but not on the side the merge brought in.
        git(&fx.dir, &["checkout", "-q", "topic"]);
        fx.write("topic2.txt", "topic two\n");
        fx.commit_all("topic two");
        git(&fx.dir, &["checkout", "-q", "main"]);
        git(&fx.dir, &["merge", "-q", "--no-ff", "-m", "merge topic again", "topic"]);
        let side = check(&fx, &["topic", "topic~1"]).unwrap_err();
        assert!(side.contains("came into main through a merge"), "{side}");
        git(&fx.dir, &["reset", "-q", "--hard", "HEAD~1"]);

        // Commits of a branch that was never merged.
        git(&fx.dir, &["checkout", "-q", "-b", "other", "HEAD~4"]);
        fx.write("x.txt", "x
");
        fx.commit_all("x");
        fx.write("y.txt", "y
");
        fx.commit_all("y");
        git(&fx.dir, &["checkout", "-q", "main"]);
        let other = check(&fx, &["other", "other~1"]).unwrap_err();
        assert!(other.contains("Not all of these commits are on main"), "{other}");
        git(&fx.dir, &["checkout", "-q", "--detach", "HEAD"]);
        assert!(check(&fx, &["HEAD~3", "HEAD~2"]).unwrap_err().starts_with("HEAD is detached"));
    }

    // ---- comparing --------------------------------------------------------------

    #[test]
    fn two_branches_compare_by_their_tips() {
        let fx = two_branches("multi-compare");
        let (feature, main) = (sha(&fx, "feature"), sha(&fx, "main"));
        let files = fx.job(move |loader| loader.compare_files(main, feature)).unwrap();
        let names: Vec<(&str, crate::git::ChangeKind)> = files.iter().map(|f| (f.path.as_str(), f.kind)).collect();
        use crate::git::ChangeKind::{Added, Deleted};
        assert_eq!(names, [("main.txt", Deleted), ("one file.txt", Added), ("three.txt", Added), ("zwei-ü.txt", Added)]);
        let diff = fx.diff(crate::git::DiffSource::Compare { from: main, to: feature }, "zwei-ü.txt");
        assert_eq!((diff.additions, diff.deletions), (1, 0));
        // The same commit on both sides: nothing differs.
        assert!(fx.job(move |loader| loader.compare_files(main, main)).unwrap().is_empty());
    }

    // ---- facts from the loaded history ----------------------------------------

    fn oid(n: u8) -> Oid {
        Oid::from_bytes(&[n; 20]).unwrap()
    }

    /// Rows children first; `parents` by number.
    fn history(rows: &[(u8, &[u8], RowKind)]) -> History {
        let commits: Vec<CommitInfo> = rows
            .iter()
            .map(|(n, parents, kind)| CommitInfo {
                oid: oid(*n),
                parents: parents.iter().map(|p| oid(*p)).collect(),
                summary: format!("commit {n}"),
                body_preview: String::new(),
                author: "A".to_owned(),
                email: "a@example.com".to_owned(),
                time: 0,
                kind: *kind,
            })
            .collect();
        let graph = Graph::build(&commits);
        let index_of = commits.iter().enumerate().map(|(i, c)| (c.oid, i as u32)).collect();
        History { commits, graph, index_of, has_more: false }
    }

    use RowKind::{Commit as C, Stash as S, Wip as W};

    /// 9 (WIP) on 8; 8 on 7; stash 20 on 7; 7 merges 6 and 5; 6 on 4; 5 on
    /// 4; 4 on 3; 3 on 2; 2 is the root. Side branch 10 on 3.
    fn sample() -> History {
        history(&[
            (9, &[8], W),
            (8, &[7], C),
            (20, &[7], S),
            (7, &[6, 5], C),
            (10, &[3], C),
            (6, &[4], C),
            (5, &[4], C),
            (4, &[3], C),
            (3, &[2], C),
            (2, &[], C),
        ])
    }

    fn head(n: u8) -> Head {
        Head { branch: Some("main".to_owned()), oid: Some(oid(n)), detached: false }
    }

    fn set(ns: &[u8]) -> HashSet<Oid> {
        ns.iter().map(|n| oid(*n)).collect()
    }

    #[test]
    fn facts_put_commits_in_graph_order_and_find_lines() {
        let h = sample();
        let facts = facts(&h, &head(8), &set(&[3, 6, 4, 9, 20]));
        assert_eq!(facts.commits, [oid(6), oid(4), oid(3)], "WIP and stash rows are left out");
        assert!(facts.line);
        assert_eq!(facts.below, Some(oid(2)));
        assert_eq!(facts.combined(), Some((oid(2), oid(6))));
        assert_eq!((facts.on_head, facts.has_head, facts.merges), (3, false, 0));

        let broken = super::facts(&h, &head(8), &set(&[6, 3]));
        assert!(!broken.line);
        assert_eq!(broken.combined(), None);
        // 5 is on the second-parent side: a line through 5, not 6.
        assert!(super::facts(&h, &head(8), &set(&[5, 4])).line);
        // The root commit has nothing below it.
        assert_eq!(super::facts(&h, &head(8), &set(&[3, 2])).combined(), None);
    }

    #[test]
    fn facts_count_what_the_checked_out_commit_has() {
        let h = sample();
        let side = facts(&h, &head(8), &set(&[10, 4, 8]));
        assert_eq!((side.on_head, side.has_head), (2, true));
        assert_eq!(facts(&h, &head(10), &set(&[8, 7])).on_head, 0);
        assert_eq!(facts(&h, &head(10), &set(&[10, 3])).on_head, 2);
        // Some of them newer than the checked-out commit.
        assert_eq!(facts(&h, &head(6), &set(&[8, 7, 6, 4])).on_head, 2);
        let unborn = Head { branch: Some("main".to_owned()), oid: None, detached: false };
        assert_eq!(facts(&h, &unborn, &set(&[8, 7])).on_head, 0);
    }

    #[test]
    fn facts_say_why_commits_cannot_be_squashed() {
        let h = sample();
        let squash = |head: Head, ns: &[u8]| facts(&h, &head, &set(ns)).squash;
        assert_eq!(squash(head(10), &[10, 3]), Ok(oid(2)));
        assert!(squash(head(10), &[3, 2]).unwrap_err().contains("first commit of the repository"));
        // 7 is a merge; 6 and 4 sit below it on main.
        assert!(squash(head(8), &[8, 7]).unwrap_err().contains("\"commit 7\" is a merge commit"));
        // Below the merge on main's own line; the rebase makes 7 again.
        assert_eq!(squash(head(8), &[6, 4]), Ok(oid(3)));
        // 5 came in through the merge.
        assert!(squash(head(8), &[5, 4]).unwrap_err().contains("came into main through a merge"));
        assert!(squash(head(8), &[10, 3]).unwrap_err().starts_with("Not all of these commits are on main"));
        assert!(squash(head(10), &[10, 2]).unwrap_err().contains("not one unbroken line"));
        let detached = Head { detached: true, ..head(10) };
        assert!(squash(detached, &[10, 3]).unwrap_err().starts_with("HEAD is detached"));
        assert!(squash(head(10), &[10]).unwrap_err().contains("at least two"));
    }
}
