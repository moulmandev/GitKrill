//! Preferences > Commit Graph: what the graph shows and how much history it loads.
//!
//! Shown by `crate::ui::prefs::graph`. The author, date and SHA columns are
//! older settings and stay where they were (`Settings::show_author`,
//! `show_date`, `show_sha`).
//!
//! The graph reads these fields every frame (plain field reads). How many
//! commits a repository loads is also needed where a tab is created, which
//! has no access to the settings: [`apply`] keeps it in an atomic for
//! [`initial_limit`].

use std::sync::atomic::{AtomicUsize, Ordering};

use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::state::{INITIAL_LIMIT, Settings};

/// The lowest "Initial commits in graph" accepted.
pub const MIN_INITIAL: u32 = 500;
/// The highest "Initial commits in graph" accepted; past this, "Show all
/// commits" is the setting to use.
pub const MAX_INITIAL: u32 = 1_000_000;

/// Held by `Settings::graph`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(default)]
pub struct GraphPrefs {
    /// The BRANCH / TAG column.
    pub show_refs: bool,
    /// The GRAPH column: lanes and commit nodes.
    pub show_tree: bool,
    /// The COMMIT MESSAGE column (its summary line).
    pub show_message: bool,
    /// The rest of the commit message, dimmed, after the summary.
    pub description: Description,
    /// A faded "branch" and "tag" label on the hovered or selected row that
    /// creates one there when clicked.
    pub ghost_refs: bool,
    /// Hovering a branch (in the graph or the left panel) dims the commits
    /// it does not contain.
    pub highlight_branch: bool,
    /// Commits loaded when a repository opens (at least [`MIN_INITIAL`]).
    pub initial_commits: u32,
    /// Load the next page by itself when the graph is scrolled to its end;
    /// off shows a "Load more commits" button there instead.
    pub lazy_load: bool,
    /// Load the whole history at once.
    pub show_all: bool,
}

impl Default for GraphPrefs {
    fn default() -> Self {
        GraphPrefs {
            show_refs: true,
            show_tree: true,
            show_message: true,
            // The graph has always shown the description after the summary.
            description: Description::Always,
            ghost_refs: true,
            highlight_branch: true,
            initial_commits: INITIAL_LIMIT as u32,
            lazy_load: true,
            show_all: false,
        }
    }
}

/// When a row shows its commit's description.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Description {
    Always,
    OnHover,
    Never,
}

impl Description {
    pub const CHOICES: [(Description, &'static str); 3] =
        [(Description::Always, "Always"), (Description::OnHover, "On hover"), (Description::Never, "Never")];
}

impl GraphPrefs {
    /// How many commits a repository loads before any scrolling: every one
    /// with "Show all commits", otherwise the number asked for.
    pub fn limit(&self) -> usize {
        if self.show_all { usize::MAX } else { self.initial_commits.clamp(MIN_INITIAL, MAX_INITIAL) as usize }
    }

    /// Whether row `hovered` shows its description.
    pub fn shows_description(&self, hovered: bool) -> bool {
        self.show_message
            && match self.description {
                Description::Always => true,
                Description::OnHover => hovered,
                Description::Never => false,
            }
    }
}

/// [`GraphPrefs::limit`] as last applied.
static LIMIT: AtomicUsize = AtomicUsize::new(INITIAL_LIMIT);

/// How many commits a repository loads up front, for code without access to
/// the settings (`RepoTab::new`). Follows the setting as soon as it changes.
pub fn initial_limit() -> usize {
    LIMIT.load(Ordering::Relaxed)
}

/// Puts these preferences into effect; see [`crate::prefs::apply`].
pub fn apply(_ctx: &egui::Context, settings: &Settings) {
    LIMIT.store(settings.graph.limit(), Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_keep_the_graph_as_it_was() {
        let prefs = GraphPrefs::default();
        assert!(prefs.show_refs && prefs.show_tree && prefs.show_message && prefs.lazy_load);
        assert!(!prefs.show_all);
        assert_eq!(prefs.limit(), INITIAL_LIMIT);
        assert!(prefs.shows_description(false));
    }

    #[test]
    fn older_settings_files_load_with_the_defaults() {
        let prefs: GraphPrefs = serde_json::from_str("{}").unwrap();
        assert_eq!(prefs, GraphPrefs::default());
        let prefs: GraphPrefs = serde_json::from_str(r#"{"show_refs": false, "description": "OnHover"}"#).unwrap();
        assert!(!prefs.show_refs);
        assert_eq!(prefs.description, Description::OnHover);
        assert!(prefs.show_tree);
    }

    #[test]
    fn the_limit_follows_show_all_and_the_minimum() {
        let mut prefs = GraphPrefs { initial_commits: 20, ..Default::default() };
        assert_eq!(prefs.limit(), MIN_INITIAL as usize);
        prefs.initial_commits = 12_000;
        assert_eq!(prefs.limit(), 12_000);
        prefs.show_all = true;
        assert_eq!(prefs.limit(), usize::MAX);
    }

    #[test]
    fn descriptions_show_when_asked_and_only_with_the_message() {
        let mut prefs = GraphPrefs { description: Description::OnHover, ..Default::default() };
        assert!(!prefs.shows_description(false));
        assert!(prefs.shows_description(true));
        prefs.description = Description::Never;
        assert!(!prefs.shows_description(true));
        prefs.description = Description::Always;
        prefs.show_message = false;
        assert!(!prefs.shows_description(true));
    }
}
