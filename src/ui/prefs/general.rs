//! Preferences > General: auto-fetch and pruning, submodules, the default
//! branch of new repositories, remembering tabs, long paths and line
//! endings. GitKrill's own switches are in `crate::prefs::general`; the
//! last three rows are git settings in the user's global file.
//!
//! Hooks need no "Path to sh.exe" here: GitKrill runs them through git,
//! which finds its own shell.

use std::sync::mpsc::{Receiver, channel};

use eframe::egui::{self, Ui};

use super::{Page, form};
use crate::git::cli;
use crate::prefs::general;
use crate::prefs::gitconfig;

/// What this section remembers while preferences are open.
#[derive(Default)]
pub struct State {
    /// Git's system configuration (the file Git for Windows' installer
    /// writes), for what git uses when the global file sets nothing. `None`
    /// until read.
    system: Option<System>,
    loading: Option<Receiver<System>>,
}

/// The values of git's system configuration this page shows.
#[derive(Default)]
struct System {
    autocrlf: Option<String>,
    longpaths: Option<bool>,
}

impl State {
    /// Reads the system configuration once, in the background.
    fn load(&mut self, ctx: &egui::Context) {
        if self.system.is_some() {
            return;
        }
        match &self.loading {
            Some(rx) => {
                if let Ok(system) = rx.try_recv() {
                    self.system = Some(system);
                    self.loading = None;
                }
            }
            None => {
                let (tx, rx) = channel();
                let ctx = ctx.clone();
                std::thread::spawn(move || {
                    let _ = tx.send(System {
                        autocrlf: system_value("core.autocrlf"),
                        longpaths: system_value("core.longpaths").as_deref().and_then(gitconfig::parse_bool),
                    });
                    ctx.request_repaint();
                });
                self.loading = Some(rx);
            }
        }
    }
}

/// `git config --system --get <key>`; `None` when unset or unreadable.
fn system_value(key: &str) -> Option<String> {
    let home = dirs::home_dir().unwrap_or_else(std::env::temp_dir);
    let args = ["config", "--system", "--get", key].map(str::to_owned);
    let out = cli::run(&home, &args, cli::Options::default()).ok()?;
    let value = out.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

pub fn show(ui: &mut Ui, page: &mut Page<'_>, state: &mut State) {
    page.global.load(page.ctx);
    state.load(page.ctx);
    let (true, Some(system)) = (page.global.ready(), &state.system) else {
        form::loading(ui);
        return;
    };
    let settings = &mut page.shared.settings;

    form::row(ui, "Auto-Fetch Interval", |ui| {
        let mut minutes = settings.auto_fetch_minutes.min(general::MAX_FETCH_MINUTES);
        if form::number(ui, &mut minutes, 0, general::MAX_FETCH_MINUTES) {
            general::set_fetch_interval(settings, minutes);
        }
        form::help(ui, "In minutes, between 0 and 60 (0 disables auto-fetch). Fetches all remotes.");
        form::warning(ui, "With many remotes, each fetch takes longer. LFS objects are not fetched automatically.");
    });

    form::row(ui, "Auto-Prune", |ui| {
        form::checkbox(ui, &mut settings.general.auto_prune, "");
        form::help(ui, "Remove remote branches that no longer exist on the remote when fetching.");
    });

    form::row(ui, "Keep submodules up to date", |ui| {
        form::checkbox(
            ui,
            &mut settings.general.update_submodules,
            "Automatically update all submodules after performing a Git action",
        );
        form::help(ui, "After checkout, pull, merge, rebase, reset, cherry-pick and revert, in repositories with submodules.");
    });

    form::row(ui, "Default Branch Name", |ui| {
        form::config_text(ui, page.ctx, page.global, "init.defaultBranch", "main");
        form::help(ui, "Sets the default branch name when initializing new repositories.");
    });

    form::row(ui, "Remember tabs", |ui| {
        form::checkbox(ui, &mut settings.general.remember_tabs, "");
        form::help(ui, "Reopen the tabs of the last session when GitKrill starts.");
    });

    if cfg!(windows) {
        form::row(ui, "Longpaths", |ui| {
            form::config_bool(ui, page.ctx, page.global, "core.longpaths", system.longpaths.unwrap_or(false), "");
            form::help(
                ui,
                "Sets core.longpaths in your global Git config. By default, Windows only supports file paths \
                 shorter than 260 characters. Enable support for longer paths.",
            );
        });
    }

    form::row(ui, "Line Endings (AutoCRLF)", |ui| {
        autocrlf(ui, page, system);
        form::help(ui, "Sets core.autocrlf in your global Git config.");
    });
}

/// The `core.autocrlf` drop-down: git's three values, or nothing set (git
/// then uses the system configuration's value, or leaves line endings
/// alone).
fn autocrlf(ui: &mut Ui, page: &mut Page<'_>, system: &System) {
    const KEY: &str = "core.autocrlf";
    let stored = page.global.get(KEY).map(str::trim);
    // As git reads it: "input", or a boolean in any of git's spellings.
    let mut value: Option<String> = stored.map(|raw| match gitconfig::parse_bool(raw) {
        Some(on) => on.to_string(),
        None => raw.to_ascii_lowercase(),
    });
    let unset = match &system.autocrlf {
        Some(v) => format!("Not set (Git uses {v}, from its system config)"),
        None => "Not set (Git leaves line endings alone)".to_owned(),
    };
    let mut options: Vec<(Option<String>, &str)> = vec![
        (None, unset.as_str()),
        (Some("true".to_owned()), "Convert to CRLF on checkout, LF on commit (true)"),
        (Some("input".to_owned()), "Convert to LF on commit only (input)"),
        (Some("false".to_owned()), "Leave line endings alone (false)"),
    ];
    // Something git does not understand stays visible rather than blank.
    let other = value.as_deref().filter(|v| !["true", "input", "false"].contains(v)).map(|v| format!("{v} (not a value Git knows)"));
    if let Some(label) = &other {
        options.push((value.clone(), label.as_str()));
    }
    if form::choice(ui, "prefs-general-autocrlf", &mut value, &options) {
        page.global.set(page.ctx, KEY, value.as_deref());
    }
}
