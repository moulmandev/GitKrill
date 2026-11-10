//! "Show ghost branch/tag when hovering over or selecting a commit": faded
//! "branch" and "tag" labels in the BRANCH / TAG column of the hovered and
//! the selected row, after the labels already there. Clicking one opens the
//! dialog that creates a branch or tag at that commit.
//!
//! Only one or two rows have them in a frame, so laying them out costs
//! nothing worth counting.

use std::sync::Arc;

use eframe::egui::{Color32, Galley, Painter, Rect, Shape, Stroke, pos2, vec2};

use super::{Columns, MORE_W, PILL_GAP, PILL_PAD, Pills};
use crate::git::{Oid, short};
use crate::state::Dialog;
use crate::theme::{self, icons};

/// One offer, placed.
pub struct Ghost {
    /// A tag rather than a branch.
    pub tag: bool,
    pub rect: Rect,
    text: Arc<Galley>,
}

/// Places the offers in the room left after the row's labels: both with
/// their words when they fit, else both as bare icons, else the branch
/// alone, else none.
pub fn layout(painter: &Painter, rect: Rect, cols: &Columns, pills: &Pills) -> [Option<Ghost>; 2] {
    let x = match pills.shown.last() {
        Some(last) => last.rect.right() + PILL_GAP + if pills.hidden > 0 { MORE_W } else { 0.0 },
        None => cols.refs.min + 8.0,
    };
    let limit = cols.refs.max - 10.0;
    let top = rect.center().y - 10.0;
    let text = |tag: bool, words: bool| {
        let icon = if tag { icons::TAG } else { icons::GIT_BRANCH };
        let text = match (words, tag) {
            (true, true) => format!("{icon} tag"),
            (true, false) => format!("{icon} branch"),
            (false, _) => icon.to_owned(),
        };
        // Coloured when painted: it depends on the pointer.
        painter.layout_no_wrap(text, theme::font(12.0), Color32::PLACEHOLDER)
    };
    let ghost = |tag: bool, x: f32, text: Arc<Galley>| {
        let rect = Rect::from_min_size(pos2(x, top), vec2(text.size().x + PILL_PAD * 2.0, 20.0));
        Ghost { tag, rect, text }
    };

    for words in [true, false] {
        let branch = ghost(false, x, text(false, words));
        let tag = ghost(true, branch.rect.right() + PILL_GAP, text(true, words));
        if tag.rect.right() <= limit {
            return [Some(branch), Some(tag)];
        }
    }
    let branch = ghost(false, x, text(false, false));
    if branch.rect.right() <= limit { [Some(branch), None] } else { [None, None] }
}

/// Paints the offers: a dashed outline in the row's lane colour, filled in
/// while the pointer is on one.
pub fn paint(painter: &Painter, rect: Rect, cols: &Columns, ghosts: &[Option<Ghost>; 2], hovered: [bool; 2], color: Color32) {
    let clip = Rect::from_x_y_ranges(cols.refs, rect.y_range()).intersect(painter.clip_rect());
    let painter = painter.with_clip_rect(clip);
    for (ghost, hovered) in ghosts.iter().zip(hovered) {
        let Some(ghost) = ghost else { continue };
        let text = if hovered {
            painter.rect(
                ghost.rect,
                3.0,
                theme::mix(theme::bg_app(), color, 0.22),
                Stroke::new(1.0, theme::mix(theme::bg_app(), color, 0.8)),
                eframe::egui::StrokeKind::Inside,
            );
            theme::text_bright()
        } else {
            let r = ghost.rect.shrink(0.5);
            let outline = [r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom(), r.left_top()];
            let stroke = Stroke::new(1.0, theme::mix(theme::bg_app(), color, 0.6));
            painter.extend(Shape::dashed_line(&outline, stroke, 3.0, 2.0));
            theme::text_dim()
        };
        let pos = pos2(ghost.rect.left() + PILL_PAD, ghost.rect.center().y - ghost.text.size().y / 2.0);
        painter.galley_with_override_text_color(pos.round(), ghost.text.clone(), text);
    }
}

/// The dialog that creates what `tag` says at `oid`, as the row menu's
/// "Create branch here" and "Create tag here" open it.
pub fn dialog(oid: Oid, tag: bool) -> Dialog {
    let (at, at_label) = (oid.to_string(), short(oid));
    if tag {
        Dialog::CreateTag { at, at_label, name: String::new(), message: String::new() }
    } else {
        Dialog::CreateBranch { at: Some(at), at_label, name: String::new(), checkout: true }
    }
}
