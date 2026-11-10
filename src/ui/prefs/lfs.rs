//! Preferences > LFS (repository-specific): set Git LFS up, choose what it
//! tracks, download its objects (`crate::git::lfs`).
//!
//! The repository is looked at once in the background when the page opens,
//! and again after each command the page runs.

use std::time::Instant;

use eframe::egui::{self, Align, Label, Layout, RichText, Sense, TextEdit, Ui, vec2};

use super::{Page, form};
use crate::git::lfs::{self, Status};
use crate::theme;

const STATUS: &str = "prefs-lfs:status";
const COUNT: &str = "prefs-lfs:count";
const SITE: &str = "https://git-lfs.com";
/// Width of the pattern column and of the field to add one.
const PATTERN_WIDTH: f32 = form::FIELD_WIDTH - 70.0;

/// What this section remembers while preferences are open.
#[derive(Default)]
pub struct State {
    /// `None` while the repository is being looked at.
    status: Option<Status>,
    requested: bool,
    /// How many files are in LFS; `None` while counting.
    count: Option<Result<usize, String>>,
    /// The pattern being typed.
    pattern: String,
    /// A command the page started (label, when), to look again once it ends.
    running: Vec<(String, Instant)>,
}

impl State {
    fn reload(&mut self) {
        self.requested = false;
    }
}

pub fn show(ui: &mut Ui, page: &mut Page<'_>, state: &mut State) {
    let Some(tab) = page.tab.as_deref_mut() else { return };

    if let Some(status) = tab.take_result::<Status>(STATUS) {
        // Counting runs git-lfs, which sets itself up in a repository that
        // does not use it yet: only ask once it does.
        if status.initialized {
            let workdir = tab.path.clone();
            tab.spawn_job(COUNT, move |_| lfs::count_files(&workdir));
        }
        state.status = Some(status);
    }
    if let Some(count) = tab.take_result::<Result<usize, String>>(COUNT) {
        state.count = Some(count);
    }
    let before = state.running.len();
    state.running.retain(|(label, since)| tab.take_outcome(label, *since).is_none());
    if state.running.len() != before {
        state.reload();
    }
    if !state.requested {
        state.requested = true;
        let workdir = tab.path.clone();
        tab.spawn_job(STATUS, move |_| lfs::status(&workdir));
    }

    let Some(status) = state.status.clone() else {
        form::loading(ui);
        return;
    };
    let name = tab.name.clone();
    let busy = !state.running.is_empty();
    let mut run = None;

    if let Err(e) = &status.version {
        ui.label(RichText::new("Git LFS is not installed").size(17.0).color(theme::text_bright()));
        ui.add_space(8.0);
        text(ui, "Git LFS keeps large files such as images, videos and builds outside the repository, and downloads them when they are checked out. Install it, then open this page again.");
        ui.add_space(6.0);
        link(ui, "Download Git LFS", SITE);
        ui.add_space(10.0);
        if form::button(ui, "Check again", theme::accent()).clicked() {
            state.reload();
        }
        ui.add_space(6.0);
        ui.add(Label::new(RichText::new(e.trim()).size(11.5).color(theme::text_faint())).wrap());
        return;
    }

    if !status.initialized {
        ui.label(RichText::new("Initialize LFS").size(17.0).color(theme::text_bright()));
        ui.add_space(8.0);
        text(ui, &format!("LFS has not been initialized on {name}"));
        ui.add_space(10.0);
        let clicked = ui
            .add_enabled_ui(!busy, |ui| form::button(ui, "Initialize LFS on this repo", theme::green()))
            .inner
            .clicked();
        if clicked {
            run = Some(lfs::install());
        }
        form::help(
            ui,
            "Sets up LFS's filter and hooks in this repository only (git lfs install --local). Nothing is added to the working tree.",
        );
    } else {
        form::intro(ui, &format!("Files matching a tracked pattern are stored in Git LFS on {name}."));

        form::row(ui, "Tracked patterns", |ui| {
            if status.patterns.is_empty() {
                ui.label(RichText::new("No patterns yet.").size(13.0).color(theme::text_faint()));
            }
            for pattern in &status.patterns {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    // The pattern in the width of the field below, Remove
                    // under Track.
                    let size = vec2(PATTERN_WIDTH, 24.0);
                    ui.allocate_ui_with_layout(size, Layout::left_to_right(Align::Center), |ui| {
                        ui.set_width(PATTERN_WIDTH);
                        let text = RichText::new(&pattern.pattern).font(theme::mono(13.0)).color(theme::text_bright());
                        let label = ui.add(Label::new(text).truncate());
                        if !pattern.folder().is_empty() {
                            label.on_hover_text(format!("In {}", pattern.source));
                        }
                    });
                    let remove = ui.add_enabled_ui(!busy, |ui| form::button(ui, "Remove", theme::red())).inner;
                    if remove.on_hover_text("Stop tracking it (git lfs untrack)").clicked() {
                        run = Some(lfs::untrack(pattern));
                    }
                });
            }
            if let Some(problem) = &status.problem {
                form::error(ui, problem.trim());
            }
        });

        form::row(ui, "Add pattern", |ui| {
            let mut submit = false;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let field = ui.add(
                    TextEdit::singleline(&mut state.pattern)
                        .hint_text("*.psd")
                        .desired_width(PATTERN_WIDTH)
                        .margin(vec2(8.0, 5.0)),
                );
                submit |= field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let ready = !state.pattern.trim().is_empty() && !busy;
                submit |= ui.add_enabled_ui(ready, |ui| form::button(ui, "Track", theme::green())).inner.clicked();
                submit &= ready;
            });
            form::help(
                ui,
                "A file name or a glob, such as *.psd or assets/**. It is added to .gitattributes, which then shows as a change to commit.",
            );
            if submit {
                run = Some(lfs::track(state.pattern.trim()));
                state.pattern.clear();
            }
        });

        form::row(ui, "LFS files", |ui| {
            match &state.count {
                None => form::loading(ui),
                Some(Ok(count)) => {
                    let text = format!("{count} file{} in the checked-out commit", if *count == 1 { "" } else { "s" });
                    ui.label(RichText::new(text).size(13.0).color(theme::text()));
                }
                Some(Err(e)) => form::error(ui, e.trim()),
            }
        });

        form::row(ui, "LFS objects", |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.add_enabled_ui(!busy, |ui| {
                    if form::button(ui, "Fetch LFS objects", theme::accent()).clicked() {
                        run = Some(lfs::fetch());
                    }
                    if form::button(ui, "Pull LFS objects", theme::accent()).clicked() {
                        run = Some(lfs::pull());
                    }
                });
            });
            form::help(
                ui,
                "Fetch downloads the LFS files of the checked-out commit from the remote; Pull also puts them in the working tree.",
            );
        });

        if let Ok(version) = &status.version {
            ui.add_space(6.0);
            form::indented(ui, |ui| {
                ui.label(RichText::new(version).size(11.5).color(theme::text_faint()));
            });
        }
    }

    if let Some(cmd) = run {
        state.running.push((cmd.label.clone(), Instant::now()));
        tab.run(cmd);
    }
}

fn text(ui: &mut Ui, text: &str) {
    ui.scope(|ui| {
        ui.set_max_width(form::HELP_WIDTH + 140.0);
        ui.add(Label::new(RichText::new(text).size(13.0).color(theme::text())).wrap());
    });
}

/// A link to a fixed address, the size of the text around it.
fn link(ui: &mut Ui, text: &str, url: &'static str) {
    let response = ui.add(Label::new(RichText::new(text).size(13.0).color(theme::accent())).sense(Sense::click()));
    if response.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(url).clicked() {
        crate::ui::hosting::open_link(url);
    }
}
