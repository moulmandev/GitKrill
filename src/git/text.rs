//! Lines of a file as the diff, blame and conflict views show them, the way
//! back from what is shown to the file's own text (for copying), and the
//! text encodings files are read and written in.
//!
//! A shown line has no line ending, its tabs expanded to the next tab stop
//! (every [`tab_width`] columns, Preferences > Editor), control characters
//! replaced by a dot, and is cut after [`MAX_LINE_CHARS`] columns. Every
//! character shown takes one column, so a column is also a character index
//! into the shown text.
//!
//! Bytes become text in the repository's encoding (Preferences > Encoding,
//! see [`format_for`]); a file that starts with a UTF-16 byte order mark is
//! read as UTF-16 whatever the setting. Git's own data (paths, refs, commit
//! messages) never goes through here.
//!
//! The settings live in globals set by `prefs::editor::apply`: the reader
//! thread, the blame thread and the UI all read them, and a change bumps
//! [`generation`] so views loaded before it know to load again.

use std::borrow::Cow;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};

/// Longer lines are cut for display (minified bundles and the like).
pub const MAX_LINE_CHARS: usize = 2_000;
/// Tab stops every this many columns unless Preferences > Editor says else.
pub const DEFAULT_TAB_WIDTH: usize = 4;

// ---- encodings ----------------------------------------------------------------

/// A text encoding a repository's files can be in.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Encoding {
    #[default]
    #[serde(rename = "utf-8")]
    Utf8,
    /// UTF-8, written with a byte order mark.
    #[serde(rename = "utf-8-bom")]
    Utf8Bom,
    #[serde(rename = "utf-16le")]
    Utf16Le,
    #[serde(rename = "utf-16be")]
    Utf16Be,
    #[serde(rename = "windows-1252")]
    Windows1252,
    /// Latin-1 proper: every byte is the code point of the same number.
    #[serde(rename = "iso-8859-1")]
    Iso8859_1,
    #[serde(rename = "iso-8859-15")]
    Iso8859_15,
    #[serde(rename = "windows-1250")]
    Windows1250,
    #[serde(rename = "iso-8859-2")]
    Iso8859_2,
    #[serde(rename = "windows-1251")]
    Windows1251,
    #[serde(rename = "koi8-r")]
    Koi8R,
    #[serde(rename = "windows-1253")]
    Windows1253,
    #[serde(rename = "windows-1254")]
    Windows1254,
    #[serde(rename = "shift_jis")]
    ShiftJis,
    #[serde(rename = "euc-jp")]
    EucJp,
    #[serde(rename = "gbk")]
    Gbk,
    #[serde(rename = "gb18030")]
    Gb18030,
    #[serde(rename = "big5")]
    Big5,
    #[serde(rename = "euc-kr")]
    EucKr,
}

impl Encoding {
    /// Every encoding, in the order the preferences list them.
    pub const ALL: [Encoding; 19] = [
        Encoding::Utf8,
        Encoding::Utf8Bom,
        Encoding::Utf16Le,
        Encoding::Utf16Be,
        Encoding::Windows1252,
        Encoding::Iso8859_1,
        Encoding::Iso8859_15,
        Encoding::Windows1250,
        Encoding::Iso8859_2,
        Encoding::Windows1251,
        Encoding::Koi8R,
        Encoding::Windows1253,
        Encoding::Windows1254,
        Encoding::ShiftJis,
        Encoding::EucJp,
        Encoding::Gbk,
        Encoding::Gb18030,
        Encoding::Big5,
        Encoding::EucKr,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Encoding::Utf8 => "UTF-8",
            Encoding::Utf8Bom => "UTF-8 with BOM",
            Encoding::Utf16Le => "UTF-16 LE",
            Encoding::Utf16Be => "UTF-16 BE",
            Encoding::Windows1252 => "Windows-1252 (Western)",
            Encoding::Iso8859_1 => "ISO-8859-1 (Latin-1)",
            Encoding::Iso8859_15 => "ISO-8859-15 (Latin-9)",
            Encoding::Windows1250 => "Windows-1250 (Central European)",
            Encoding::Iso8859_2 => "ISO-8859-2 (Central European)",
            Encoding::Windows1251 => "Windows-1251 (Cyrillic)",
            Encoding::Koi8R => "KOI8-R (Cyrillic)",
            Encoding::Windows1253 => "Windows-1253 (Greek)",
            Encoding::Windows1254 => "Windows-1254 (Turkish)",
            Encoding::ShiftJis => "Shift_JIS (Japanese)",
            Encoding::EucJp => "EUC-JP (Japanese)",
            Encoding::Gbk => "GBK (Simplified Chinese)",
            Encoding::Gb18030 => "GB18030 (Simplified Chinese)",
            Encoding::Big5 => "Big5 (Traditional Chinese)",
            Encoding::EucKr => "EUC-KR (Korean)",
        }
    }

    /// The codec behind the ones `encoding_rs` does as the standard says.
    /// UTF-8, UTF-16 and Latin-1 are done here: `encoding_rs` cannot write
    /// UTF-16, and reads "ISO-8859-1" as Windows-1252 (as browsers do).
    fn codec(self) -> Option<&'static encoding_rs::Encoding> {
        Some(match self {
            Encoding::Utf8 | Encoding::Utf8Bom | Encoding::Utf16Le | Encoding::Utf16Be | Encoding::Iso8859_1 => {
                return None;
            }
            Encoding::Windows1252 => encoding_rs::WINDOWS_1252,
            Encoding::Iso8859_15 => encoding_rs::ISO_8859_15,
            Encoding::Windows1250 => encoding_rs::WINDOWS_1250,
            Encoding::Iso8859_2 => encoding_rs::ISO_8859_2,
            Encoding::Windows1251 => encoding_rs::WINDOWS_1251,
            Encoding::Koi8R => encoding_rs::KOI8_R,
            Encoding::Windows1253 => encoding_rs::WINDOWS_1253,
            Encoding::Windows1254 => encoding_rs::WINDOWS_1254,
            Encoding::ShiftJis => encoding_rs::SHIFT_JIS,
            Encoding::EucJp => encoding_rs::EUC_JP,
            Encoding::Gbk => encoding_rs::GBK,
            Encoding::Gb18030 => encoding_rs::GB18030,
            Encoding::Big5 => encoding_rs::BIG5,
            Encoding::EucKr => encoding_rs::EUC_KR,
        })
    }

    pub fn is_utf16(self) -> bool {
        matches!(self, Encoding::Utf16Le | Encoding::Utf16Be)
    }

    /// `bytes` as text. Never fails: what cannot be decoded becomes U+FFFD.
    /// A UTF-8 byte order mark at the start is dropped.
    pub fn decode(self, bytes: &[u8]) -> Cow<'_, str> {
        match self {
            Encoding::Utf8 | Encoding::Utf8Bom => String::from_utf8_lossy(bytes.strip_prefix(UTF8_BOM).unwrap_or(bytes)),
            Encoding::Utf16Le => Cow::Owned(decode_utf16(bytes, u16::from_le_bytes)),
            Encoding::Utf16Be => Cow::Owned(decode_utf16(bytes, u16::from_be_bytes)),
            Encoding::Iso8859_1 => match std::str::from_utf8(bytes) {
                Ok(ascii) if bytes.is_ascii() => Cow::Borrowed(ascii),
                _ => Cow::Owned(bytes.iter().map(|&b| char::from(b)).collect()),
            },
            other => match other.codec() {
                Some(codec) => codec.decode_without_bom_handling(bytes).0,
                None => String::from_utf8_lossy(bytes),
            },
        }
    }

    /// `text` in this encoding, without a byte order mark. Characters it
    /// cannot represent become `?`.
    pub fn encode(self, text: &str) -> Cow<'_, [u8]> {
        match self {
            Encoding::Utf8 | Encoding::Utf8Bom => Cow::Borrowed(text.as_bytes()),
            Encoding::Utf16Le => Cow::Owned(text.encode_utf16().flat_map(u16::to_le_bytes).collect()),
            Encoding::Utf16Be => Cow::Owned(text.encode_utf16().flat_map(u16::to_be_bytes).collect()),
            Encoding::Iso8859_1 => {
                if text.is_ascii() {
                    return Cow::Borrowed(text.as_bytes());
                }
                Cow::Owned(text.chars().map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?')).collect())
            }
            other => match other.codec() {
                Some(codec) => Cow::Owned(encode_with(codec, text)),
                None => Cow::Borrowed(text.as_bytes()),
            },
        }
    }

    /// The byte order mark written at the start of a file, if any.
    pub fn bom(self) -> &'static [u8] {
        match self {
            Encoding::Utf8Bom => UTF8_BOM,
            Encoding::Utf16Le => b"\xFF\xFE",
            Encoding::Utf16Be => b"\xFE\xFF",
            _ => b"",
        }
    }
}

const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";

fn decode_utf16(bytes: &[u8], unit: fn([u8; 2]) -> u16) -> String {
    let units = bytes.chunks(2).map(|pair| match pair {
        [a, b] => unit([*a, *b]),
        // An odd byte at the end: half a character.
        _ => 0xFFFD,
    });
    char::decode_utf16(units).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)).collect()
}

/// `encoding_rs` writes characters it cannot encode as HTML character
/// references (`&#12354;`), which is right for a web form and wrong for a
/// source file; a question mark is what editors write.
fn encode_with(codec: &'static encoding_rs::Encoding, text: &str) -> Vec<u8> {
    let mut encoder = codec.new_encoder();
    let mut out = Vec::with_capacity(text.len() + 16);
    let mut rest = text;
    loop {
        let room = encoder.max_buffer_length_from_utf8_without_replacement(rest.len()).unwrap_or(rest.len() * 4 + 16);
        out.reserve(room);
        let (result, read) = encoder.encode_from_utf8_to_vec_without_replacement(rest, &mut out, true);
        rest = &rest[read..];
        match result {
            encoding_rs::EncoderResult::InputEmpty => return out,
            encoding_rs::EncoderResult::OutputFull => {}
            encoding_rs::EncoderResult::Unmappable(_) => out.push(b'?'),
        }
    }
}

/// How a whole file was read, so it can be written back the same way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileEncoding {
    pub encoding: Encoding,
    /// It started with a byte order mark (always written back).
    pub bom: bool,
}

impl FileEncoding {
    /// Text back to the bytes of the file: the encoding, and the byte order
    /// mark when there was one or the encoding asks for it.
    pub fn encode(self, text: &str) -> Vec<u8> {
        let mut out = Vec::with_capacity(text.len() + 4);
        if self.bom || self.encoding == Encoding::Utf8Bom {
            out.extend_from_slice(self.encoding.bom());
        }
        out.extend_from_slice(&self.encoding.encode(text));
        out
    }
}

/// The encoding a file announces with a byte order mark, and the mark's length.
pub fn sniff_bom(bytes: &[u8]) -> Option<(Encoding, usize)> {
    if bytes.starts_with(UTF8_BOM) {
        Some((Encoding::Utf8Bom, 3))
    } else if bytes.starts_with(b"\xFF\xFE") {
        Some((Encoding::Utf16Le, 2))
    } else if bytes.starts_with(b"\xFE\xFF") {
        Some((Encoding::Utf16Be, 2))
    } else {
        None
    }
}

/// The encoding to read a whole file in: a byte order mark (UTF-16, or
/// UTF-8 with BOM) wins over `default`.
pub fn detect(bytes: &[u8], default: Encoding) -> FileEncoding {
    match sniff_bom(bytes) {
        Some((encoding, _)) => FileEncoding { encoding, bom: true },
        None => FileEncoding { encoding: default, bom: false },
    }
}

/// A whole file as text, read as [`detect`] says, without its byte order mark.
pub fn decode_file(bytes: &[u8], default: Encoding) -> (String, FileEncoding) {
    let found = detect(bytes, default);
    let body = if found.bom { &bytes[sniff_bom(bytes).map_or(0, |(_, len)| len)..] } else { bytes };
    (found.encoding.decode(body).into_owned(), found)
}

// ---- line endings -------------------------------------------------------------

/// The line endings GitKrill gives files it writes (Preferences > Editor).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Eol {
    /// Whatever the file has.
    #[default]
    Keep,
    Lf,
    Crlf,
}

/// `bytes` (in an encoding where line ends are the bytes CR and LF, which is
/// every one but UTF-16) with every line ending made `eol`.
pub fn convert_eol(bytes: &[u8], eol: Eol) -> Cow<'_, [u8]> {
    match eol {
        Eol::Keep => Cow::Borrowed(bytes),
        Eol::Lf => {
            if !bytes.windows(2).any(|pair| pair == b"\r\n") {
                return Cow::Borrowed(bytes);
            }
            let mut out = Vec::with_capacity(bytes.len());
            for (i, &b) in bytes.iter().enumerate() {
                if !(b == b'\r' && bytes.get(i + 1) == Some(&b'\n')) {
                    out.push(b);
                }
            }
            Cow::Owned(out)
        }
        Eol::Crlf => {
            let lone = |i: usize| bytes[i] == b'\n' && (i == 0 || bytes[i - 1] != b'\r');
            if !(0..bytes.len()).any(lone) {
                return Cow::Borrowed(bytes);
            }
            let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 16);
            for (i, &b) in bytes.iter().enumerate() {
                if lone(i) {
                    out.push(b'\r');
                }
                out.push(b);
            }
            Cow::Owned(out)
        }
    }
}

// ---- settings in effect -------------------------------------------------------

/// How a file's bytes become shown lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub encoding: Encoding,
    /// Columns between tab stops, at least 1.
    pub tab_width: usize,
    /// [`generation`] when this was looked up.
    pub generation: u64,
}

impl Default for Format {
    fn default() -> Self {
        Format { encoding: Encoding::Utf8, tab_width: DEFAULT_TAB_WIDTH, generation: 0 }
    }
}

static TAB_WIDTH: AtomicUsize = AtomicUsize::new(DEFAULT_TAB_WIDTH);
static GENERATION: AtomicU64 = AtomicU64::new(0);
/// Repositories whose encoding is not UTF-8, by working directory.
static ENCODINGS: RwLock<Vec<(String, Encoding)>> = RwLock::new(Vec::new());

/// Columns between tab stops in effect.
pub fn tab_width() -> usize {
    TAB_WIDTH.load(Ordering::Relaxed)
}

/// Goes up whenever the tab width or an encoding changes.
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

/// A working directory as a key that ignores how the path was spelled:
/// libgit2 writes `C:/repo/`, the tab may hold `C:\repo`.
fn dir_key(path: &Path) -> String {
    let joined = path
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/");
    if cfg!(windows) { joined.to_lowercase() } else { joined }
}

/// Puts the editor and encoding preferences into effect. Cheap when nothing
/// changed; bumps [`generation`] otherwise.
pub fn set_format(tab_width: usize, encodings: &[(PathBuf, Encoding)]) {
    let tab_width = tab_width.clamp(1, 16);
    let mut keyed: Vec<(String, Encoding)> = encodings
        .iter()
        .filter(|(_, encoding)| *encoding != Encoding::Utf8)
        .map(|(dir, encoding)| (dir_key(dir), *encoding))
        .collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    let mut changed = TAB_WIDTH.swap(tab_width, Ordering::Relaxed) != tab_width;
    {
        let mut current = ENCODINGS.write().unwrap_or_else(|e| e.into_inner());
        if *current != keyed {
            *current = keyed;
            changed = true;
        }
    }
    if changed {
        GENERATION.fetch_add(1, Ordering::Relaxed);
    }
}

/// The repository encoding of `workdir` (UTF-8 unless set).
pub fn encoding_for(workdir: &Path) -> Encoding {
    let list = ENCODINGS.read().unwrap_or_else(|e| e.into_inner());
    if list.is_empty() {
        return Encoding::Utf8;
    }
    let key = dir_key(workdir);
    list.iter().find(|(dir, _)| *dir == key).map_or(Encoding::Utf8, |(_, encoding)| *encoding)
}

/// How to show the files of the repository at `workdir`, as things stand.
pub fn format_for(workdir: &Path) -> Format {
    // Read first: a change racing with this lookup then makes the result
    // look stale rather than current.
    let generation = generation();
    Format { encoding: encoding_for(workdir), tab_width: tab_width(), generation }
}

// ---- shown lines ----------------------------------------------------------------

/// A line as it is in the file: decoded as UTF-8, without its line ending.
#[cfg(test)]
pub fn source_text(content: &[u8]) -> Cow<'_, str> {
    source_text_in(content, Encoding::Utf8)
}

/// A line as it is in the file, decoded from `encoding`, without its line
/// ending.
pub fn source_text_in(content: &[u8], encoding: Encoding) -> Cow<'_, str> {
    let ending = ['\n', '\r'];
    match encoding.decode(content) {
        Cow::Borrowed(text) => Cow::Borrowed(text.trim_end_matches(ending)),
        Cow::Owned(text) => Cow::Owned(text.trim_end_matches(ending).to_owned()),
    }
}

/// A line of the file as it is shown, in UTF-8 with the tab width in effect.
#[cfg(test)]
pub fn display_text(content: &[u8]) -> String {
    shown(&source_text(content))
}

/// A line of the file as it is shown in `format`.
pub fn display_text_in(content: &[u8], format: &Format) -> String {
    shown_with(&source_text_in(content, format.encoding), format.tab_width)
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

fn pieces(source: &str, tab_width: usize) -> impl Iterator<Item = Piece> + '_ {
    let tab_width = tab_width.max(1);
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
                '\t' => (tab_width - column % tab_width, Look::Tab),
                c if c.is_control() => (1, Look::Dot),
                c => (1, Look::Plain(c)),
            }
        };
        column += width;
        let end = if cut { source.len() } else { at + ch.len_utf8() };
        Some(Piece { bytes: at..end, columns: start..column, look })
    })
}

/// `source` (as from [`source_text`]) the way it is shown, with the tab
/// width in effect.
#[cfg(test)]
pub fn shown(source: &str) -> String {
    shown_with(source, tab_width())
}

/// `source` the way it is shown, with tab stops every `tab_width` columns.
pub fn shown_with(source: &str, tab_width: usize) -> String {
    let mut out = String::with_capacity(source.len());
    for piece in pieces(source, tab_width) {
        match piece.look {
            Look::Plain(c) => out.push(c),
            Look::Tab => out.extend(std::iter::repeat_n(' ', piece.columns.len())),
            Look::Dot => out.push('·'),
            Look::Cut => out.push('…'),
        }
    }
    out
}

/// One character of a source line and where it is in [`shown_with`]'s text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShownChar {
    /// Its bytes in the source; for the `…` of a cut line, everything left.
    pub source: Range<usize>,
    /// Its bytes in the shown text: the spaces of a tab, the `·` of a
    /// control character.
    pub shown: Range<usize>,
    pub columns: Range<usize>,
}

/// Where each character of `source` lands in the shown text, in order, with
/// tab stops every `tab_width` columns: the way between the file's text and
/// what is shown, for an editor working on the former laid out as the latter.
pub fn shown_chars(source: &str, tab_width: usize) -> impl Iterator<Item = ShownChar> + '_ {
    let mut at = 0;
    pieces(source, tab_width).map(move |piece| {
        let len = match piece.look {
            Look::Plain(c) => c.len_utf8(),
            Look::Tab => piece.columns.len(),
            Look::Dot => '·'.len_utf8(),
            Look::Cut => '…'.len_utf8(),
        };
        let shown = at..at + len;
        at += len;
        ShownChar { source: piece.bytes, shown, columns: piece.columns }
    })
}

/// The part of `source` that shows in `columns`, with the tab width in
/// effect. A character is included when any of its columns is (a tab spans
/// several); the `…` of a cut line stands for the rest of it.
pub fn source_range(source: &str, columns: Range<usize>) -> &str {
    source_range_with(source, columns, tab_width())
}

pub fn source_range_with(source: &str, columns: Range<usize>, tab_width: usize) -> &str {
    let mut bytes: Option<Range<usize>> = None;
    for piece in pieces(source, tab_width) {
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

// ---- word wrap ------------------------------------------------------------------

/// Where a shown line breaks into rows at most `width` columns wide: the
/// first column of every row after the first (empty when it fits).
///
/// A row breaks after the last space in it, so words stay whole, unless
/// that would leave the row less than a quarter full; then (and for a word
/// longer than a row) it breaks at the width. Spaces at a break stay at the
/// end of the row they follow, past the width, as editors do.
pub fn wrap_points(shown: &str, width: usize) -> Vec<usize> {
    let mut breaks = Vec::new();
    let width = width.max(1);
    // Most lines fit, and a line has no more columns than bytes.
    if shown.len() <= width {
        return breaks;
    }
    let mut row_start = 0;
    let mut after_space: Option<usize> = None;
    for (col, c) in shown.chars().enumerate() {
        if c == ' ' {
            after_space = Some(col + 1);
            continue;
        }
        if col >= row_start + width {
            let at = match after_space {
                Some(at) if at > row_start && at - row_start > width / 4 => at,
                _ => col,
            };
            breaks.push(at);
            row_start = at;
            after_space = None;
        }
    }
    breaks
}

/// How many rows [`wrap_points`] makes of a shown line.
pub fn wrapped_rows(shown: &str, width: usize) -> usize {
    if shown.len() <= width.max(1) {
        return 1;
    }
    wrap_points(shown, width).len() + 1
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
    fn tab_stops_follow_the_tab_width() {
        assert_eq!(shown_with("a\tbc\td", 8), "a       bc      d");
        assert_eq!(shown_with("\tx", 2), "  x");
        assert_eq!(shown_with("ab\tc", 1), "ab c");
        // A width of 0 is taken as 1 rather than dividing by zero.
        assert_eq!(shown_with("\tx", 0), " x");
        // Columns map back to the source with the same stops.
        assert_eq!(source_range_with("a\tbc", 1..8, 8), "\t");
        assert_eq!(source_range_with("a\tbc", 7..9, 8), "\tb");
        let format = Format { encoding: Encoding::Utf8, tab_width: 8, generation: 0 };
        assert_eq!(display_text_in(b"\tx\n", &format), "        x");
    }

    #[test]
    fn each_character_knows_where_it_is_shown() {
        let source = "a\t\u{e9}\u{7}b";
        let shown = shown_with(source, 4);
        assert_eq!(shown, "a   \u{e9}·b");
        let chars: Vec<ShownChar> = shown_chars(source, 4).collect();
        let at = |source: Range<usize>, shown: Range<usize>, columns: Range<usize>| ShownChar { source, shown, columns };
        assert_eq!(
            chars,
            [at(0..1, 0..1, 0..1), at(1..2, 1..4, 1..4), at(2..4, 4..6, 4..5), at(4..5, 6..8, 5..6), at(5..6, 8..9, 6..7)]
        );
        // The shown bytes line up with the shown text.
        assert!(chars.iter().all(|c| shown.is_char_boundary(c.shown.start) && shown.is_char_boundary(c.shown.end)));
        assert_eq!(&shown[chars[2].shown.clone()], "\u{e9}");
        assert_eq!(shown_chars("", 4).count(), 0);
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
        assert_eq!(source_range_with(source, 0..1, 4), "a");
        assert_eq!(source_range_with(source, 0..4, 4), "a\t");
        assert_eq!(source_range_with(source, 4..9, 4), "bc\td");
        // Part of a tab brings the whole tab.
        assert_eq!(source_range_with(source, 2..3, 4), "\t");
        assert_eq!(source_range_with(source, 3..5, 4), "\tb");
        assert_eq!(source_range_with(source, 0..100, 4), source);
        assert_eq!(source_range_with(source, 5..5, 4), "");
        assert_eq!(source_range_with("bell\x07!", 4..5, 4), "\x07");
        assert_eq!(source_range_with("中文 ok", 1..4, 4), "文 o");
    }

    #[test]
    fn the_cut_stands_for_the_rest_of_the_line() {
        let source = format!("{}tail", "x".repeat(MAX_LINE_CHARS));
        assert_eq!(source_range_with(&source, MAX_LINE_CHARS - 1..MAX_LINE_CHARS + 1, 4), "xtail");
        assert_eq!(source_range_with(&source, 0..MAX_LINE_CHARS + 1, 4), source);
    }

    /// Every encoding gives back the text it was given, byte order mark and all.
    #[test]
    fn encodings_round_trip() {
        let samples: &[(Encoding, &str)] = &[
            (Encoding::Utf8, "naïve café ✓ 中文"),
            (Encoding::Utf8Bom, "naïve café ✓ 中文"),
            (Encoding::Utf16Le, "naïve café ✓ 中文 𝄞"),
            (Encoding::Utf16Be, "naïve café ✓ 中文 𝄞"),
            (Encoding::Windows1252, "naïve café – “quoted” €5"),
            (Encoding::Iso8859_1, "naïve café ¼ ÿ"),
            (Encoding::Iso8859_15, "€ œ Š"),
            (Encoding::Windows1250, "Příliš žluťoučký kůň"),
            (Encoding::Iso8859_2, "Zażółć gęślą jaźń"),
            (Encoding::Windows1251, "Привет, мир"),
            (Encoding::Koi8R, "Привет, мир"),
            (Encoding::Windows1253, "Καλημέρα"),
            (Encoding::Windows1254, "Günaydın ışık"),
            (Encoding::ShiftJis, "こんにちは世界"),
            (Encoding::EucJp, "こんにちは世界"),
            (Encoding::Gbk, "你好，世界"),
            (Encoding::Gb18030, "你好，世界 €"),
            (Encoding::Big5, "你好，世界"),
            (Encoding::EucKr, "안녕하세요"),
        ];
        assert_eq!(samples.len(), Encoding::ALL.len());
        for &(encoding, text) in samples {
            let bytes = encoding.encode(text);
            assert_eq!(encoding.decode(&bytes), text, "{encoding:?}");
            if !matches!(encoding, Encoding::Utf8 | Encoding::Utf8Bom) {
                assert_ne!(&*bytes, text.as_bytes(), "{encoding:?} should not be UTF-8");
            }
            // Whole files, with the byte order mark the encoding has, if any.
            let marked = !encoding.bom().is_empty();
            let file = FileEncoding { encoding, bom: marked }.encode(text);
            assert!(file.starts_with(encoding.bom()));
            // Read with the setting at UTF-8: the mark, where there is one, decides.
            let default = if marked { Encoding::Utf8 } else { encoding };
            let (back, found) = decode_file(&file, default);
            assert_eq!(back, text, "{encoding:?}");
            assert_eq!(found, FileEncoding { encoding, bom: marked });
            assert_eq!(found.encode(&back), file, "{encoding:?}");
        }
        // Names are what settings.json keeps.
        assert_eq!(serde_json::to_string(&Encoding::Windows1252).unwrap(), "\"windows-1252\"");
    }

    #[test]
    fn single_byte_encodings_read_every_byte_back() {
        let every: Vec<u8> = (0..=255).collect();
        for encoding in [Encoding::Windows1252, Encoding::Iso8859_1, Encoding::Windows1251, Encoding::Koi8R] {
            let text = encoding.decode(&every);
            assert_eq!(&*encoding.encode(&text), &every[..], "{encoding:?}");
        }
        // Latin-1 is the code point of each byte; Windows-1252 has curly
        // quotes and the euro sign where Latin-1 has control codes.
        assert_eq!(Encoding::Iso8859_1.decode(b"\x80\xe9"), "\u{80}é");
        assert_eq!(Encoding::Windows1252.decode(b"\x80\x96\x93\xe9"), "€–“é");
    }

    #[test]
    fn unencodable_characters_become_question_marks() {
        assert_eq!(&*Encoding::Windows1252.encode("a中b"), b"a?b");
        assert_eq!(&*Encoding::Iso8859_1.encode("a€b"), b"a?b");
        assert_eq!(&*Encoding::ShiftJis.encode("aé"), b"a?");
        // Broken input decodes to replacement characters, not a failure.
        assert_eq!(Encoding::Utf8.decode(b"a\xffb"), "a\u{fffd}b");
        assert_eq!(Encoding::Utf16Le.decode(b"a\x00b"), "a\u{fffd}");
    }

    #[test]
    fn a_utf16_byte_order_mark_wins_over_the_setting() {
        let le = FileEncoding { encoding: Encoding::Utf16Le, bom: true }.encode("hi\r\n");
        assert_eq!(le, b"\xFF\xFEh\0i\0\r\0\n\0");
        for default in [Encoding::Utf8, Encoding::Windows1252, Encoding::Utf16Be] {
            let (text, found) = decode_file(&le, default);
            assert_eq!((text.as_str(), found), ("hi\r\n", FileEncoding { encoding: Encoding::Utf16Le, bom: true }));
        }
        let be = FileEncoding { encoding: Encoding::Utf16Be, bom: true }.encode("hi");
        assert_eq!(decode_file(&be, Encoding::Utf8).0, "hi");
        // A UTF-8 mark is kept for writing back, and is not part of the text.
        let (text, found) = decode_file(b"\xEF\xBB\xBFcaf\xC3\xA9", Encoding::Utf8);
        assert_eq!((text.as_str(), found.encoding, found.bom), ("café", Encoding::Utf8Bom, true));
        assert_eq!(found.encode(&text), b"\xEF\xBB\xBFcaf\xC3\xA9");
        // No mark: the setting decides, and UTF-8 with BOM adds one on writing.
        assert_eq!(decode_file(b"caf\xE9", Encoding::Windows1252).0, "café");
        assert_eq!(FileEncoding { encoding: Encoding::Utf8Bom, bom: false }.encode("x"), b"\xEF\xBB\xBFx");
        assert_eq!(FileEncoding { encoding: Encoding::Utf8, bom: false }.encode("x"), b"x");
        // A UTF-8 mark at the start of a line read on its own is not shown.
        assert_eq!(source_text(b"\xEF\xBB\xBFfirst\n"), "first");
    }

    #[test]
    fn line_endings_are_made_what_was_asked() {
        let mixed = b"a\r\nb\nc\r\n\nd";
        assert_eq!(&*convert_eol(mixed, Eol::Keep), mixed);
        assert_eq!(&*convert_eol(mixed, Eol::Lf), b"a\nb\nc\n\nd");
        assert_eq!(&*convert_eol(mixed, Eol::Crlf), b"a\r\nb\r\nc\r\n\r\nd");
        // Nothing to change: nothing copied.
        assert!(matches!(convert_eol(b"a\nb\n", Eol::Lf), Cow::Borrowed(_)));
        assert!(matches!(convert_eol(b"a\r\nb", Eol::Crlf), Cow::Borrowed(_)));
        // A lone carriage return is not a line ending.
        assert_eq!(&*convert_eol(b"a\rb\n", Eol::Crlf), b"a\rb\r\n");
        assert_eq!(&*convert_eol(b"\n", Eol::Crlf), b"\r\n");
    }

    #[test]
    fn lines_wrap_at_spaces_or_at_the_width() {
        let rows = |text: &str, width: usize| -> Vec<String> {
            let chars: Vec<char> = text.chars().collect();
            let mut starts = vec![0];
            starts.extend(wrap_points(text, width));
            starts.push(chars.len());
            starts.windows(2).map(|w| chars[w[0]..w[1]].iter().collect()).collect()
        };
        assert_eq!(rows("short", 10), ["short"]);
        assert_eq!(rows("exactly10!", 10), ["exactly10!"]);
        assert_eq!(rows("aaaa bbbb", 4), ["aaaa ", "bbbb"]);
        assert_eq!(rows("let x = compute(a, b);", 10), ["let x = ", "compute(a, ", "b);"]);
        // A word longer than a row is cut at the width.
        assert_eq!(rows("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        // A space too early in the row would waste it: cut at the width.
        assert_eq!(rows("a bcdefghijkl", 8), ["a bcdefg", "hijkl"]);
        // Wide characters count one column each, as everywhere in this module.
        assert_eq!(rows("中文中文中文", 4), ["中文中文", "中文"]);
        assert_eq!(wrapped_rows("", 5), 1);
        assert_eq!(wrapped_rows("abcdefghij", 3), 4);
        assert_eq!(wrapped_rows("abc", 0), 3);
        for (text, width) in [("let x = compute(a, b);", 10), ("abcdefghij", 4), ("aaaa bbbb", 4)] {
            assert_eq!(wrapped_rows(text, width), rows(text, width).len());
        }
    }

    #[test]
    fn settings_reach_the_readers_whatever_the_path_looks_like() {
        // `set_format` changes globals other tests read; this test only
        // looks at a directory no other test uses, and keeps the tab width.
        let dir = std::env::temp_dir().join("gitkrill-text-format-test");
        let before = generation();
        set_format(tab_width(), &[(dir.clone(), Encoding::Windows1252)]);
        assert!(generation() > before);
        let spelled = format!("{}{}", dir.display().to_string().replace('\\', "/"), "/");
        assert_eq!(encoding_for(Path::new(&spelled)), Encoding::Windows1252);
        assert_eq!(format_for(&dir).encoding, Encoding::Windows1252);
        assert_eq!(encoding_for(&dir.join("elsewhere")), Encoding::Utf8);
        // The same again changes nothing.
        let now = generation();
        set_format(tab_width(), &[(dir, Encoding::Windows1252)]);
        assert_eq!(generation(), now);
        set_format(tab_width(), &[]);
    }
}
