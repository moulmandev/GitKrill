//! Preferences > Editor: the font and layout of diffs, blame and the conflict editor.
//!
//! Shown by `crate::ui::prefs::editor`.

use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::state::Settings;

/// Held by `Settings::editor`.
#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
#[serde(default)]
pub struct Editor {}

/// Puts these preferences into effect; see [`crate::prefs::apply`].
pub fn apply(_ctx: &egui::Context, _settings: &Settings) {}

/// Preferences > Encoding, for one repository; held by `RepoPrefs::encoding`.
#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
#[serde(default)]
pub struct RepoEncoding {}
