//! Preferences > General: fetching, submodules, new repositories, line endings, tabs.
//!
//! Shown by `crate::ui::prefs::general`.

use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::state::Settings;

/// Held by `Settings::general`.
#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
#[serde(default)]
pub struct General {}

/// Puts these preferences into effect; see [`crate::prefs::apply`].
pub fn apply(_ctx: &egui::Context, _settings: &Settings) {}
