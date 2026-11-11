//! The text of the conflict resolver's panes as it is shown: tabs out to the
//! tab stops of Preferences > Editor, syntax colours, and the result editor
//! laid out exactly like the painted rows.
//!
//! Each pane with a text of its own (the current side, the incoming side,
//! the ancestor) is a [`PaneText`]: the merged lines plus that version of
//! every conflict, in file order, held as the lines of one hunk of a
//! `FileDiff`. That is the shape `ui::highlight` colours in the background,
//! here as one stretch of file. The result pane borrows the sides' colours
//! for the lines it takes from them; text typed by hand is coloured the same
//! way, one hunk per piece ([`typed_hunk`]).
//!
//! Lines are kept as shown (see `git::text`): one column per character, so
//! wrapping and selecting work on them as in the diff view, and the file's
//! own text is kept beside the lines that look different, for copying.

use std::ops::Range;
use std::sync::Arc;

use eframe::egui::text::{LayoutJob, TextFormat};
use eframe::egui::{Color32, FontId};

use crate::git::conflict::{Segment, TextMerge};
use crate::git::text;
use crate::git::{DiffLine, FileDiff, Hunk};
use crate::theme;
use crate::ui::highlight::{Class, Span};
use crate::ui::textsel;

/// Which version of the conflicting lines a pane shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Version {
    Ours = 0,
    Theirs = 1,
    Base = 2,
}

/// One pane's lines: the merged text with one version of every conflict.
pub struct PaneText {
    /// Every line as shown, as the one hunk of a diff (see the module
    /// comment); also what a selection follows.
    pub diff: Arc<FileDiff>,
    /// The file's own text of the lines that are shown differently (tabs,
    /// control characters), for copying.
    source: Vec<Option<String>>,
    /// The line at which each segment of the merge starts, then the total.
    starts: Vec<usize>,
    /// The longest line, in columns.
    pub longest: usize,
}

impl PaneText {
    pub fn new(merge: &TextMerge, version: Version, tab_width: usize) -> Self {
        let mut lines = Vec::new();
        let mut source = Vec::new();
        let mut starts = Vec::with_capacity(merge.segments.len() + 1);
        let mut longest = 0;
        for segment in &merge.segments {
            starts.push(lines.len());
            let chunk = match (segment, version) {
                (Segment::Stable(chunk), _) => chunk,
                (Segment::Conflict { ours, .. }, Version::Ours) => ours,
                (Segment::Conflict { theirs, .. }, Version::Theirs) => theirs,
                (Segment::Conflict { base, .. }, Version::Base) => base,
            };
            for i in 0..chunk.len() {
                let line = chunk.line(i);
                let shown = text::shown_with(&line, tab_width);
                longest = longest.max(shown.chars().count());
                source.push((shown != *line).then(|| line.into_owned()));
                lines.push(DiffLine { origin: ' ', old_no: None, new_no: None, text: shown });
            }
        }
        starts.push(lines.len());
        let hunk = Hunk { lines, ..Default::default() };
        PaneText { diff: Arc::new(FileDiff { hunks: vec![hunk], ..Default::default() }), source, starts, longest }
    }

    pub fn lines(&self) -> &[DiffLine] {
        &self.diff.hunks[0].lines
    }

    /// The lines of segment `seg` of the merge.
    pub fn segment(&self, seg: usize) -> Range<usize> {
        self.starts[seg]..self.starts[seg + 1]
    }
}

impl textsel::Lines for PaneText {
    fn count(&self) -> usize {
        self.lines().len()
    }

    fn shown(&self, line: usize) -> &str {
        &self.lines()[line].text
    }

    fn source(&self, line: usize) -> Option<&str> {
        self.source.get(line)?.as_deref()
    }
}

/// Text typed by hand as a hunk of shown lines: none for empty text.
pub fn typed_hunk(typed: &str, tab_width: usize) -> Hunk {
    let lines = if typed.is_empty() { Vec::new() } else { typed.split('\n').collect() };
    let lines = lines
        .into_iter()
        .map(|line| DiffLine { origin: ' ', old_no: None, new_no: None, text: text::shown_with(line, tab_width) })
        .collect();
    Hunk { lines, ..Default::default() }
}

/// Bytes `part` of `line` cut into runs of one colour each: the classes of
/// `spans` (the runs of the whole line, as the highlighter gives them), and
/// `plain` for plain text and for whatever the spans do not reach.
fn runs(line: &str, part: Range<usize>, spans: &[Span], plain: Color32) -> Vec<(Range<usize>, Color32)> {
    let part = part.start.min(line.len())..part.end.min(line.len());
    let mut out = Vec::new();
    let mut start = 0;
    for span in spans {
        let end = span.end as usize;
        // Spans made for other text (a line typed since): they hold up to
        // where they stop fitting, the rest is plain.
        if end <= start || end > line.len() || !line.is_char_boundary(end) {
            break;
        }
        let (from, to) = (start.max(part.start), end.min(part.end));
        if from < to {
            let color = if span.class == Class::Plain { plain } else { span.class.color() };
            out.push((from..to, color));
        }
        start = end;
        if start >= part.end {
            return out;
        }
    }
    let from = start.max(part.start);
    if from < part.end {
        out.push((from..part.end, plain));
    }
    out
}

/// Bytes `part` of a shown line (one row of it, when wrapped) as a row of
/// code `line_h` high: coloured by `spans` (see [`runs`]), every colour
/// faded towards the background by `fade` (0: not at all).
pub fn row_job(line: &str, part: Range<usize>, spans: &[Span], font: &FontId, plain: Color32, fade: f32, line_h: f32) -> LayoutJob {
    let mut job = LayoutJob { break_on_newline: false, ..Default::default() };
    job.wrap.max_width = f32::INFINITY;
    for (range, color) in runs(line, part, spans, plain) {
        let color = if fade > 0.0 { theme::mix(theme::bg_app(), color, 1.0 - fade) } else { color };
        let format = TextFormat { font_id: font.clone(), color, line_height: Some(line_h), ..Default::default() };
        job.append(&line[range], 0.0, format);
    }
    job
}

/// The runs of a shown line (`spans`, from the highlighter) as offsets into
/// `source`, the line it was made from with tab stops every `tab_width`.
pub fn source_spans(source: &str, spans: &[Span], tab_width: usize) -> Vec<Span> {
    let chars: Vec<text::ShownChar> = text::shown_chars(source, tab_width).collect();
    let mut out: Vec<Span> = Vec::with_capacity(spans.len());
    let mut taken = 0;
    for span in spans {
        // Every character the span reaches into goes with it.
        while taken < chars.len() && chars[taken].shown.start < span.end as usize {
            taken += 1;
        }
        let end = taken.checked_sub(1).map_or(0, |last| chars[last].source.end);
        if out.last().is_none_or(|last| (last.end as usize) < end) {
            out.push(Span { end: end as u32, class: span.class });
        }
    }
    out
}

/// The character of `source` at the boundary before shown column `col`:
/// for a point inside a tab, the side of it that is nearer.
pub fn source_char(source: &str, col: usize, tab_width: usize) -> usize {
    for (i, c) in text::shown_chars(source, tab_width).enumerate() {
        if col <= c.columns.start {
            return i;
        }
        if col < c.columns.end {
            return if col - c.columns.start <= c.columns.end - col { i } else { i + 1 };
        }
    }
    source.chars().count()
}

/// What the result editor needs to lay its text out as the panes paint it.
pub struct EditorLook {
    pub font: FontId,
    pub line_h: f32,
    pub color: Color32,
    /// The width of a space, which is one column.
    pub space_w: f32,
    /// How far egui moves on for a tab character, whatever the column.
    pub tab_w: f32,
    pub tab_width: usize,
    /// The width to wrap at; `None` keeps every line on one row.
    pub wrap: Option<f32>,
}

/// The editor's text (the file's own text, with its tabs) laid out like the
/// painted rows: every tab reaches the next tab stop, and line `i` is
/// coloured by `spans[i]` (offsets into that line; see [`source_spans`]).
///
/// egui gives every tab the same width, so the text after each tab starts a
/// section of its own, moved by the difference.
pub fn editor_job(typed: &str, look: &EditorLook, spans: &[Vec<Span>]) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = look.wrap.unwrap_or(f32::INFINITY);
    let format = |color| TextFormat { font_id: look.font.clone(), color, line_height: Some(look.line_h), ..Default::default() };
    let tab_width = look.tab_width.max(1);
    let mut lead = 0.0;
    for (i, line) in typed.split('\n').enumerate() {
        if i > 0 {
            job.append("\n", lead, format(look.color));
            lead = 0.0;
        }
        let mut column = 0;
        for (range, color) in runs(line, 0..line.len(), spans.get(i).map_or(&[], Vec::as_slice), look.color) {
            let mut from = range.start;
            for (at, c) in line[range.clone()].char_indices() {
                let at = range.start + at;
                if c != '\t' {
                    column += 1;
                    continue;
                }
                if from < at {
                    job.append(&line[from..at], lead, format(color));
                    lead = 0.0;
                }
                job.append("\t", lead, format(color));
                let stop = tab_width - column % tab_width;
                lead = stop as f32 * look.space_w - look.tab_w;
                column += stop;
                from = at + 1;
            }
            if from < range.end {
                job.append(&line[from..range.end], lead, format(color));
                lead = 0.0;
            }
        }
    }
    job
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::conflict::merge_text;
    use crate::ui::textsel::{Lines, Pos, Selection};
    use eframe::egui;
    use eframe::egui::epaint::text::ByteRangeExt;

    fn span(end: u32, class: Class) -> Span {
        Span { end, class }
    }

    #[test]
    fn panes_hold_the_merged_text_with_one_version_of_each_conflict() {
        let base = b"fn a() {\n\tone();\n}\nfn b() {}\n";
        let ours = b"fn a() {\n\tours();\n}\nfn b() {}\n";
        let theirs = b"fn a() {\n\ttheirs();\n\tmore();\n}\nfn b() {}\n";
        let merge = merge_text(base, ours, theirs).unwrap();
        assert_eq!(merge.conflicts.len(), 1);

        let pane = PaneText::new(&merge, Version::Theirs, 4);
        let shown: Vec<&str> = pane.lines().iter().map(|l| l.text.as_str()).collect();
        assert_eq!(shown, ["fn a() {", "    theirs();", "    more();", "}", "fn b() {}"]);
        assert_eq!(pane.segment(0), 0..1);
        assert_eq!(pane.segment(1), 1..3);
        assert_eq!(pane.segment(2), 3..5);
        assert_eq!(pane.longest, 13);
        // The file's own text is there to copy: tabs come back as tabs.
        assert_eq!(pane.source(1), Some("\ttheirs();"));
        assert_eq!(pane.source(0), None);
        let mut selection = Selection::default();
        selection.press(Pos::new(0, 7), 1, false, &pane);
        selection.drag_to(Pos::new(2, 11), &pane);
        assert_eq!(selection.text(&pane).as_deref(), Some("{\n\ttheirs();\n\tmore();"));

        let base_pane = PaneText::new(&merge, Version::Base, 8);
        assert_eq!(base_pane.lines()[1].text, "        one();");
        assert_eq!(base_pane.segment(1), 1..2);
        let ours_pane = PaneText::new(&merge, Version::Ours, 2);
        assert_eq!(ours_pane.lines()[1].text, "  ours();");
    }

    #[test]
    fn typed_text_becomes_shown_lines() {
        assert!(typed_hunk("", 4).lines.is_empty());
        let hunk = typed_hunk("a\tb\n\nc", 4);
        let lines: Vec<&str> = hunk.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(lines, ["a   b", "", "c"]);
    }

    #[test]
    fn colours_follow_the_spans_and_fade() {
        let text = "let x = 1;";
        let spans = [span(3, Class::Keyword), span(8, Class::Plain), span(9, Class::Number), span(10, Class::Punctuation)];
        let plain = theme::text_bright();
        let all = runs(text, 0..text.len(), &spans, plain);
        assert_eq!(
            all,
            [
                (0..3, Class::Keyword.color()),
                (3..8, plain),
                (8..9, Class::Number.color()),
                (9..10, Class::Punctuation.color()),
            ]
        );
        // One row of a wrapped line takes the runs it covers.
        assert_eq!(runs(text, 2..5, &spans, plain), [(2..3, Class::Keyword.color()), (3..5, plain)]);
        // Spans that no longer fit the text stop where they stop fitting.
        assert_eq!(runs("le", 0..2, &spans, plain), [(0..2, plain)]);
        assert_eq!(runs("let", 0..3, &[], plain), [(0..3, plain)]);

        let font = theme::code(12.5);
        let job = row_job(text, 0..text.len(), &spans, &font, plain, 0.0, 19.0);
        assert_eq!(job.text, text);
        assert!(job.sections.iter().all(|s| s.format.line_height == Some(19.0)));
        assert_eq!(job.sections[0].format.color, Class::Keyword.color());
        let faded = row_job(text, 0..text.len(), &spans, &font, plain, 0.5, 19.0);
        assert_eq!(faded.sections[0].format.color, theme::mix(theme::bg_app(), Class::Keyword.color(), 0.5));
    }

    #[test]
    fn spans_and_columns_map_back_to_the_file_text() {
        // Shown "    if  x" from "\tif\tx": a keyword, then plain.
        let source = "\tif\tx";
        assert_eq!(text::shown_with(source, 4), "    if  x");
        let spans = [span(4, Class::Plain), span(6, Class::Keyword), span(9, Class::Plain)];
        assert_eq!(source_spans(source, &spans, 4), [span(1, Class::Plain), span(3, Class::Keyword), span(5, Class::Plain)]);
        // A span ending inside a tab takes the whole tab.
        assert_eq!(source_spans(source, &[span(2, Class::Comment), span(9, Class::Plain)], 4), [
            span(1, Class::Comment),
            span(5, Class::Plain)
        ]);
        assert_eq!(source_spans("", &[], 4), []);

        // Shown columns to characters of the file's text.
        assert_eq!(source_char(source, 0, 4), 0);
        assert_eq!(source_char(source, 1, 4), 0); // inside the tab, nearer its start
        assert_eq!(source_char(source, 3, 4), 1); // nearer its end
        assert_eq!(source_char(source, 4, 4), 1);
        assert_eq!(source_char(source, 6, 4), 3); // the second tab, at its start
        assert_eq!(source_char(source, 8, 4), 4);
        assert_eq!(source_char(source, 40, 4), 5); // past the end
    }

    /// Lays `job` out with egui's own fonts.
    fn galley(job: LayoutJob) -> Arc<egui::Galley> {
        let ctx = egui::Context::default();
        let mut out = None;
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            out = Some(ui.painter().layout_job(job.clone()));
        });
        output.drop_without_applying_deltas();
        out.expect("laid out")
    }

    #[test]
    fn the_editor_puts_tabs_at_the_tab_stops() {
        let font = theme::mono(12.5);
        let space_w = galley(LayoutJob::simple_singleline("    ".into(), font.clone(), theme::text())).size().x / 4.0;
        let tab_w = galley(LayoutJob::simple_singleline("\t".into(), font.clone(), theme::text())).size().x;
        let x_of = |job: LayoutJob, row: usize, index: usize| {
            let laid = galley(job);
            let placed = &laid.rows[row];
            placed.pos.x + placed.row.glyphs[index].pos.x
        };
        for tab_width in [2, 4, 8] {
            let look = EditorLook {
                font: font.clone(),
                line_h: 19.0,
                color: theme::text(),
                space_w,
                tab_w,
                tab_width,
                wrap: None,
            };
            // "ab\tc": `c` sits at the first tab stop after column 2.
            let job = editor_job("ab\tc\n\t\tx", &look, &[]);
            assert_eq!(job.text, "ab\tc\n\t\tx", "the text itself is untouched");
            let stop = |columns: usize| columns as f32 * space_w;
            let first = if tab_width == 2 { 4 } else { tab_width };
            assert!((x_of(job.clone(), 0, 3) - stop(first)).abs() < 0.6, "tab width {tab_width}");
            assert!((x_of(job, 1, 2) - stop(tab_width * 2)).abs() < 0.6, "tab width {tab_width}");
        }

        // Colours go with the right characters around the tabs.
        let look =
            EditorLook { font, line_h: 19.0, color: theme::text(), space_w, tab_w, tab_width: 4, wrap: None };
        let job = editor_job("\tif\tx", &look, &[vec![span(1, Class::Plain), span(3, Class::Keyword)]]);
        let keyword = job.sections.iter().find(|s| &job.text[s.byte_range.as_usize()] == "if").expect("a section for if");
        assert_eq!(keyword.format.color, Class::Keyword.color());
        assert!(job.sections.iter().all(|s| s.format.line_height == Some(19.0)));
    }
}
