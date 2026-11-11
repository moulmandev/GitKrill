//! Branches, folders of branches, remotes and tags the user chose to leave
//! out of the graph.
//!
//! Rules are kept per repository. Each one names either a full ref
//! (`refs/heads/t3/fix`) or a folder, written with a trailing slash
//! (`refs/heads/t3/`, or `refs/remotes/origin/` for a whole remote), and says
//! whether it is hidden or shown. A ref follows the rule on itself if there is
//! one, otherwise the rule on its nearest folder, otherwise it is shown. So
//! hiding a folder also hides branches created in it later, and a single
//! branch can still be shown inside a hidden folder.
//!
//! Hidden refs stay in the sidebar (dimmed) but are not walked: commits only
//! they reach drop out of the graph, and their labels disappear from it.
//!
//! The checked out branch is always shown, unless a rule on [`HEAD`] hides
//! it too: [`Hidden::only`] sets one to show a single branch alone.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const HEADS: &str = "refs/heads/";
pub const REMOTES: &str = "refs/remotes/";
pub const TAGS: &str = "refs/tags/";
/// The rule that hides the checked out branch (and a detached HEAD) as well.
pub const HEAD: &str = "HEAD";

/// The full ref name of a local branch.
pub fn local(name: &str) -> String {
    format!("{HEADS}{name}")
}

/// The full ref name of a remote branch given as `origin/main`.
pub fn remote(full: &str) -> String {
    format!("{REMOTES}{full}")
}

/// The full ref name of a tag.
pub fn tag(name: &str) -> String {
    format!("{TAGS}{name}")
}

/// A folder of refs: `prefix` (such as [`HEADS`]) followed by `path` and a
/// slash. `origin` under [`REMOTES`] is the whole remote.
pub fn folder(prefix: &str, path: &str) -> String {
    format!("{prefix}{path}/")
}

/// The folder a ref or folder sits in, or `None` at the top.
fn parent(path: &str) -> Option<&str> {
    let trimmed = path.strip_suffix('/').unwrap_or(path);
    trimmed.rfind('/').map(|slash| &path[..=slash])
}

#[derive(Clone, Default, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Hidden(BTreeMap<String, bool>);

impl Hidden {
    /// Everything hidden but the ref `path` (a full ref name), the checked
    /// out branch included unless it is that one: one branch shown alone.
    pub fn only(path: &str) -> Hidden {
        let mut hidden = Hidden::default();
        hidden.0.insert(HEAD.to_owned(), true);
        hidden.set("refs/", true);
        hidden.set(path, false);
        hidden
    }

    /// Whether the checked out branch is hidden too (see [`Hidden::only`]).
    pub fn hides_head(&self) -> bool {
        self.0.get(HEAD) == Some(&true)
    }

    /// The refs and folders shown by a rule of their own, for a view that
    /// hides everything else.
    pub fn shown(&self) -> impl Iterator<Item = &str> {
        self.0.iter().filter(|(_, hidden)| !**hidden).map(|(path, _)| path.as_str())
    }

    /// Whether nothing is hidden at all.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether the ref or folder `path` is hidden.
    pub fn hides(&self, path: &str) -> bool {
        !self.0.is_empty() && self.rule(path).unwrap_or(false)
    }

    pub fn hides_local(&self, name: &str) -> bool {
        !self.0.is_empty() && self.hides(&local(name))
    }

    pub fn hides_remote(&self, full: &str) -> bool {
        !self.0.is_empty() && self.hides(&remote(full))
    }

    pub fn hides_tag(&self, name: &str) -> bool {
        !self.0.is_empty() && self.hides(&tag(name))
    }

    /// Whether anything at or under `prefix` is hidden.
    pub fn hides_any(&self, prefix: &str) -> bool {
        self.0.iter().any(|(path, hidden)| *hidden && path.starts_with(prefix))
    }

    /// The rules on `prefix` and on everything under it, each folder before
    /// what is in it: applied in this order with [`Hidden::set`], they come
    /// out the same.
    pub fn rules_under(&self, prefix: &str) -> Vec<(String, bool)> {
        self.0
            .range(prefix.to_owned()..)
            .take_while(|(path, _)| path.starts_with(prefix))
            .map(|(path, hidden)| (path.clone(), *hidden))
            .collect()
    }

    fn rule(&self, path: &str) -> Option<bool> {
        match self.0.get(path) {
            Some(hidden) => Some(*hidden),
            None => self.rule(parent(path)?),
        }
    }

    /// Hides or shows the ref or folder `path`. For a folder this covers
    /// everything in it: rules set on what is inside are dropped.
    pub fn set(&mut self, path: &str, hide: bool) {
        if path.ends_with('/') {
            self.0.retain(|key, _| !key.starts_with(path));
        } else {
            self.0.remove(path);
        }
        let inherited = parent(path).and_then(|p| self.rule(p)).unwrap_or(false);
        if inherited != hide {
            self.0.insert(path.to_owned(), hide);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hidden_folder_hides_everything_in_it_including_later_branches() {
        let mut hidden = Hidden::default();
        hidden.set(&folder(HEADS, "t3"), true);
        assert!(hidden.hides_local("t3/one"));
        assert!(hidden.hides_local("t3/deeper/two"));
        assert!(hidden.hides(&folder(HEADS, "t3/deeper")));
        assert!(!hidden.hides_local("t3"), "a branch named like the folder is not in it");
        assert!(!hidden.hides_local("t30/x"));
        assert!(!hidden.hides_local("main"));
        assert!(!hidden.hides_remote("origin/t3/one"), "remote branches are separate");
    }

    #[test]
    fn the_rules_under_a_folder_carry_over_to_another() {
        let mut hidden = Hidden::default();
        hidden.set(&folder(REMOTES, "origin"), true);
        hidden.set(&remote("origin/keep"), false);
        hidden.set(&remote("originals/main"), true);
        let from = folder(REMOTES, "origin");
        let rules = hidden.rules_under(&from);
        assert_eq!(rules, [(from.clone(), true), (remote("origin/keep"), false)]);
        for (path, hide) in rules {
            hidden.set(&format!("refs/remotes/upstream/{}", &path[from.len()..]), hide);
        }
        assert!(hidden.hides_remote("upstream/main") && !hidden.hides_remote("upstream/keep"));
        assert!(hidden.hides_remote("origin/main"), "the old name keeps its rules");
    }

    #[test]
    fn one_branch_can_be_shown_inside_a_hidden_folder() {
        let mut hidden = Hidden::default();
        hidden.set(&folder(HEADS, "t3"), true);
        hidden.set(&local("t3/keep"), false);
        assert!(!hidden.hides_local("t3/keep"));
        assert!(hidden.hides_local("t3/other"));

        // Hiding it again leaves no rule of its own behind.
        hidden.set(&local("t3/keep"), true);
        assert_eq!(hidden.0.len(), 1);

        // Showing the folder shows everything in it, exceptions or not.
        hidden.set(&local("t3/keep"), false);
        hidden.set(&folder(HEADS, "t3"), false);
        assert!(hidden.is_empty());
    }

    #[test]
    fn hiding_a_folder_replaces_rules_inside_it() {
        let mut hidden = Hidden::default();
        hidden.set(&local("t3/a"), true);
        hidden.set(&local("t3/b"), true);
        hidden.set(&local("main"), true);
        hidden.set(&folder(HEADS, "t3"), true);
        assert_eq!(hidden.0.len(), 2);
        assert!(hidden.hides_any(HEADS));
        assert!(!hidden.hides_any(REMOTES));
    }

    #[test]
    fn a_whole_remote_and_all_tags() {
        let mut hidden = Hidden::default();
        hidden.set(&folder(REMOTES, "upstream"), true);
        hidden.set(TAGS, true);
        assert!(hidden.hides_remote("upstream/main"));
        assert!(!hidden.hides_remote("origin/main"));
        assert!(hidden.hides_tag("v1.0"));
        hidden.set(&tag("v1.0"), false);
        assert!(!hidden.hides_tag("v1.0"));
        assert!(hidden.hides_tag("v2.0"));
    }

    #[test]
    fn one_branch_alone() {
        let hidden = Hidden::only(&local("feature/x"));
        assert!(hidden.hides_head());
        assert!(!hidden.hides_local("feature/x"));
        assert!(hidden.hides_local("main") && hidden.hides_local("feature/y"));
        assert!(hidden.hides_remote("origin/feature/x") && hidden.hides_tag("v1.0"));
        assert_eq!(hidden.shown().collect::<Vec<_>>(), ["refs/heads/feature/x"]);

        let mut more = Hidden::only(&remote("origin/main"));
        assert!(!more.hides_remote("origin/main") && more.hides_local("main"));
        more.set(&local("main"), false);
        assert_eq!(more.shown().count(), 2);
        assert!(!Hidden::default().hides_head());
    }

    #[test]
    fn one_branch_alone_leaves_the_checked_out_one_and_the_rest_out() {
        use crate::git::RowKind;
        use crate::tests::{Fixture, git};

        let fx = Fixture::new("hidden-only");
        fx.write("a.txt", "base\n");
        fx.commit_all("base");
        for name in ["feature/ünï-x", "other"] {
            git(&fx.dir, &["checkout", "-q", "-b", name, "main"]);
            fx.write("a.txt", &format!("{name}\n"));
            fx.commit_all(&format!("on {name}"));
        }
        git(&fx.dir, &["checkout", "-q", "main"]);
        fx.write("a.txt", "main\n");
        fx.commit_all("on main");
        git(&fx.dir, &["tag", "v1", "other"]);
        // Uncommitted changes, so the graph has a WIP row on HEAD.
        fx.write("a.txt", "dirty\n");

        let summaries = |hidden: &Hidden| {
            fx.handle.set_hidden(hidden.clone());
            let snap = fx.snapshot();
            let mut found: Vec<String> = snap.history.commits.iter().map(|c| c.summary.clone()).collect();
            found.sort();
            (found, snap)
        };
        let (all, _) = summaries(&Hidden::default());
        assert_eq!(all, ["// WIP", "base", "on feature/ünï-x", "on main", "on other"]);

        // A branch other than the checked out one: main's commit goes too,
        // and the WIP row stays without a line to a commit not shown.
        let (alone, snap) = summaries(&Hidden::only(&local("feature/ünï-x")));
        assert_eq!(alone, ["// WIP", "base", "on feature/ünï-x"]);
        let wip = snap.history.commits.iter().find(|c| c.kind == RowKind::Wip).unwrap();
        assert!(wip.parents.is_empty());
        let labels: Vec<&str> = snap.refs_by_oid.values().flatten().map(|l| l.name.as_str()).collect();
        assert_eq!(labels, ["feature/ünï-x"]);

        // The checked out branch itself keeps its WIP line.
        let (alone, snap) = summaries(&Hidden::only(&local("main")));
        assert_eq!(alone, ["// WIP", "base", "on main"]);
        let wip = snap.history.commits.iter().find(|c| c.kind == RowKind::Wip).unwrap();
        assert_eq!(wip.parents.len(), 1);

        // A branch shown on top of it joins in.
        let mut more = Hidden::only(&local("main"));
        more.set(&local("other"), false);
        assert_eq!(summaries(&more).0, ["// WIP", "base", "on main", "on other"]);
    }

    #[test]
    fn survives_a_settings_round_trip() {
        let mut hidden = Hidden::default();
        hidden.set(&folder(HEADS, "t3"), true);
        hidden.set(&local("t3/keep"), false);
        let mut by_repo = BTreeMap::new();
        by_repo.insert(std::path::PathBuf::from(r"E:\repo"), hidden.clone());
        let json = serde_json::to_string(&by_repo).unwrap();
        let back: BTreeMap<std::path::PathBuf, Hidden> = serde_json::from_str(&json).unwrap();
        assert_eq!(back[&std::path::PathBuf::from(r"E:\repo")], hidden);
    }
}
