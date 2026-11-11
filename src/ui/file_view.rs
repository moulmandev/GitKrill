//! A file as it is in a commit (`DiffSource::At`), shown on its own: one
//! column of line numbers and the code, syntax-coloured, wrapped, selected
//! and copied as in the diff view, with nothing compared. The diff view
//! hands it over from its body; the lines come as one gap hunk
//! (`git::versions`), so the diff view's pieces paint them.

use std::sync::Arc;

use eframe::egui::{Pos2, Rect, ScrollArea, Sense, Stroke, Ui, pos2, vec2};

use crate::git::{DiffTarget, FileDiff};
use crate::theme;
use crate::ui::diff::{
    DiffText, Metrics, WRAP_MARGIN, column_at, fingerprint, first_rows, paint_code, paint_number, paint_truncated,
    text_selection, wrap_rows,
};
use crate::ui::highlight;
use crate::ui::textsel::{Lines, Pos};

pub fn show(ui: &mut Ui, target: &DiffTarget, diff: &Arc<FileDiff>) {
    let Some(hunk) = diff.hunks.first() else { return };
    let m = Metrics::new(ui);
    // Colours arrive from a background thread; until then lines are plain.
    let style = highlight::style_for(ui.ctx(), diff, &target.path, m.look.highlight);
    let styled = style.hunk(0);
    let gutter = if m.look.numbers { m.num_w + 6.0 } else { 10.0 };
    let text = DiffText::new(diff);
    let count = hunk.lines.len();

    let area = ui.available_rect_before_wrap();
    // Wrapped, the code fits the view and only scrolls up and down.
    let columns = m.columns(area.width() - gutter - WRAP_MARGIN);
    let rows = columns.map(|columns| {
        let lines = || hunk.lines.iter().map(|line| line.text.as_str());
        wrap_rows(ui, ui.id().with("file-view-wrap"), diff, columns, || first_rows(lines(), columns))
    });
    // The first row of line `i` (of the end, for `count`), and the line a
    // row belongs to.
    let row_of = |i: usize| rows.as_ref().map_or(i, |rows| rows[i] as usize);
    let line_of = |row: usize| match &rows {
        None => row.min(count.saturating_sub(1)),
        Some(rows) => rows[..count].partition_point(|first| *first as usize <= row).saturating_sub(1),
    };
    let lines_h = row_of(count) as f32 * m.line_h;
    let height = lines_h + if diff.truncated { m.hunk_h } else { 0.0 };
    let text_w = if columns.is_some() { 0.0 } else { gutter + diff.max_chars as f32 * m.char_w + 40.0 };

    let selection = text_selection(ui, "file-view-text-selection");
    let mut selection = selection.lock().unwrap_or_else(|e| e.into_inner());
    selection.follow(diff, ui.ctx().cumulative_frame_nr(), || fingerprint(target, diff));

    let scroll = if columns.is_some() { ScrollArea::vertical() } else { ScrollArea::both() };
    // Each file opens at its top.
    scroll.id_salt(("file-view", target.source, &target.path)).auto_shrink([false, false]).show_viewport(
        ui,
        |ui, viewport| {
            let size = vec2(text_w.max(ui.available_width()), height.max(ui.available_height()));
            let (content, _) = ui.allocate_exact_size(size, Sense::hover());
            let painter = ui.painter().clone();
            // The numbers stay put while the code scrolls sideways.
            let left = content.left() + viewport.min.x;
            let right = content.left() + viewport.max.x;
            let text_left = content.left() + gutter;

            let text_rect = Rect::from_min_max(
                pos2(left + gutter, content.top() + viewport.min.y),
                pos2(right, content.top() + viewport.max.y),
            );
            let hit = |pos: Pos2| {
                let y = pos.y - content.top();
                if y < 0.0 {
                    return Pos::new(0, 0);
                }
                if y >= lines_h {
                    let last = count - 1;
                    return Pos::new(last, text.shown(last).chars().count());
                }
                let row = (y / m.line_h) as usize;
                let line = line_of(row);
                Pos::new(line, column_at(&painter, &m, text.shown(line), row - row_of(line), pos.x, text_left, columns))
            };
            let response = selection.interact(ui, ui.id().with("text"), text_rect, area, &text, hit);
            response.context_menu(|ui| selection.menu(ui, &text));

            let text_clip = Rect::from_min_max(pos2(left + gutter, painter.clip_rect().top()), painter.clip_rect().max);
            let text_painter = painter.with_clip_rect(text_clip.intersect(painter.clip_rect()));
            let first = line_of((viewport.min.y / m.line_h).floor().max(0.0) as usize);
            let last = (line_of((viewport.max.y / m.line_h).ceil().max(0.0) as usize) + 1).min(count);
            for (i, line) in hunk.lines.iter().enumerate().take(last).skip(first) {
                let y = content.top() + row_of(i) as f32 * m.line_h;
                if m.look.numbers {
                    paint_number(&painter, &m, line.new_no, left + m.num_w - 6.0, y + m.line_h / 2.0, false);
                }
                paint_code(&text_painter, &m, line, styled.as_ref(), i, text_left, y, (left, right), columns, &selection, i);
            }
            if diff.truncated {
                paint_truncated(&painter, &m, (left, right), content.top() + height);
            }
            // Separator between the line numbers and the code.
            if m.look.numbers {
                painter.vline(
                    left + m.num_w,
                    painter.clip_rect().y_range(),
                    Stroke::new(1.0, theme::with_alpha(theme::border_light(), 90)),
                );
            }
        },
    );
}
