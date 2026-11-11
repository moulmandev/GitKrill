//! How diffs are shown: inline or side by side, the hunks or the whole file,
//! with or without whitespace changes. The choices are kept in Preferences >
//! Editor (`prefs::editor::Editor`); the diff view's title bar and the
//! command palette switch them, and every diff view follows.

use eframe::egui::{self, Align2, CornerRadius, CursorIcon, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use crate::prefs::editor::{DiffContext, DiffLayout, Editor};
use crate::state::Settings;
use crate::theme::{self, icons};
use crate::ui::palette::{Entry, Env, Kind};

/// Height of the controls, to sit in the diff's title bar.
const CONTROL_H: f32 = 24.0;

/// The controls of the title bar, laid out right to left: whitespace, then
/// layout, then context. `compact` leaves the labels out (their tooltips
/// say what they are).
pub fn controls(ui: &mut Ui, editor: &mut Editor, compact: bool) {
    let mut ignore = editor.ignore_whitespace;
    let tip = if ignore {
        "Whitespace changes are hidden (git diff -w). Click to show them."
    } else {
        "Hide changes to whitespace alone (git diff -w)"
    };
    if toggle(ui, icons::PARAGRAPH, &mut ignore, tip) {
        editor.ignore_whitespace = ignore;
    }
    ui.add_space(2.0);
    segmented(
        ui,
        "diff-layout",
        &mut editor.diff_layout,
        &[
            (DiffLayout::Inline, icons::ROWS, "Inline", "Removed and added lines one under the other"),
            (DiffLayout::Split, icons::COLUMNS, "Split", "The old version on the left, the new one on the right"),
        ],
        compact,
    );
    segmented(
        ui,
        "diff-context",
        &mut editor.diff_context,
        &[
            (DiffContext::Hunks, icons::LIST_DASHES, "Hunks", "Only the changes, with three lines around each"),
            (DiffContext::File, icons::FILE_TEXT, "File", "The whole file, with the changes in place"),
        ],
        compact,
    );
}

/// A row of segments, one per choice: `(value, icon, label, tooltip)`.
fn segmented<T: Copy + PartialEq>(ui: &mut Ui, id: &str, value: &mut T, choices: &[(T, &str, &str, &str)], compact: bool) {
    let font = theme::font(12.0);
    let text = |icon: &str, label: &str| if compact { icon.to_owned() } else { format!("{icon} {label}") };
    let widths: Vec<f32> = choices
        .iter()
        .map(|(_, icon, label, _)| {
            let size = ui.painter().layout_no_wrap(text(icon, label), font.clone(), theme::text()).size().x;
            size + if compact { 14.0 } else { 18.0 }
        })
        .collect();
    let (rect, _) = ui.allocate_exact_size(vec2(widths.iter().sum(), CONTROL_H), Sense::hover());
    ui.painter().rect_stroke(rect, 3.0, Stroke::new(1.0, theme::border()), StrokeKind::Inside);
    let mut x = rect.left();
    let count = choices.len();
    for (i, ((choice, icon, label, tip), width)) in choices.iter().zip(widths).enumerate() {
        let part = Rect::from_min_size(pos2(x, rect.top()), vec2(width, rect.height()));
        x += width;
        let response = ui
            .interact(part, ui.id().with((id, i)), Sense::click())
            .on_hover_cursor(CursorIcon::PointingHand)
            .on_hover_text(if compact { format!("{label}: {tip}") } else { (*tip).to_owned() });
        let active = *value == *choice;
        let fill = if active {
            theme::bg_selected()
        } else if response.hovered() {
            theme::bg_raised()
        } else {
            egui::Color32::TRANSPARENT
        };
        let corner = |on: bool| if on { 3 } else { 0 };
        let radius = CornerRadius {
            nw: corner(i == 0),
            sw: corner(i == 0),
            ne: corner(i + 1 == count),
            se: corner(i + 1 == count),
        };
        ui.painter().rect_filled(part.shrink(1.0), radius, fill);
        let color = if active { theme::text_bright() } else { theme::text_dim() };
        ui.painter().text(part.center(), Align2::CENTER_CENTER, text(icon, label), font.clone(), color);
        if response.clicked() {
            *value = *choice;
        }
    }
}

/// An icon that stays pressed while `on`; true when it was clicked.
fn toggle(ui: &mut Ui, icon: &str, on: &mut bool, tip: &str) -> bool {
    let (rect, response) = ui.allocate_exact_size(vec2(CONTROL_H + 4.0, CONTROL_H), Sense::click());
    let response = response.on_hover_cursor(CursorIcon::PointingHand).on_hover_text(tip);
    let fill = if *on {
        theme::bg_selected()
    } else if response.hovered() {
        theme::bg_raised()
    } else {
        egui::Color32::TRANSPARENT
    };
    ui.painter().rect(rect, 3.0, fill, Stroke::new(1.0, theme::border()), StrokeKind::Inside);
    let color = if *on { theme::accent() } else { theme::text_dim() };
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, icon, theme::font(14.0), color);
    if response.clicked() {
        *on = !*on;
    }
    response.clicked()
}

/// Command palette entries that switch each choice.
pub fn palette_entries(list: &mut Vec<Entry>, settings: &Settings) {
    let editor = &settings.editor;
    let (icon, label) = match editor.diff_layout {
        DiffLayout::Inline => (icons::COLUMNS, "Show diffs side by side"),
        DiffLayout::Split => (icons::ROWS, "Show diffs inline"),
    };
    list.push(
        Entry::new(Kind::Action, "diff-layout", icon, label, |env: &mut Env<'_>| {
            let editor = &mut env.shared.settings.editor;
            editor.diff_layout = match editor.diff_layout {
                DiffLayout::Inline => DiffLayout::Split,
                DiffLayout::Split => DiffLayout::Inline,
            };
        })
        .keywords("diff split side by side two columns inline unified layout view"),
    );
    let (icon, label) = match editor.diff_context {
        DiffContext::Hunks => (icons::FILE_TEXT, "Show whole files in diffs"),
        DiffContext::File => (icons::LIST_DASHES, "Show only the changed hunks in diffs"),
    };
    list.push(
        Entry::new(Kind::Action, "diff-context", icon, label, |env: &mut Env<'_>| {
            let editor = &mut env.shared.settings.editor;
            editor.diff_context = match editor.diff_context {
                DiffContext::Hunks => DiffContext::File,
                DiffContext::File => DiffContext::Hunks,
            };
        })
        .keywords("diff whole full file context hunks expand collapse"),
    );
    let label = if editor.ignore_whitespace { "Show whitespace changes in diffs" } else { "Ignore whitespace changes in diffs" };
    list.push(
        Entry::new(Kind::Action, "diff-whitespace", icons::PARAGRAPH, label, |env: &mut Env<'_>| {
            let editor = &mut env.shared.settings.editor;
            editor.ignore_whitespace = !editor.ignore_whitespace;
        })
        .keywords("diff whitespace spaces indentation tabs -w ignore hide"),
    );
}
