//! Small painting helpers and custom widgets shared across the UI.

use std::sync::Arc;

use eframe::egui::{
    self, Align2, Color32, CursorIcon, FontId, Galley, Painter, Pos2, Rect, Response, Sense,
    Stroke, StrokeKind, Ui, Vec2, pos2, vec2,
};
use eframe::egui::text::{LayoutJob, TextFormat, TextWrapping};

use crate::git::ChangeKind;
use crate::theme::{self, icons};

/// Single-line text cut off with an ellipsis at `max_width`.
pub fn elided(painter: &Painter, text: &str, font: FontId, color: Color32, max_width: f32) -> Arc<Galley> {
    let mut job = LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = TextWrapping { max_width: max_width.max(0.0), max_rows: 1, break_anywhere: true, ..Default::default() };
    painter.layout_job(job)
}

/// Two differently coloured runs on one elided line (`primary` then `secondary`).
pub fn elided_pair(
    painter: &Painter,
    primary: (&str, FontId, Color32),
    secondary: (&str, FontId, Color32),
    max_width: f32,
) -> Arc<Galley> {
    let mut job = LayoutJob::default();
    job.append(primary.0, 0.0, TextFormat::simple(primary.1, primary.2));
    if !secondary.0.is_empty() {
        job.append(secondary.0, 8.0, TextFormat::simple(secondary.1, secondary.2));
    }
    job.wrap = TextWrapping { max_width: max_width.max(0.0), max_rows: 1, break_anywhere: true, ..Default::default() };
    painter.layout_job(job)
}

/// Paints a galley vertically centred at `x` within `rect`.
pub fn galley_left(painter: &Painter, rect: Rect, x: f32, galley: Arc<Galley>) -> f32 {
    let width = galley.size().x;
    let y = rect.center().y - galley.size().y / 2.0;
    painter.galley(pos2(x, y.round()), galley, theme::text());
    width
}

pub fn text_left(painter: &Painter, rect: Rect, x: f32, text: &str, font: FontId, color: Color32) -> f32 {
    painter.text(pos2(x, rect.center().y), Align2::LEFT_CENTER, text, font, color).width()
}

/// A rotating arc, for "this is running". The caller keeps frames coming.
pub fn spinner(painter: &Painter, center: Pos2, radius: f32, color: Color32) {
    let time = painter.ctx().input(|i| i.time) as f32;
    let start = time * 5.0;
    let points: Vec<Pos2> = (0..=20)
        .map(|k| {
            let angle = start + k as f32 / 20.0 * std::f32::consts::TAU * 0.72;
            center + radius * vec2(angle.cos(), angle.sin())
        })
        .collect();
    painter.circle_stroke(center, radius, Stroke::new(2.0, theme::with_alpha(color, 50)));
    painter.add(egui::Shape::line(points, Stroke::new(2.0, color)));
}

/// A frameless square icon button.
pub fn icon_button(ui: &mut Ui, icon: &str, size: f32, tooltip: &str) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size + 8.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let color = if !ui.is_enabled() {
            theme::text_faint()
        } else if response.hovered() {
            theme::text_bright()
        } else {
            theme::text_dim()
        };
        if response.is_pointer_button_down_on() && ui.is_enabled() {
            ui.painter().rect_filled(rect, 4.0, theme::bg_pressed());
        } else if response.hovered() && ui.is_enabled() {
            ui.painter().rect_filled(rect, 4.0, theme::bg_raised());
        }
        ui.painter().text(rect.center(), Align2::CENTER_CENTER, icon, theme::font(size), color);
    }
    if tooltip.is_empty() { response } else { response.on_hover_text(tooltip) }
}

pub fn tint_button_size(ui: &Ui, text: &str) -> Vec2 {
    ui.painter().layout_no_wrap(text.to_owned(), theme::font(12.0), theme::text_bright()).size()
        + vec2(16.0, 7.0)
}

/// A compact outlined button tinted with `color` ("Stage all changes").
pub fn tint_button(ui: &mut Ui, text: &str, color: Color32) -> Response {
    let size = tint_button_size(ui, text);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    paint_tint_button(ui, rect, &response, text, color);
    response.on_hover_cursor(CursorIcon::PointingHand)
}

/// Same as [`tint_button`] but placed at an explicit rectangle, for buttons
/// that float over custom-painted rows.
pub fn tint_button_at(ui: &mut Ui, rect: Rect, text: &str, color: Color32) -> Response {
    let response = ui.interact(rect, ui.id().with((text, rect.min.y as i32, rect.min.x as i32)), Sense::click());
    paint_tint_button(ui, rect, &response, text, color);
    response.on_hover_cursor(CursorIcon::PointingHand)
}

fn paint_tint_button(ui: &Ui, rect: Rect, response: &Response, text: &str, color: Color32) {
    let galley = ui.painter().layout_no_wrap(text.to_owned(), theme::font(12.0), theme::text_bright());
    {
        let strength = if response.is_pointer_button_down_on() {
            0.45
        } else if response.hovered() {
            0.32
        } else {
            0.18
        };
        ui.painter().rect(
            rect,
            3.0,
            theme::mix(theme::bg_app(), color, strength),
            Stroke::new(1.0, theme::mix(theme::bg_app(), color, 0.75)),
            StrokeKind::Inside,
        );
        let pos = rect.center() - galley.size() / 2.0;
        ui.painter().galley(pos2(pos.x.round(), pos.y.round()), galley, theme::text_bright());
    }
}

/// Background for a list row. A selected row gets a solid fill plus a bright
/// bar on its leading edge, so it reads as selected at a glance.
pub fn row_background(painter: &Painter, rect: Rect, selected: bool, hovered: bool) {
    if selected {
        painter.rect_filled(rect, 0.0, theme::bg_selected());
        painter.rect_filled(
            Rect::from_min_max(rect.min, pos2(rect.left() + 3.0, rect.bottom())),
            0.0,
            theme::accent(),
        );
    } else if hovered {
        painter.rect_filled(rect, 0.0, theme::bg_hover());
    }
}

pub fn initials(name: &str) -> String {
    let mut out = String::new();
    for word in name.split(|c: char| c.is_whitespace() || c == '.' || c == '_' || c == '-') {
        if let Some(ch) = word.chars().find(|c| c.is_alphanumeric()) {
            out.extend(ch.to_uppercase());
            if out.chars().count() == 2 {
                break;
            }
        }
    }
    if out.is_empty() { "?".to_owned() } else { out }
}

/// A stable, muted colour per author.
pub fn author_color(key: &str) -> Color32 {
    let mut hash: u32 = 2166136261;
    for b in key.bytes() {
        hash = (hash ^ b.to_ascii_lowercase() as u32).wrapping_mul(16777619);
    }
    let hue = (hash % 360) as f32 / 360.0;
    egui::ecolor::Hsva::new(hue, 0.5, 0.5, 1.0).into()
}

/// Round avatar with the author's initials, optionally ringed in a lane colour.
pub fn avatar(painter: &Painter, center: Pos2, radius: f32, name: &str, email: &str, ring: Option<Color32>) {
    let key = if email.is_empty() { name } else { email };
    painter.circle_filled(center, radius, author_color(key));
    painter.text(
        center,
        Align2::CENTER_CENTER,
        initials(name),
        theme::bold((radius * 0.95).max(8.0)),
        Color32::WHITE,
    );
    if let Some(color) = ring {
        painter.circle_stroke(center, radius, Stroke::new(2.0, color));
    }
}

/// Round avatar showing the author's picture, or their initials while there
/// is none. Get `picture` from `shared.avatars.get(email, enabled)`, which
/// also starts loading it; do that only for avatars that are on screen.
pub fn avatar_picture(
    painter: &Painter,
    picture: Option<egui::TextureId>,
    center: Pos2,
    radius: f32,
    name: &str,
    email: &str,
    ring: Option<Color32>,
) {
    let Some(texture) = picture else {
        return avatar(painter, center, radius, name, email, ring);
    };
    // Pictures with transparent parts are meant to sit on a light page.
    // Slightly smaller, so it never shows as a rim around a dark picture.
    painter.circle_filled(center, radius - 0.75, theme::text_bright());
    let whole = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
    painter.add(
        egui::epaint::RectShape::filled(
            Rect::from_center_size(center, Vec2::splat(radius * 2.0)),
            egui::CornerRadius::same(radius.ceil().min(255.0) as u8),
            Color32::WHITE,
        )
        .with_texture(texture, whole),
    );
    if let Some(color) = ring {
        painter.circle_stroke(center, radius, Stroke::new(2.0, color));
    }
}

pub fn kind_icon(kind: ChangeKind) -> (&'static str, Color32) {
    match kind {
        ChangeKind::Added | ChangeKind::Untracked => (icons::PLUS, theme::green()),
        ChangeKind::Modified | ChangeKind::Typechange => (icons::PENCIL_SIMPLE, theme::yellow()),
        ChangeKind::Deleted => (icons::MINUS, theme::red()),
        ChangeKind::Renamed => (icons::ARROW_RIGHT, theme::blue()),
        ChangeKind::Conflicted => (icons::WARNING, theme::orange()),
    }
}

pub fn kind_name(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "added",
        ChangeKind::Untracked => "untracked",
        ChangeKind::Modified => "modified",
        ChangeKind::Typechange => "type changed",
        ChangeKind::Deleted => "deleted",
        ChangeKind::Renamed => "renamed",
        ChangeKind::Conflicted => "conflicted",
    }
}

/// A menu entry with a leading icon. Returns true when chosen.
pub fn menu_item(ui: &mut Ui, icon: &str, text: &str) -> bool {
    ui.button(format!("{icon}  {text}")).clicked()
}

/// A two-way segmented toggle (Path / Tree). `value` is true for the second.
pub fn segmented(ui: &mut Ui, value: &mut bool, first: (&str, &str), second: (&str, &str)) {
    let font = theme::font(12.0);
    let measure = |ui: &Ui, (icon, label): (&str, &str)| {
        ui.painter()
            .layout_no_wrap(format!("{icon} {label}"), font.clone(), theme::text())
            .size()
            .x
            + 18.0
    };
    let (w1, w2) = (measure(ui, first), measure(ui, second));
    let (rect, _) = ui.allocate_exact_size(vec2(w1 + w2, 24.0), Sense::hover());
    let halves = [
        (Rect::from_min_size(rect.min, vec2(w1, rect.height())), first, false),
        (Rect::from_min_size(rect.min + vec2(w1, 0.0), vec2(w2, rect.height())), second, true),
    ];
    for (half, (icon, label), is_second) in halves {
        let response = ui.interact(half, ui.id().with((label, "segment")), Sense::click());
        let active = *value == is_second;
        let fill = if active {
            theme::bg_selected()
        } else if response.hovered() {
            theme::bg_raised()
        } else {
            theme::bg_header()
        };
        let radius = if is_second {
            egui::CornerRadius { nw: 0, sw: 0, ne: 3, se: 3 }
        } else {
            egui::CornerRadius { nw: 3, sw: 3, ne: 0, se: 0 }
        };
        ui.painter().rect_filled(half, radius, fill);
        ui.painter().text(
            half.center(),
            Align2::CENTER_CENTER,
            format!("{icon} {label}"),
            font.clone(),
            if active { theme::text_bright() } else { theme::text_dim() },
        );
        if response.clicked() {
            *value = is_second;
        }
    }
}

/// The title bar shared by full-size overlays: an icon, a title, a dimmer
/// subtitle and a close button. Returns true when close was clicked.
///
/// Draw extra controls into the bar by laying them out in the returned rect
/// (the free space between the title and the close button).
pub fn overlay_bar(ui: &mut Ui, icon: &str, title: &str, subtitle: &str) -> (bool, Rect) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 38.0), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, theme::bg_header());
    ui.painter().hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::border()));

    let close = Rect::from_center_size(pos2(rect.right() - 20.0, rect.center().y), Vec2::splat(24.0));
    let response = ui
        .interact(close, ui.id().with(("overlay-close", title)), Sense::click())
        .on_hover_text("Close (Esc)");
    if response.hovered() {
        ui.painter().rect_filled(close, 4.0, theme::bg_raised());
    }
    ui.painter().text(close.center(), Align2::CENTER_CENTER, icons::X, theme::font(14.0), theme::text_dim());

    let painter = ui.painter().with_clip_rect(rect);
    let mut x = rect.left() + 12.0;
    x += text_left(&painter, rect, x, icon, theme::font(15.0), theme::accent()) + 8.0;
    x += text_left(&painter, rect, x, title, theme::bold(13.5), theme::text_bright()) + 12.0;
    x += text_left(&painter, rect, x, subtitle, theme::font(12.0), theme::text_dim()) + 12.0;

    let free = Rect::from_min_max(pos2(x.min(close.left()), rect.top()), pos2(close.left() - 8.0, rect.bottom()));
    (response.clicked(), free)
}

/// Centred placeholder text for empty or loading regions.
pub fn placeholder(ui: &mut Ui, text: &str) {
    let rect = ui.available_rect_before_wrap();
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, text, theme::font(14.0), theme::text_faint());
    ui.allocate_rect(rect, Sense::hover());
}
