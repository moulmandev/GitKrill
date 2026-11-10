//! Preferences > Profiles: the profiles to commit as (author name and
//! email here; each one's SSH key and signing on the SSH and GPG pages),
//! which one is in use, and whether the global .gitconfig follows it.
//!
//! Also here: the quick switcher at the right of the tab strip
//! ([`switcher`]), the author line of the commit panel ([`commit_author`])
//! and what the SSH and GPG pages share.

use std::sync::Arc;

use eframe::egui::{
    self, Align2, Frame, Id, Margin, Painter, Popup, Rect, RichText, Sense, Stroke, StrokeKind, TextEdit, Ui, pos2, vec2,
};

use super::{Page, Section, form};
use crate::prefs::identity::{self, Identity, Profile};
use crate::state::{AppAction, RepoTab, Settings, Shared, ToastKind};
use crate::theme::{self, icons};
use crate::ui::widgets;

/// What this section remembers while preferences are open.
#[derive(Default)]
pub struct State {
    /// The profile being edited or added.
    editing: Option<Editing>,
}

struct Editing {
    /// Which profile; `None` for a new one.
    index: Option<usize>,
    name: String,
    author_name: String,
    author_email: String,
}

enum RowAction {
    Use(usize),
    Edit(usize),
    Delete(usize),
}

pub fn show(ui: &mut Ui, page: &mut Page<'_>, state: &mut State) {
    poll_failure(page.shared);
    form::intro(
        ui,
        "A profile keeps an author name and email, an SSH key and commit signing settings together, so you can switch \
         between them for different work environments. Your first profile is made from your global Git config.",
    );
    if !ensure_profile(ui, page) {
        return;
    }

    card(ui, |ui| {
        ui.label(RichText::new("My Profiles").font(theme::bold(16.0)).color(theme::text_bright()));
        ui.add_space(4.0);
        let identity = &mut page.shared.settings.identity;
        ui.checkbox(
            &mut identity.keep_gitconfig,
            RichText::new("Keep my .gitconfig updated with my profile preferences").size(13.5).color(theme::text()),
        );
        form::help(
            ui,
            "When checked, GitKrill updates your global .gitconfig to match the profile in use, including commit author \
             name and email, and any GPG settings. When unchecked, your .gitconfig is left alone and only GitKrill's own \
             git commands use the profile.",
        );
    });
    ui.add_space(18.0);

    if form::button(ui, &format!("{}  Add a profile", icons::PLUS), theme::green()).clicked() {
        let identity = &page.shared.settings.identity;
        let from = identity.current();
        state.editing = Some(Editing {
            index: None,
            name: identity.free_name("New Profile"),
            author_name: from.map(|p| p.author_name.clone()).unwrap_or_default(),
            author_email: from.map(|p| p.author_email.clone()).unwrap_or_default(),
        });
    }
    ui.add_space(12.0);

    if let Some(action) = table(ui, page.shared, state) {
        let identity = &mut page.shared.settings.identity;
        match action {
            RowAction::Use(index) => {
                identity.current = index;
                // The first click of a double-click opened the form.
                state.editing = None;
            }
            RowAction::Edit(index) => {
                state.editing = match &state.editing {
                    Some(editing) if editing.index == Some(index) => None,
                    _ => identity.profiles.get(index).map(|p| Editing {
                        index: Some(index),
                        name: p.name.clone(),
                        author_name: p.author_name.clone(),
                        author_email: p.author_email.clone(),
                    }),
                }
            }
            RowAction::Delete(index) => {
                let name = identity.profiles.get(index).map(|p| p.name.clone()).unwrap_or_default();
                identity.remove(index);
                state.editing = None;
                page.shared.toast(ToastKind::Success, "Profile deleted", name);
            }
        }
    }

    if state.editing.as_ref().is_some_and(|e| e.index.is_none()) {
        ui.add_space(8.0);
        edit_form(ui, page.shared, state);
    }
}

const ROW: f32 = 56.0;

/// The list of profiles, sorted by name, the one in use outlined. The
/// form of the one being edited opens under its row.
fn table(ui: &mut Ui, shared: &mut Shared, state: &mut State) -> Option<RowAction> {
    let width = ui.available_width();
    let columns = [16.0, width * 0.34, width * 0.62];

    let (rect, _) = ui.allocate_exact_size(vec2(width, 30.0), Sense::hover());
    for (x, title) in columns.iter().zip(["Profile Name", "Author Name", "Author Email"]) {
        widgets::text_left(ui.painter(), rect, rect.left() + x, title, theme::font(13.5), theme::text());
    }
    ui.painter().hline(rect.x_range(), rect.bottom(), Stroke::new(1.0, theme::border()));
    ui.add_space(10.0);

    let identity = &shared.settings.identity;
    let current = identity.current_index();
    let count = identity.profiles.len();
    let mut order: Vec<usize> = (0..count).collect();
    order.sort_by_cached_key(|&i| identity.profiles[i].name.to_lowercase());

    let mut action = None;
    for index in order {
        let profile = &shared.settings.identity.profiles[index];
        let (outer, response) = ui.allocate_exact_size(vec2(width, ROW), Sense::click());
        let rect = outer.shrink2(vec2(0.0, 3.0));
        let painter = ui.painter();
        let used = index == current;
        let fill = if used {
            theme::bg_selected()
        } else if response.hovered() {
            theme::bg_hover()
        } else {
            theme::bg_panel()
        };
        let stroke = if used { Stroke::new(1.5, theme::accent()) } else { Stroke::new(1.0, theme::border()) };
        painter.rect(rect, 4.0, fill, stroke, StrokeKind::Inside);
        if used {
            let badge = rect.left_top() + vec2(1.0, 1.0);
            painter.circle_filled(badge, 8.0, theme::accent());
            painter.text(badge, Align2::CENTER_CENTER, icons::CHECK, theme::bold(10.0), theme::on_accent());
        }
        widgets::avatar(painter, pos2(rect.left() + 30.0, rect.center().y), 13.0, &profile.author_name, &profile.author_email, None);
        let cell = |x: f32, next: f32, text: &str, color| {
            let text = if text.is_empty() { "—" } else { text };
            let galley = widgets::elided(painter, text, theme::font(14.0), color, next - x - 16.0);
            widgets::galley_left(painter, rect, rect.left() + x, galley);
        };
        cell(columns[0] + 38.0, columns[1], &profile.name, theme::text_bright());
        cell(columns[1], columns[2], &profile.author_name, theme::text_dim());
        cell(columns[2], width - 40.0, &profile.author_email, theme::text_dim());

        // ⋮ menu
        let dots = Rect::from_center_size(pos2(rect.right() - 22.0, rect.center().y), vec2(26.0, 26.0));
        let menu = ui.interact(dots, ui.id().with(("profile-menu", index)), Sense::click());
        if menu.hovered() || Popup::is_id_open(ui.ctx(), Popup::default_response_id(&menu)) {
            ui.painter().rect_filled(dots, 4.0, theme::bg_raised());
        }
        ui.painter().text(dots.center(), Align2::CENTER_CENTER, icons::DOTS_THREE_VERTICAL, theme::bold(16.0), theme::text());
        Popup::menu(&menu).show(|ui| {
            if !used && widgets::menu_item(ui, icons::USER_SWITCH, "Use this profile") {
                action = Some(RowAction::Use(index));
            }
            if widgets::menu_item(ui, icons::PENCIL_SIMPLE, "Edit") {
                action = Some(RowAction::Edit(index));
            }
            if count > 1 && widgets::menu_item(ui, icons::TRASH, "Delete") {
                action = Some(RowAction::Delete(index));
            }
        });

        let hint = if used { "In use. Click to edit." } else { "Click to edit, double-click to use this profile" };
        let response = response.on_hover_text(hint);
        if response.double_clicked() && !used {
            action = Some(RowAction::Use(index));
        } else if response.clicked() {
            action = Some(RowAction::Edit(index));
        }

        if state.editing.as_ref().is_some_and(|e| e.index == Some(index)) {
            ui.add_space(4.0);
            edit_form(ui, shared, state);
            ui.add_space(8.0);
        }
    }
    action
}

/// Name, author and email of the profile being edited or added, with Save
/// and Cancel.
fn edit_form(ui: &mut Ui, shared: &mut Shared, state: &mut State) {
    let Some(editing) = &mut state.editing else { return };
    let identity = &mut shared.settings.identity;
    let mut close = false;
    card(ui, |ui| {
        ui.label(
            RichText::new(if editing.index.is_some() { "Edit profile" } else { "New profile" })
                .font(theme::bold(15.0))
                .color(theme::text_bright()),
        );
        ui.add_space(10.0);
        form::row(ui, "Profile Name", |ui| form::text(ui, &mut editing.name, "Work"));
        form::row(ui, "Author Name", |ui| form::text(ui, &mut editing.author_name, "Ada Lovelace"));
        form::row(ui, "Author Email", |ui| form::text(ui, &mut editing.author_email, "ada@example.com"));

        let name = editing.name.trim();
        let problem = if name.is_empty() {
            Some("Give the profile a name.")
        } else if identity.profiles.iter().enumerate().any(|(i, p)| Some(i) != editing.index && p.name.eq_ignore_ascii_case(name)) {
            Some("Another profile has this name.")
        } else if editing.author_name.trim().is_empty() || editing.author_email.trim().is_empty() {
            Some("Commits need an author name and email.")
        } else {
            None
        };
        form::indented(ui, |ui| {
            if let Some(problem) = problem {
                form::error(ui, problem);
            }
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let save = ui.add_enabled_ui(problem.is_none(), |ui| form::button(ui, "Save", theme::green())).inner;
                if save.clicked() && problem.is_none() {
                    let name = name.to_owned();
                    let author_name = editing.author_name.trim().to_owned();
                    let author_email = editing.author_email.trim().to_owned();
                    match editing.index.and_then(|i| identity.profiles.get_mut(i)) {
                        Some(profile) => {
                            profile.name = name;
                            profile.author_name = author_name;
                            profile.author_email = author_email;
                        }
                        None => identity.profiles.push(Profile { name, author_name, author_email, ..Default::default() }),
                    }
                    close = true;
                }
                if form::button(ui, "Cancel", theme::accent()).clicked() {
                    close = true;
                }
                if let Some(index) = editing.index
                    && index != identity.current_index()
                    && form::button(ui, "Use this profile", theme::accent()).clicked()
                {
                    identity.current = index;
                }
            });
        });
    });
    if close {
        state.editing = None;
    }
}

/// A rounded panel around a group of controls.
fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    Frame::new()
        .fill(theme::bg_panel())
        .stroke(Stroke::new(1.0, theme::border()))
        .corner_radius(4.0)
        .inner_margin(Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

// ---- shared with the SSH and GPG pages --------------------------------------

/// Makes "Default Profile" from the global git config the first time
/// Profiles, SSH or GPG opens. False, with a spinner, while that file is
/// being read.
pub(super) fn ensure_profile(ui: &mut Ui, page: &mut Page<'_>) -> bool {
    if !page.shared.settings.identity.profiles.is_empty() {
        return true;
    }
    page.global.load(page.ctx);
    if !page.global.ready() {
        form::loading(ui);
        return false;
    }
    let global = &*page.global;
    let profile = Profile::from_config("Default Profile", |key| global.get(key).map(str::to_owned));
    let identity = &mut page.shared.settings.identity;
    identity.profiles.push(profile);
    identity.current = 0;
    true
}

/// Shows a failed write of the global config as a toast.
pub(super) fn poll_failure(shared: &mut Shared) {
    if let Some(error) = identity::take_failure() {
        shared.toast(ToastKind::Error, "Profile not saved to .gitconfig", error);
    }
}

/// "These settings belong to the profile <name>", and where they go.
pub(super) fn profile_note(ui: &mut Ui, identity: &Identity, gitconfig: bool) {
    let Some(profile) = identity.current() else { return };
    let where_to = if !gitconfig {
        ""
    } else if identity.keep_gitconfig {
        " They are written to your global .gitconfig."
    } else {
        " They are used by GitKrill's own git commands only (\"Keep my .gitconfig updated\" is off in Profiles)."
    };
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        let text = |t: &str| RichText::new(t).size(13.0).color(theme::text());
        ui.label(text("These settings belong to the profile "));
        ui.label(RichText::new(&profile.name).size(13.0).color(theme::text_bright()).strong());
        ui.label(text(&format!(".{where_to}")));
    });
    ui.add_space(14.0);
}

/// A path field with a Browse button whose value is written to `value`
/// only when the field loses focus (or on Enter, or when a file is
/// picked), so typing does not change settings on every key. True when
/// `value` changed.
pub(super) fn draft_path(ui: &mut Ui, salt: &str, value: &mut String, hint: &str, dialog_title: &str) -> bool {
    let id = Id::new(("identity-draft", salt));
    let mut text = ui.data_mut(|d| d.get_temp::<String>(id)).unwrap_or_else(|| value.clone());
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let field = TextEdit::singleline(&mut text).hint_text(hint).desired_width(form::FIELD_WIDTH - 70.0).margin(vec2(8.0, 5.0));
        let mut response = ui.add(field);
        if !value.is_empty() {
            // The field is narrower than most paths.
            response = response.on_hover_text(value.as_str());
        }
        if response.has_focus() {
            ui.data_mut(|d| d.insert_temp(id, text.clone()));
        } else {
            ui.data_mut(|d| d.remove::<String>(id));
            if response.lost_focus() && text.trim() != value.trim() {
                *value = text.trim().to_owned();
                changed = true;
            }
        }
        if form::button(ui, "Browse", theme::accent()).clicked() {
            let mut dialog = rfd::FileDialog::new().set_title(dialog_title);
            if let Some(dir) = std::path::Path::new(value.as_str()).parent()
                && !dir.as_os_str().is_empty()
            {
                dialog = dialog.set_directory(dir);
            }
            if let Some(picked) = dialog.pick_file() {
                *value = picked.display().to_string();
                changed = true;
            }
        }
    });
    changed
}

// ---- outside preferences ----------------------------------------------------

const SWITCHER_FONT: f32 = 13.0;

/// Width the profile switcher needs at the right of the tab strip; 0 while
/// there is only one profile (the switcher is hidden).
pub fn switcher_width(painter: &Painter, settings: &Settings) -> f32 {
    let identity = &settings.identity;
    match identity.current() {
        Some(profile) if identity.profiles.len() > 1 => {
            let text = painter.layout_no_wrap(profile.name.clone(), theme::font(SWITCHER_FONT), theme::text()).size().x;
            (text.min(150.0) + 58.0).round()
        }
        _ => 0.0,
    }
}

/// The "<profile> ▾" menu at the right of the tab strip, left of the
/// preferences gear: every profile, then "Manage profiles…". `rect` is
/// [`switcher_width`] wide; nothing is drawn when that is 0. Also reports a
/// failed write of the global config (a switch made here).
pub fn switcher(ui: &mut Ui, rect: Rect, shared: &mut Shared) {
    poll_failure(shared);
    if rect.width() <= 0.0 {
        return;
    }
    let chip = rect.shrink2(vec2(4.0, 8.0));
    let response = ui.interact(chip, ui.id().with("profile-switcher"), Sense::click());
    let open = Popup::is_id_open(ui.ctx(), Popup::default_response_id(&response));
    let painter = ui.painter();
    if open || response.hovered() {
        painter.rect_filled(chip, 4.0, theme::bg_raised());
    }
    let identity = &shared.settings.identity;
    let Some(profile) = identity.current() else { return };
    let color = if open || response.hovered() { theme::text_bright() } else { theme::text() };
    let x = widgets::text_left(painter, chip, chip.left() + 6.0, icons::USER_CIRCLE, theme::font(16.0), theme::accent());
    let galley = widgets::elided(painter, &profile.name, theme::font(SWITCHER_FONT), color, chip.width() - x - 32.0);
    widgets::galley_left(painter, chip, chip.left() + x + 12.0, galley);
    painter.text(pos2(chip.right() - 10.0, chip.center().y), Align2::CENTER_CENTER, icons::CARET_DOWN, theme::font(11.0), theme::text_dim());
    let tooltip = format!("Profile in use: {} <{}>", profile.author_name, profile.author_email);
    let response = if open { response } else { response.on_hover_text(tooltip) };

    let mut chosen = None;
    let mut manage = false;
    Popup::menu(&response).show(|ui| {
        ui.set_min_width(220.0);
        let current = identity.current_index();
        let font = egui::TextStyle::Button.resolve(ui.style());
        for (index, profile) in identity.profiles.iter().enumerate() {
            // A check before the one in use; the same glyph, invisible,
            // keeps the other names in line with it.
            let mut label = egui::text::LayoutJob::default();
            let check = if index == current { theme::accent() } else { egui::Color32::TRANSPARENT };
            label.append(icons::CHECK, 0.0, egui::TextFormat::simple(font.clone(), check));
            label.append(&format!("  {}", profile.name), 0.0, egui::TextFormat::simple(font.clone(), theme::text()));
            let hover = format!("{} <{}>", profile.author_name, profile.author_email);
            if ui.button(label).on_hover_text(hover).clicked() {
                chosen = Some(index);
            }
        }
        ui.separator();
        if widgets::menu_item(ui, icons::GEAR, "Manage profiles…") {
            manage = true;
        }
    });
    if let Some(index) = chosen {
        switch(shared, index);
    }
    if manage {
        shared.actions.push(AppAction::OpenPreferences(Section::Profiles));
    }
}

/// Makes profile `index` current, saying so in a toast. For the switcher
/// and the command palette.
pub fn switch(shared: &mut Shared, index: usize) {
    let identity = &mut shared.settings.identity;
    if index == identity.current_index() || index >= identity.profiles.len() {
        return;
    }
    identity.current = index;
    let profile = &identity.profiles[index];
    let body = format!("Committing as {} <{}>", profile.author_name, profile.author_email);
    let title = format!("Switched to {}", profile.name);
    shared.toast(ToastKind::Success, title, body);
}

/// Profiles the command palette can switch to ("Switch to profile Work"):
/// every one but the current, and none while there is only one.
pub fn switchable(settings: &Settings) -> Vec<(usize, String)> {
    let identity = &settings.identity;
    let current = identity.current_index();
    if identity.profiles.len() < 2 {
        return Vec::new();
    }
    identity.profiles.iter().enumerate().filter(|(i, _)| *i != current).map(|(i, p)| (i, p.name.clone())).collect()
}

const AUTHOR_JOB: &str = "identity:author";

/// A dim "Name <email>" line under the commit message: who the next commit
/// will be by, as git works it out for this repository (`git var
/// GIT_AUTHOR_IDENT`, asked in the background whenever the profile
/// changes). Only once there is more than one profile.
pub fn commit_author(ui: &mut Ui, tab: &mut RepoTab, settings: &Settings) {
    if settings.identity.profiles.len() < 2 {
        return;
    }
    let id = Id::new(("commit-author", &tab.path));
    let generation = identity::generation();
    if let Some(answer) = tab.take_result::<(u64, Option<String>)>(AUTHOR_JOB) {
        ui.data_mut(|d| d.insert_temp(id, Arc::new(answer)));
    }
    let known: Option<Arc<(u64, Option<String>)>> = ui.data(|d| d.get_temp(id));
    if known.as_ref().is_none_or(|k| k.0 != generation) {
        // Ask once per change: remember the question until the answer is in.
        ui.data_mut(|d| d.insert_temp(id, Arc::new((generation, known.as_ref().and_then(|k| k.1.clone())))));
        tab.spawn_job(AUTHOR_JOB, move |loader| {
            let dir = loader.repo().workdir().map(|d| d.to_path_buf()).unwrap_or_default();
            let ident = crate::git::cli::run(&dir, &["var".to_owned(), "GIT_AUTHOR_IDENT".to_owned()], Default::default()).ok();
            // "Name <email> 1700000000 +0100": drop the date.
            let ident = ident.and_then(|i| i.rfind('>').map(|end| i[..=end].trim().to_owned()));
            (generation, ident)
        });
    }
    let Some(author) = known.and_then(|k| k.1.clone()) else { return };
    let profile = settings.identity.current().map(|p| p.name.as_str()).unwrap_or_default();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(RichText::new(icons::USER_CIRCLE).size(12.5).color(theme::text_faint()));
        let label = egui::Label::new(RichText::new(author).size(11.5).color(theme::text_dim())).truncate();
        ui.add(label).on_hover_text(format!("The next commit's author (profile {profile})"));
    });
}
