//! Building blocks for preferences pages, so every page looks the same: a
//! label column on the left (right-aligned, wrapping), controls on the
//! right, dim help text and warnings under a control.
//!
//! ```ignore
//! form::row(ui, "Auto-Prune", |ui| {
//!     form::checkbox(ui, &mut prefs.prune, "");
//!     form::help(ui, "Remove remote branches that no longer exist when fetching.");
//! });
//! form::row(ui, "Longpaths", |ui| {
//!     form::config_bool(ui, ctx, page.global, "core.longpaths", false, "");
//!     form::help(ui, "Sets core.longpaths in your global Git config.");
//! });
//! ```

use std::path::PathBuf;

use eframe::egui::{
    self, Align, Color32, ComboBox, DragValue, Id, Label, Layout, RichText, Sense, TextEdit, Ui, vec2,
};

use crate::prefs::gitconfig::ConfigFile;
use crate::theme::{self, icons};
use crate::ui::widgets;

/// Width of the label column.
pub const LABEL_WIDTH: f32 = 290.0;
/// Width of text fields and drop-downs.
pub const FIELD_WIDTH: f32 = 300.0;
/// Help text wraps at this width.
pub const HELP_WIDTH: f32 = 520.0;
/// Between the label column and the controls.
const COLUMN_GAP: f32 = 24.0;

/// The page title ("General").
pub fn title(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).font(theme::bold(22.0)).color(theme::text_bright()));
    ui.add_space(14.0);
}

/// A paragraph under the title, before the rows (what the page is for).
pub fn intro(ui: &mut Ui, text: &str) {
    ui.add(Label::new(RichText::new(text).size(13.0).color(theme::text())).wrap());
    ui.add_space(14.0);
}

/// A group heading inside a page ("Commit Template", "Prefixes").
pub fn heading(ui: &mut Ui, text: &str) {
    ui.add_space(10.0);
    ui.label(RichText::new(text).font(theme::bold(16.0)).color(theme::text_bright()));
    ui.add_space(8.0);
}

/// One labelled row. `add` lays out the control column, top to bottom:
/// the control first, then any [`help`] or [`warning`] under it.
pub fn row<R>(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    let inner = ui
        .horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.allocate_ui_with_layout(vec2(LABEL_WIDTH, 0.0), Layout::top_down(Align::Max), |ui| {
                ui.set_width(LABEL_WIDTH);
                // Level with the middle of a text field or drop-down.
                ui.add_space(4.0);
                ui.add(Label::new(RichText::new(label).size(13.5).color(theme::text())).wrap());
            });
            ui.add_space(COLUMN_GAP);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing = vec2(8.0, 4.0);
                ui.set_max_width((ui.available_width()).min(HELP_WIDTH + 140.0));
                add(ui)
            })
            .inner
        })
        .inner;
    ui.add_space(14.0);
    inner
}

/// A control column without a label, under the previous row (a second
/// button, a list).
pub fn indented<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    row(ui, "", add)
}

/// Dim text under a control saying what it does.
pub fn help(ui: &mut Ui, text: &str) {
    ui.scope(|ui| {
        ui.set_max_width(HELP_WIDTH);
        ui.add(Label::new(RichText::new(text).size(12.0).color(theme::text_dim())).wrap());
    });
}

/// A warning under a control ("may slow down large repositories").
pub fn warning(ui: &mut Ui, text: &str) {
    note(ui, icons::WARNING, theme::orange(), text);
}

/// An error under a control ("gpg was not found").
pub fn error(ui: &mut Ui, text: &str) {
    note(ui, icons::WARNING_CIRCLE, theme::red(), text);
}

fn note(ui: &mut Ui, icon: &str, color: Color32, text: &str) {
    ui.scope(|ui| {
        ui.set_max_width(HELP_WIDTH);
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.label(RichText::new(icon).size(13.0).color(color));
            ui.add(Label::new(RichText::new(text).size(12.0).color(theme::text_dim())).wrap());
        });
    });
}

/// A checkbox, with `text` beside it (may be empty). True when it changed.
pub fn checkbox(ui: &mut Ui, value: &mut bool, text: &str) -> bool {
    ui.checkbox(value, RichText::new(text).size(12.5).color(theme::text_dim())).changed()
}

/// A one-line text field of the standard width.
pub fn text(ui: &mut Ui, value: &mut String, hint: &str) -> egui::Response {
    ui.add(TextEdit::singleline(value).hint_text(hint).desired_width(FIELD_WIDTH).margin(vec2(8.0, 5.0)))
}

/// A whole number between `min` and `max`. True when it changed.
pub fn number(ui: &mut Ui, value: &mut u32, min: u32, max: u32) -> bool {
    ui.add_sized(vec2(FIELD_WIDTH, 26.0), DragValue::new(value).range(min..=max).speed(0.2)).changed()
}

/// A drop-down of `options` (value, label). True when the choice changed.
pub fn choice<T: PartialEq + Clone>(ui: &mut Ui, id: impl std::hash::Hash + std::fmt::Debug, value: &mut T, options: &[(T, &str)]) -> bool {
    let current = options.iter().find(|(v, _)| v == value).map(|(_, label)| *label).unwrap_or("");
    let mut changed = false;
    ComboBox::from_id_salt(Id::new(id)).selected_text(current).width(FIELD_WIDTH).show_ui(ui, |ui| {
        for (option, label) in options {
            if ui.selectable_label(option == value, *label).clicked() && option != value {
                *value = option.clone();
                changed = true;
            }
        }
    });
    changed
}

/// A button tinted with `color`: [`theme::accent`] for Browse,
/// [`theme::green`] for Generate / Initialize, [`theme::red`] for
/// Disconnect / Remove.
pub fn button(ui: &mut Ui, text: &str, color: Color32) -> egui::Response {
    widgets::tint_button(ui, text, color)
}

/// What a Browse button asks for.
pub enum Pick {
    File,
    #[allow(dead_code)] // Every page that asks for a folder has its own picker so far.
    Folder,
}

/// A path field with a Browse button. True when the path changed (typed
/// or picked).
pub fn path(ui: &mut Ui, value: &mut String, hint: &str, pick: Pick, dialog_title: &str) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let field = TextEdit::singleline(value).hint_text(hint).desired_width(FIELD_WIDTH - 70.0).margin(vec2(8.0, 5.0));
        changed |= ui.add(field).changed();
        if button(ui, "Browse", theme::accent()).clicked() {
            let mut dialog = rfd::FileDialog::new().set_title(dialog_title);
            let start = PathBuf::from(value.as_str());
            if let Some(dir) = if start.is_dir() { Some(start.as_path()) } else { start.parent() }
                && dir.is_dir()
            {
                dialog = dialog.set_directory(dir);
            }
            let picked = match pick {
                Pick::File => dialog.pick_file(),
                Pick::Folder => dialog.pick_folder(),
            };
            if let Some(picked) = picked {
                *value = picked.display().to_string();
                changed = true;
            }
        }
    });
    changed
}

/// "Loading…" while a configuration file is being read.
pub fn loading(ui: &mut Ui) {
    ui.horizontal(|ui| {
        ui.add(egui::Spinner::new().size(13.0).color(theme::accent()));
        ui.label(RichText::new("Loading…").color(theme::text_dim()));
    });
}

/// A checkbox bound to a git boolean in `file` (`default` when unset).
/// Writes on change. True when it changed.
pub fn config_bool(
    ui: &mut Ui,
    ctx: &egui::Context,
    file: &mut ConfigFile,
    key: &str,
    default: bool,
    text: &str,
) -> bool {
    let mut value = file.get_bool(key).unwrap_or(default);
    let changed = checkbox(ui, &mut value, text);
    if changed {
        file.set_bool(ctx, key, value);
    }
    changed
}

/// A text field bound to a git value in `file`. What is typed is kept
/// aside while the field has focus and written when it loses it (or on
/// Enter), so typing does not run git for every key; an empty field
/// removes the value. True on the frame it was written.
pub fn config_text(ui: &mut Ui, ctx: &egui::Context, file: &mut ConfigFile, key: &str, hint: &str) -> bool {
    let id = Id::new(("prefs-config-text", key, format!("{:?}", file.scope)));
    let stored = file.get(key).unwrap_or("").to_owned();
    let mut value = ui.data_mut(|d| d.get_temp::<String>(id)).unwrap_or_else(|| stored.clone());
    let response = text(ui, &mut value, hint);
    // Not `response.has_focus()`: that is false while the window itself is
    // in the background, which would drop what was typed.
    if ui.memory(|m| m.has_focus(response.id)) {
        ui.data_mut(|d| d.insert_temp(id, value));
        return false;
    }
    ui.data_mut(|d| d.remove::<String>(id));
    if response.lost_focus() && value.trim() != stored {
        let value = value.trim();
        file.set(ctx, key, (!value.is_empty()).then_some(value));
        return true;
    }
    false
}

/// A small link that opens `url` in the browser. Only for fixed addresses
/// (documentation), never ones read from a file or a server.
pub fn link(ui: &mut Ui, text: &str, url: &'static str) {
    let response = ui.add(Label::new(RichText::new(text).size(12.0).color(theme::accent())).sense(Sense::click()));
    if response.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(url).clicked() {
        crate::ui::hosting::open_link(url);
    }
}
