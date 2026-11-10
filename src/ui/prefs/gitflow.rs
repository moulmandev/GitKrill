//! Preferences > Gitflow (repository-specific): the branch names and prefixes
//! git-flow keeps in the repository's configuration (`gitflow.*`), and the
//! button that sets Gitflow up. Starting and finishing branches is in
//! `crate::ui::gitflow`.

use std::collections::HashMap;
use std::time::Instant;

use eframe::egui::{self, RichText, Ui};

use super::{Page, form};
use crate::git::RepoSnapshot;
use crate::git::gitflow::{self, Config, Existing, keys};
use crate::prefs::gitconfig::ConfigFile;
use crate::state::RepoTab;
use crate::theme::{self, icons};
use crate::ui::dialogs::valid_ref;

/// What this section remembers while preferences are open.
#[derive(Default)]
pub struct State {
    /// The names typed before Gitflow is set up; git-flow's defaults at first.
    draft: Option<Config>,
    /// Text in a field that is being typed, or that was left invalid (it
    /// stays on screen with its error until fixed), by key.
    editing: HashMap<&'static str, String>,
    /// When "Initialize Gitflow" was clicked, until Gitflow shows as set up
    /// or the command fails.
    initializing: Option<Instant>,
}

/// The fields of the page, in order: key, label, placeholder.
const BRANCHES: [(&str, &str, &str); 2] = [(keys::MASTER, "Master", "master"), (keys::DEVELOP, "Develop", "develop")];
const PREFIXES: [(&str, &str, &str); 4] = [
    (keys::FEATURE, "Feature", "feature/"),
    (keys::RELEASE, "Release", "release/"),
    (keys::HOTFIX, "Hotfix", "hotfix/"),
    (keys::VERSION_TAG, "Version Tag", ""),
];

fn value<'a>(config: &'a Config, key: &str) -> &'a str {
    match key {
        keys::MASTER => &config.master,
        keys::DEVELOP => &config.develop,
        keys::FEATURE => &config.feature,
        keys::RELEASE => &config.release,
        keys::HOTFIX => &config.hotfix,
        _ => &config.version_tag,
    }
}

fn value_mut<'a>(config: &'a mut Config, key: &str) -> &'a mut String {
    match key {
        keys::MASTER => &mut config.master,
        keys::DEVELOP => &mut config.develop,
        keys::FEATURE => &mut config.feature,
        keys::RELEASE => &mut config.release,
        keys::HOTFIX => &mut config.hotfix,
        _ => &mut config.version_tag,
    }
}

/// Whether `value` can be the setting `key`; `config` holds the others.
fn check(key: &str, value: &str, config: &Config) -> Result<(), String> {
    match key {
        keys::MASTER | keys::DEVELOP => {
            let other = if key == keys::MASTER { &config.develop } else { &config.master };
            if value.is_empty() {
                Err("A branch name is needed.".to_owned())
            } else if !valid_ref(value) {
                Err(format!("{value} is not a valid branch name."))
            } else if value == other {
                Err("Master and develop must be different branches.".to_owned())
            } else {
                Ok(())
            }
        }
        keys::VERSION_TAG if !value.is_empty() && !valid_ref(&format!("{value}1.0")) => {
            Err(format!("Tags cannot start with {value}."))
        }
        keys::VERSION_TAG => Ok(()),
        _ if !valid_ref(&format!("{value}name")) => Err(format!("Branch names cannot start with {value}.")),
        _ => Ok(()),
    }
}

fn help_for(key: &str) -> Option<&'static str> {
    match key {
        keys::MASTER => Some("The production branch: releases and hotfixes are merged into it and tagged."),
        keys::DEVELOP => Some("Features and releases start from it, and everything finished is merged back into it."),
        keys::VERSION_TAG => Some("Put before a version to name its tag: v makes v1.2.0. May be empty."),
        _ => None,
    }
}

pub fn show(ui: &mut Ui, page: &mut Page<'_>, state: &mut State) {
    let (Some(file), Some(tab)) = (page.repo.as_deref_mut(), page.tab.as_deref_mut()) else { return };
    let ctx = page.ctx;
    file.load(ctx);
    if let Some(since) = state.initializing {
        match tab.take_outcome(gitflow::INIT_LABEL, since) {
            // The command wrote the file behind `file`'s back.
            Some(true) => file.reload(ctx),
            Some(false) => state.initializing = None,
            None => {}
        }
    }
    let Some(snap) = tab.snap.clone() else {
        form::loading(ui);
        return;
    };
    if !file.ready() {
        form::loading(ui);
        return;
    }
    // Right after initializing, the snapshot can know before the file does.
    let config = Config::from_lookup(|key| file.get(key).map(str::to_owned)).or_else(|| snap.gitflow.clone());
    match config {
        Some(config) => {
            state.initializing = None;
            set_up(ui, ctx, file, &snap, state, &config);
        }
        None => not_set_up(ui, tab, &snap, state),
    }
}

/// The page once Gitflow is set up: every field edits its key.
fn set_up(ui: &mut Ui, ctx: &egui::Context, file: &mut ConfigFile, snap: &RepoSnapshot, state: &mut State, config: &Config) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.label(RichText::new(icons::CHECK_CIRCLE).size(16.0).color(theme::green()));
        ui.label(RichText::new("Gitflow is set up for this repository.").size(13.5).color(theme::text_bright()));
    });
    form::intro(
        ui,
        "Start and finish features, releases and hotfixes from Gitflow in the toolbar's branch menu, \
         from a branch's right-click menu, or from the command palette. Changing a name here does not \
         rename a branch.",
    );

    form::heading(ui, "Branches");
    for (key, label, hint) in BRANCHES {
        form::row(ui, label, |ui| {
            config_field(ui, ctx, file, state, config, key, hint);
            let name = value(config, key);
            if !name.is_empty() && !state.editing.contains_key(key) && !snap.branches.iter().any(|b| b.name == name) {
                form::warning(ui, &format!("There is no branch named {name} in this repository."));
            }
            if let Some(help) = help_for(key) {
                form::help(ui, help);
            }
        });
    }
    form::heading(ui, "Prefixes");
    for (key, label, hint) in PREFIXES {
        form::row(ui, label, |ui| {
            config_field(ui, ctx, file, state, config, key, hint);
            if let Some(help) = help_for(key) {
                form::help(ui, help);
            }
        });
    }
}

/// A text field bound to `key`. What is typed is kept aside while the
/// field has focus and written when it loses it, if it is valid. Unlike
/// `form::config_text`, an empty prefix is written as an empty value, not
/// removed: git-flow takes a missing prefix key for "not set up".
fn config_field(
    ui: &mut Ui,
    ctx: &egui::Context,
    file: &mut ConfigFile,
    state: &mut State,
    config: &Config,
    key: &'static str,
    hint: &str,
) {
    let stored = value(config, key);
    let mut text = state.editing.get(key).cloned().unwrap_or_else(|| stored.to_owned());
    let response = form::text(ui, &mut text, hint);
    let trimmed = text.trim();
    let result = check(key, trimmed, config);
    if response.has_focus() {
        state.editing.insert(key, text.clone());
    } else if response.lost_focus() || state.editing.contains_key(key) {
        match &result {
            Ok(()) => {
                if trimmed != stored {
                    file.set(ctx, key, Some(trimmed));
                }
                state.editing.remove(key);
            }
            Err(_) => {
                state.editing.insert(key, text.clone());
            }
        }
    }
    if let Err(error) = result {
        form::error(ui, &error);
    }
}

/// The page before Gitflow is set up: the names to use, and the button.
fn not_set_up(ui: &mut Ui, tab: &mut RepoTab, snap: &RepoSnapshot, state: &mut State) {
    form::intro(
        ui,
        "Gitflow keeps a production branch and a develop branch, and names feature, release and hotfix \
         branches by a prefix. Initializing saves these names in this repository's Git configuration, \
         where git-flow and other Git tools find them, and creates the develop branch from the \
         production branch if it is missing.",
    );
    let draft = state.draft.get_or_insert_with(|| {
        let names = snap.branches.iter().map(|b| b.name.as_str()).chain(snap.head.branch.as_deref());
        Config::defaults(gitflow::default_master(names))
    });

    let mut valid = true;
    for (heading, fields) in [("Branches", &BRANCHES[..]), ("Prefixes", &PREFIXES[..])] {
        form::heading(ui, heading);
        for (key, label, hint) in fields {
            form::row(ui, label, |ui| {
                let mut text = value(draft, key).to_owned();
                if form::text(ui, &mut text, hint).changed() {
                    *value_mut(draft, key) = text.trim().to_owned();
                }
                if let Err(error) = check(key, value(draft, key), draft) {
                    form::error(ui, &error);
                    valid = false;
                }
                if let Some(help) = help_for(key) {
                    form::help(ui, help);
                }
            });
        }
    }

    let existing = existing(snap, draft);
    let unborn_staged = existing.unborn && !snap.status.staged.is_empty();
    let plan = if unborn_staged {
        Err("This repository has no commit yet, and initializing would make the first one with your \
             staged changes in it. Commit them first."
            .to_owned())
    } else {
        gitflow::init(draft, &existing)
    };
    let busy = state.initializing.is_some() || tab.is_running(gitflow::INIT_LABEL);
    form::indented(ui, |ui| {
        ui.horizontal(|ui| {
            let enabled = valid && plan.is_ok() && !busy;
            let clicked = ui.add_enabled_ui(enabled, |ui| form::button(ui, "Initialize Gitflow", theme::green())).inner.clicked();
            if busy {
                ui.add(egui::Spinner::new().size(13.0).color(theme::accent()));
            }
            if clicked && let Ok(cmd) = &plan {
                state.initializing = Some(Instant::now());
                tab.run(cmd.clone());
            }
        });
        match &plan {
            Err(error) if valid => form::error(ui, error),
            Ok(_) if existing.unborn => form::help(
                ui,
                &format!("This repository has no commit yet: initializing makes an empty first commit on {}.", draft.master),
            ),
            Ok(_) if !existing.develop_local => {
                let from = existing.develop_remote.as_deref().unwrap_or(&draft.master);
                form::help(ui, &format!("Creates {} from {from}.", draft.develop));
            }
            _ => {}
        }
    });
}

/// Which of the branches `config` names exist, locally or on a remote.
fn existing(snap: &RepoSnapshot, config: &Config) -> Existing {
    let local = |name: &str| snap.branches.iter().any(|b| b.name == name);
    // The default remote's copy first, as git-flow uses `origin`.
    let remote = |name: &str| {
        let mut found: Vec<_> = snap.remote_branches.iter().filter(|b| b.name == name).collect();
        found.sort_by_key(|b| Some(b.remote.as_str()) != snap.default_remote());
        found.first().map(|b| b.full.clone())
    };
    Existing {
        unborn: snap.head.oid.is_none(),
        master_local: local(&config.master),
        master_remote: remote(&config.master),
        develop_local: local(&config.develop),
        develop_remote: remote(&config.develop),
    }
}
