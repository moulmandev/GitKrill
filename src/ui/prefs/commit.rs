//! Preferences > Commit (repository-specific): the commit button's default
//! action and the commit message template (`crate::prefs::commit`).

use std::path::Path;

use eframe::egui::{self, Label, Margin, RichText, Sense, TextEdit, Ui, vec2};

use super::{Page, form};
use crate::prefs::commit::{CommitAction, ConfigEnv, OWN_FILE, Template, TemplateFile, is_inside, read_template, save_template};
use crate::state::ToastKind;
use crate::theme;
use crate::ui::commit_button;

const READ: &str = "prefs-commit:read";
const SAVE: &str = "prefs-commit:save";
const DOCS: &str = "https://git-scm.com/docs/git-commit#Documentation/git-commit.txt---templateltfilegt";

/// What this section remembers while preferences are open.
#[derive(Default)]
pub struct State {
    /// The template as read from disk; `None` while it is read.
    file: Option<Loaded>,
    requested: bool,
    saving: bool,
    /// The template being edited.
    summary: String,
    description: String,
}

/// The template, read in the background.
#[derive(Clone)]
struct Loaded {
    file: TemplateFile,
    /// `commit.template` points inside the `.git` folder, so saving writes
    /// that file.
    inside_git: bool,
}

/// Looks where `file` is. Blocks (it resolves paths on disk).
fn locate(common_dir: &Path, file: TemplateFile) -> Loaded {
    let inside_git = file.path.as_deref().is_some_and(|path| is_inside(path, common_dir));
    Loaded { file, inside_git }
}

pub fn show(ui: &mut Ui, page: &mut Page<'_>, state: &mut State) {
    let Some(tab) = page.tab.as_deref_mut() else { return };
    let ctx = page.ctx;

    if let Some(loaded) = tab.take_result::<Loaded>(READ) {
        state.summary = loaded.file.template.summary.clone();
        state.description = loaded.file.template.description.clone();
        commit_button::template_changed(ctx, tab, &loaded.file.template);
        state.file = Some(loaded);
    }
    if let Some(saved) = tab.take_result::<Result<Loaded, String>>(SAVE) {
        state.saving = false;
        match saved {
            Ok(loaded) => {
                commit_button::template_changed(ctx, tab, &loaded.file.template);
                let file = &loaded.file;
                let place = file.path.as_ref().map(|path| path.display().to_string()).unwrap_or_default();
                page.shared.toast(ToastKind::Success, "Commit template saved", place);
                state.file = Some(loaded);
                // `commit.template` may have been set: other pages see it.
                if let Some(repo) = page.repo.as_deref_mut() {
                    repo.reload(ctx);
                }
            }
            Err(e) => page.shared.toast(ToastKind::Error, "Commit template not saved", e),
        }
    }
    if !state.requested {
        state.requested = true;
        let workdir = tab.path.clone();
        tab.spawn_job(READ, move |loader| {
            let file = read_template(&workdir, &ConfigEnv::default());
            locate(loader.repo().commondir(), file)
        });
    }

    let workdir = tab.path.clone();
    let mut prefs = crate::prefs::repo(&page.shared.settings, &workdir).commit.clone();
    let before = prefs.clone();
    let hosting = tab.hosting.connected();
    let pull = hosting.as_ref().map_or("pull request", |shown| shown.repo.service.pull_word());
    let has_remote = tab.snap.as_ref().is_some_and(|snap| !snap.remotes.is_empty());

    form::row(ui, "Default commit action", |ui| {
        let titles: Vec<(CommitAction, String)> =
            CommitAction::ALL.into_iter().map(|action| (action, action.title(pull))).collect();
        let options: Vec<(CommitAction, &str)> = titles.iter().map(|(action, title)| (*action, title.as_str())).collect();
        form::choice(ui, "prefs-commit-action", &mut prefs.action, &options);
        form::help(
            ui,
            "The default action for the commit panel button. Other options are available from the split button menu.",
        );
        match prefs.action {
            CommitAction::CommitAndPush if !has_remote => {
                form::warning(ui, "This repository has no remote, so the button only commits.")
            }
            CommitAction::CommitAndPullRequest if hosting.is_none() => form::warning(
                ui,
                "No GitHub or GitLab remote is connected for this repository, so the button only commits.",
            ),
            _ => {}
        }
    });

    form::row(ui, "Sign off commits", |ui| {
        form::checkbox(ui, &mut prefs.sign_off, "");
        form::help(
            ui,
            "Adds a Signed-off-by line with your name and email to each commit made from the commit panel \
             (git commit --signoff), as projects using a Developer Certificate of Origin ask. The commit \
             options beside the Amend box can leave it out of one commit.",
        );
    });

    form::heading(ui, "Commit Template");
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.label(
            RichText::new(
                "Create and apply a default initial message for commits to keep commit summaries and \
                 descriptions consistent. For more information see the ",
            )
            .size(13.0)
            .color(theme::text()),
        );
        let link = ui.add(
            Label::new(RichText::new("Git commit template documentation").size(13.0).color(theme::accent()))
                .sense(Sense::click()),
        );
        if link.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(DOCS).clicked() {
            crate::ui::hosting::open_link(DOCS);
        }
        ui.label(RichText::new(".").size(13.0).color(theme::text()));
    });
    ui.add_space(14.0);

    let withdrawn = form::row(ui, "Apply this template to commit messages", |ui| {
        let changed = form::checkbox(ui, &mut prefs.apply_template, "");
        form::help(ui, "Start each new commit message in the commit panel from the template.");
        changed && !prefs.apply_template
    });
    form::row(ui, "Remove comments from commit messages", |ui| {
        form::checkbox(ui, &mut prefs.strip_comments, "");
        form::help(
            ui,
            "Lines starting with # are left out of the message when committing, as in git's own editor \
             (git commit --cleanup=strip).",
        );
    });

    match &state.file {
        None => form::indented(ui, form::loading),
        Some(file) => {
            let file = file.clone();
            template_fields(ui, page, state, &file, prefs.strip_comments);
        }
    }

    if prefs != before {
        crate::prefs::repo_mut(&mut page.shared.settings, &workdir).commit = prefs;
        if withdrawn && let Some(tab) = page.tab.as_deref_mut() {
            commit_button::template_withdrawn(ctx, tab);
        }
    }
}

fn template_fields(ui: &mut Ui, page: &mut Page<'_>, state: &mut State, loaded: &Loaded, strip: bool) {
    let file = &loaded.file;
    form::row(ui, "Summary", |ui| {
        let field = ui.add(
            TextEdit::singleline(&mut state.summary)
                .desired_width(form::FIELD_WIDTH)
                .margin(Margin { left: 8, right: 34, top: 5, bottom: 5 }),
        );
        commit_button::summary_counter(ui, &field, &state.summary);
    });
    form::row(ui, "Description", |ui| {
        ui.add(
            TextEdit::multiline(&mut state.description)
                .desired_rows(16)
                .desired_width(ui.available_width())
                .margin(vec2(8.0, 6.0)),
        );
        if !strip && state.description.lines().any(|line| line.starts_with('#')) {
            form::warning(ui, "Lines starting with # stay in the message unless comments are removed.");
        }
    });

    let edited = Template { summary: state.summary.clone(), description: state.description.clone() };
    let changed = Template::parse(&edited.to_text()) != file.template;
    form::indented(ui, |ui| {
        let clicked = ui
            .add_enabled_ui(changed && !state.saving, |ui| form::button(ui, "Save commit template", theme::green()))
            .inner
            .clicked();
        if let Some(problem) = &file.problem {
            form::warning(ui, problem);
        }
        let help = match &file.path {
            Some(path) if loaded.inside_git => format!("Saved in {}.", path.display()),
            Some(path) => format!(
                "Read from {} (commit.template). Saving keeps that file as it is and gives this repository \
                 its own template in .git/{OWN_FILE}.",
                path.display()
            ),
            None => {
                format!("Saving puts the template in .git/{OWN_FILE} and sets commit.template for this repository.")
            }
        };
        form::help(ui, &help);
        if clicked && let Some(tab) = page.tab.as_deref_mut() {
            state.saving = true;
            let workdir = tab.path.clone();
            let current = file.path.clone();
            tab.spawn_job(SAVE, move |loader| {
                let common_dir = loader.repo().commondir();
                save_template(&workdir, common_dir, current.as_deref(), &edited, &ConfigEnv::default())
                    .map(|file| locate(common_dir, file))
            });
        }
    });
}
