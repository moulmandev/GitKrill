//! Preferences: what the pages under File > Preferences change, apart from
//! their user interface (`crate::ui::prefs`).
//!
//! Two kinds of preference live here:
//!
//! - **GitKrill's own**, kept in [`Settings`] (saved to `settings.json` by
//!   the app shell, like every other setting). Each area has its own struct
//!   in its own module, held by one field of `Settings` (`settings.general`,
//!   `settings.graph`, ...), so areas never edit each other's code.
//!   Preferences of one repository are in [`RepoPrefs`], by working
//!   directory (`settings.repos`).
//! - **Git's**, kept in git's own configuration files so that git and every
//!   other tool sees them (`core.autocrlf`, `commit.gpgsign`,
//!   `gitflow.prefix.feature`): read and written through
//!   [`gitconfig::ConfigFile`].
//!
//! Settings that must be put into effect somewhere outside `Settings` (the
//! environment of git commands, fonts, a theme file) do it in their area's
//! `apply`, which [`apply`] calls at start and after every change.

pub mod appearance;
pub mod commit;
pub mod editor;
pub mod general;
pub mod gitconfig;
pub mod graph;
pub mod identity;
pub mod system_date;
pub mod tools;

use std::path::Path;

use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::state::Settings;

/// Preferences of one repository that git has no setting for; see
/// [`repo`] and [`repo_mut`].
#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
#[serde(default)]
pub struct RepoPrefs {
    /// Preferences > Encoding.
    pub encoding: editor::RepoEncoding,
    /// Preferences > Commit.
    pub commit: commit::RepoCommit,
    /// In a fork, the hosting sidebar shows its parent's pull requests,
    /// issues and CI rather than the fork's own (the switch above them,
    /// `crate::ui::hosting`).
    pub hosting_parent: bool,
}

/// The preferences of the repository at `workdir`, or the defaults.
pub fn repo<'a>(settings: &'a Settings, workdir: &Path) -> std::borrow::Cow<'a, RepoPrefs> {
    match settings.repos.get(workdir) {
        Some(prefs) => std::borrow::Cow::Borrowed(prefs),
        None => std::borrow::Cow::Owned(RepoPrefs::default()),
    }
}

/// The preferences of the repository at `workdir`, to change. Only call it
/// to make a change: it adds an entry for the repository.
pub fn repo_mut<'a>(settings: &'a mut Settings, workdir: &Path) -> &'a mut RepoPrefs {
    settings.repos.entry(workdir.to_path_buf()).or_default()
}

/// Puts preferences that live outside [`Settings`] into effect. Called with
/// the loaded settings at start and again each time they changed (after
/// they are saved), whatever changed: every `apply` must be cheap and do
/// nothing when its own part is as it was last time.
pub fn apply(ctx: &egui::Context, settings: &Settings) {
    general::apply(ctx, settings);
    tools::apply(ctx, settings);
    identity::apply(ctx, settings);
    graph::apply(ctx, settings);
    appearance::apply(ctx, settings);
    editor::apply(ctx, settings);
}

/// Drops repository entries that hold nothing but defaults, so settings do
/// not grow with every repository ever looked at.
pub fn tidy(settings: &mut Settings) {
    settings.repos.retain(|_, prefs| *prefs != RepoPrefs::default());
}
