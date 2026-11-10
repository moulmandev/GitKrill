//! Preferences > Commit (repository-specific): what the commit button does
//! by default, and the commit message template.
//!
//! Shown by `crate::ui::prefs::commit`.

use serde::{Deserialize, Serialize};

/// Held by `RepoPrefs::commit`, for one repository.
#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
#[serde(default)]
pub struct RepoCommit {}
