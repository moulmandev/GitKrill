//! Preferences > External Tools: merge and diff tools, the editor and the terminal to open files and repositories in.
//!
//! Shown by `crate::ui::prefs::tools`; `crate::external` finds the tools on
//! this computer and starts them.

use std::path::PathBuf;

use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::state::Settings;

/// Held by `Settings::tools`.
#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
#[serde(default)]
pub struct Tools {
    /// Resolves conflicted files (`git mergetool`).
    pub merge: MergeTool,
    /// Compares two versions of a file (`git difftool`).
    pub diff: DiffTool,
    /// Opens files and the repository folder.
    pub editor: Editor,
    /// The command line of [`Editor::Custom`], kept while another editor is
    /// chosen so switching back does not lose it.
    pub editor_command: String,
    /// Opens a terminal in the repository.
    pub terminal: Terminal,
    /// Use [`Tools::terminal_command`] instead of [`Tools::terminal`].
    pub custom_terminal: bool,
    pub terminal_command: String,
}

/// A program chosen from the tools found on this computer: which known tool
/// it is (`crate::external` knows how to start each), its name, and where it
/// was found, so starting it never has to look for it again.
#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct Program {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
}

#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MergeTool {
    /// Whatever `merge.tool` in git's configuration says.
    #[default]
    GitConfig,
    Program(Program),
}

#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiffTool {
    /// The merge tool, whichever it is.
    #[default]
    SameAsMerge,
    /// Whatever `diff.tool` (or else `merge.tool`) in git's configuration says.
    GitConfig,
    Program(Program),
}

#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Editor {
    #[default]
    None,
    Program(Program),
    /// [`Tools::editor_command`].
    Custom,
}

#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Terminal {
    /// `crate::platform::terminal`: Windows Terminal or the classic console
    /// on Windows, Terminal on macOS, the first terminal found on Linux.
    #[default]
    System,
    Program(Program),
}

impl Tools {
    /// Whether "Open in editor" has anything to open with.
    pub fn has_editor(&self) -> bool {
        match &self.editor {
            Editor::None => false,
            Editor::Program(_) => true,
            Editor::Custom => !self.editor_command.trim().is_empty(),
        }
    }

    /// What the editor is, for the command palette ("Visual Studio Code").
    pub fn editor_name(&self) -> &str {
        match &self.editor {
            Editor::Program(program) => &program.name,
            Editor::Custom => "custom command",
            Editor::None => "none",
        }
    }
}

/// Puts these preferences into effect; see [`crate::prefs::apply`]. Nothing
/// to do: every launch reads the settings as they are.
pub fn apply(_ctx: &egui::Context, _settings: &Settings) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_and_default_when_missing() {
        let tools = Tools {
            merge: MergeTool::Program(Program { id: "bc".into(), name: "Beyond Compare".into(), path: "C:/bc/BComp.exe".into() }),
            diff: DiffTool::GitConfig,
            editor: Editor::Custom,
            editor_command: "code {path}".into(),
            terminal: Terminal::System,
            custom_terminal: true,
            terminal_command: "wt -d {path}".into(),
        };
        let json = serde_json::to_string(&tools).unwrap();
        assert_eq!(serde_json::from_str::<Tools>(&json).unwrap(), tools);
        assert_eq!(serde_json::from_str::<Tools>("{}").unwrap(), Tools::default());
        assert!(tools.has_editor());
        assert!(!Tools::default().has_editor());
    }
}
