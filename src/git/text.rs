//! Lines of a file as the diff and blame views show them, and the way back
//! from what is shown to the file's own text, for copying.
//!
//! A shown line has no line ending, its tabs expanded to the next multiple of
//! four columns, control characters replaced by a dot, and is cut after
//! [`MAX_LINE_CHARS`] columns. Every character shown takes one column, so a
//! column is also a character index into the shown text.

use std::borrow::Cow;
use std::ops::Range;

/// Longer lines are cut for display (minified bundles and the like).
pub const MAX_LINE_CHARS: usize = 2_000;
const TAB_WIDTH: usize = 4;

/// A line as it is in the file: decoded, without its line ending.
pub fn source_text(content: &[u8]) -> Cow<'_, str> {
    let ending = ['\n', '\r'];
    match String::from_utf8_lossy(content) {
        Cow::Borrowed(text) => Cow::Borrowed(text.trim_end_matches(ending)),
        Cow::Owned(text) => Cow::Owned(text.trim_end_matches(ending).to_owned()),
    }
}

/// A line of the file as it is shown.
pub fn display_text(content: &[u8]) -> String {
    shown(&source_text(content))
}

/// What a character of the file looks like on screen.
#[derive(Clone, Copy)]
enum Look {
    Plain(char),
    /// A tab, as spaces up to the next tab stop.
    Tab,
    /// A control character, which has no glyph and confuses text layout.
    Dot,
    /// Where a long line is cut: one `…` stands for the rest of it.
    Cut,
}

/// One character of the file and the columns it takes on screen.
struct Piece {
    /// Bytes of the source text; for `Cut`, everything from there on.
    bytes: Range<usize>,
    columns: Range<usize>,
    look: Look,
}

fn pieces(source: &str) -> impl Iterator<Item = Piece> + '_ {
    let mut column = 0;
    let mut cut = false;
    source.char_indices().map_while(move |(at, ch)| {
        if cut {
            return None;
        }
        let start = column;
        let (width, look) = if column >= MAX_LINE_CHARS {
            cut = true;
            (1, Look::Cut)
        } else {
            match ch {
                '\t' => (TAB_WIDTH - column % TAB_WIDTH, Look::Tab),
                c if c.is_control() => (1, Look::Dot),
                c => (1, Look::Plain(c)),
            }
        };
        column += width;
        let end = if cut { source.len() } else { at + ch.len_utf8() };
        Some(Piece { bytes: at..end, columns: start..column, look })
    })
}

/// `source` (as from [`source_text`]) the way it is shown.
pub fn shown(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    for piece in pieces(source) {
        match piece.look {
            Look::Plain(c) => out.push(c),
            Look::Tab => out.extend(std::iter::repeat_n(' ', piece.columns.len())),
            Look::Dot => out.push('·'),
            Look::Cut => out.push('…'),
        }
    }
    out
}

/// The part of `source` that shows in `columns`. A character is included
/// when any of its columns is (a tab spans several); the `…` of a cut line
/// stands for the rest of it.
pub fn source_range(source: &str, columns: Range<usize>) -> &str {
    let mut bytes: Option<Range<usize>> = None;
    for piece in pieces(source) {
        if piece.columns.end <= columns.start {
            continue;
        }
        if piece.columns.start >= columns.end {
            break;
        }
        let start = bytes.as_ref().map_or(piece.bytes.start, |b| b.start);
        bytes = Some(start..piece.bytes.end);
    }
    bytes.map_or("", |bytes| &source[bytes])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_shown_with_tabs_expanded_and_controls_visible() {
        assert_eq!(display_text(b"a\tbc\td\r\n"), "a   bc  d");
        assert_eq!(display_text(b"\t\tx\n"), "        x");
        assert_eq!(display_text(b"bell\x07!"), "bell·!");
        assert_eq!(display_text("中\tx".as_bytes()), "中   x");
        assert_eq!(display_text(b""), "");
        assert_eq!(source_text(b"crlf\r\n"), "crlf");
    }

    #[test]
    fn long_lines_are_cut() {
        let long = "x".repeat(MAX_LINE_CHARS + 50);
        let shown = display_text(long.as_bytes());
        assert_eq!(shown.chars().count(), MAX_LINE_CHARS + 1);
        assert!(shown.ends_with("x…"));
        // Exactly the limit: nothing to cut.
        let exact = "y".repeat(MAX_LINE_CHARS);
        assert_eq!(display_text(exact.as_bytes()), exact);
    }

    #[test]
    fn shown_columns_map_back_to_the_source() {
        let source = "a\tbc\td";
        // "a   bc  d": the first tab takes columns 1-3, the second 6-7.
        assert_eq!(source_range(source, 0..1), "a");
        assert_eq!(source_range(source, 0..4), "a\t");
        assert_eq!(source_range(source, 4..9), "bc\td");
        // Part of a tab brings the whole tab.
        assert_eq!(source_range(source, 2..3), "\t");
        assert_eq!(source_range(source, 3..5), "\tb");
        assert_eq!(source_range(source, 0..100), source);
        assert_eq!(source_range(source, 5..5), "");
        assert_eq!(source_range("bell\x07!", 4..5), "\x07");
        assert_eq!(source_range("中文 ok", 1..4), "文 o");
    }

    #[test]
    fn the_cut_stands_for_the_rest_of_the_line() {
        let source = format!("{}tail", "x".repeat(MAX_LINE_CHARS));
        assert_eq!(source_range(&source, MAX_LINE_CHARS - 1..MAX_LINE_CHARS + 1), "xtail");
        assert_eq!(source_range(&source, 0..MAX_LINE_CHARS + 1), source);
    }
}
