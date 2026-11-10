//! Preferences > Commit Graph: which columns the graph shows, what hovering
//! does, and how much history it loads. Everything is a setting of
//! GitKrill's own (`settings.graph`, plus the older `show_author`,
//! `show_date` and `show_sha`), so the page reads nothing from disk; open
//! graphs follow each change on their next frame.

use eframe::egui::Ui;

use super::{Page, form};
use crate::prefs::graph::{Description, MAX_INITIAL, MIN_INITIAL};

/// What this section remembers while preferences are open.
#[derive(Default)]
pub struct State {}

pub fn show(ui: &mut Ui, page: &mut Page<'_>, state: &mut State) {
    let _ = state;
    let settings = &mut page.shared.settings;

    form::intro(ui, "What the commit graph shows and how much history it loads. Changes apply to open repositories at once.");

    form::heading(ui, "Columns");
    form::row(ui, "Show branches and tags in graph", |ui| {
        form::checkbox(ui, &mut settings.graph.show_refs, "");
    });
    form::row(ui, "Show commit tree in graph", |ui| {
        form::checkbox(ui, &mut settings.graph.show_tree, "");
    });
    form::row(ui, "Show commit message in graph", |ui| {
        form::checkbox(ui, &mut settings.graph.show_message, "");
    });
    form::row(ui, "Show commit description in graph", |ui| {
        let message = settings.graph.show_message;
        ui.add_enabled_ui(message, |ui| {
            form::choice(ui, "prefs-graph-description", &mut settings.graph.description, &Description::CHOICES);
        });
        if message {
            form::help(ui, "The rest of the commit message, dimmed, after the summary on the same line.");
        } else {
            form::help(ui, "Shown after the commit message, which is hidden.");
        }
    });
    form::row(ui, "Show commit author in graph", |ui| {
        form::checkbox(ui, &mut settings.show_author, "");
    });
    form::row(ui, "Show commit date/time in graph", |ui| {
        form::checkbox(ui, &mut settings.show_date, "");
    });
    form::row(ui, "Show commit sha in graph", |ui| {
        form::checkbox(ui, &mut settings.show_sha, "");
    });

    form::heading(ui, "Hovering");
    form::row(ui, "Show ghost branch/tag when hovering over or selecting a commit", |ui| {
        form::checkbox(ui, &mut settings.graph.ghost_refs, "");
        form::help(ui, "Faded branch and tag labels on the commit under the pointer and on the selected one. Click one to create a branch or tag there.");
        if !settings.graph.show_refs {
            form::help(ui, "They appear in the branch and tag column, which is hidden.");
        }
    });
    form::row(ui, "Highlight associated rows when hovering over a branch", |ui| {
        form::checkbox(ui, &mut settings.graph.highlight_branch, "");
        form::help(ui, "While the pointer is on a branch, in the graph or in the left panel, the commits that are not on it are dimmed.");
    });

    form::heading(ui, "Loading");
    form::row(ui, "Show all commits in graph", |ui| {
        form::checkbox(ui, &mut settings.graph.show_all, "");
        form::warning(ui, "May be slow on large repositories: the whole history is read again whenever a branch or tag moves.");
    });
    form::row(ui, "Initial commits in graph", |ui| {
        let all = settings.graph.show_all;
        ui.add_enabled_ui(!all, |ui| {
            form::number(ui, &mut settings.graph.initial_commits, MIN_INITIAL, MAX_INITIAL);
        });
        if all {
            form::help(ui, "Every commit is loaded: see Show all commits in graph.");
        } else {
            form::help(ui, &format!("Minimum {MIN_INITIAL}. With lazy loading on, more commits are loaded when you scroll to the oldest one in the graph."));
            form::warning(ui, "Higher numbers make opening and refreshing large repositories slower.");
        }
    });
    form::row(ui, "Lazy load commits in graph", |ui| {
        ui.add_enabled_ui(!settings.graph.show_all, |ui| {
            form::checkbox(ui, &mut settings.graph.lazy_load, "");
        });
        form::help(ui, "Load the next commits when you scroll to the end of the graph. When off, a Load more commits button is there instead.");
    });
}
