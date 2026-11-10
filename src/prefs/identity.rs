//! Profiles, SSH and commit signing (Preferences > Profiles, SSH, GPG): who commits, with which key.
//!
//! Shown by `crate::ui::prefs::profiles`, `ssh` and `gpg`.

use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::state::Settings;

/// Held by `Settings::identity`.
#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
#[serde(default)]
pub struct Identity {}

/// Puts these preferences into effect; see [`crate::prefs::apply`].
pub fn apply(_ctx: &egui::Context, _settings: &Settings) {}

/// The profile in use, for the preferences sidebar ("Current profile");
/// `None` hides that part of the sidebar.
pub fn current_profile_name(_identity: &Identity) -> Option<&str> {
    None
}
