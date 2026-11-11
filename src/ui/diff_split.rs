//! The side-by-side layout of a diff: the old version on the left, the new
//! one on the right, unchanged lines level with each other.
//!
//! A removed line is only on the left and an added line only on the right;
//! where a block of removals is replaced by a block of additions, lines that
//! are versions of each other (the ones word emphasis compares) share a row,
//! and the rest of the shorter side is filled with empty space. The rows are
//! worked out once per loaded diff ([`rows`], kept by [`cached`]) and only
//! those in view are painted.
//!
//! Each side is a text of its own for selecting and copying: a selection
//! stays within the side it was started in.

use std::any::Any;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Weak};

use eframe::egui::{self, CursorIcon, PointerButton, Pos2, Rect, ScrollArea, Sense, Stroke, Ui, pos2, vec2};

use super::diff::{
    Anchoring, DiffText, Geometry, LineRef, View, WRAP_MARGIN, anchor_number, changed, column_at, fingerprint,
    line_background, paint_code, paint_hunk_bar, paint_number, paint_pick, paint_sign, paint_truncated,
    text_selection, wrap_rows,
};
use crate::git::{FileDiff, text};
use crate::state::{RepoTab, Shared};
use crate::theme;
use crate::ui::highlight;
use crate::ui::textsel::{Lines, Pos};

/// Comparing every removed line of a block with every added one is
/// quadratic, and the rows are worked out on the UI thread: past this many
/// comparisons in one diff, the remaining blocks pair line for line.
const PAIRING_BUDGET: usize = 4_096;

/// The two sides, as indices into per-side arrays.
pub const LEFT: usize = 0;
pub const RIGHT: usize = 1;

/// One row: a line of the old version on the left, one of the new version on
/// the right, or a line on one side and nothing on the other. The numbers
/// are indices into the hunk's lines; a context line is on both sides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row(pub [Option<u32>; 2]);

/// The rows of a diff.
#[derive(Debug, Default)]
pub struct Rows {
    /// The rows of every hunk, one hunk after the other.
    pub rows: Vec<Row>,
    /// Where each hunk's rows start in `rows`, then the total.
    pub starts: Vec<usize>,
    /// Each side's lines in order as `(hunk, line)`: the text of that side.
    pub lines: [Vec<(u32, u32)>; 2],
    /// For every row and side, the index in `lines` of the row's line on
    /// that side, or of the next one there is when the row has none.
    pub at: Vec<[u32; 2]>,
}

impl Rows {
    /// The rows of hunk `h`.
    fn of(&self, h: usize) -> &[Row] {
        &self.rows[self.starts[h]..self.starts[h + 1]]
    }
}

/// Lays the hunks of `diff` out side by side.
pub fn rows(diff: &FileDiff) -> Rows {
    let mut out = Rows::default();
    let mut budget = PAIRING_BUDGET;
    for hunk in &diff.hunks {
        out.starts.push(out.rows.len());
        let lines = &hunk.lines;
        let mut i = 0;
        while i < lines.len() {
            if lines[i].origin == ' ' {
                out.rows.push(Row([Some(i as u32); 2]));
                i += 1;
                continue;
            }
            let removed = lines[i..].iter().take_while(|l| l.origin == '-').count();
            let added = lines[i + removed..].iter().take_while(|l| l.origin == '+').count();
            let (old, new) = (&lines[i..i + removed], &lines[i + removed..i + removed + added]);
            // Equal blocks pair line for line, as word emphasis does.
            let cells = removed * added;
            let pairs = if removed == added || cells == 0 || cells > budget {
                Vec::new()
            } else {
                budget -= cells;
                highlight::similar_pairs(old, new).unwrap_or_default()
            };
            block(&mut out.rows, i, removed, added, &pairs);
            i += removed + added;
        }
    }
    out.starts.push(out.rows.len());

    out.at.reserve(out.rows.len());
    for h in 0..diff.hunks.len() {
        for k in out.starts[h]..out.starts[h + 1] {
            let row = out.rows[k];
            out.at.push([out.lines[LEFT].len() as u32, out.lines[RIGHT].len() as u32]);
            for side in [LEFT, RIGHT] {
                if let Some(line) = row.0[side] {
                    out.lines[side].push((h as u32, line));
                }
            }
        }
    }
    out
}

/// The rows of a block of `removed` lines followed by `added` ones, from
/// line `start` of the hunk. `pairs` (old, new) share a row; the lines
/// between two pairs fill rows side by side, the longer side's rest next to
/// nothing.
fn block(rows: &mut Vec<Row>, start: usize, removed: usize, added: usize, pairs: &[(usize, usize)]) {
    let old = |k: usize| (start + k) as u32;
    let new = |k: usize| (start + removed + k) as u32;
    let (mut a, mut b) = (0, 0);
    for &(pa, pb) in pairs.iter().chain(std::iter::once(&(removed, added))) {
        while a < pa || b < pb {
            rows.push(Row([(a < pa).then(|| old(a)), (b < pb).then(|| new(b))]));
            if a < pa {
                a += 1;
            }
            if b < pb {
                b += 1;
            }
        }
        if pa < removed && pb < added {
            rows.push(Row([Some(old(pa)), Some(new(pb))]));
            (a, b) = (pa + 1, pb + 1);
        }
    }
}

/// [`rows`] of `diff`, kept from frame to frame under `id`: worked out again
/// only for a diff with other lines. A reload that changed nothing (a file
/// was saved, a fetch finished) is a new diff with the same lines.
pub fn cached(ui: &Ui, id: egui::Id, diff: &Arc<FileDiff>) -> Arc<Rows> {
    #[derive(Clone)]
    struct Cache {
        /// Held weakly so the address stays taken: "same address" then
        /// reliably means "same diff".
        owner: Weak<dyn Any + Send + Sync>,
        lines: u64,
        rows: Arc<Rows>,
    }
    let owner = || Arc::downgrade(diff) as Weak<dyn Any + Send + Sync>;
    let cache = ui.data(|data| data.get_temp::<Cache>(id));
    if let Some(cache) = &cache
        && std::ptr::addr_eq(cache.owner.as_ptr(), Arc::as_ptr(diff))
    {
        return cache.rows.clone();
    }
    let lines = {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for hunk in &diff.hunks {
            (hunk.gap, hunk.lines.len()).hash(&mut hasher);
            for line in &hunk.lines {
                (line.origin, &line.text).hash(&mut hasher);
            }
        }
        hasher.finish()
    };
    let rows = match cache {
        Some(cache) if cache.lines == lines => cache.rows,
        _ => Arc::new(rows(diff)),
    };
    ui.data_mut(|data| data.insert_temp(id, Cache { owner: owner(), lines, rows: rows.clone() }));
    rows
}

/// One side of a split diff as a text, for selecting and copying.
struct SideText<'a> {
    diff: &'a FileDiff,
    /// The whole diff's text, which reads the lines back from the patch.
    text: &'a DiffText<'a>,
    lines: &'a [(u32, u32)],
}

impl Lines for SideText<'_> {
    fn count(&self) -> usize {
        self.lines.len()
    }

    fn shown(&self, line: usize) -> &str {
        let (h, i) = self.lines[line];
        &self.diff.hunks[h as usize].lines[i as usize].text
    }

    fn source(&self, line: usize) -> Option<&str> {
        let (h, i) = self.lines[line];
        self.text.source_at(h as usize, i as usize)
    }
}

/// The split layout. Both sides scroll up and down together; when lines are
/// wider than a side, one horizontal scroll bar moves both, so that their
/// columns stay level.
pub(super) fn show(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, mut view: View<'_>) {
    let (target, diff) = (view.target, view.diff);
    let rows = cached(ui, ui.id().with("diff-split-rows"), diff);
    let gutter = view.m.num_w + view.m.sign_w;
    let area = ui.available_rect_before_wrap();
    let pane_w = (area.width() / 2.0).floor();
    // The same columns on both sides, so a row's height is the taller side's.
    let columns = view.m.columns(pane_w - gutter - WRAP_MARGIN);
    let heights = columns.map(|columns| {
        let count = || {
            let side = |h: u32, i: Option<u32>| {
                i.map_or(1, |i| text::wrapped_rows(&diff.hunks[h as usize].lines[i as usize].text, columns))
            };
            let mut firsts = Vec::with_capacity(rows.rows.len() + 1);
            let mut at = 0u32;
            for h in 0..diff.hunks.len() {
                for row in rows.of(h) {
                    firsts.push(at);
                    at += side(h as u32, row.0[LEFT]).max(side(h as u32, row.0[RIGHT])) as u32;
                }
            }
            firsts.push(at);
            firsts
        };
        wrap_rows(ui, ui.id().with("diff-split-wrap"), diff, columns, count)
    });
    let geometry = Geometry::new(
        diff.hunks.iter().enumerate().map(|(h, hunk)| (rows.starts[h + 1] - rows.starts[h], !hunk.gap)),
        diff.truncated,
        view.m.line_h,
        view.m.hunk_h,
        heights,
    );
    let height = geometry.height();
    // How far the code is wider than a side: the room to scroll sideways.
    let overflow = if columns.is_some() {
        0.0
    } else {
        (gutter + diff.max_chars as f32 * view.m.char_w + 40.0 - pane_w).max(0.0)
    };

    let text = DiffText::new(diff);
    let sides = [
        SideText { diff, text: &text, lines: &rows.lines[LEFT] },
        SideText { diff, text: &text, lines: &rows.lines[RIGHT] },
    ];
    let selections = [text_selection(ui, "diff-text-left"), text_selection(ui, "diff-text-right")];
    let mut selections = selections.each_ref().map(|s| s.lock().unwrap_or_else(|e| e.into_inner()));
    for selection in &mut selections {
        selection.follow(diff, ui.ctx().cumulative_frame_nr(), || fingerprint(target, diff));
    }

    let anchoring = Anchoring::new(ui, &view);
    let numbers = |h: usize, k: usize| {
        let row = rows.of(h).get(k)?;
        let number = |side: usize| {
            let line = &diff.hunks[h].lines[row.0[side]? as usize];
            if side == LEFT { line.old_no } else { line.new_no }
        };
        Some((number(LEFT), number(RIGHT)))
    };
    let restore = anchoring.restore(ui, &geometry, diff.hunks.len(), numbers);

    let mut scroll = if columns.is_some() { ScrollArea::vertical() } else { ScrollArea::both() };
    if let Some(y) = restore {
        scroll = scroll.vertical_scroll_offset(y.max(0.0));
    }
    let stageable = view.picking.is_some();
    scroll.id_salt("diff-body").auto_shrink([false, false]).show_viewport(ui, |ui, viewport| {
        let m = &view.m;
        let size = vec2(ui.available_width() + overflow, height.max(ui.available_height()));
        let sense = if stageable { Sense::click() } else { Sense::hover() };
        let (content, response) = ui.allocate_exact_size(size, sense);
        let painter = ui.painter().clone();
        let clip = painter.clip_rect();

        // Gutters, hunk bars and the line between the sides stay put; only
        // the code scrolls sideways.
        let left = content.left() + viewport.min.x;
        let right = content.left() + viewport.max.x;
        let mid = left + pane_w;
        let pane = [(left, mid), (mid, right)];
        let text_left = [left + gutter - viewport.min.x, mid + gutter - viewport.min.x];
        let top = content.top() + viewport.min.y;
        let bottom = content.top() + viewport.max.y;

        // Selecting text, on either side.
        let mut responses = Vec::with_capacity(2);
        for side in [LEFT, RIGHT] {
            let text_rect = Rect::from_min_max(pos2(pane[side].0 + gutter, top), pos2(pane[side].1, bottom));
            let pane_rect = Rect::from_min_max(pos2(pane[side].0, top), pos2(pane[side].1, bottom));
            let lines = &sides[side];
            let hit = |pos: Pos2| {
                let count = lines.count();
                let Some(last) = count.checked_sub(1) else { return Pos::default() };
                let end = |k: usize| Pos::new(k, lines.shown(k).chars().count());
                let (flat, row, col) = geometry.locate(pos.y - content.top());
                let k = rows.at[flat][side] as usize;
                let here = rows.rows[flat].0[side].is_some();
                match col {
                    // Below a hunk: the end of its last line on this side.
                    Some(usize::MAX) => end(if here { k } else { k.saturating_sub(1) }.min(last)),
                    // On a header: the start of the next line.
                    Some(_) if k > last => end(last),
                    Some(_) => Pos::new(k, 0),
                    None if here => {
                        Pos::new(k, column_at(&painter, m, lines.shown(k), row, pos.x, text_left[side], columns))
                    }
                    // Next to nothing: between this side's lines.
                    None if k > last => end(last),
                    None => Pos::new(k, 0),
                }
            };
            let id = ui.id().with(("text", side));
            let text_response = selections[side].interact(ui, id, text_rect, pane_rect, lines, hit);
            text_response.context_menu(|ui| selections[side].menu(ui, lines));
            responses.push(text_response);
        }
        // A selection stays on one side: starting one clears the other.
        if ui.input(|i| i.pointer.button_pressed(PointerButton::Primary)) && !ui.input(|i| i.modifiers.shift) {
            for side in [LEFT, RIGHT] {
                if responses[side].hovered() {
                    selections[1 - side].clear();
                }
            }
        }

        // The changed line under a point in either gutter, if any.
        let line_at = |pos: Pos2| -> Option<LineRef> {
            let side = [LEFT, RIGHT].into_iter().find(|side| (pane[*side].0..pane[*side].0 + gutter).contains(&pos.x))?;
            let (flat, _, col) = geometry.locate(pos.y - content.top());
            if col.is_some() || rows.rows.is_empty() {
                return None;
            }
            let h = geometry.hunk_of(flat);
            changed(diff, h, rows.rows[flat].0[side]? as usize)
        };
        let mut hovered = None;
        if let Some(picking) = view.picking.as_deref_mut() {
            hovered = response.hover_pos().and_then(line_at);
            if hovered.is_some() {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            }
            if response.clicked()
                && let Some(at) = response.interact_pointer_pos().and_then(line_at)
            {
                picking.click(diff, at, ui.input(|i| i.modifiers.shift));
            }
        }

        let text_painters = pane.map(|(from, to)| {
            painter.with_clip_rect(Rect::from_min_max(pos2(from + gutter, clip.top()), pos2(to, clip.bottom())).intersect(clip))
        });
        // The line numbers, or without them the column of +/- signs, are
        // where a picked line is marked.
        let numbers_right =
            pane.map(|(from, _)| if m.look.numbers { from + m.num_w - 1.0 } else { from + m.sign_w - 1.0 });

        for h in geometry.hunks_between(viewport.min.y, viewport.max.y) {
            if geometry.bottom(h) < viewport.min.y {
                continue;
            }
            if !diff.hunks[h].gap {
                let y = content.top() + geometry.top(h);
                let bar = Rect::from_min_max(pos2(left, y), pos2(right, y + view.m.hunk_h));
                paint_hunk_bar(ui, &painter, tab, shared, &mut view, h, bar);
            }
            let m = &view.m;
            let hunk = &diff.hunks[h];
            let styled = view.style.hunk(h);
            for k in geometry.items_between(h, viewport.min.y, viewport.max.y) {
                let flat = geometry.first(h) + k;
                let row = rows.rows[flat];
                let y = content.top() + geometry.item_top(h, k);
                let row_h = geometry.item_rows(flat) as f32 * m.line_h;
                for side in [LEFT, RIGHT] {
                    let (from, to) = pane[side];
                    let cell = Rect::from_min_max(pos2(from, y), pos2(to, y + row_h));
                    let Some(i) = row.0[side].map(|i| i as usize) else {
                        painter.rect_filled(cell, 0.0, theme::bg_void());
                        continue;
                    };
                    let line = &hunk.lines[i];
                    let selected = view.picking.as_ref().is_some_and(|s| s.lines.contains(&(h, i)));
                    if let Some(bg) = line_background(line.origin, selected) {
                        painter.rect_filled(cell, 0.0, bg);
                    }
                    let numbers = Rect::from_min_max(cell.min, pos2(numbers_right[side], cell.bottom()));
                    paint_pick(&painter, numbers, selected, hovered == Some((h, i)));
                    let cy = y + m.line_h / 2.0;
                    if m.look.numbers {
                        let number = if side == LEFT { line.old_no } else { line.new_no };
                        paint_number(&painter, m, number, from + m.num_w - 6.0, cy, selected);
                    }
                    paint_sign(&painter, m, line.origin, from + m.num_w + (m.sign_w * 0.36).round(), cy);
                    let text_line = rows.at[flat][side] as usize;
                    paint_code(
                        &text_painters[side],
                        m,
                        line,
                        styled.as_ref(),
                        i,
                        text_left[side],
                        y,
                        (from, to),
                        columns,
                        &selections[side],
                        text_line,
                    );
                }
            }
        }

        let m = &view.m;
        if diff.truncated {
            paint_truncated(&painter, m, (left, right), content.top() + height);
        }
        let faint = Stroke::new(1.0, theme::with_alpha(theme::border_light(), 90));
        for x in numbers_right {
            painter.vline(x, clip.y_range(), faint);
        }
        // Between the two sides, up to the hunk bars, which span both.
        let divider = Stroke::new(1.0, theme::border());
        for h in geometry.hunks_between(viewport.min.y, viewport.max.y) {
            let from = content.top() + geometry.top(h) + geometry.header(h);
            let to = content.top() + geometry.bottom(h);
            painter.vline(mid - 0.5, from.max(clip.top())..=to.min(clip.bottom()), divider);
        }

        anchoring.keep(ui, &geometry, viewport.min.y, |flat| {
            let row = rows.rows.get(flat)?;
            let h = geometry.hunk_of(flat);
            let line = |side: usize| Some(&diff.hunks[h].lines[row.0[side]? as usize]);
            line(RIGHT).or_else(|| line(LEFT)).and_then(anchor_number)
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::DiffSource::Unstaged;
    use crate::git::{DiffLine, Hunk};
    use crate::tests::Fixture;

    /// A hunk of `(origin, text)` lines, numbered as git numbers them.
    fn hunk(lines: &[(char, &str)]) -> Hunk {
        let (mut old, mut new) = (1, 1);
        let lines = lines
            .iter()
            .map(|(origin, text)| {
                let line = DiffLine {
                    origin: *origin,
                    old_no: (*origin != '+').then_some(old),
                    new_no: (*origin != '-').then_some(new),
                    text: (*text).to_owned(),
                };
                old += u32::from(*origin != '+');
                new += u32::from(*origin != '-');
                line
            })
            .collect();
        Hunk { lines, ..Hunk::default() }
    }

    /// Each row as the texts on its two sides, `None` for nothing.
    fn shape(diff: &FileDiff) -> Vec<(Option<String>, Option<String>)> {
        let rows = rows(diff);
        let mut out = Vec::new();
        for h in 0..diff.hunks.len() {
            for row in rows.of(h) {
                let text = |side: usize| row.0[side].map(|i| diff.hunks[h].lines[i as usize].text.clone());
                out.push((text(LEFT), text(RIGHT)));
            }
        }
        out
    }

    fn rows_of(expected: &[(Option<&str>, Option<&str>)]) -> Vec<(Option<String>, Option<String>)> {
        expected.iter().map(|(l, r)| (l.map(str::to_owned), r.map(str::to_owned))).collect()
    }

    fn of(hunks: Vec<Hunk>) -> FileDiff {
        FileDiff { hunks, ..FileDiff::default() }
    }

    #[test]
    fn unchanged_lines_stay_level_and_replacements_pair_up() {
        // A line edited, with a new line inserted before it: the edit pairs
        // with its new version, the insertion has nothing beside it.
        let diff = of(vec![hunk(&[
            (' ', "fn main() {"),
            ('-', "    let total = 1;"),
            ('+', "    // the sum"),
            ('+', "    let total = 2;"),
            (' ', "}"),
        ])]);
        assert_eq!(
            shape(&diff),
            rows_of(&[
                (Some("fn main() {"), Some("fn main() {")),
                (None, Some("    // the sum")),
                (Some("    let total = 1;"), Some("    let total = 2;")),
                (Some("}"), Some("}")),
            ])
        );

        // Two lines become one: the one that matches pairs, the other stands
        // alone on the left.
        let diff = of(vec![hunk(&[('-', "let a = 1;"), ('-', "fn unrelated() {}"), ('+', "let a = 10;")])]);
        assert_eq!(
            shape(&diff),
            rows_of(&[(Some("let a = 1;"), Some("let a = 10;")), (Some("fn unrelated() {}"), None)])
        );

        // Nothing alike: line for line, the longer side's rest alone.
        let diff = of(vec![hunk(&[('-', "alpha"), ('+', "1"), ('+', "2"), ('+', "3")])]);
        assert_eq!(shape(&diff), rows_of(&[(Some("alpha"), Some("1")), (None, Some("2")), (None, Some("3"))]));
    }

    #[test]
    fn additions_and_deletions_have_nothing_on_the_other_side() {
        let diff = of(vec![
            hunk(&[(' ', "a"), ('+', "new 1"), ('+', "new 2"), (' ', "b")]),
            hunk(&[(' ', "c"), ('-', "gone"), (' ', "d")]),
        ]);
        assert_eq!(
            shape(&diff),
            rows_of(&[
                (Some("a"), Some("a")),
                (None, Some("new 1")),
                (None, Some("new 2")),
                (Some("b"), Some("b")),
                (Some("c"), Some("c")),
                (Some("gone"), None),
                (Some("d"), Some("d")),
            ])
        );
        let rows = rows(&diff);
        assert_eq!(rows.starts, [0, 4, 7]);
        // Each side's text: the old file on the left, the new on the right.
        let side = |s: usize| -> Vec<&str> {
            rows.lines[s].iter().map(|(h, i)| diff.hunks[*h as usize].lines[*i as usize].text.as_str()).collect()
        };
        assert_eq!(side(LEFT), ["a", "b", "c", "gone", "d"]);
        assert_eq!(side(RIGHT), ["a", "new 1", "new 2", "b", "c", "d"]);
        // Next to nothing, a row points at the side's next line.
        assert_eq!(rows.at[1], [1, 1]);
        assert_eq!(rows.at[5], [3, 5]);
        assert_eq!(rows.at[6], [4, 5]);
    }

    #[test]
    fn an_equal_block_pairs_line_for_line_even_when_unlike() {
        let diff = of(vec![hunk(&[('-', "one"), ('-', "two"), ('+', "uno"), ('+', "dos")])]);
        assert_eq!(shape(&diff), rows_of(&[(Some("one"), Some("uno")), (Some("two"), Some("dos"))]));
        // A pure addition at the start of a hunk, then a replacement.
        let diff = of(vec![hunk(&[('+', "first"), ('-', "x"), ('+', "y")])]);
        assert_eq!(shape(&diff), rows_of(&[(None, Some("first")), (Some("x"), Some("y"))]));
        assert!(rows(&of(Vec::new())).rows.is_empty());
    }

    #[test]
    fn real_diffs_with_a_missing_newline_or_crlf_line_up() {
        let fx = Fixture::new("split-rows");
        fx.write("tail.txt", "alpha\nbeta");
        fx.write("crlf.txt", "one\r\ntwo\r\nthree\r\n");
        fx.commit_all("base");
        // The last line gains its newline and a line follows it.
        fx.write("tail.txt", "alpha\nbeta\ngamma\n");
        // Line endings change throughout, and one line for real.
        fx.write("crlf.txt", "one\ntwo, edited\nthree\n");

        let tail = fx.diff(Unstaged, "tail.txt");
        assert_eq!(
            shape(&tail),
            rows_of(&[(Some("alpha"), Some("alpha")), (Some("beta"), Some("beta")), (None, Some("gamma"))])
        );
        let crlf = fx.diff(Unstaged, "crlf.txt");
        assert_eq!(
            shape(&crlf),
            rows_of(&[
                (Some("one"), Some("one")),
                (Some("two"), Some("two, edited")),
                (Some("three"), Some("three")),
            ])
        );
        // Copying the left side gives the old file's own text.
        let text = DiffText::new(&crlf);
        let rows = rows(&crlf);
        let left = SideText { diff: &crlf, text: &text, lines: &rows.lines[LEFT] };
        assert_eq!(left.count(), 3);
        let mut selection = crate::ui::textsel::Selection::default();
        selection.select_all(&left);
        assert_eq!(selection.text(&left).as_deref(), Some("one\ntwo\nthree"));
    }

    #[test]
    fn the_whole_file_lines_up_through_its_gaps() {
        let fx = Fixture::new("split-whole");
        let original: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        fx.write("f.txt", &original);
        fx.commit_all("base");
        fx.write("f.txt", &original.replace("line 10\n", "line ten\nline ten and a half\n"));
        let target = crate::git::DiffTarget { source: Unstaged, path: "f.txt".to_owned(), old_path: None };
        let mode = crate::git::diff::Mode { whole_file: true, ignore_whitespace: false };
        let diff = fx.job(move |loader| loader.diff_with(&target, mode)).unwrap();
        let rows = rows(&diff);
        // 19 unchanged lines, the edit, the insertion.
        assert_eq!(rows.rows.len(), 21);
        assert_eq!(rows.lines[LEFT].len(), 20);
        assert_eq!(rows.lines[RIGHT].len(), 21);
        let shape = shape(&diff);
        let edit = shape.iter().position(|(l, _)| l.as_deref() == Some("line 10")).unwrap();
        assert_eq!(shape[edit].1.as_deref(), Some("line ten"));
        assert_eq!(shape[edit + 1], (None, Some("line ten and a half".to_owned())));
        assert_eq!(shape[edit + 2].0, shape[edit + 2].1);
    }

    /// Not a check, a measurement: `cargo test split_rows_speed -- --ignored --nocapture`.
    /// The worst case for pairing: 50,000 lines in blocks of uneven size.
    #[test]
    #[ignore]
    fn split_rows_speed() {
        let source = include_str!("diff.rs");
        let mut lines = Vec::new();
        for (k, line) in source.lines().cycle().take(50_000).enumerate() {
            let origin = match k % 12 {
                0..3 => ' ',
                3..7 => '-',
                _ => '+',
            };
            lines.push((origin, line));
        }
        let diff = of(vec![hunk(&lines)]);
        let started = std::time::Instant::now();
        let rows = rows(&diff);
        println!("{} lines, {} rows in {:.0} ms", lines.len(), rows.rows.len(), started.elapsed().as_secs_f32() * 1000.0);
    }
}
