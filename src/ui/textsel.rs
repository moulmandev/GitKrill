//! Selecting and copying text in the views that paint code themselves: the
//! diff and blame.
//!
//! egui selects text only inside its own labels, and these views lay out
//! only the rows in view, so they do it here instead, the way a code
//! editor's read-only view does: press and drag (the text scrolls when the
//! pointer goes past its edge), double-click for a word, triple-click for a
//! line, Shift+click to extend, Ctrl+A, Ctrl+C and a context menu.
//!
//! A view describes its text as numbered lines ([`Lines`]), turns screen
//! points into positions in that text, and paints the highlight of the rows
//! it draws ([`Selection::paint`]); the rest happens here. Positions are
//! columns of the text as shown, which are character indices (see
//! `git::text`). Copying maps them back to the file's own text, so tabs
//! come out as tabs.

use std::any::Any;
use std::ops::Range;
use std::sync::{Arc, Weak};

use eframe::egui::style::ScrollAnimation;
use eframe::egui::text::{CharIndex, Galley};
use eframe::egui::{
    self, CursorIcon, Event, Id, Key, Modifiers, Painter, PointerButton, Pos2, Rect, Response,
    Sense, Ui, Vec2,
};

use crate::git::text::source_range;
use crate::theme::{self, icons};
use crate::ui::widgets::menu_item;

/// Points scrolled per second for each point the pointer is past the edge
/// of the text while selecting.
const AUTO_SCROLL: f32 = 12.0;
/// How far apart two presses may be to count as a double or triple click.
const MULTI_CLICK_DISTANCE: f32 = 6.0;

/// A place between two characters: a line, and a column of it as shown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pos {
    pub line: usize,
    pub col: usize,
}

impl Pos {
    pub fn new(line: usize, col: usize) -> Self {
        Pos { line, col }
    }
}

/// The text of a view, line by line.
pub trait Lines {
    fn count(&self) -> usize;
    /// A line as it is shown.
    fn shown(&self, line: usize) -> &str;
    /// A line as it is in the file, when that is not what is shown (see
    /// `git::text`). Copying takes the text from here.
    fn source(&self, _line: usize) -> Option<&str> {
        None
    }
}

impl Lines for Vec<String> {
    fn count(&self) -> usize {
        self.len()
    }

    fn shown(&self, line: usize) -> &str {
        &self[line]
    }
}

fn char_count(text: &str) -> usize {
    text.chars().count()
}

/// Columns `cols` of `text`, clamped to its length.
fn char_slice(text: &str, cols: Range<usize>) -> &str {
    let byte = |col: usize| text.char_indices().nth(col).map_or(text.len(), |(at, _)| at);
    let start = byte(cols.start);
    &text[start..byte(cols.end).max(start)]
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Word,
    Space,
    Other,
}

fn class(c: char) -> Class {
    if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else if c.is_whitespace() {
        Class::Space
    } else {
        Class::Other
    }
}

/// The columns of the word at `col`: a run of letters, digits and
/// underscores, of spaces, or of other characters. Between a word and
/// something else, the word wins.
pub fn word_at(text: &str, col: usize) -> Range<usize> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return 0..0;
    }
    let col = col.min(chars.len());
    let after = chars.get(col).copied().map(class);
    let before = col.checked_sub(1).map(|i| class(chars[i]));
    let at = if after.is_none() || (after != Some(Class::Word) && before == Some(Class::Word)) {
        col - 1
    } else {
        col
    };
    let kind = class(chars[at]);
    let mut start = at;
    while start > 0 && class(chars[start - 1]) == kind {
        start -= 1;
    }
    let mut end = at + 1;
    while end < chars.len() && class(chars[end]) == kind {
        end += 1;
    }
    start..end
}

/// The column nearest to `x` in a line laid out as a one-row galley, `x`
/// measured from where the galley is placed. Glyph positions, not a grid:
/// wide characters do not keep to the monospace width.
pub fn column_at(galley: &Galley, x: f32) -> usize {
    galley.rows.first().map_or(0, |placed| placed.row.char_at(x - placed.pos.x).0)
}

/// Where column `col` starts in a one-row galley, from where it is placed.
pub fn x_at(galley: &Galley, col: usize) -> f32 {
    galley.rows.first().map_or(0.0, |placed| placed.pos.x + placed.row.x_offset(CharIndex(col)))
}

/// What a drag extends the selection by, chosen by the press that began it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Unit {
    #[default]
    Char,
    Word,
    Line,
}

/// The selection of one view, kept from frame to frame.
#[derive(Default)]
pub struct Selection {
    /// What the press that began the selection took: a bare position for a
    /// single click, the word or the line for a double or triple click. It
    /// stays selected wherever the drag goes.
    origin: (Pos, Pos),
    /// What is selected, in order. Empty after a plain click.
    range: (Pos, Pos),
    unit: Unit,
    /// The primary button went down in the text and is still held.
    dragging: bool,
    /// The last press in the text: when, where on screen, and how many
    /// quick presses in a row it completed (1 to 3).
    last_press: Option<(f64, Pos2, u8)>,
    /// What the selection was made in; see [`Selection::follow`]. Held
    /// weakly so the address stays taken: "same address" then reliably
    /// means "same object".
    shown: Option<Weak<dyn Any + Send + Sync>>,
    fingerprint: u64,
    /// The frame in which the view was last on screen.
    seen: u64,
    /// Whether keys go to this view. An inactive selection is painted fainter.
    active: bool,
}

impl Selection {
    /// Points the selection at `data`, the content of the view, shown in
    /// frame `frame`. It is dropped when the view shows something else (a
    /// reload with the same `fingerprint` is not something else) or has been
    /// off screen in between.
    pub fn follow<T: Any + Send + Sync>(&mut self, data: &Arc<T>, frame: u64, fingerprint: impl FnOnce() -> u64) {
        if frame > self.seen + 1 {
            self.clear();
        }
        self.seen = frame;
        if self.shown.as_ref().is_some_and(|known| std::ptr::addr_eq(known.as_ptr(), Arc::as_ptr(data))) {
            return;
        }
        let fingerprint = fingerprint();
        if fingerprint != self.fingerprint {
            self.clear();
        }
        self.fingerprint = fingerprint;
        let weak: Weak<dyn Any + Send + Sync> = Arc::downgrade(data) as Weak<T>;
        self.shown = Some(weak);
    }

    pub fn clear(&mut self) {
        self.origin = Default::default();
        self.range = Default::default();
        self.unit = Unit::Char;
        self.dragging = false;
    }

    /// The selected range, start first; `None` when nothing is selected.
    pub fn range(&self) -> Option<(Pos, Pos)> {
        (self.range.0 < self.range.1).then_some(self.range)
    }

    /// The word or line at `at`, or just `at`.
    fn unit_at(&self, at: Pos, unit: Unit, lines: &dyn Lines) -> (Pos, Pos) {
        if at.line >= lines.count() {
            return (at, at);
        }
        let text = lines.shown(at.line);
        match unit {
            Unit::Char => (at, at),
            Unit::Word => {
                let word = word_at(text, at.col);
                (Pos::new(at.line, word.start), Pos::new(at.line, word.end))
            }
            Unit::Line if at.line + 1 < lines.count() => (Pos::new(at.line, 0), Pos::new(at.line + 1, 0)),
            Unit::Line => (Pos::new(at.line, 0), Pos::new(at.line, char_count(text))),
        }
    }

    /// The primary button went down at `at`, the `clicks`-th press in quick
    /// succession. With `extend` (Shift held) the selection instead grows or
    /// shrinks to `at` from where it began.
    pub fn press(&mut self, at: Pos, clicks: u8, extend: bool, lines: &dyn Lines) {
        self.dragging = true;
        if extend {
            self.unit = Unit::Char;
            self.drag_to(at, lines);
            return;
        }
        self.unit = match clicks {
            0 | 1 => Unit::Char,
            2 => Unit::Word,
            _ => Unit::Line,
        };
        self.origin = self.unit_at(at, self.unit, lines);
        self.range = self.origin;
    }

    /// The pointer moved to `at` with the button still down.
    pub fn drag_to(&mut self, at: Pos, lines: &dyn Lines) {
        let (start, end) = self.unit_at(at, self.unit, lines);
        self.range = if start < self.origin.0 {
            (start, self.origin.1)
        } else {
            (self.origin.0, end.max(self.origin.1))
        };
    }

    pub fn select_all(&mut self, lines: &dyn Lines) {
        let Some(last) = lines.count().checked_sub(1) else {
            return self.clear();
        };
        self.unit = Unit::Char;
        self.origin = (Pos::default(), Pos::new(last, char_count(lines.shown(last))));
        self.range = self.origin;
    }

    /// The selected columns of `line`, which is `len` columns long, and
    /// whether the line break after it is selected too.
    pub fn columns(&self, line: usize, len: usize) -> Option<(Range<usize>, bool)> {
        let (start, end) = self.range()?;
        if line < start.line || line > end.line {
            return None;
        }
        let from = if line == start.line { start.col.min(len) } else { 0 };
        let to = if line == end.line { end.col.min(len) } else { len }.max(from);
        let newline = line < end.line;
        (from < to || newline).then_some((from..to, newline))
    }

    /// The selected text, lines joined by `\n`, from the file's own text
    /// where there is one. `None` when nothing is selected.
    pub fn text(&self, lines: &dyn Lines) -> Option<String> {
        let (start, end) = self.range()?;
        let last = end.line.min(lines.count().checked_sub(1)?);
        let mut out = String::new();
        for line in start.line..=last {
            if line > start.line {
                out.push('\n');
            }
            let shown = lines.shown(line);
            let Some((cols, _)) = self.columns(line, char_count(shown)) else { continue };
            out.push_str(match lines.source(line) {
                Some(source) => source_range(source, cols),
                None => char_slice(shown, cols),
            });
        }
        Some(out)
    }

    /// Handles the pointer and the keys for a view's text in this frame, and
    /// returns the text's response (for a context menu).
    ///
    /// `rect` is the text on screen: presses there select, and the pointer
    /// is an I-beam. `view` is all of the view: a press anywhere in it makes
    /// it the view that Ctrl+A and Ctrl+C go to. `hit` turns a screen point,
    /// which may be outside `rect` during a drag, into the nearest position
    /// in the text.
    pub fn interact(
        &mut self,
        ui: &Ui,
        id: Id,
        rect: Rect,
        view: Rect,
        lines: &dyn Lines,
        hit: impl Fn(Pos2) -> Pos,
    ) -> Response {
        let ctx = ui.ctx().clone();
        let response = ui.interact(rect, id, Sense::click_and_drag()).on_hover_cursor(CursorIcon::Text);

        // Which view keys go to is shared by all of them: the last one
        // pressed in, while it stays on screen.
        let frame = ctx.cumulative_frame_nr();
        let key = Id::new("textsel-active");
        let (owner, owner_seen) = ctx.data(|d| d.get_temp::<(Id, u64)>(key)).unwrap_or((Id::NULL, 0));
        let owner_live = owner != Id::NULL && owner_seen + 1 >= frame;
        let mut active = owner_live && owner == id;
        let (pressed, primary, pointer, time, shift) = ui.input(|i| {
            (
                i.pointer.any_pressed(),
                i.pointer.button_pressed(PointerButton::Primary),
                i.pointer.latest_pos(),
                i.time,
                i.modifiers.shift,
            )
        });
        if pressed {
            if ui.rect_contains_pointer(view) {
                active = true;
            } else if !egui::Popup::is_any_open(&ctx) {
                // Not when choosing from a menu, which may be this view's.
                active = false;
            }
        }

        if primary
            && response.hovered()
            && let Some(at) = pointer
        {
            let delay = ctx.options(|o| o.input_options.max_double_click_delay);
            let clicks = match self.last_press {
                Some((then, place, count)) if time - then <= delay && place.distance(at) <= MULTI_CLICK_DISTANCE => {
                    (count + 1).min(3)
                }
                _ => 1,
            };
            self.last_press = Some((time, at, clicks));
            self.press(hit(at), clicks, shift, lines);
        }

        if self.dragging {
            if !ui.input(|i| i.pointer.primary_down()) {
                self.dragging = false;
            } else if let Some(at) = pointer {
                // Past an edge the text scrolls towards the pointer, the
                // faster the further out it is.
                let past = |v: f32, low: f32, high: f32| {
                    if v < low {
                        v - low
                    } else if v > high {
                        v - high
                    } else {
                        0.0
                    }
                };
                let out = Vec2::new(past(at.x, rect.left(), rect.right()), past(at.y, rect.top(), rect.bottom()));
                if out != Vec2::ZERO {
                    let dt = ui.input(|i| i.stable_dt).min(0.05);
                    ui.scroll_with_delta_animation(-out * AUTO_SCROLL * dt, ScrollAnimation::none());
                    ctx.request_repaint();
                }
                self.drag_to(hit(at), lines);
                ctx.set_cursor_icon(CursorIcon::Text);
            }
        }

        // Keys go to the active view, or with none, to the one under the
        // pointer; never while a text field has the keyboard.
        let keys = active || (!owner_live && ui.rect_contains_pointer(view));
        if keys && !ctx.egui_wants_keyboard_input() {
            if ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::A)) {
                self.select_all(lines);
                active = true;
            }
            // The window turns Ctrl+C into a copy event; scripted input
            // sends the key itself.
            let copy = ui.input(|i| {
                i.events.iter().any(|event| {
                    matches!(event, Event::Copy)
                        || matches!(event, Event::Key { key: Key::C, pressed: true, modifiers, .. } if modifiers.command)
                })
            });
            if copy && let Some(text) = self.text(lines) {
                ctx.copy_text(text);
            }
        }

        if active {
            ctx.data_mut(|d| d.insert_temp(key, (id, frame)));
        } else if owner == id {
            ctx.data_mut(|d| d.remove::<(Id, u64)>(key));
        }
        self.active = active;
        response
    }

    /// The entries of the text's context menu: Copy and Select all.
    pub fn menu(&mut self, ui: &mut Ui, lines: &dyn Lines) {
        let copy = ui.add_enabled_ui(self.range().is_some(), |ui| menu_item(ui, icons::COPY, "Copy")).inner;
        if copy && let Some(text) = self.text(lines) {
            ui.ctx().copy_text(text);
        }
        if menu_item(ui, icons::SELECTION_ALL, "Select all") {
            self.select_all(lines);
        }
    }

    /// Paints the highlight behind the selected part of `line`, whose text
    /// is laid out as `galley` placed at `left`, in the row `row`. A
    /// selected line break shows as `newline` points more.
    pub fn paint(&self, painter: &Painter, line: usize, galley: &Galley, left: f32, row: Rect, newline: f32) {
        let Some((cols, more)) = self.columns(line, char_count(galley.text())) else { return };
        let from = left + x_at(galley, cols.start);
        let to = left + x_at(galley, cols.end) + if more { newline } else { 0.0 };
        let fill = if self.active {
            theme::text_selection()
        } else {
            theme::with_alpha(theme::text_selection(), 150)
        };
        painter.rect_filled(Rect::from_x_y_ranges(from..=to, row.y_range()), 0.0, fill);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|line| (*line).to_owned()).collect()
    }

    /// A view whose file has tabs where the shown text has spaces.
    struct Tabbed {
        shown: Vec<String>,
        source: Vec<Option<String>>,
    }

    impl Lines for Tabbed {
        fn count(&self) -> usize {
            self.shown.len()
        }

        fn shown(&self, line: usize) -> &str {
            &self.shown[line]
        }

        fn source(&self, line: usize) -> Option<&str> {
            self.source[line].as_deref()
        }
    }

    fn tabbed(file: &[&str]) -> Tabbed {
        Tabbed {
            shown: file.iter().map(|line| crate::git::text::shown(line)).collect(),
            source: file.iter().map(|line| line.contains('\t').then(|| (*line).to_owned())).collect(),
        }
    }

    fn select(lines: &dyn Lines, from: Pos, to: Pos) -> Selection {
        let mut selection = Selection::default();
        selection.press(from, 1, false, lines);
        selection.drag_to(to, lines);
        selection
    }

    #[test]
    fn words_are_runs_of_one_kind() {
        let text = "let foo_bar = a.b(1);";
        assert_eq!(word_at(text, 5), 4..11); // inside foo_bar
        assert_eq!(word_at(text, 4), 4..11); // at its start
        assert_eq!(word_at(text, 11), 4..11); // at its end, before a space
        assert_eq!(word_at(text, 12), 12..13); // the "="
        assert_eq!(word_at(text, 3), 0..3); // between "let" and a space: the word wins
        assert_eq!(word_at(text, 16), 16..17); // "b", after the dot
        assert_eq!(word_at(text, 21), 19..21); // at the end of the line: ");"
        assert_eq!(word_at("    x", 1), 0..4); // indentation
        assert_eq!(word_at("", 0), 0..0);
        assert_eq!(word_at("名前 = 1", 1), 0..2); // letters of any script
    }

    #[test]
    fn a_drag_selects_in_order_whichever_way_it_goes() {
        let text = lines(&["first line", "second", "third line"]);
        let forward = select(&text, Pos::new(0, 6), Pos::new(2, 5));
        assert_eq!(forward.range(), Some((Pos::new(0, 6), Pos::new(2, 5))));
        let backward = select(&text, Pos::new(2, 5), Pos::new(0, 6));
        assert_eq!(backward.range(), forward.range());
        assert_eq!(backward.text(&text).as_deref(), Some("line\nsecond\nthird"));

        // A click without a drag selects nothing.
        let click = select(&text, Pos::new(1, 2), Pos::new(1, 2));
        assert_eq!(click.range(), None);
        assert_eq!(click.text(&text), None);
    }

    #[test]
    fn double_and_triple_clicks_select_words_and_lines() {
        let text = lines(&["fn main() {", "    run(1);", "}"]);
        let mut selection = Selection::default();
        selection.press(Pos::new(0, 4), 2, false, &text);
        assert_eq!(selection.text(&text).as_deref(), Some("main"));
        // Dragging after a double click goes by whole words, and the first
        // word stays selected when the drag turns back past it.
        selection.drag_to(Pos::new(1, 6), &text);
        assert_eq!(selection.text(&text).as_deref(), Some("main() {\n    run"));
        selection.drag_to(Pos::new(0, 1), &text);
        assert_eq!(selection.text(&text).as_deref(), Some("fn main"));

        // A triple click takes the line with its line break; the last line
        // has none.
        selection.press(Pos::new(1, 3), 3, false, &text);
        assert_eq!(selection.text(&text).as_deref(), Some("    run(1);\n"));
        selection.press(Pos::new(2, 0), 3, false, &text);
        assert_eq!(selection.text(&text).as_deref(), Some("}"));
    }

    #[test]
    fn shift_click_moves_the_free_end() {
        let text = lines(&["abcdef", "ghijkl"]);
        let mut selection = select(&text, Pos::new(0, 2), Pos::new(0, 4));
        selection.press(Pos::new(1, 3), 1, true, &text);
        assert_eq!(selection.range(), Some((Pos::new(0, 2), Pos::new(1, 3))));
        // Past the start, the selection flips around where it began.
        selection.press(Pos::new(0, 1), 1, true, &text);
        assert_eq!(selection.text(&text).as_deref(), Some("b"));
    }

    #[test]
    fn select_all_and_the_columns_of_each_line() {
        let text = lines(&["one", "", "three"]);
        let mut selection = Selection::default();
        selection.select_all(&text);
        assert_eq!(selection.text(&text).as_deref(), Some("one\n\nthree"));
        assert_eq!(selection.columns(0, 3), Some((0..3, true)));
        // An empty line in the middle is selected through its line break.
        assert_eq!(selection.columns(1, 0), Some((0..0, true)));
        assert_eq!(selection.columns(2, 5), Some((0..5, false)));
        assert_eq!(selection.columns(3, 1), None);

        // Ending at the start of a line: that line has nothing selected.
        let selection = select(&text, Pos::new(0, 1), Pos::new(2, 0));
        assert_eq!(selection.columns(2, 5), None);
        assert_eq!(selection.text(&text).as_deref(), Some("ne\n\n"));

        let mut nothing = Selection::default();
        nothing.select_all(&Vec::<String>::new());
        assert_eq!(nothing.range(), None);
    }

    #[test]
    fn copying_gives_the_file_text_back() {
        // Tabs come back as tabs, however much of their width is selected.
        let file = tabbed(&["\tif x {", "\t\treturn\t1;", "}"]);
        assert_eq!(file.shown[1], "        return  1;");
        let mut selection = Selection::default();
        selection.select_all(&file);
        assert_eq!(selection.text(&file).as_deref(), Some("\tif x {\n\t\treturn\t1;\n}"));
        let part = select(&file, Pos::new(0, 4), Pos::new(1, 15));
        assert_eq!(part.text(&file).as_deref(), Some("if x {\n\t\treturn\t"));
        let inside_a_tab = select(&file, Pos::new(1, 2), Pos::new(1, 9));
        assert_eq!(inside_a_tab.text(&file).as_deref(), Some("\t\tr"));

        // Wide characters and emoji are one column each.
        let text = lines(&["名前 = \"🦀\";"]);
        let selection = select(&text, Pos::new(0, 1), Pos::new(0, 7));
        assert_eq!(selection.text(&text).as_deref(), Some("前 = \"🦀"));

        // Positions past the end of a line are clamped to it.
        let selection = select(&text, Pos::new(0, 7), Pos::new(0, 99));
        assert_eq!(selection.text(&text).as_deref(), Some("\";"));
    }

    #[test]
    fn the_selection_follows_the_content() {
        let text = lines(&["a", "b"]);
        let first = Arc::new(1u32);
        let mut selection = Selection::default();
        selection.follow(&first, 10, || 7);
        selection.select_all(&text);
        selection.follow(&first, 11, || unreachable!("same object, no fingerprint needed"));
        assert!(selection.range().is_some());
        // Reloaded with the same content: kept.
        let reloaded = Arc::new(1u32);
        selection.follow(&reloaded, 12, || 7);
        assert!(selection.range().is_some());
        // Something else: dropped.
        selection.follow(&Arc::new(2u32), 13, || 8);
        assert!(selection.range().is_none());
        // Off screen for a while: dropped too.
        selection.select_all(&text);
        selection.follow(&reloaded, 20, || 8);
        assert!(selection.range().is_none());
    }

    /// A galley laid out by egui's own fonts.
    fn galley(text: &str, font: egui::FontId) -> Arc<Galley> {
        let ctx = egui::Context::default();
        let mut out = None;
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            out = Some(ui.painter().layout_no_wrap(text.to_owned(), font.clone(), theme::text()));
        });
        // Nothing is drawn; the font atlas it set up is not needed.
        output.drop_without_applying_deltas();
        out.expect("laid out")
    }

    #[test]
    fn hit_testing_follows_the_glyphs() {
        // Characters of different widths, as CJK text is in a code font: a
        // fixed grid would put the pointer on the wrong one.
        let line = galley("iWiW", theme::font(13.0));
        let edges: Vec<f32> = (0..=4).map(|col| x_at(&line, col)).collect();
        assert!(edges.windows(2).all(|pair| pair[0] < pair[1]), "{edges:?}");
        assert!(edges[2] - edges[1] > (edges[1] - edges[0]) * 2.0, "{edges:?}");
        // Each point goes to the nearest boundary between characters.
        for col in 0..4 {
            let (left, right) = (edges[col], edges[col + 1]);
            assert_eq!(column_at(&line, left + (right - left) * 0.3), col);
            assert_eq!(column_at(&line, left + (right - left) * 0.7), col + 1);
        }
        assert_eq!(column_at(&line, -20.0), 0);
        assert_eq!(column_at(&line, edges[4] + 50.0), 4);

        let empty = galley("", theme::mono(12.5));
        assert_eq!(column_at(&empty, 30.0), 0);
        assert_eq!(x_at(&empty, 0), 0.0);
    }
}
