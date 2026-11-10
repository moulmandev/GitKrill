//! Preferences > Commit Graph: what the graph shows and how much history it loads.
//!
//! Shown by `crate::ui::prefs::graph`.

use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::state::Settings;

/// Held by `Settings::graph`.
#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
#[serde(default)]
pub struct GraphPrefs {}

/// Puts these preferences into effect; see [`crate::prefs::apply`].
pub fn apply(_ctx: &egui::Context, _settings: &Settings) {}
