//! Preferences > UI Customization and Notifications: themes, the toolbar, dates, where toasts go.
//!
//! Shown by `crate::ui::prefs::appearance` and `notifications`.

use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::state::Settings;

/// Held by `Settings::appearance`.
#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
#[serde(default)]
pub struct Appearance {}

/// Puts these preferences into effect; see [`crate::prefs::apply`].
pub fn apply(_ctx: &egui::Context, _settings: &Settings) {}
