//! Reflogs: where HEAD, or a branch, pointed after each thing git did to it,
//! newest first, as `git reflog` lists them.
//!
//! [`read`] runs on the reader thread. Besides the entries it says which of
//! their commits no branch, tag or other ref reaches any more (an amended
//! commit, the commits before a rebase or a reset), since those are the ones
//! the graph cannot show.

use std::collections::{HashMap, HashSet};

use git2::Repository;

use super::repo::lossy;
use super::Oid;

/// Entries read at most, newest first. A reflog this long is years of work;
/// the oldest are left out rather than slowing the view down.
pub const MAX_ENTRIES: usize = 20_000;

/// Unreachable commits looked for at most: the walk that finds them stops
/// here, and the entries it did not get to count as reachable.
const MAX_WALK: usize = 50_000;

#[derive(Clone, Debug)]
pub struct Entry {
    /// Its place in the reflog: `HEAD@{index}`.
    pub index: usize,
    /// Where the ref pointed after this.
    pub oid: Oid,
    /// Where it pointed before (zero when it was created).
    pub old: Oid,
    /// What git did: "commit", "checkout", "rebase (finish)", "reset", ...
    pub action: String,
    /// The rest of git's message: "moving from main to topic", a commit
    /// summary, "Fast-forward". Empty when there is none.
    pub detail: String,
    /// When, in seconds since the epoch.
    pub time: i64,
    pub who: String,
    /// The summary of the commit, or `None` once git has thrown it away.
    pub summary: Option<String>,
    pub is_merge: bool,
    /// No branch, tag or other ref reaches the commit any more.
    pub unreachable: bool,
}

impl Entry {
    /// The broad kind of action, for its icon and colour.
    pub fn kind(&self) -> Kind {
        let action = self.action.as_str();
        let first = action.split([' ', ':']).next().unwrap_or_default();
        match first {
            "commit" if action.contains("amend") => Kind::Amend,
            "commit" => Kind::Commit,
            "checkout" | "switch" => Kind::Checkout,
            "reset" => Kind::Reset,
            "rebase" | "pull" if action.contains("rebase") => Kind::Rebase,
            "merge" | "pull" => Kind::Merge,
            "cherry-pick" | "revert" => Kind::Pick,
            "branch" | "Branch" | "clone" | "fetch" | "push" | "update" => Kind::Branch,
            _ => Kind::Other,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Commit,
    Amend,
    Checkout,
    Reset,
    Rebase,
    Merge,
    Pick,
    Branch,
    Other,
}

#[derive(Clone, Debug)]
pub struct Reflog {
    /// `HEAD`, or the full name of the branch (`refs/heads/main`).
    pub refname: String,
    /// Newest first.
    pub entries: Vec<Entry>,
    /// How many older entries were left out (see [`MAX_ENTRIES`]).
    pub more: usize,
}

/// `HEAD` as it is, a branch by its short name.
pub fn display_name(refname: &str) -> &str {
    refname.strip_prefix("refs/heads/").unwrap_or(refname)
}

/// Splits git's reflog message into the action and the rest:
/// "checkout: moving from a to b" is ("checkout", "moving from a to b").
pub fn split_message(message: &str) -> (String, String) {
    let message = message.trim();
    match message.split_once(": ") {
        Some((action, detail)) => (action.trim().to_owned(), detail.trim().to_owned()),
        None => (message.trim_end_matches(':').to_owned(), String::new()),
    }
}

/// Reads the reflog of `refname` (`HEAD` or `refs/heads/<name>`).
pub fn read(repo: &Repository, refname: &str) -> Result<Reflog, String> {
    if refname != "HEAD" && repo.find_reference(refname).is_err() {
        return Err(format!("There is no branch {} any more.", display_name(refname)));
    }
    let log = repo.reflog(refname).map_err(|e| e.message().to_owned())?;
    let total = log.len();
    let mut entries = Vec::with_capacity(total.min(MAX_ENTRIES));
    // Many entries share a commit (checkouts back and forth).
    let mut commits: HashMap<Oid, Option<(String, bool)>> = HashMap::new();
    for (index, entry) in log.iter().enumerate().take(MAX_ENTRIES) {
        let oid = entry.id_new();
        let (action, detail) = split_message(&entry.message_bytes().map(lossy).unwrap_or_default());
        let commit = commits
            .entry(oid)
            .or_insert_with(|| {
                let commit = repo.find_commit(oid).ok()?;
                Some((commit.summary_bytes().map(lossy).unwrap_or_default(), commit.parent_count() > 1))
            })
            .clone();
        let committer = entry.committer();
        entries.push(Entry {
            index,
            oid,
            old: entry.id_old(),
            action,
            detail,
            time: committer.when().seconds(),
            who: lossy(committer.name_bytes()),
            is_merge: commit.as_ref().is_some_and(|(_, merge)| *merge),
            summary: commit.map(|(summary, _)| summary),
            unreachable: false,
        });
    }

    let present: Vec<Oid> = commits.iter().filter(|(_, c)| c.is_some()).map(|(oid, _)| *oid).collect();
    let lost = unreachable(repo, &present);
    for entry in &mut entries {
        entry.unreachable = lost.contains(&entry.oid);
    }
    Ok(Reflog { refname: refname.to_owned(), entries, more: total.saturating_sub(MAX_ENTRIES) })
}

/// Which of `oids` no ref reaches: everything they lead to, minus what the
/// refs lead to (`git rev-list <oids> --not --all`). The walk stops where
/// the two meet, so it costs about as much as the history since the oldest
/// of them.
fn unreachable(repo: &Repository, oids: &[Oid]) -> HashSet<Oid> {
    let wanted: HashSet<Oid> = oids.iter().copied().collect();
    let mut found = HashSet::new();
    let Ok(mut walk) = repo.revwalk() else { return found };
    for oid in oids {
        let _ = walk.push(*oid);
    }
    if let Ok(head) = repo.head()
        && let Ok(commit) = head.peel_to_commit()
    {
        let _ = walk.hide(commit.id());
    }
    if let Ok(refs) = repo.references() {
        for reference in refs.flatten() {
            if let Ok(commit) = reference.peel_to_commit() {
                let _ = walk.hide(commit.id());
            }
        }
    }
    for oid in walk.take(MAX_WALK).flatten() {
        if wanted.contains(&oid) {
            found.insert(oid);
            if found.len() == wanted.len() {
                break;
            }
        }
    }
    found
}

/// `HEAD@{3}` for the entry at `index` of `refname`'s reflog.
pub fn selector(refname: &str, index: usize) -> String {
    format!("{}@{{{index}}}", display_name(refname))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::ops;
    use crate::tests::{Fixture, git};

    fn reflog(fx: &Fixture, refname: &str) -> Result<Reflog, String> {
        let refname = refname.to_owned();
        fx.job(move |loader| read(loader.repo(), &refname))
    }

    fn actions(log: &Reflog) -> Vec<String> {
        log.entries.iter().map(|e| format!("{}: {}", e.action, e.detail)).collect()
    }

    #[test]
    fn messages_split_into_action_and_detail() {
        assert_eq!(split_message("checkout: moving from main to topic"), ("checkout".into(), "moving from main to topic".into()));
        assert_eq!(split_message("commit (amend): Fix: the thing"), ("commit (amend)".into(), "Fix: the thing".into()));
        assert_eq!(split_message("rebase (finish): returning to refs/heads/main"), ("rebase (finish)".into(), "returning to refs/heads/main".into()));
        assert_eq!(split_message("pull:"), ("pull".into(), String::new()));
        assert_eq!(split_message("odd message"), ("odd message".into(), String::new()));
        assert_eq!(selector("refs/heads/main", 2), "main@{2}");
        assert_eq!(selector("HEAD", 0), "HEAD@{0}");
    }

    #[test]
    fn head_and_branch_reflogs_list_what_happened_newest_first() {
        let fx = Fixture::new("reflog-read");
        fx.write("a.txt", "a\n");
        fx.commit_all("first");
        fx.write("b file.txt", "b\n");
        fx.commit_all("second: with a colon");
        git(&fx.dir, &["checkout", "-q", "-b", "tópico"]);
        fx.write("c.txt", "c\n");
        fx.commit_all("on the topic");
        git(&fx.dir, &["commit", "-q", "--amend", "-m", "on the topic, amended"]);
        git(&fx.dir, &["checkout", "-q", "main"]);

        let head = reflog(&fx, "HEAD").unwrap();
        assert_eq!(
            actions(&head),
            [
                "checkout: moving from tópico to main",
                "commit (amend): on the topic, amended",
                "commit: on the topic",
                "checkout: moving from main to tópico",
                "commit: second: with a colon",
                "commit (initial): first",
            ]
        );
        assert_eq!(head.entries[0].index, 0);
        assert_eq!(head.entries[1].kind(), Kind::Amend);
        assert_eq!(head.entries[0].kind(), Kind::Checkout);
        assert_eq!(head.entries[0].who, "Test User");
        assert_eq!(head.entries[4].summary.as_deref(), Some("second: with a colon"));
        // The commit the amend replaced is reachable from nothing but the reflog.
        let lost: Vec<bool> = head.entries.iter().map(|e| e.unreachable).collect();
        assert_eq!(lost, [false, false, true, false, false, false]);
        assert_eq!(head.more, 0);

        let topic = reflog(&fx, "refs/heads/tópico").unwrap();
        assert_eq!(
            actions(&topic),
            ["commit (amend): on the topic, amended", "commit: on the topic", "branch: Created from HEAD"]
        );
        assert!(reflog(&fx, "refs/heads/gone").unwrap_err().contains("no branch gone"));
    }

    #[test]
    fn commits_left_behind_by_a_reset_are_unreachable_until_a_ref_has_them_again() {
        let fx = Fixture::new("reflog-reset");
        fx.write("a.txt", "a\n");
        fx.commit_all("one");
        fx.write("a.txt", "b\n");
        fx.commit_all("two");
        let two = fx.snapshot().head.oid.unwrap();
        fx.run(ops::reset("HEAD~1", ops::ResetMode::Hard));
        let log = reflog(&fx, "HEAD").unwrap();
        assert!(log.entries[0].action.starts_with("reset"), "{:?}", actions(&log));
        assert_eq!(log.entries[1].oid, two);
        assert!(log.entries[1].unreachable);
        assert!(!log.entries[0].unreachable);

        // A tag on it makes it reachable again.
        git(&fx.dir, &["tag", "kept", &two.to_string()]);
        assert!(!reflog(&fx, "HEAD").unwrap().entries[1].unreachable);
    }

    #[test]
    fn a_new_repository_has_an_empty_reflog() {
        let fx = Fixture::new("reflog-empty");
        let log = reflog(&fx, "HEAD").unwrap();
        assert!(log.entries.is_empty());
    }
}
