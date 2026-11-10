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

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const HEADS: &str = "refs/heads/";
pub const REMOTES: &str = "refs/remotes/";
pub const TAGS: &str = "refs/tags/";

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
