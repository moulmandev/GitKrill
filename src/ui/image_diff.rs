//! Preview of a changed image file: the two versions side by side, a swipe
//! between them, or one faded over the other (onion skin). An added or
//! deleted image is shown on its own.
//!
//! Pixels arrive decoded from the reader thread (`git::picture`); this only
//! uploads them as textures, keeps the last few, and lays them out.

use eframe::egui::{
    self, Align2, Color32, ColorImage, Context, CornerRadius, Id, Mesh, Pos2, Rect, Sense, Shape, Stroke, StrokeKind,
    TextureHandle, TextureOptions, Ui, Vec2, pos2, vec2,
};

use crate::git::picture::{ImageDiff, Picture, Side};
use crate::theme::{self, icons};

/// Textures kept: the two sides of the image shown, plus the previous pair
/// so stepping back and forth through files does not upload again.
const KEEP: usize = 4;
/// Room around the pictures.
const PAD: f32 = 16.0;
const TOOLBAR_H: f32 = 36.0;
/// Small images are enlarged by whole steps until about this size (points),
/// with sharp pixels, so a 16 px icon is not a speck.
const SMALL: f32 = 128.0;
/// Size of a transparency checkerboard square, in points.
const CHECKER: f32 = 8.0;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Mode {
    #[default]
    SideBySide,
    Swipe,
    Onion,
}

/// The comparison chosen, kept for the session: it is a habit, not a
/// property of one file.
#[derive(Clone, Copy)]
struct Choice {
    mode: Mode,
    /// Swipe: where the divider is, 0 (all after) to 1 (all before).
    split: f32,
    /// Onion skin: how much of the new version shows over the old.
    blend: f32,
}

impl Default for Choice {
    fn default() -> Self {
        Choice { mode: Mode::SideBySide, split: 0.5, blend: 0.5 }
    }
}

pub fn show(ui: &mut Ui, images: &ImageDiff) {
    let choice_id = Id::new("image-diff-choice");
    let mut choice: Choice = ui.data(|d| d.get_temp(choice_id)).unwrap_or_default();

    let both = matches!((&images.old, &images.new), (Some(old), Some(new)) if old.picture.is_ok() && new.picture.is_ok());
    let area = ui.available_rect_before_wrap();
    ui.allocate_rect(area, Sense::hover());
    ui.painter().rect_filled(area, 0.0, theme::bg_app());

    // Toolbar: the comparison modes, when there are two pictures to compare.
    let mut body = area;
    if both {
        let bar = Rect::from_min_size(area.min, vec2(area.width(), TOOLBAR_H));
        body.min.y = bar.bottom();
        ui.painter().hline(bar.x_range(), bar.bottom() - 0.5, Stroke::new(1.0, theme::border()));
        let modes = [
            (Mode::SideBySide, icons::COLUMNS, "Side by side"),
            (Mode::Swipe, icons::ARROWS_HORIZONTAL, "Swipe"),
            (Mode::Onion, icons::STACK, "Onion skin"),
        ];
        let left = modes_toggle(ui, pos2(bar.left() + 12.0, bar.center().y), &modes, &mut choice.mode);
        if choice.mode == Mode::Onion {
            let slider = Rect::from_min_size(pos2(left + 18.0, bar.center().y - 11.0), vec2(260.0, 22.0));
            ui.scope_builder(egui::UiBuilder::new().max_rect(slider), |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(egui::RichText::new("Before").color(theme::red()).size(12.0));
                    ui.spacing_mut().slider_width = 140.0;
                    ui.add(egui::Slider::new(&mut choice.blend, 0.0..=1.0).show_value(false));
                    ui.label(egui::RichText::new("After").color(theme::green()).size(12.0));
                });
            });
        }
    }

    let body = body.shrink(PAD);
    match (&images.old, &images.new) {
        (Some(old), Some(new)) if both => {
            let (Ok(before), Ok(after)) = (&old.picture, &new.picture) else { unreachable!() };
            match choice.mode {
                Mode::SideBySide => {
                    let half = (body.width() - PAD) / 2.0;
                    let left = Rect::from_min_size(body.min, vec2(half, body.height()));
                    let right = Rect::from_min_max(pos2(left.right() + PAD, body.top()), body.max);
                    column(ui, left, old, "Before", theme::red(), None);
                    column(ui, right, new, "After", theme::green(), Some(old.bytes));
                }
                Mode::Swipe => overlaid(ui, body, (old, before), (new, after), &mut choice, true),
                Mode::Onion => overlaid(ui, body, (old, before), (new, after), &mut choice, false),
            }
        }
        // One of the two could not be decoded: show what there is.
        (Some(old), Some(new)) => {
            let half = (body.width() - PAD) / 2.0;
            let left = Rect::from_min_size(body.min, vec2(half, body.height()));
            let right = Rect::from_min_max(pos2(left.right() + PAD, body.top()), body.max);
            column(ui, left, old, "Before", theme::red(), None);
            column(ui, right, new, "After", theme::green(), Some(old.bytes));
        }
        (None, Some(new)) => column(ui, body, new, "Added", theme::green(), None),
        (Some(old), None) => column(ui, body, old, "Deleted", theme::red(), None),
        (None, None) => {}
    }

    ui.data_mut(|d| d.insert_temp(choice_id, choice));
}

/// One version with its caption above it, fitted into `rect`.
fn column(ui: &mut Ui, rect: Rect, side: &Side, label: &str, color: Color32, compared_to: Option<u64>) {
    let caption_h = caption(ui, rect.min, side, label, color, compared_to, rect.width());
    let room = Rect::from_min_max(pos2(rect.left(), rect.top() + caption_h + 10.0), rect.max);
    match &side.picture {
        Ok(picture) => {
            let size = fitted(ui.ctx(), picture, room.size());
            let place = Rect::from_center_size(pos2(room.center().x, room.top() + size.y / 2.0), size);
            draw(ui, place, picture, Color32::WHITE);
            ui.painter().rect_stroke(place.expand(1.0), 0.0, Stroke::new(1.0, theme::with_alpha(color, 110)), StrokeKind::Outside);
            hover_zoom(ui, place, picture);
        }
        Err(reason) => {
            let box_rect = Rect::from_min_size(room.min, vec2(room.width(), room.height().min(160.0)));
            ui.painter().rect_stroke(box_rect, 4.0, Stroke::new(1.0, theme::border_light()), StrokeKind::Inside);
            ui.painter().text(box_rect.center(), Align2::CENTER_CENTER, reason, theme::font(13.0), theme::text_faint());
        }
    }
}

/// "Before  PNG · 512 × 512 · 12.4 KB", returning its height.
fn caption(ui: &Ui, at: Pos2, side: &Side, label: &str, color: Color32, compared_to: Option<u64>, width: f32) -> f32 {
    let painter = ui.painter();
    let dot = 8.0;
    let line_h = 18.0;
    painter.circle_filled(pos2(at.x + dot / 2.0, at.y + line_h / 2.0), dot / 2.0, color);
    let galley = painter.layout_no_wrap(label.to_owned(), theme::bold(13.0), theme::text_bright());
    let label_w = galley.size().x;
    painter.galley(pos2(at.x + dot + 6.0, at.y + (line_h - galley.size().y) / 2.0), galley, theme::text_bright());

    let mut details = match &side.picture {
        Ok(picture) => format!("{} · {} × {} · {}", picture.format, picture.width, picture.height, file_size(side.bytes)),
        Err(_) => file_size(side.bytes),
    };
    if let Some(before) = compared_to
        && before != side.bytes
    {
        let change = side.bytes as i64 - before as i64;
        let sign = if change > 0 { "+" } else { "−" };
        details.push_str(&format!(" ({sign}{})", file_size(change.unsigned_abs())));
    }
    let x = at.x + dot + 6.0 + label_w + 10.0;
    let room = (at.x + width - x).max(0.0);
    let details = crate::ui::widgets::elided(painter, &details, theme::font(12.5), theme::text_dim(), room);
    painter.galley(pos2(x, at.y + (line_h - details.size().y) / 2.0), details, theme::text_dim());
    line_h
}

/// The two versions on top of each other in one box: a divider to drag
/// (`swipe`), or the new one faded over the old.
fn overlaid(
    ui: &mut Ui,
    body: Rect,
    (old, before): (&Side, &Picture),
    (new, after): (&Side, &Picture),
    choice: &mut Choice,
    swipe: bool,
) {
    // Captions on either side of one line.
    let half = (body.width() - PAD) / 2.0;
    let caption_h = caption(ui, body.min, old, "Before", theme::red(), None, half);
    caption(ui, pos2(body.left() + half + PAD, body.top()), new, "After", theme::green(), Some(old.bytes), half);
    let room = Rect::from_min_max(pos2(body.left(), body.top() + caption_h + 10.0), body.max);

    // One scale for both, so equal pixels line up; each centred in the box.
    let ctx = ui.ctx().clone();
    let natural = |p: &Picture| vec2(p.width as f32, p.height as f32) / ctx.pixels_per_point();
    let (a, b) = (natural(before), natural(after));
    let union = a.max(b);
    let scale = scale_for(union, room.size());
    let frame = Rect::from_center_size(pos2(room.center().x, room.top() + union.y * scale / 2.0), union * scale);
    let place = |size: Vec2| Rect::from_center_size(frame.center(), size * scale);
    let (old_rect, new_rect) = (place(a), place(b));

    let response = ui.interact(frame, Id::new("image-diff-overlay"), Sense::click_and_drag());
    if swipe {
        if (response.dragged() || response.clicked())
            && let Some(pointer) = response.interact_pointer_pos()
        {
            choice.split = ((pointer.x - frame.left()) / frame.width()).clamp(0.0, 1.0);
        }
        let x = frame.left() + frame.width() * choice.split;
        let clip = ui.clip_rect();
        let left_clip = Rect::from_min_max(frame.min, pos2(x, frame.bottom())).intersect(clip);
        let right_clip = Rect::from_min_max(pos2(x, frame.top()), frame.max).intersect(clip);
        draw_clipped(ui, old_rect, before, Color32::WHITE, left_clip);
        draw_clipped(ui, new_rect, after, Color32::WHITE, right_clip);
        ui.painter().rect_stroke(frame.expand(1.0), 0.0, Stroke::new(1.0, theme::border_light()), StrokeKind::Outside);

        // The divider and its handle.
        let painter = ui.painter();
        painter.vline(x, frame.y_range(), Stroke::new(2.0, theme::accent()));
        let knob = Pos2::new(x, frame.center().y);
        painter.circle_filled(knob, 11.0, theme::accent());
        painter.text(knob, Align2::CENTER_CENTER, icons::ARROWS_HORIZONTAL, theme::font(13.0), theme::on_accent());
        if response.hovered() || response.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }
    } else {
        draw(ui, old_rect, before, Color32::WHITE);
        let alpha = (choice.blend * 255.0).round() as u8;
        draw_texture(ui, new_rect, after, Color32::from_white_alpha(alpha), ui.clip_rect());
        ui.painter().rect_stroke(frame.expand(1.0), 0.0, Stroke::new(1.0, theme::border_light()), StrokeKind::Outside);
    }
}

/// Size of `picture` drawn in `room`: shrunk to fit, enlarged in whole steps
/// when it is tiny, 1:1 with the screen's pixels otherwise.
fn fitted(ctx: &Context, picture: &Picture, room: Vec2) -> Vec2 {
    let natural = vec2(picture.width as f32, picture.height as f32) / ctx.pixels_per_point();
    natural * scale_for(natural, room)
}

fn scale_for(natural: Vec2, room: Vec2) -> f32 {
    let longest = natural.x.max(natural.y).max(1.0);
    let enlarge = if longest < SMALL / 2.0 { (SMALL / longest).floor().clamp(1.0, 8.0) } else { 1.0 };
    let fit = (room.x / natural.x.max(1.0)).min(room.y / natural.y.max(1.0)).max(0.01);
    enlarge.min(fit)
}

/// A checkerboard where the picture is transparent, then the picture.
fn draw(ui: &Ui, rect: Rect, picture: &Picture, tint: Color32) {
    draw_clipped(ui, rect, picture, tint, ui.clip_rect());
}

fn draw_clipped(ui: &Ui, rect: Rect, picture: &Picture, tint: Color32, clip: Rect) {
    checkerboard(ui, rect, clip);
    draw_texture(ui, rect, picture, tint, clip);
}

fn draw_texture(ui: &Ui, rect: Rect, picture: &Picture, tint: Color32, clip: Rect) {
    let texture = texture(ui.ctx(), picture);
    let uv = Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0));
    ui.painter().with_clip_rect(clip).image(texture.id(), rect, uv, tint);
}

/// Grey squares behind transparent pixels, in the theme's tones: a white
/// and light grey texture, tinted, repeated across `rect`.
fn checkerboard(ui: &Ui, rect: Rect, clip: Rect) {
    let id = Id::new("image-diff-checker");
    let texture = ui.data(|d| d.get_temp::<TextureHandle>(id)).unwrap_or_else(|| {
        let (light, dark) = (Color32::from_gray(255), Color32::from_gray(218));
        let image = ColorImage::new([2, 2], vec![light, dark, dark, light]);
        let options = TextureOptions { wrap_mode: egui::TextureWrapMode::Repeat, ..TextureOptions::NEAREST };
        let handle = ui.ctx().load_texture("image-diff-checker", image, options);
        ui.data_mut(|d| d.insert_temp(id, handle.clone()));
        handle
    });
    let tiles = rect.size() / (CHECKER * 2.0);
    let mut mesh = Mesh::with_texture(texture.id());
    mesh.add_rect_with_uv(rect, Rect::from_min_size(Pos2::ZERO, tiles), checker_tint());
    ui.painter().with_clip_rect(clip).add(Shape::mesh(mesh));
}

/// What the checkerboard's white squares become: a raised grey on dark,
/// the page itself on light (its grey squares are a shade darker).
fn checker_tint() -> Color32 {
    if theme::palette().dark { theme::bg_raised() } else { theme::bg_app() }
}

/// The texture for `picture`, uploaded the first time it is drawn. The last
/// few are kept; older ones are dropped, which frees them on the GPU.
fn texture(ctx: &Context, picture: &Picture) -> TextureHandle {
    let id = Id::new("image-diff-textures");
    let mut kept: Vec<(u64, TextureHandle)> = ctx.data(|d| d.get_temp(id)).unwrap_or_default();
    if let Some(index) = kept.iter().position(|(key, _)| *key == picture.id) {
        let entry = kept.remove(index);
        let handle = entry.1.clone();
        kept.push(entry);
        ctx.data_mut(|d| d.insert_temp(id, kept));
        return handle;
    }
    let image = ColorImage::from_rgba_unmultiplied(picture.rgba_size, &picture.rgba);
    // Sharp pixels when enlarged, smooth when shrunk.
    let options = TextureOptions {
        magnification: egui::TextureFilter::Nearest,
        minification: egui::TextureFilter::Linear,
        ..TextureOptions::LINEAR
    };
    let handle = ctx.load_texture(format!("image-diff-{}", picture.id), image, options);
    kept.push((picture.id, handle.clone()));
    if kept.len() > KEEP {
        drop(kept.remove(0));
    }
    ctx.data_mut(|d| d.insert_temp(id, kept));
    handle
}

/// Hovering a shrunk picture shows the pixel under the pointer's position and
/// the zoom it is drawn at.
fn hover_zoom(ui: &Ui, place: Rect, picture: &Picture) {
    let response = ui.interact(place, Id::new(("image-diff-hover", picture.id)), Sense::hover());
    let natural = picture.width as f32 / ui.ctx().pixels_per_point();
    let zoom = (place.width() / natural * 100.0).round();
    if zoom == 100.0 {
        return;
    }
    response.on_hover_text(format!("Shown at {zoom}%"));
}

/// A row of segments like the Path / Tree switch; returns its right edge.
fn modes_toggle(ui: &mut Ui, left_center: Pos2, modes: &[(Mode, &str, &str)], value: &mut Mode) -> f32 {
    let font = theme::font(12.0);
    let mut x = left_center.x;
    let count = modes.len();
    for (i, (mode, icon, label)) in modes.iter().enumerate() {
        let text = format!("{icon} {label}");
        let width = ui.painter().layout_no_wrap(text.clone(), font.clone(), theme::text()).size().x + 18.0;
        let rect = Rect::from_min_size(pos2(x, left_center.y - 12.0), vec2(width, 24.0));
        let response = ui.interact(rect, Id::new(("image-diff-mode", i)), Sense::click());
        let active = value == mode;
        let fill = if active {
            theme::bg_selected()
        } else if response.hovered() {
            theme::bg_raised()
        } else {
            theme::bg_header()
        };
        let radius = CornerRadius {
            nw: if i == 0 { 3 } else { 0 },
            sw: if i == 0 { 3 } else { 0 },
            ne: if i + 1 == count { 3 } else { 0 },
            se: if i + 1 == count { 3 } else { 0 },
        };
        ui.painter().rect_filled(rect, radius, fill);
        let color = if active { theme::text_bright() } else { theme::text_dim() };
        ui.painter().text(rect.center(), Align2::CENTER_CENTER, text, font.clone(), color);
        if response.clicked() {
            *value = *mode;
        }
        x += width;
    }
    x
}

fn file_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} bytes")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiny_images_are_enlarged_in_whole_steps_and_big_ones_fit() {
        let room = vec2(800.0, 600.0);
        // A 16-point icon: eight times, capped.
        assert_eq!(scale_for(vec2(16.0, 16.0), room), 8.0);
        // 40 points: three times (120 points).
        assert_eq!(scale_for(vec2(40.0, 20.0), room), 3.0);
        // Ordinary sizes stay 1:1.
        assert_eq!(scale_for(vec2(300.0, 200.0), room), 1.0);
        // Too big: shrunk to the room's limiting side.
        assert_eq!(scale_for(vec2(1600.0, 600.0), room), 0.5);
        // Enlarging never overflows a small room.
        assert_eq!(scale_for(vec2(16.0, 16.0), vec2(64.0, 64.0)), 4.0);
    }

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(file_size(812), "812 bytes");
        assert_eq!(file_size(12_700), "12.4 KB");
        assert_eq!(file_size(3 * 1024 * 1024), "3.0 MB");
    }
}
