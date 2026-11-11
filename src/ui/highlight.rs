//! Syntax highlighting and word-level change emphasis for diffs.
//!
//! Nothing is worked out while painting. The first time a diff is painted,
//! every hunk is handed to a background thread, which fills in a
//! [`HunkStyle`] a batch of lines at a time; painting only looks the finished
//! lines up, and lines that are not ready yet are simply drawn plain. Results
//! are cached by the hunk's content, so a diff that is reloaded after staging
//! something (or because an unrelated file changed) keeps its colours without
//! a flicker.
//!
//! The old side (context and removed lines) and the new side (context and
//! added lines) are parsed as two separate streams, so a string opened on a
//! removed line cannot leak into the added lines after it. While the two
//! sides are in the same state (between the changes, mostly), a context line
//! is parsed once.
//!
//! Colours follow the whole file: when a diff has every line of the file
//! (a whole-file diff, `Hunk::gap` hunks between the real ones; or the
//! `FileDiff::whole` the loader adds to a diff of the hunks of a file no
//! larger than `git::diff::WHOLE_FOR_COLOURS_BYTES`), those lines are parsed
//! from the top as one stream, and each line shown takes its colours from
//! there, so a hunk that begins inside a block comment or a multi-line
//! string is coloured as such. Until the stream has got that far, a line
//! shows the colours of the same line in the version of the file shown
//! before (after staging, or saving in an editor), or else those of its
//! hunk parsed on its own, which is quick.
//!
//! Files of more than [`STREAM_MAX_LINES`] lines (or [`STREAM_MAX_BYTES`]),
//! whose diff of the hunks comes with the file's new version instead
//! (`FileDiff::new_file`), are not coloured line by line from the top: that
//! would keep the colours of every line, and take long. Each hunk is
//! coloured on its own first; then the parser is carried through the lines
//! between the hunks, which are not coloured, and each hunk is parsed again
//! from the state it starts in ([`Carried`]). Where that differs from what
//! the hunk shows already (it starts inside a block comment or a string),
//! its new colours take over. That goes on for [`CARRY_MAX_TIME`] at most;
//! hunks it has not reached by then keep their own colours. A whole-file
//! diff of such a file is coloured from the top for its first
//! [`STREAM_MAX_LINES`] lines, the rest left plain, as the hunks of any
//! diff are past [`MAX_LINES`].

use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, OnceLock, RwLock, RwLockReadGuard, Weak};
use std::time::{Duration, Instant};

use eframe::egui::text::{LayoutJob, TextFormat};
use eframe::egui::{self, Color32, FontId};
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxDefinition, SyntaxSet, SyntaxSetBuilder};

use crate::git::text::display_text_in;
use crate::git::{DiffLine, FileDiff, Hunk};
use crate::theme;

/// Lines of one diff that get syntax colours; whatever follows stays plain.
/// Parsing manages something like ten thousand lines a second, so this keeps
/// the background work for the largest diffs to a couple of seconds; the
/// lines come out top to bottom, and a diff opened meanwhile goes first.
const MAX_LINES: usize = 20_000;
/// The same limit in bytes, for diffs made of few but long lines.
const MAX_BYTES: usize = 1_500_000;
/// Longer lines are not parsed at all: minified bundles, embedded data.
const MAX_LINE_BYTES: usize = 1_000;
/// Word-level comparison is quadratic in the number of words; beyond this
/// many cells only the common start and end of the two lines are found.
const MAX_LCS_CELLS: usize = 60_000;
/// Pairing removed with added lines of unequal number compares every one
/// with every other; larger blocks than this are left unpaired.
const MAX_PAIRING_CELLS: usize = 256;
/// How many diffs keep their styling alive after they were last asked for.
const RECENT_DIFFS: usize = 6;
/// Finished lines are published this many at a time.
const BATCH: usize = 256;
/// Files with more lines than this (both versions together, as a whole-file
/// diff shows them) are not coloured from the top line by line; the loader
/// leaves out `FileDiff::whole` past it.
pub const STREAM_MAX_LINES: usize = crate::git::diff::WHOLE_FOR_COLOURS_LINES;
/// The same limit in bytes.
pub const STREAM_MAX_BYTES: usize = crate::git::diff::WHOLE_FOR_COLOURS_TEXT;
/// How long the parser is carried through a large file at most, in time
/// spent on it, for colours of its hunks that follow the file ([`Carried`]).
/// It gets through some 4,000 to 11,000 lines of Rust a second and 24,000
/// of JSON (`carry_speed` in the tests; the release build is no faster, the
/// time goes in syntect), so this covers files of 70,000 lines and more,
/// several megabytes. It runs on the highlight thread only while the diff
/// is shown and nothing newer waits; past it, the hunks not reached keep
/// their own colours.
const CARRY_MAX_TIME: Duration = Duration::from_secs(20);
/// No line: in a map from shown lines to the lines of a stream.
const NONE: u32 = u32::MAX;

// ---- token classes and their colours ---------------------------------------

/// What a piece of source text is, as far as colouring goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Class {
    Plain,
    Comment,
    Keyword,
    String,
    /// Escape sequences and format placeholders inside strings.
    Escape,
    /// Numbers and named constants (`true`, `None`, `NULL`).
    Number,
    Function,
    Type,
    /// Markup tags.
    Tag,
    /// Keys of key/value data, attribute names, CSS properties.
    Property,
    Punctuation,
    /// Added / removed lines inside a diff shown as a file (`.patch`).
    Inserted,
    Deleted,
    /// Operators: resolved to `Keyword` (`and`, `as`) or `Punctuation` (`+`)
    /// from the token's text before a span is stored.
    Operator,
    /// "Storage types": likewise resolved, to `Keyword` (`let`, `struct`) or
    /// `Type` (`u32`, `int`).
    Storage,
}

impl Class {
    /// The colour of this class in the theme in use.
    pub fn color(self) -> Color32 {
        if theme::palette().dark { self.dark_color() } else { self.light_color() }
    }

    /// Blends of the theme's accent colours (the lane colours used are
    /// violet, pink and mint) towards its text colour: light enough to read
    /// on the plain, the added (green) and the removed (red) line backgrounds
    /// alike. A test checks the contrast.
    fn dark_color(self) -> Color32 {
        use theme::mix;
        match self {
            Class::Plain => theme::text(),
            Class::Comment => mix(theme::text_dim(), theme::text(), 0.25),
            Class::Keyword | Class::Operator | Class::Storage => mix(theme::lanes()[4], theme::text_bright(), 0.45),
            Class::String => mix(theme::orange(), theme::text(), 0.5),
            Class::Escape => mix(theme::lanes()[1], theme::text_bright(), 0.5),
            Class::Number => mix(theme::yellow(), theme::text(), 0.3),
            Class::Function => mix(theme::blue(), theme::text_bright(), 0.4),
            Class::Type => mix(theme::lanes()[6], theme::text(), 0.4),
            Class::Tag => mix(theme::red(), theme::text_bright(), 0.45),
            Class::Property => mix(theme::accent(), theme::text_bright(), 0.5),
            Class::Punctuation => mix(theme::text(), theme::text_dim(), 0.35),
            Class::Inserted => mix(theme::green(), theme::text_bright(), 0.35),
            Class::Deleted => mix(theme::red(), theme::text_bright(), 0.35),
        }
    }

    /// The same hues as the dark colours, deep enough for a white page and
    /// its pale green and red line backgrounds. Blending towards the (nearly
    /// black) text would make them muddy, so they are picked one by one.
    fn light_color(self) -> Color32 {
        let rgb = |hex: u32| Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8);
        match self {
            Class::Plain => theme::text(),
            Class::Comment => rgb(0x56616d),
            Class::Keyword | Class::Operator | Class::Storage => rgb(0x8335c4),
            Class::String => rgb(0xa34d08),
            Class::Escape => rgb(0xbb1766),
            Class::Number => rgb(0x855700),
            Class::Function => rgb(0x1d5cbd),
            Class::Type => rgb(0x0a7262),
            Class::Tag => rgb(0xb3302a),
            Class::Property => rgb(0x0a6588),
            Class::Punctuation => theme::mix(theme::text(), theme::text_dim(), 0.35),
            Class::Inserted => rgb(0x18752f),
            Class::Deleted => rgb(0xb3302a),
        }
    }
}

/// Background behind the part of an added / removed line that actually
/// changed, over [`theme::diff_add_bg()`] / [`theme::diff_del_bg()`].
pub fn emphasis_color(origin: char) -> Color32 {
    // A little stronger on the light set's paler green, to stay clear of a
    // selected added line.
    let added = if theme::palette().dark { 0.3 } else { 0.36 };
    match origin {
        '+' => theme::mix(theme::diff_add_bg(), theme::green(), added),
        _ => theme::mix(theme::diff_del_bg(), theme::red(), 0.34),
    }
}

/// Scope prefixes and the class they stand for. For one scope the longest
/// matching prefix wins; for a stack of scopes the innermost that maps to
/// anything does.
const SELECTORS: &[(&str, Class)] = &[
    ("comment", Class::Comment),
    ("punctuation.definition.comment", Class::Comment),
    ("string", Class::String),
    ("punctuation.definition.string", Class::String),
    ("markup.raw", Class::String),
    ("constant.character.escape", Class::Escape),
    ("constant.other.placeholder", Class::Escape),
    ("string.regexp", Class::Escape),
    ("constant", Class::Number),
    ("support.constant", Class::Number),
    ("entity.name.constant", Class::Number),
    ("markup.bold", Class::Number),
    ("markup.italic", Class::Number),
    ("keyword", Class::Keyword),
    ("keyword.operator", Class::Operator),
    ("storage", Class::Keyword),
    ("storage.type", Class::Storage),
    ("variable.language", Class::Keyword),
    ("variable.type", Class::Type),
    ("punctuation.definition.heading", Class::Function),
    ("entity.name.function", Class::Function),
    ("support.function", Class::Function),
    ("variable.function", Class::Function),
    ("support.macro", Class::Function),
    ("entity.name.macro", Class::Function),
    ("markup.heading", Class::Function),
    ("entity.name.section", Class::Function),
    ("markup.underline.link", Class::Function),
    ("entity.name", Class::Type),
    ("entity.other.inherited-class", Class::Type),
    ("support.type", Class::Type),
    ("support.class", Class::Type),
    ("storage.modifier.lifetime", Class::Type),
    ("entity.name.tag", Class::Tag),
    ("entity.name.tag.yaml", Class::Property),
    ("entity.name.tag.toml", Class::Property),
    ("entity.other.attribute-name", Class::Property),
    ("support.type.property-name", Class::Property),
    ("variable.annotation", Class::Property),
    ("entity.name.label", Class::Property),
    ("punctuation", Class::Punctuation),
    ("markup.quote", Class::Comment),
    ("markup.inserted", Class::Inserted),
    ("markup.deleted", Class::Deleted),
];

/// File extensions the bundled syntaxes do not know, and the bundled syntax
/// that is close enough to be worth using for them.
const ALIASES: &[(&str, &str)] = &[
    // JSX is TSX without the types; the JavaScript grammar does not know it.
    ("jsx", "tsx"),
    ("mjs", "js"),
    ("cjs", "js"),
    ("mts", "ts"),
    ("cts", "ts"),
    ("jsonc", "json"),
    ("json5", "json"),
    ("webmanifest", "json"),
    ("pyi", "py"),
    ("mdx", "md"),
    ("zsh", "sh"),
    ("csproj", "xml"),
    ("fsproj", "xml"),
    ("vbproj", "xml"),
    ("props", "xml"),
    ("targets", "xml"),
    ("xaml", "xml"),
    ("resx", "xml"),
    ("ps1xml", "xml"),
    ("lock", "toml"),
    ("cfg", "ini"),
    ("conf", "ini"),
    ("editorconfig", "ini"),
    ("gitconfig", "ini"),
];

/// PowerShell. Bat's grammar for it needs regular expressions that the
/// pure-Rust engine syntect runs here does not have, so this smaller one
/// stands in: comments, strings (here-strings too) with the variables in
/// them, variables, keywords, `-eq`-style operators, parameters, cmdlet
/// names, types and numbers.
const POWERSHELL_SYNTAX: &str = r##"%YAML 1.2
---
name: PowerShell
file_extensions: [ps1, psm1, psd1]
scope: source.powershell
contexts:
  main:
    - match: '<#'
      scope: punctuation.definition.comment.begin.powershell
      push:
        - meta_scope: comment.block.powershell
        - match: '#>'
          scope: punctuation.definition.comment.end.powershell
          pop: true
    - match: '#.*$'
      scope: comment.line.number-sign.powershell
    - match: '@"(?=\s*$)'
      scope: punctuation.definition.string.begin.powershell
      push:
        - meta_scope: string.quoted.double.heredoc.powershell
        - match: '^"@'
          scope: punctuation.definition.string.end.powershell
          pop: true
        - include: interpolation
    - match: "@'(?=\\s*$)"
      scope: punctuation.definition.string.begin.powershell
      push:
        - meta_scope: string.quoted.single.heredoc.powershell
        - match: "^'@"
          scope: punctuation.definition.string.end.powershell
          pop: true
    - match: '"'
      scope: punctuation.definition.string.begin.powershell
      push:
        - meta_scope: string.quoted.double.powershell
        - match: '""'
          scope: constant.character.escape.powershell
        - include: interpolation
        - match: '"'
          scope: punctuation.definition.string.end.powershell
          pop: true
    - match: "'"
      scope: punctuation.definition.string.begin.powershell
      push:
        - meta_scope: string.quoted.single.powershell
        - match: "''"
          scope: constant.character.escape.powershell
        - match: "'"
          scope: punctuation.definition.string.end.powershell
          pop: true
    - match: '\$(?i:true|false|null)\b'
      scope: constant.language.powershell
    - match: '\$\{[^}]*\}|\$(?:\w+:)?\w+|\$[$?^]'
      scope: variable.other.readwrite.powershell
    - match: '(?i)\b(function|filter|workflow|configuration)\s+([\w-]+)'
      captures:
        1: storage.type.powershell
        2: entity.name.function.powershell
    - match: '(?i)\b(class|enum)\s+(\w+)'
      captures:
        1: storage.type.powershell
        2: entity.name.type.powershell
    - match: '(?i)\b(?:begin|break|catch|continue|data|do|dynamicparam|else|elseif|end|exit|finally|for|foreach|from|hidden|if|in|param|process|return|static|switch|throw|trap|try|until|using|while)\b'
      scope: keyword.control.powershell
    - match: '(?i)(?<![\w-])-(?:eq|ne|gt|ge|lt|le|like|notlike|match|notmatch|contains|notcontains|in|notin|replace|split|join|is|isnot|as|and|or|xor|not|band|bor|bxor|bnot|shl|shr|f|c(?:eq|ne|gt|ge|lt|le|like|notlike|match|notmatch|contains|notcontains|in|notin|replace|split)|i(?:eq|ne|gt|ge|lt|le|like|notlike|match|notmatch|contains|notcontains|in|notin|replace|split))\b'
      scope: keyword.other.word-operator.powershell
    - match: '(?<![\w-])-[A-Za-z]\w*'
      scope: entity.other.attribute-name.parameter.powershell
    - match: '(?<![\w$-])[A-Za-z]+-[A-Za-z]\w*'
      scope: support.function.cmdlet.powershell
    - match: '(\[)([A-Za-z][\w.]*(?:\[\])?)(\])'
      captures:
        1: punctuation.section.brackets.powershell
        2: support.type.powershell
        3: punctuation.section.brackets.powershell
    - match: '\b(?:0x[0-9A-Fa-f]+|\d+(?:\.\d+)?(?:[eE][+-]?\d+)?)(?i:[kmgtp]b)?\b'
      scope: constant.numeric.powershell
    - match: '[{}()\[\];,|=+*/%!<>.]'
      scope: punctuation.powershell
  interpolation:
    - match: '`.'
      scope: constant.character.escape.powershell
    - match: '\$\{[^}]*\}|\$(?:\w+:)?\w+'
      scope: constant.other.placeholder.powershell
"##;

/// TOML is not among the bundled syntaxes, and a Rust project's diffs are
/// full of it. This small grammar also does for INI-style files.
const TOML_SYNTAX: &str = r##"%YAML 1.2
---
name: TOML
file_extensions: [toml]
scope: source.toml
contexts:
  main:
    - include: comments
    - match: '^\s*(\[\[?)([^\]#]*)(\]\]?)'
      captures:
        1: punctuation.definition.table.toml
        2: entity.name.section.toml
        3: punctuation.definition.table.toml
    - match: '^\s*([A-Za-z0-9_.\-]+|"[^"]*"|''[^'']*'')\s*(=|:)'
      captures:
        1: entity.name.tag.toml
        2: punctuation.separator.key-value.toml
    - include: values
  comments:
    - match: '(#|^\s*;).*$'
      scope: comment.line.number-sign.toml
  values:
    - match: '"""'
      scope: punctuation.definition.string.begin.toml
      push:
        - meta_scope: string.quoted.triple.toml
        - match: '\\.'
          scope: constant.character.escape.toml
        - match: '"""'
          scope: punctuation.definition.string.end.toml
          pop: true
    - match: "'''"
      scope: punctuation.definition.string.begin.toml
      push:
        - meta_scope: string.quoted.triple.toml
        - match: "'''"
          scope: punctuation.definition.string.end.toml
          pop: true
    - match: '"'
      scope: punctuation.definition.string.begin.toml
      push:
        - meta_scope: string.quoted.double.toml
        - match: '\\.'
          scope: constant.character.escape.toml
        - match: '"|$'
          scope: punctuation.definition.string.end.toml
          pop: true
    - match: "'"
      scope: punctuation.definition.string.begin.toml
      push:
        - meta_scope: string.quoted.single.toml
        - match: "'|$"
          scope: punctuation.definition.string.end.toml
          pop: true
    - match: '\b(true|false|yes|no|on|off)\b'
      scope: constant.language.toml
    - match: '\b\d{4}-\d{2}-\d{2}([Tt ]\d{2}:\d{2}:\d{2}(\.\d+)?)?([Zz]|[+-]\d{2}:\d{2})?'
      scope: constant.numeric.date.toml
    - match: '[+-]?\b(0x[0-9A-Fa-f_]+|0o[0-7_]+|0b[01_]+|\d[\d_]*(\.[\d_]+)?([eE][+-]?\d+)?|inf|nan)\b'
      scope: constant.numeric.toml
    - match: '[A-Za-z0-9_\-]+(?=\s*=)'
      scope: entity.name.tag.toml
    - match: '[\[\]{},=]'
      scope: punctuation.toml
"##;

// ---- results ----------------------------------------------------------------

/// A run of text in one colour. Runs follow each other without gaps, so only
/// the end of each is stored (a byte offset into `DiffLine::text`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub end: u32,
    pub class: Class,
}

/// Coloured runs for the first lines of a hunk; it grows while the
/// background thread works through the hunk.
#[derive(Default, Clone, PartialEq, Debug)]
struct Lines {
    /// Per finished line, where its runs end in `spans`.
    ends: Vec<u32>,
    spans: Vec<Span>,
}

impl Lines {
    /// The runs of line `line`, once it is done.
    fn get(&self, line: usize) -> Option<&[Span]> {
        let end = *self.ends.get(line)?;
        let start = if line == 0 { 0 } else { self.ends[line - 1] };
        self.spans.get(start as usize..end as usize)
    }
}

/// One emphasised stretch: line index, then the range in characters.
type Stretch = (u32, u32, u32);

/// Styling of one hunk. Shared between every diff that shows the same hunk.
#[derive(Default)]
pub struct HunkStyle {
    syntax: RwLock<Lines>,
    /// The changed parts of paired removed / added lines, sorted by line.
    emphasis: OnceLock<Vec<Stretch>>,
}

/// Styling of one diff: one entry per hunk.
pub struct DiffStyle {
    hunks: Vec<Arc<HunkStyle>>,
    /// The colours of the whole file, when the diff has all its lines.
    stream: Option<StreamLink>,
    /// The colours of its hunks parsed from the state the file is in where
    /// each starts, when the file is too large for `stream`.
    carried: Option<Arc<Carried>>,
}

/// The colours of the hunks of a file too large to colour from the top
/// (see the module comment): what the parser makes of each hunk when it is
/// carried there through the lines before it, which come from
/// `FileDiff::new_file` and are parsed without keeping their colours. Shared
/// by every diff with the same hunks of the same file, and worked out while
/// one of them is shown.
pub struct Carried {
    /// Per hunk: its lines coloured from where the file is at its start, set
    /// all at once when the parser is done with it; empty when those are the
    /// colours the hunk has on its own. Until the parser gets there, the
    /// colours of the same hunk in the version shown before, if any.
    hunks: Vec<RwLock<Lines>>,
    /// Per hunk: how many of its lines are coloured (the diff's share of
    /// [`MAX_LINES`]), and its own colours, to compare with.
    limits: Vec<usize>,
    own: Vec<Weak<HunkStyle>>,
    /// Per hunk: its content key and the line of the new version it starts
    /// at, to find it again in the next version of the diff.
    marks: Vec<(u64, u32)>,
    /// How many hunks, from the first, are done.
    reached: AtomicUsize,
    /// The parser stopped short: out of time, or the file did not match.
    given_up: AtomicBool,
    link: Mutex<CarryLink>,
}

/// Who a [`Carried`] works for.
#[derive(Default)]
struct CarryLink {
    /// The diffs it was asked for. While none of them is alive (the diff was
    /// closed, or replaced by one of other content), the work stops.
    users: Vec<Weak<FileDiff>>,
    /// Its job, set aside while nobody shows it; a diff that shows it again
    /// puts it back in the queue.
    parked: Option<Job>,
}

impl Carried {
    /// Whether some diff it is for is alive besides `mine`, which the
    /// worker holds while it reads from it.
    fn shown(&self, mine: &Arc<FileDiff>) -> bool {
        if Arc::strong_count(mine) > 1 {
            return true;
        }
        let link = self.link.lock().unwrap_or_else(|e| e.into_inner());
        link.users.iter().any(|user| user.strong_count() > 0 && !std::ptr::eq(user.as_ptr(), Arc::as_ptr(mine)))
    }

    /// A diff it is for that is still alive.
    fn user(&self) -> Option<Arc<FileDiff>> {
        let mut link = self.link.lock().unwrap_or_else(|e| e.into_inner());
        link.users.retain(|user| user.strong_count() > 0);
        link.users.iter().find_map(Weak::upgrade)
    }
}

/// Where the lines of a diff are in the stream of every line of the file
/// (see the module comment), and that stream's colours.
struct StreamLink {
    style: Arc<HunkStyle>,
    /// The first shown line of every hunk among all of them.
    starts: Vec<usize>,
    /// The stream line of every shown line, hunk after hunk ([`NONE`]: not
    /// in it); `None` when the stream is the shown lines themselves.
    map: Option<Vec<u32>>,
    /// Colours of the same lines in the version shown before.
    seed: Option<Seed>,
}

/// Colours of an earlier stream of the same file, for the lines of this one
/// that it has too (same line number, same text).
struct Seed {
    style: Arc<HunkStyle>,
    /// The earlier stream's line for every line of this one ([`NONE`]: none).
    map: Vec<u32>,
}

/// Read access to one hunk's styling while its rows are painted.
pub struct HunkView<'a> {
    syntax: RwLockReadGuard<'a, Lines>,
    emphasis: &'a [Stretch],
    stream: Option<StreamView<'a>>,
    /// The hunk's colours from the file's state where it starts.
    carried: Option<RwLockReadGuard<'a, Lines>>,
}

/// One hunk's part of a [`StreamLink`], while it is painted.
struct StreamView<'a> {
    lines: RwLockReadGuard<'a, Lines>,
    /// The hunk's first line in the stream, or its lines' places there.
    start: usize,
    map: Option<&'a [u32]>,
    seed: Option<(RwLockReadGuard<'a, Lines>, &'a [u32])>,
}

/// A read lock that a worker dying mid-append does not spoil: what is in
/// there is still a consistent prefix.
fn read(lines: &RwLock<Lines>) -> RwLockReadGuard<'_, Lines> {
    lines.read().unwrap_or_else(|e| e.into_inner())
}

impl DiffStyle {
    pub fn hunk(&self, index: usize) -> Option<HunkView<'_>> {
        let hunk = self.hunks.get(index)?;
        let stream = self.stream.as_ref().and_then(|link| {
            let start = *link.starts.get(index)?;
            let end = link.starts.get(index + 1).copied().unwrap_or(start);
            Some(StreamView {
                lines: read(&link.style.syntax),
                start,
                map: link.map.as_ref().and_then(|map| map.get(start..end)),
                seed: link.seed.as_ref().map(|seed| (read(&seed.style.syntax), seed.map.as_slice())),
            })
        });
        let carried = self.carried.as_ref().and_then(|carried| carried.hunks.get(index)).map(read);
        Some(HunkView { syntax: read(&hunk.syntax), emphasis: hunk.emphasis.get().map_or(&[], Vec::as_slice), stream, carried })
    }
}

impl StreamView<'_> {
    /// The runs of the hunk's line `line` from the whole file's colours, or
    /// until those got there, from the earlier version's.
    fn spans(&self, line: usize) -> Option<&[Span]> {
        let at = match self.map {
            Some(map) => map.get(line).copied().filter(|at| *at != NONE)? as usize,
            None => self.start + line,
        };
        self.lines.get(at).or_else(|| {
            let (seed, map) = self.seed.as_ref()?;
            seed.get(*map.get(at).filter(|at| **at != NONE)? as usize)
        })
    }
}

impl HunkView<'_> {
    /// Coloured runs of a line; empty while (or if) it is not highlighted.
    pub fn spans(&self, line: usize) -> &[Span] {
        if let Some(spans) = self.stream.as_ref().and_then(|stream| stream.spans(line)) {
            return spans;
        }
        if let Some(spans) = self.carried.as_ref().and_then(|carried| carried.get(line)) {
            return spans;
        }
        self.syntax.get(line).unwrap_or(&[])
    }

    /// Character ranges of a line that differ from the line it is paired with.
    pub fn emphasis(&self, line: usize) -> impl Iterator<Item = Range<usize>> + '_ {
        let line = line as u32;
        let first = self.emphasis.partition_point(|s| s.0 < line);
        self.emphasis[first..]
            .iter()
            .take_while(move |s| s.0 == line)
            .map(|s| s.1 as usize..s.2 as usize)
    }
}

/// The text of a diff line as a layout job with one section per colour.
#[cfg(test)]
pub fn layout(text: &str, spans: &[Span], font: &FontId) -> LayoutJob {
    layout_part(text, 0..text.len(), spans, font)
}

/// Bytes `part` of a diff line (one row of a wrapped line) as a layout job
/// with one section per colour. `spans` are those of the whole line.
pub fn layout_part(text: &str, part: Range<usize>, spans: &[Span], font: &FontId) -> LayoutJob {
    let piece = text.get(part.clone()).unwrap_or("");
    let plain = || LayoutJob::simple_singleline(piece.to_owned(), font.clone(), theme::text());
    if spans.is_empty() || piece.is_empty() {
        return plain();
    }
    let mut job = LayoutJob { break_on_newline: false, ..Default::default() };
    let mut start = 0usize;
    for span in spans {
        let end = span.end as usize;
        // Spans come from parsing this very text; anything else would be a
        // cache mix-up, and plain text beats a panic in the layout code.
        if end <= start || end > text.len() || !text.is_char_boundary(end) {
            return plain();
        }
        let (from, to) = (start.max(part.start), end.min(part.end));
        if from < to {
            job.append(&text[from..to], 0.0, TextFormat::simple(font.clone(), span.class.color()));
        }
        start = end;
        if start >= part.end {
            return job;
        }
    }
    let from = start.max(part.start);
    if from < part.end {
        job.append(&text[from..part.end], 0.0, TextFormat::simple(font.clone(), theme::text()));
    }
    job
}

// ---- requesting styles (UI thread) -------------------------------------------

/// What a job colours.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Part {
    /// One hunk of the diff, on its own.
    Hunk(usize),
    /// Every line of the file from the top ([`stream_of`]).
    Stream,
}

struct Job {
    task: Task,
    file_name: Arc<str>,
    ctx: egui::Context,
    /// Which request the job came with; later requests have higher numbers
    /// and go first.
    request: u64,
}

enum Task {
    /// Colours lines of a diff.
    Colour {
        /// Kept alive so the work stays valid if the view moves on to a
        /// reloaded copy of the same diff, which shares the output below.
        diff: Arc<FileDiff>,
        part: Part,
        /// How many of the lines to parse (0: emphasis only).
        limit: usize,
        /// Dead once no diff shows this hunk any more; the job is then
        /// skipped.
        out: Weak<HunkStyle>,
        /// Where parsing stopped, if the job stepped aside for a later
        /// request.
        progress: Option<Box<Progress>>,
    },
    /// Carries the parser through a large file for its hunks' colours. It
    /// holds no diff: it reads from one of `Carried::link`'s while it runs.
    Carry {
        /// Dead once no diff style has it; the job is then dropped.
        out: Weak<Carried>,
        /// How far it has come.
        progress: Option<Box<Carry>>,
    },
}

/// What becomes of a job once the worker stops working on it.
enum Next {
    Done,
    /// Back in the queue: it stepped aside for a later request.
    Again,
    /// Set aside in its [`Carried`] until a diff shows that again.
    Park(Arc<Carried>),
}

#[derive(Default)]
struct Queue {
    jobs: Mutex<VecDeque<Job>>,
    ready: Condvar,
}

#[derive(Default)]
struct State {
    /// Newest first, with whether syntax colours were asked for.
    recent: VecDeque<(Weak<FileDiff>, bool, Arc<DiffStyle>)>,
    /// Hunk styles that are still referenced, by content hash.
    hunks: HashMap<u64, Weak<HunkStyle>>,
    /// The worker's queue, once the worker has been started.
    queue: Option<Arc<Queue>>,
    /// Number of requests that had work for the worker so far.
    requests: u64,
    /// The stream last coloured for each of the files shown lately, by
    /// path, newest first: what the next version of the file is seeded from.
    streams: VecDeque<(Arc<str>, Arc<FileDiff>, Arc<HunkStyle>)>,
    /// Carried colours that are still referenced, by content hash.
    carried: HashMap<u64, Weak<Carried>>,
    /// The carried colours last asked for each of the large files shown
    /// lately, by path, newest first: what the next version is seeded from.
    carried_by_path: VecDeque<(Arc<str>, Weak<Carried>)>,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Mutex::default);

/// The lines the file's colours are worked out on: every line of both
/// versions of the file, in file order. They are the diff's `whole` hunk
/// when it has one, or else the lines shown, which must then be the whole
/// file (see [`in_file_order`]).
fn stream_of(diff: &FileDiff) -> Vec<&DiffLine> {
    match &diff.whole {
        Some(whole) => whole.lines.iter().collect(),
        None => diff.hunks.iter().flat_map(|hunk| &hunk.lines).collect(),
    }
}

/// Whether `lines` are those of a file from its first line on, both
/// versions, in order: what a whole-file diff shows.
fn in_file_order(lines: &[&DiffLine]) -> bool {
    let (mut old, mut new) = (1u32, 1u32);
    for line in lines {
        let (on_old, on_new) = match line.origin {
            ' ' => (true, true),
            '-' => (true, false),
            '+' => (false, true),
            _ => return false,
        };
        if on_old {
            if line.old_no != Some(old) {
                return false;
            }
            old += 1;
        }
        if on_new {
            if line.new_no != Some(new) {
                return false;
            }
            new += 1;
        }
    }
    !lines.is_empty()
}

/// Every line of a stream by its number in the old and in the new version.
fn by_number(stream: &[&DiffLine]) -> (Vec<u32>, Vec<u32>) {
    let size = stream.len() + 2;
    let (mut old, mut new) = (vec![NONE; size], vec![NONE; size]);
    for (k, line) in stream.iter().enumerate() {
        if line.origin != '+'
            && let Some(slot) = line.old_no.and_then(|n| old.get_mut(n as usize))
        {
            *slot = k as u32;
        }
        if line.origin != '-'
            && let Some(slot) = line.new_no.and_then(|n| new.get_mut(n as usize))
        {
            *slot = k as u32;
        }
    }
    (old, new)
}

/// Where every shown line of `diff` is in `stream`, by its line number: a
/// removed line by its old one, the others by their new one. `None` if one
/// is not there, or differs (a mix-up: the stream is then not used).
fn map_shown(diff: &FileDiff, stream: &[&DiffLine]) -> Option<Vec<u32>> {
    let (old, new) = by_number(stream);
    diff.hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .map(|line| {
            let at = match line.origin {
                '-' => *old.get(line.old_no? as usize)?,
                _ => *new.get(line.new_no? as usize)?,
            };
            let found = stream.get(at as usize)?;
            (found.origin == line.origin && found.text == line.text).then_some(at)
        })
        .collect()
}

/// For every line of `stream`, the line of `earlier` (an earlier stream of
/// the same file) with the same number on the same side and the same text.
fn seed_map(stream: &[&DiffLine], earlier: &[&DiffLine]) -> Vec<u32> {
    let (old, new) = by_number(earlier);
    stream
        .iter()
        .map(|line| {
            let at = |table: &[u32], number: Option<u32>| {
                let at = *table.get(number? as usize)?;
                (at != NONE && earlier[at as usize].text == line.text).then_some(at)
            };
            match line.origin {
                '-' => at(&old, line.old_no),
                '+' => at(&new, line.new_no),
                _ => at(&new, line.new_no).or_else(|| at(&old, line.old_no)),
            }
            .unwrap_or(NONE)
        })
        .collect()
}

/// The stream of `diff` and how its shown lines map onto it, if it has one
/// small enough. A stream not coloured yet gets a job in `jobs`, and the
/// colours of the version of the file at `path` shown before as its seed.
/// A whole-file diff of a file too long for that is coloured from the top
/// for its first lines only, as many as the hunks of any diff would be.
fn link_stream(
    state: &mut State,
    diff: &Arc<FileDiff>,
    path: &str,
    file_name: &Arc<str>,
    mut job: impl FnMut(Part, usize, &Arc<HunkStyle>),
) -> Option<StreamLink> {
    let lines = stream_of(diff);
    if !in_file_order(&lines) {
        return None;
    }
    let mut bytes = 0;
    let limit = lines
        .iter()
        .take(STREAM_MAX_LINES)
        .take_while(|line| {
            bytes += line.text.len();
            bytes <= STREAM_MAX_BYTES
        })
        .count();
    if diff.whole.is_some() && limit < lines.len() {
        return None;
    }
    let map = match diff.whole {
        Some(_) => Some(map_shown(diff, &lines)?),
        None => None,
    };
    let mut starts = Vec::with_capacity(diff.hunks.len() + 1);
    let mut total = 0;
    for hunk in &diff.hunks {
        starts.push(total);
        total += hunk.lines.len();
    }
    starts.push(total);

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    ("stream", &**file_name, limit).hash(&mut hasher);
    for line in &lines[..limit] {
        (line.origin, line.text.as_str()).hash(&mut hasher);
    }
    let key = hasher.finish();
    let path: Arc<str> = path.into();
    let earlier = state.streams.iter().position(|(known, ..)| *known == path).and_then(|i| state.streams.remove(i));
    let (style, seed) = match state.hunks.get(&key).and_then(Weak::upgrade) {
        Some(style) => (style, None),
        None => {
            let style = Arc::new(HunkStyle::default());
            state.hunks.insert(key, Arc::downgrade(&style));
            job(Part::Stream, limit, &style);
            let seed = earlier
                .as_ref()
                .map(|(_, diff, style)| Seed { style: style.clone(), map: seed_map(&lines, &stream_of(diff)) });
            (style, seed)
        }
    };
    state.streams.push_front((path, diff.clone(), style.clone()));
    state.streams.truncate(RECENT_DIFFS);
    Some(StreamLink { style, starts, map, seed })
}

/// The carried colours of `diff` (see [`Carried`]), if it comes with its
/// file's new version: shared with the diffs of the same hunks of the same
/// file, or made anew (and then returned with its job), seeded from the
/// version of the file at `path` shown before. `keys`, `limits` and `own`
/// are the content key, the number of lines to colour and the styling of
/// each hunk, as [`style_for`] worked them out. A job that was set aside
/// because nobody showed the colours any more comes back to be queued.
fn link_carried(
    state: &mut State,
    diff: &Arc<FileDiff>,
    path: &str,
    file_name: &Arc<str>,
    (keys, limits, own): (&[u64], &[usize], &[Arc<HunkStyle>]),
    job: impl FnOnce(Weak<Carried>) -> Job,
) -> (Option<Arc<Carried>>, Option<Job>) {
    let Some(file) = &diff.new_file else { return (None, None) };
    if diff.hunks.iter().any(|hunk| hunk.gap) || limits.iter().all(|limit| *limit == 0) {
        return (None, None);
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    ("carried", &**file_name, file.id, diff.format.encoding, diff.format.tab_width, keys, limits).hash(&mut hasher);
    let key = hasher.finish();
    let path: Arc<str> = path.into();
    let earlier = state.carried_by_path.iter().position(|(known, _)| *known == path).and_then(|i| state.carried_by_path.remove(i));

    let (carried, job) = match state.carried.get(&key).and_then(Weak::upgrade) {
        Some(carried) => {
            let parked = {
                let mut link = carried.link.lock().unwrap_or_else(|e| e.into_inner());
                link.users.push(Arc::downgrade(diff));
                link.parked.take()
            };
            (carried, parked)
        }
        None => {
            let marks: Vec<(u64, u32)> =
                diff.hunks.iter().zip(keys).map(|(hunk, key)| (*key, new_range(&hunk.header).map_or(0, |(first, _)| first))).collect();
            // The hunks the earlier version had too, at the same place, take
            // the colours the parser gave them there until it gets to them.
            let earlier = earlier.as_ref().and_then(|(_, carried)| carried.upgrade());
            let seeds: HashMap<(u64, u32), usize> = earlier.as_ref().map_or_else(HashMap::new, |earlier| {
                let done = earlier.reached.load(Ordering::Acquire).min(earlier.marks.len());
                earlier.marks[..done].iter().enumerate().map(|(i, mark)| (*mark, i)).collect()
            });
            let hunks = marks
                .iter()
                .map(|mark| {
                    let seed = earlier.as_ref().zip(seeds.get(mark)).map(|(earlier, &i)| read(&earlier.hunks[i]).clone());
                    RwLock::new(seed.unwrap_or_default())
                })
                .collect();
            let carried = Arc::new(Carried {
                hunks,
                limits: limits.to_vec(),
                own: own.iter().map(Arc::downgrade).collect(),
                marks,
                reached: AtomicUsize::new(0),
                given_up: AtomicBool::new(false),
                link: Mutex::new(CarryLink { users: vec![Arc::downgrade(diff)], parked: None }),
            });
            state.carried.insert(key, Arc::downgrade(&carried));
            if state.carried.len() > 256 {
                state.carried.retain(|_, carried| carried.strong_count() > 0);
            }
            let job = job(Arc::downgrade(&carried));
            (carried, Some(job))
        }
    };
    state.carried_by_path.push_front((path, Arc::downgrade(&carried)));
    state.carried_by_path.truncate(RECENT_DIFFS);
    (Some(carried), job)
}

/// The styling for `diff`, a file called `path`: syntax colours when
/// `syntax` (Preferences > Editor), and the changed words either way.
/// Cheap enough to call every frame: after the first call for a diff this
/// is a pointer comparison.
pub fn style_for(ctx: &egui::Context, diff: &Arc<FileDiff>, path: &str, syntax: bool) -> Arc<DiffStyle> {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    // Comparing addresses is sound because the entry's weak reference keeps
    // the allocation from being reused for another diff.
    if let Some((_, _, style)) = state
        .recent
        .iter()
        .find(|(known, colours, _)| std::ptr::eq(known.as_ptr(), Arc::as_ptr(diff)) && *colours == syntax)
    {
        return style.clone();
    }

    let file_name: Arc<str> = path.rsplit(['/', '\\']).next().unwrap_or(path).to_lowercase().into();
    let request = state.requests + 1;
    let mut jobs = Vec::new();
    let new_job = |part: Part, limit: usize, style: &Arc<HunkStyle>| Job {
        task: Task::Colour { diff: diff.clone(), part, limit, out: Arc::downgrade(style), progress: None },
        file_name: file_name.clone(),
        ctx: ctx.clone(),
        request,
    };

    // The whole file, when the diff has it all; its job goes after those of
    // the hunks, which are quick.
    let mut stream_job = None;
    let stream = if syntax {
        link_stream(&mut state, diff, path, &file_name, |part, limit, style| stream_job = Some(new_job(part, limit, style)))
    } else {
        None
    };
    // Without syntax colours nothing is parsed: every hunk gets the word
    // emphasis only. Neither are the hunks of a whole-file diff coloured
    // from the stream: they are the stream.
    let shown_is_stream = stream.as_ref().is_some_and(|link| link.map.is_none());
    let (mut lines_left, mut bytes_left) = if syntax && !shown_is_stream { (MAX_LINES, MAX_BYTES) } else { (0, 0) };
    let mut hunks = Vec::with_capacity(diff.hunks.len());
    let (mut keys, mut limits) = (Vec::with_capacity(diff.hunks.len()), Vec::with_capacity(diff.hunks.len()));
    for (index, hunk) in diff.hunks.iter().enumerate() {
        let mut limit = 0;
        for line in hunk.lines.iter().take(lines_left) {
            if line.text.len() > bytes_left {
                lines_left = 0;
                break;
            }
            bytes_left -= line.text.len();
            limit += 1;
        }
        lines_left = lines_left.saturating_sub(limit);

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (limit, if limit > 0 { &*file_name } else { "" }).hash(&mut hasher);
        for line in &hunk.lines {
            (line.origin, line.text.as_str()).hash(&mut hasher);
        }
        let key = hasher.finish();

        let style = match state.hunks.get(&key).and_then(Weak::upgrade) {
            Some(style) => style,
            None => {
                let style = Arc::new(HunkStyle::default());
                state.hunks.insert(key, Arc::downgrade(&style));
                jobs.push(new_job(Part::Hunk(index), limit, &style));
                style
            }
        };
        hunks.push(style);
        keys.push(key);
        limits.push(limit);
    }
    jobs.extend(stream_job);

    // A large file's hunks parsed again from where the file is at each:
    // after the hunks' own colours, which show meanwhile.
    let carried = if syntax && stream.is_none() {
        let (carried, job) = link_carried(&mut state, diff, path, &file_name, (&keys, &limits, &hunks), |out| Job {
            task: Task::Carry { out, progress: None },
            file_name: file_name.clone(),
            ctx: ctx.clone(),
            request,
        });
        // One set aside by an earlier diff carries on for this one.
        jobs.extend(job.map(|job| Job { ctx: ctx.clone(), request, ..job }));
        carried
    } else {
        None
    };

    let style = Arc::new(DiffStyle { hunks, stream, carried });
    state.recent.push_front((Arc::downgrade(diff), syntax, style.clone()));
    state.recent.truncate(RECENT_DIFFS);
    if state.hunks.len() > 4096 {
        state.hunks.retain(|_, style| style.strong_count() > 0);
    }

    if !jobs.is_empty()
        && let Some(queue) = worker_queue(&mut state)
    {
        // Ahead of whatever is still queued for diffs opened earlier.
        state.requests = request;
        let mut queued = queue.jobs.lock().unwrap_or_else(|e| e.into_inner());
        for job in jobs.into_iter().rev() {
            queued.push_front(job);
        }
        queue.ready.notify_one();
    }
    style
}

/// Starts the worker thread on first use. `None` if it cannot be started, in
/// which case diffs simply stay plain.
fn worker_queue(state: &mut State) -> Option<Arc<Queue>> {
    if state.queue.is_none() {
        let queue = Arc::new(Queue::default());
        let theirs = queue.clone();
        std::thread::Builder::new()
            .name("diff-highlight".into())
            .spawn(move || worker(&theirs))
            .ok()?;
        state.queue = Some(queue);
    }
    state.queue.clone()
}

// ---- the worker --------------------------------------------------------------

fn worker(queue: &Queue) {
    let mut engine: Option<Engine> = None;
    let mut last_repaint = Instant::now();
    loop {
        let mut job = {
            let mut jobs = queue.jobs.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(job) = jobs.pop_front() {
                    break job;
                }
                jobs = queue.ready.wait(jobs).unwrap_or_else(|e| e.into_inner());
            }
        };
        let ctx = job.ctx.clone();
        // Asks for a frame, unless one was asked for lately and `force` is
        // not set; says whether it did.
        let mut repaint = |force: bool| {
            // Often enough to look live, not so often that a diff of a
            // thousand small hunks asks for a thousand frames.
            let ask = force || last_repaint.elapsed() > Duration::from_millis(40);
            if ask {
                ctx.request_repaint();
                last_repaint = Instant::now();
            }
            ask
        };
        // A later request is for what the user is looking at now.
        let request = job.request;
        let overtaken = || {
            let jobs = queue.jobs.lock().unwrap_or_else(|e| e.into_inner());
            jobs.front().is_some_and(|next| next.request > request)
        };
        let next = match &mut job.task {
            Task::Colour { diff, part, limit, out, progress } => {
                colour_job(&mut engine, &job.file_name, diff, *part, *limit, out, progress, &mut repaint, &overtaken)
            }
            Task::Carry { out, progress } => carry_job(&mut engine, &job.file_name, out, progress, &mut repaint, &overtaken),
        };

        let again = match next {
            Next::Done => false,
            Next::Again => true,
            Next::Park(carried) => {
                // Checked again under the lock that a diff showing it takes
                // to put it back in the queue, so it is never left aside
                // while one does.
                let mut link = carried.link.lock().unwrap_or_else(|e| e.into_inner());
                link.users.retain(|user| user.strong_count() > 0);
                if link.users.is_empty() {
                    link.parked = Some(job);
                    drop(link);
                    repaint(false);
                    continue;
                }
                true
            }
        };
        let mut jobs = queue.jobs.lock().unwrap_or_else(|e| e.into_inner());
        if again {
            // Back in line behind the newer requests, ahead of the rest of
            // its own diff.
            let at = jobs.iter().position(|other| other.request <= job.request).unwrap_or(jobs.len());
            jobs.insert(at, job);
        }
        let idle = jobs.is_empty();
        drop(jobs);
        repaint(idle);
    }
}

/// Works on a [`Task::Colour`]: the word emphasis of a hunk, then the
/// colours of `limit` of its lines (or of the whole file's).
#[allow(clippy::too_many_arguments)]
fn colour_job(
    engine: &mut Option<Engine>,
    file_name: &str,
    diff: &Arc<FileDiff>,
    part: Part,
    limit: usize,
    out: &Weak<HunkStyle>,
    progress: &mut Option<Box<Progress>>,
    repaint: &mut dyn FnMut(bool) -> bool,
    overtaken: &dyn Fn() -> bool,
) -> Next {
    let Some(out) = out.upgrade() else { return Next::Done };
    let hunk = match part {
        Part::Hunk(index) => match diff.hunks.get(index) {
            Some(hunk) => Some(hunk),
            None => return Next::Done,
        },
        Part::Stream => None,
    };
    let lines: Vec<&DiffLine> = match hunk {
        Some(hunk) => hunk.lines.iter().collect(),
        None => stream_of(diff),
    };
    if progress.is_none() {
        let _ = out.emphasis.set(hunk.map(emphasize_hunk).unwrap_or_default());
        repaint(false);
    }
    if limit == 0 {
        return Next::Done;
    }
    // A grammar that trips over some input must not take the thread (and
    // with it all later highlighting) down.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let engine = engine.get_or_insert_with(Engine::new);
        if progress.is_none() {
            let Some(syntax) = engine.syntax_for(file_name) else { return true };
            *progress = engine.start(syntax).map(Box::new);
        }
        let Some(progress) = progress.as_deref_mut() else { return true };
        engine.colour_lines(progress, &lines, limit, &out, &mut || {
            repaint(false);
            !overtaken()
        })
    }));
    match outcome {
        Ok(true) => Next::Done,
        Ok(false) => Next::Again,
        Err(_) => {
            *engine = None;
            Next::Done
        }
    }
}

/// Works on a [`Task::Carry`] for as long as a diff it is for is shown, no
/// later request waits and [`CARRY_MAX_TIME`] is not used up.
fn carry_job(
    engine: &mut Option<Engine>,
    file_name: &str,
    out: &Weak<Carried>,
    progress: &mut Option<Box<Carry>>,
    repaint: &mut dyn FnMut(bool) -> bool,
    overtaken: &dyn Fn() -> bool,
) -> Next {
    let Some(carried) = out.upgrade() else { return Next::Done };
    let Some(diff) = carried.user() else { return Next::Park(carried) };
    let started = Instant::now();
    let spent = progress.as_ref().map_or(Duration::ZERO, |carry| carry.spent);
    let in_time = || spent + started.elapsed() < CARRY_MAX_TIME;
    // Nothing shows until a hunk is done: a frame is asked for then only.
    let mut painted = carried.reached.load(Ordering::Acquire);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let engine = engine.get_or_insert_with(Engine::new);
        if progress.is_none() {
            let syntax = engine.syntax_for(file_name)?;
            *progress = Some(Box::new(engine.start_carry(syntax)?));
        }
        let carry = progress.as_deref_mut()?;
        Some(engine.carry(carry, &diff, &carried, &mut || {
            let reached = carried.reached.load(Ordering::Acquire);
            if reached != painted && repaint(false) {
                painted = reached;
            }
            !overtaken() && carried.shown(&diff) && in_time()
        }))
    }));
    if let Some(carry) = progress.as_deref_mut() {
        carry.spent += started.elapsed();
    }
    match outcome {
        Ok(Some(false)) if !in_time() => {
            carried.given_up.store(true, Ordering::Relaxed);
            Next::Done
        }
        Ok(Some(false)) if !carried.shown(&diff) => Next::Park(carried),
        Ok(Some(false)) => Next::Again,
        Ok(_) => Next::Done,
        Err(_) => {
            *engine = None;
            carried.given_up.store(true, Ordering::Relaxed);
            Next::Done
        }
    }
}

/// The loaded grammars plus what is needed to turn scopes into classes.
struct Engine {
    /// Our own grammars (TOML, PowerShell), which win for the file names
    /// they share with the others, then bat's (syntect's own among them).
    /// Two sets, not one: adding ours to bat's would have syntect link all
    /// two hundred grammars again, which takes half a second.
    sets: [SyntaxSet; 2],
    selectors: Vec<(Scope, Class)>,
    /// Class of a single scope, as resolved so far.
    classes: HashMap<Scope, Option<Class>>,
    comment: Option<Scope>,
    /// Scopes that mark the key of a key/value pair.
    key_scopes: Vec<Scope>,
    /// Syntax per lower-case file name.
    by_name: HashMap<String, Option<Grammar>>,
}

/// A syntax of the [`Engine`]: the set it is in, and where.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Grammar {
    set: usize,
    index: usize,
}

impl Engine {
    fn new() -> Self {
        let mut own = SyntaxSetBuilder::new();
        for source in [TOML_SYNTAX, POWERSHELL_SYNTAX] {
            if let Ok(syntax) = SyntaxDefinition::load_from_str(source, true, None) {
                own.add(syntax);
            }
        }
        Engine {
            sets: [own.build(), two_face::syntax::extra_newlines()],
            selectors: SELECTORS
                .iter()
                .filter_map(|(prefix, class)| Some((Scope::new(prefix).ok()?, *class)))
                .collect(),
            classes: HashMap::new(),
            comment: Scope::new("comment").ok(),
            key_scopes: ["meta.mapping.key", "meta.structure.dictionary.key"]
                .iter()
                .filter_map(|scope| Scope::new(scope).ok())
                .collect(),
            by_name: HashMap::new(),
        }
    }

    /// The syntax for a (lower-case) file name, if there is one worth
    /// parsing with.
    fn syntax_for(&mut self, file_name: &str) -> Option<Grammar> {
        if let Some(known) = self.by_name.get(file_name) {
            return *known;
        }
        let extension = file_name.rsplit_once('.').map_or("", |(_, ext)| ext);
        let alias = ALIASES.iter().find(|(from, _)| *from == extension).map(|(_, to)| *to);
        // Whole names first: "makefile", "cmakelists.txt", ".bashrc".
        let found = [Some(file_name), Some(extension), alias]
            .into_iter()
            .flatten()
            .filter(|token| !token.is_empty())
            .find_map(|token| {
                self.sets.iter().enumerate().find_map(|(set, syntaxes)| {
                    let syntax = syntaxes.find_syntax_by_extension(token)?;
                    let index = syntaxes.syntaxes().iter().position(|s| std::ptr::eq(s, syntax))?;
                    Some((syntax.name != "Plain Text").then_some(Grammar { set, index }))
                })
            })
            .flatten();
        self.by_name.insert(file_name.to_owned(), found);
        found
    }

    /// The name of a syntax.
    #[cfg(test)]
    fn name(&self, grammar: Grammar) -> &str {
        &self.sets[grammar.set].syntaxes()[grammar.index].name
    }

    fn class_of_scope(&mut self, scope: Scope) -> Option<Class> {
        if let Some(known) = self.classes.get(&scope) {
            return *known;
        }
        let class = self
            .selectors
            .iter()
            .filter(|(prefix, _)| prefix.is_prefix_of(scope))
            .max_by_key(|(prefix, _)| prefix.len())
            .map(|(_, class)| *class);
        self.classes.insert(scope, class);
        class
    }

    fn class_of(&mut self, stack: &[Scope]) -> Class {
        let within = |outer: Option<Scope>| outer.is_some_and(|o| stack.iter().any(|s| o.is_prefix_of(*s)));
        // Whatever a grammar marks up inside a comment, it reads as comment.
        if within(self.comment) {
            return Class::Comment;
        }
        let class = stack.iter().rev().find_map(|scope| self.class_of_scope(*scope)).unwrap_or(Class::Plain);
        // Keys of JSON-like data are strings to the grammar; telling them
        // from the values is most of what makes such a file readable.
        if class == Class::String && self.key_scopes.iter().any(|key| within(Some(*key))) {
            return Class::Property;
        }
        class
    }

    /// The parser state for the start of a hunk.
    fn start(&self, grammar: Grammar) -> Option<Progress> {
        let syntax = self.sets.get(grammar.set)?.syntaxes().get(grammar.index)?;
        let names_tell = syntax.name == "Rust";
        Some(Progress {
            old: Stream::new(ParseState::new(syntax), grammar.set, names_tell),
            new: Stream::new(ParseState::new(syntax), grammar.set, names_tell),
            next: 0,
            joined: true,
            keep_both: false,
        })
    }

    /// The start of a [`Carry`] through a file in `grammar`.
    fn start_carry(&self, grammar: Grammar) -> Option<Carry> {
        let mut sides = self.start(grammar)?;
        sides.keep_both = true;
        Some(Carry { sides, hunk: 0, under_way: None, at: 0, line: 1, spent: Duration::ZERO })
    }

    /// Carries the parser through the file of `diff` (`FileDiff::new_file`)
    /// from where `carry` is, and colours each hunk afresh from the state it
    /// finds at its start, into `carried`. `go_on` is asked after every
    /// batch of lines; when it says no, the rest is left for a later call
    /// and the result is `false`. `true` once every hunk that gets colours
    /// is done, or when the file turns out not to be the one the diff was
    /// made from (`Carried::given_up`).
    fn carry(&mut self, carry: &mut Carry, diff: &FileDiff, carried: &Carried, go_on: &mut dyn FnMut() -> bool) -> bool {
        let Some(file) = diff.new_file.as_deref() else { return true };
        let bytes = file.bytes.as_slice();
        let give_up = || {
            carried.given_up.store(true, Ordering::Relaxed);
            true
        };
        let Carry { sides, hunk: index, under_way, at, line, .. } = carry;
        let mut parsed = 0usize;
        while let Some(hunk) = diff.hunks.get(*index) {
            // The hunks past the diff's share of `MAX_LINES` get no colours.
            let limit = carried.limits.get(*index).copied().unwrap_or(0);
            if limit == 0 {
                return true;
            }
            if under_way.is_none() {
                let Some((first, count)) = new_range(&hunk.header) else { return give_up() };
                // The lines before it: the same in both versions, parsed
                // for the state they leave, their colours not kept.
                while *line < first {
                    let Some(text) = next_line(bytes, at, line) else { return give_up() };
                    sides.context(self, &display_text_in(text, &diff.format));
                    parsed += 1;
                    if parsed.is_multiple_of(BATCH) && !go_on() {
                        return false;
                    }
                }
                // Its own lines of the new version are the file's next ones:
                // a file that changed since the diff was made stops here.
                for shown in hunk.lines.iter().filter(|shown| shown.origin != '-') {
                    match next_line(bytes, at, line) {
                        Some(text) if display_text_in(text, &diff.format) == shown.text => {}
                        _ => return give_up(),
                    }
                }
                // Lines a diff cut short does not show.
                while *line < first + count && next_line(bytes, at, line).is_some() {}
                sides.next = 0;
                *under_way = Some(HunkStyle::default());
            }
            let lines: Vec<&DiffLine> = hunk.lines.iter().collect();
            let Some(scratch) = under_way.as_ref() else { return true };
            if !self.colour_lines(sides, &lines, limit, scratch, go_on) {
                return false;
            }
            let Some(scratch) = under_way.take() else { return true };
            let done = scratch.syntax.into_inner().unwrap_or_else(|e| e.into_inner());
            // Come out as the hunk did on its own (it starts where the
            // syntax does, as most do): its own colours are right, and stay,
            // without a second copy of them.
            let same = carried.own.get(*index).and_then(Weak::upgrade).is_some_and(|own| *read(&own.syntax) == done);
            if let Some(slot) = carried.hunks.get(*index) {
                *slot.write().unwrap_or_else(|e| e.into_inner()) = if same { Lines::default() } else { done };
            }
            *index += 1;
            carried.reached.store(*index, Ordering::Release);
            // Lines of it left unparsed: nothing after it gets colours.
            if limit < hunk.lines.len() {
                return true;
            }
        }
        true
    }

    /// [`Engine::colour_lines`] for the lines of a hunk.
    #[cfg(test)]
    fn colour_hunk(
        &mut self,
        progress: &mut Progress,
        hunk: &Hunk,
        limit: usize,
        out: &HunkStyle,
        go_on: &mut dyn FnMut() -> bool,
    ) -> bool {
        let lines: Vec<&DiffLine> = hunk.lines.iter().collect();
        self.colour_lines(progress, &lines, limit, out, go_on)
    }

    /// Parses the first `limit` of `lines` (a hunk's, or a whole file's)
    /// from where `progress` says, appending their coloured runs to `out` a
    /// batch at a time. `go_on` is asked after every batch; when it says no,
    /// the rest is left for a later call and the result is `false`.
    fn colour_lines(
        &mut self,
        progress: &mut Progress,
        lines: &[&DiffLine],
        limit: usize,
        out: &HunkStyle,
        go_on: &mut dyn FnMut() -> bool,
    ) -> bool {
        let lines = &lines[..limit.min(lines.len())];
        let Progress { old, new, next, joined, keep_both } = progress;

        // Context lines belong to both sides. They are coloured from the new
        // side, and fed to the old side only while it still has removed
        // lines coming that depend on them (to the end with `keep_both`).
        // While both sides are in the same state (`joined`), only the new
        // side is parsed, and the old one takes over its state when the two
        // part.
        let last_removed = lines.iter().rposition(|l| l.origin == '-');
        let any_added = lines.iter().any(|l| l.origin == '+');

        let mut batch = Lines::default();
        let flush = |batch: &mut Lines| {
            let mut syntax = out.syntax.write().unwrap_or_else(|e| e.into_inner());
            let base = syntax.spans.len() as u32;
            syntax.spans.append(&mut batch.spans);
            syntax.ends.extend(batch.ends.drain(..).map(|end| base + end));
        };
        while let Some(line) = lines.get(*next) {
            if *joined && line.origin != ' ' {
                old.take_state(new);
                *joined = false;
            }
            match line.origin {
                '-' => old.line(self, &line.text, Some(&mut batch.spans)),
                '+' => new.line(self, &line.text, Some(&mut batch.spans)),
                _ if *joined => new.line(self, &line.text, Some(&mut batch.spans)),
                _ if *keep_both => {
                    new.line(self, &line.text, Some(&mut batch.spans));
                    old.line(self, &line.text, None);
                    *joined = old.same_state(new);
                }
                _ if any_added => {
                    new.line(self, &line.text, Some(&mut batch.spans));
                    if last_removed.is_some_and(|last| *next < last) {
                        old.line(self, &line.text, None);
                        *joined = old.same_state(new);
                    }
                }
                _ => old.line(self, &line.text, Some(&mut batch.spans)),
            }
            batch.ends.push(batch.spans.len() as u32);
            *next += 1;
            if batch.ends.len() >= BATCH {
                flush(&mut batch);
                if *next < lines.len() && !go_on() {
                    return false;
                }
            }
        }
        flush(&mut batch);
        true
    }
}

/// How far the parsing of a hunk has come.
struct Progress {
    old: Stream,
    new: Stream,
    /// The first line that is not done yet.
    next: usize,
    /// The two sides are in the same state, kept by `new` alone.
    joined: bool,
    /// Both sides are kept where they are to the end of the lines, for what
    /// comes after them (a [`Carry`]), not only as far as their colours need.
    keep_both: bool,
}

impl Progress {
    /// Parses a line both sides have, keeping nothing of its colours.
    fn context(&mut self, engine: &mut Engine, text: &str) {
        self.new.line(engine, text, None);
        if !self.joined {
            self.old.line(engine, text, None);
            self.joined = self.old.same_state(&self.new);
        }
    }
}

/// How far the parser has been carried through a large file (see
/// [`Carried`]).
struct Carry {
    /// Both versions of the file, where the parser is in them.
    sides: Progress,
    /// The hunk being worked towards, or on.
    hunk: usize,
    /// That hunk's colours so far, once its lines are being parsed.
    under_way: Option<HunkStyle>,
    /// Where the next line of the new version starts in its bytes, and its
    /// number.
    at: usize,
    line: u32,
    /// Time spent on it so far.
    spent: Duration,
}

/// The lines of the new version a hunk covers, from its header (`@@ -a,b
/// +c,d @@`): the first one (for none, the one after the place they would
/// be), and how many.
fn new_range(header: &str) -> Option<(u32, u32)> {
    let new = header.strip_prefix("@@ -")?.split(' ').nth(1)?.strip_prefix('+')?;
    let mut numbers = new.split(',');
    let start: u32 = numbers.next()?.parse().ok()?;
    let count: u32 = numbers.next().map_or(Some(1), |n| n.parse().ok())?;
    Some((if count == 0 { start + 1 } else { start }, count))
}

/// The line of `bytes` that starts at `at`, line ending included; `at` and
/// the line number `line` move on to the next one.
fn next_line<'a>(bytes: &'a [u8], at: &mut usize, line: &mut u32) -> Option<&'a [u8]> {
    let rest = bytes.get(*at..).filter(|rest| !rest.is_empty())?;
    let len = rest.iter().position(|b| *b == b'\n').map_or(rest.len(), |end| end + 1);
    *at += len;
    *line += 1;
    Some(&rest[..len])
}

/// One side of a hunk being parsed line by line.
struct Stream {
    state: ParseState,
    /// Which of the engine's sets the syntax is in.
    set: usize,
    stack: ScopeStack,
    /// The parser gave up; the remaining lines of this side stay plain.
    failed: bool,
    /// See [`class_of_name`].
    names_tell: bool,
    buffer: String,
}

/// Words that grammars file under "storage type" together with the built-in
/// types (`let` next to `u32`, `class` next to `int`) but that read as
/// keywords.
const DECLARATION_WORDS: &[&str] = &[
    "let", "const", "static", "var", "val", "fn", "fun", "func", "function", "def", "class", "struct",
    "enum", "union", "trait", "impl", "type", "mod", "module", "interface", "namespace", "typedef",
    "async", "lambda", "object", "package", "extern", "record", "template", "typename", "using",
];

/// The identifiers in a piece of code, with their byte offsets.
fn identifiers(piece: &str) -> impl Iterator<Item = (usize, &str)> {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut rest = piece;
    std::iter::from_fn(move || {
        let start = rest.find(is_word)?;
        let len = rest[start..].find(|c| !is_word(c)).unwrap_or(rest.len() - start);
        let name = &rest[start..start + len];
        let offset = piece.len() - rest.len() + start;
        rest = &rest[start + len..];
        Some((offset, name))
    })
}

/// What an identifier the grammar left unmarked is, going by how it is
/// written. Only used for Rust, where the bundled grammar marks few type
/// names but the naming convention is all but universal: `CamelCase` is a
/// type, trait or variant, `UPPER_CASE` a constant.
fn class_of_name(name: &str) -> Option<Class> {
    if !name.starts_with(|c: char| c.is_ascii_uppercase()) {
        return None;
    }
    let shouting = name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if !shouting || name.len() == 1 {
        // A single capital is a generic parameter.
        return (!name.contains('_')).then_some(Class::Type);
    }
    Some(Class::Number)
}

impl Stream {
    fn new(state: ParseState, set: usize, names_tell: bool) -> Self {
        Stream { state, set, stack: ScopeStack::new(), failed: false, names_tell, buffer: String::new() }
    }

    /// Carries on from where `other` is.
    fn take_state(&mut self, other: &Stream) {
        self.state.clone_from(&other.state);
        self.stack.clone_from(&other.stack);
        self.failed = other.failed;
    }

    /// Whether the next line would come out the same on both.
    fn same_state(&self, other: &Stream) -> bool {
        self.failed == other.failed && self.stack == other.stack && self.state == other.state
    }

    /// Advances the parser over one line, appending the line's coloured runs
    /// to `spans` when given.
    fn line(&mut self, engine: &mut Engine, text: &str, mut spans: Option<&mut Vec<Span>>) {
        if self.failed || text.len() > MAX_LINE_BYTES {
            return;
        }
        self.buffer.clear();
        self.buffer.push_str(text);
        self.buffer.push('\n');
        let Ok(ops) = self.state.parse_line(&self.buffer, &engine.sets[self.set]) else {
            self.failed = true;
            return;
        };

        let first = spans.as_ref().map_or(0, |s| s.len());
        let names_tell = self.names_tell;
        let mut emit = |stack: &ScopeStack, from: usize, to: usize| {
            let Some(spans) = spans.as_deref_mut() else { return };
            let to = to.min(text.len());
            if to <= from {
                return;
            }
            // Extends the line's last run, or starts a new one.
            let mut push = |end: usize, class: Class| {
                let last = spans[first..].last_mut();
                if end <= last.as_ref().map_or(0, |last| last.end as usize) {
                    return;
                }
                match last {
                    Some(last) if last.class == class => last.end = end as u32,
                    _ => spans.push(Span { end: end as u32, class }),
                }
            };
            let piece = &text[from..to];
            let word = piece.trim();
            let starts_with_letter = word.starts_with(|c: char| c.is_alphabetic());
            match engine.class_of(stack.as_slice()) {
                Class::Operator if starts_with_letter => push(to, Class::Keyword),
                Class::Operator => push(to, Class::Punctuation),
                Class::Storage if DECLARATION_WORDS.contains(&word) => push(to, Class::Keyword),
                Class::Storage if starts_with_letter => push(to, Class::Type),
                Class::Storage => push(to, Class::Punctuation),
                Class::Plain if names_tell => {
                    for (start, name) in identifiers(piece) {
                        if let Some(class) = class_of_name(name) {
                            push(from + start, Class::Plain);
                            push(from + start + name.len(), class);
                        }
                    }
                    push(to, Class::Plain);
                }
                class => push(to, class),
            }
        };

        let mut at = 0usize;
        for (offset, op) in &ops {
            emit(&self.stack, at, *offset);
            at = at.max(*offset);
            if self.stack.apply(op).is_err() {
                self.failed = true;
                return;
            }
        }
        emit(&self.stack, at, text.len());
    }
}

// ---- word-level emphasis -----------------------------------------------------

/// Finds, for the removed lines that are directly replaced by added lines,
/// which parts of the two differ.
fn emphasize_hunk(hunk: &Hunk) -> Vec<Stretch> {
    let lines = &hunk.lines;
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].origin != '-' {
            i += 1;
            continue;
        }
        let removed = lines[i..].iter().take_while(|l| l.origin == '-').count();
        let added = lines[i + removed..].iter().take_while(|l| l.origin == '+').count();
        let (old, new) = (&lines[i..i + removed], &lines[i + removed..i + removed + added]);

        // (index in old, index in new, what differs), ascending in both.
        let pairs: Vec<(usize, usize, Change)> = if removed == added {
            // A block replaced by one of the same size: line for line.
            (0..removed).filter_map(|k| Some((k, k, emphasize(&old[k].text, &new[k].text)?))).collect()
        } else if removed * added <= MAX_PAIRING_CELLS {
            pair_by_similarity(old, new)
        } else {
            Vec::new()
        };
        // Old lines come first in the hunk, so this keeps the result sorted.
        out.extend(pairs.iter().flat_map(|(k, _, change)| {
            let line = (i + k) as u32;
            change.old.iter().map(move |r| (line, r.start, r.end))
        }));
        out.extend(pairs.iter().flat_map(|(_, k, change)| {
            let line = (i + removed + k) as u32;
            change.new.iter().map(move |r| (line, r.start, r.end))
        }));
        i += removed + added;
    }
    out
}

/// Which of the removed lines `old` and the added lines `new` that follow
/// them are versions of each other, when their numbers differ: the pairs
/// that word emphasis compares, in order on both sides. `None` when the
/// block is too large to compare every line with every other.
///
/// Lines that read the same (they differ in their line ending, or in the
/// newline at the end of the file) pair best of all, where emphasis leaves
/// them out for having nothing to mark.
pub fn similar_pairs(old: &[DiffLine], new: &[DiffLine]) -> Option<Vec<(usize, usize)>> {
    if old.len() * new.len() > MAX_PAIRING_CELLS {
        return None;
    }
    let m = new.len();
    let mut gains: Vec<Option<f32>> = Vec::with_capacity(old.len() * m);
    for removed in old {
        for added in new {
            gains.push(if removed.text == added.text {
                Some(1.0)
            } else {
                emphasize(&removed.text, &added.text).map(|c| c.similarity).filter(|s| *s >= 0.5)
            });
        }
    }
    Some(best_pairs(old.len(), m, |i, j| gains[i * m + j]))
}

/// Pairs removed with added lines when their numbers differ (one line became
/// two, a line was edited and another added after it): the pairing that
/// keeps the order of both sides and has the most in common overall, using
/// only pairs that are clearly versions of each other.
fn pair_by_similarity(old: &[DiffLine], new: &[DiffLine]) -> Vec<(usize, usize, Change)> {
    let (n, m) = (old.len(), new.len());
    let mut changes: Vec<Option<Change>> = Vec::with_capacity(n * m);
    for removed in old {
        for added in new {
            changes.push(emphasize(&removed.text, &added.text).filter(|c| c.similarity >= 0.5));
        }
    }
    let gain = |i: usize, j: usize| changes[i * m + j].as_ref().map(|c| c.similarity);
    let picked = best_pairs(n, m, gain);
    picked.into_iter().filter_map(|(i, j)| Some((i, j, changes[i * m + j].take()?))).collect()
}

/// The pairs `(i, j)` of `n` old and `m` new lines, ascending in both, with
/// the most `gain` in total; `gain` is `None` for lines that must not pair.
fn best_pairs(n: usize, m: usize, gain: impl Fn(usize, usize) -> Option<f32>) -> Vec<(usize, usize)> {
    // best[i][j]: the most that pairing old[i..] with new[j..] can gain.
    let width = m + 1;
    let mut best = vec![0f32; (n + 1) * width];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            let skip = best[(i + 1) * width + j].max(best[i * width + j + 1]);
            let pair = gain(i, j).map_or(0.0, |g| g + best[(i + 1) * width + j + 1]);
            best[i * width + j] = skip.max(pair);
        }
    }
    let mut picked = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        let here = best[i * width + j];
        if gain(i, j).is_some_and(|g| g + best[(i + 1) * width + j + 1] >= here) {
            picked.push((i, j));
            i += 1;
            j += 1;
        } else if best[(i + 1) * width + j] >= here {
            i += 1;
        } else {
            j += 1;
        }
    }
    picked
}

/// What differs between a removed line and the added line that replaces it.
#[derive(Debug, PartialEq)]
struct Change {
    /// Character ranges of the old line that are not in the new one.
    old: Vec<Range<u32>>,
    /// Character ranges of the new line that are not in the old one.
    new: Vec<Range<u32>>,
    /// Share of the longer line (whitespace aside) that both have: 0 to 1.
    similarity: f32,
}

/// A word, a run of whitespace, or a single other character.
#[derive(Clone, Copy)]
struct Token<'a> {
    text: &'a str,
    /// Position in characters.
    start: u32,
    end: u32,
}

impl Token<'_> {
    fn is_space(&self) -> bool {
        self.text.starts_with(char::is_whitespace)
    }
}

fn tokens(text: &str) -> Vec<Token<'_>> {
    #[derive(PartialEq, Clone, Copy)]
    enum Kind {
        Word,
        Space,
        Other,
    }
    let kind = |c: char| {
        if c.is_alphanumeric() || c == '_' {
            Kind::Word
        } else if c.is_whitespace() {
            Kind::Space
        } else {
            Kind::Other
        }
    };
    let mut out: Vec<Token> = Vec::new();
    let mut open: Option<(Kind, usize, u32)> = None;
    let mut count = 0u32;
    for (byte, c) in text.char_indices() {
        let k = kind(c);
        match open {
            Some((current, ..)) if current == k && k != Kind::Other => {}
            _ => {
                if let Some((_, from, start)) = open {
                    out.push(Token { text: &text[from..byte], start, end: count });
                }
                open = Some((k, byte, count));
            }
        }
        count += 1;
    }
    if let Some((_, from, start)) = open {
        out.push(Token { text: &text[from..], start, end: count });
    }
    out
}

/// Compares a removed line with an added one, word by word. `None` when
/// marking the difference would say nothing: the lines are equal, or have too
/// little in common to be versions of each other.
fn emphasize(old: &str, new: &str) -> Option<Change> {
    if old == new {
        return None;
    }
    let (a, b) = (tokens(old), tokens(new));
    let same = |x: &Token, y: &Token| x.text == y.text;

    let prefix = a.iter().zip(&b).take_while(|(x, y)| same(x, y)).count();
    let suffix = a[prefix..].iter().rev().zip(b[prefix..].iter().rev()).take_while(|(x, y)| same(x, y)).count();
    let (mid_a, mid_b) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);

    // Which tokens of the two middles are part of a longest common
    // subsequence, and therefore unchanged.
    let mut kept_a = vec![false; mid_a.len()];
    let mut kept_b = vec![false; mid_b.len()];
    let (n, m) = (mid_a.len(), mid_b.len());
    if n > 0 && m > 0 && n * m <= MAX_LCS_CELLS {
        let width = m + 1;
        let mut table = vec![0u16; (n + 1) * width];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                table[i * width + j] = if same(&mid_a[i], &mid_b[j]) {
                    table[(i + 1) * width + j + 1] + 1
                } else {
                    table[(i + 1) * width + j].max(table[i * width + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n && j < m {
            if same(&mid_a[i], &mid_b[j]) {
                kept_a[i] = true;
                kept_b[j] = true;
                i += 1;
                j += 1;
            } else if table[(i + 1) * width + j] >= table[i * width + j + 1] {
                i += 1;
            } else {
                j += 1;
            }
        }
    }

    // Lines with next to nothing in common were rewritten, not edited;
    // marking nearly all of both adds noise and no information.
    let weight = |tokens: &[Token]| -> u32 { tokens.iter().filter(|t| !t.is_space()).map(|t| t.end - t.start).sum() };
    let kept: u32 = weight(&a[..prefix])
        + weight(&a[a.len() - suffix..])
        + mid_a.iter().zip(&kept_a).filter(|(t, kept)| **kept && !t.is_space()).map(|(t, _)| t.end - t.start).sum::<u32>();
    let longest = weight(&a).max(weight(&b));
    let similarity = if longest == 0 { 0.0 } else { kept as f32 / longest as f32 };
    if similarity < 0.35 {
        return None;
    }

    let ranges = |middle: &[Token], kept: &[bool]| {
        let mut out: Vec<Range<u32>> = Vec::new();
        // True while nothing but unchanged whitespace has gone by since the
        // last changed token: the next change then extends the same stretch,
        // so "a b" -> "c d" reads as one change rather than two.
        let mut joins = false;
        for (token, kept) in middle.iter().zip(kept) {
            if *kept {
                joins &= token.is_space();
                continue;
            }
            match out.last_mut() {
                Some(last) if joins => last.end = token.end,
                _ => out.push(token.start..token.end),
            }
            joins = true;
        }
        out
    };
    Some(Change { old: ranges(mid_a, &kept_a), new: ranges(mid_b, &kept_b), similarity })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Loading the grammars takes a moment; the tests share one set.
    static ENGINE: LazyLock<Mutex<Engine>> = LazyLock::new(|| Mutex::new(Engine::new()));

    fn hunk(lines: &[(char, &str)]) -> Hunk {
        Hunk {
            header: String::new(),
            raw: Vec::new(),
            lines: lines
                .iter()
                .map(|(origin, text)| DiffLine { origin: *origin, old_no: None, new_no: None, text: (*text).to_owned() })
                .collect(),
            gap: false,
        }
    }

    /// Colours a hunk the way the worker does and returns every line cut
    /// into its `(text, class)` pieces. Uncoloured lines come back empty.
    fn colour(file_name: &str, lines: &[(char, &str)]) -> Vec<Vec<(String, Class)>> {
        let hunk = hunk(lines);
        let style = HunkStyle::default();
        let mut engine = ENGINE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut progress) = engine.syntax_for(file_name).and_then(|syntax| engine.start(syntax)) {
            assert!(engine.colour_hunk(&mut progress, &hunk, hunk.lines.len(), &style, &mut || true));
        }
        let diff = DiffStyle { hunks: vec![Arc::new(style)], stream: None, carried: None };
        let view = diff.hunk(0).unwrap();
        hunk.lines
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let mut start = 0;
                view.spans(i)
                    .iter()
                    .map(|span| {
                        let piece = line.text[start..span.end as usize].to_owned();
                        start = span.end as usize;
                        (piece, span.class)
                    })
                    .collect()
            })
            .collect()
    }

    fn class_of(line: &[(String, Class)], piece: &str) -> Class {
        line.iter()
            .find(|(text, _)| text.trim() == piece)
            .unwrap_or_else(|| panic!("no piece {piece:?} in {line:?}"))
            .1
    }

    #[test]
    fn rust_is_cut_into_keywords_strings_and_comments() {
        let lines = colour(
            "main.rs",
            &[(' ', "fn main() {"), ('+', "    let s = \"hi\"; // greeting"), (' ', "    println!(\"{}\", 42);"), (' ', "}")],
        );
        assert_eq!(class_of(&lines[0], "fn"), Class::Keyword);
        assert_eq!(class_of(&lines[0], "main"), Class::Function);
        assert_eq!(class_of(&lines[1], "let"), Class::Keyword);
        assert_eq!(class_of(&lines[1], "\"hi\""), Class::String);
        assert_eq!(class_of(&lines[1], "// greeting"), Class::Comment);
        assert_eq!(class_of(&lines[2], "42"), Class::Number);
        // Every line is covered exactly, with no piece left empty.
        for (line, text) in lines.iter().zip(["fn main() {", "    let s = \"hi\"; // greeting"]) {
            assert_eq!(line.iter().map(|(piece, _)| piece.as_str()).collect::<String>(), text);
            assert!(line.iter().all(|(piece, _)| !piece.is_empty()));
        }
    }

    #[test]
    fn rust_names_and_built_in_types_are_told_from_keywords() {
        let lines = colour(
            "lib.rs",
            &[
                ('+', "const MAX_LINES: usize = 3;"),
                ('+', "fn first<T: Clone>(items: &[T]) -> Option<T> {"),
                ('+', "    let found: Vec<String> = lookup(Selection::Wip, MAX_LINES);"),
            ],
        );
        assert_eq!(class_of(&lines[0], "const"), Class::Keyword);
        assert_eq!(class_of(&lines[0], "MAX_LINES"), Class::Number);
        assert_eq!(class_of(&lines[0], "usize"), Class::Type);
        assert_eq!(class_of(&lines[1], "Clone"), Class::Type);
        assert_eq!(class_of(&lines[1], "T"), Class::Type);
        assert_eq!(class_of(&lines[2], "let"), Class::Keyword);
        assert_eq!(class_of(&lines[2], "Vec"), Class::Type);
        assert_eq!(class_of(&lines[2], "String"), Class::Type);
        assert_eq!(class_of(&lines[2], "Wip"), Class::Type);
        assert_eq!(class_of(&lines[2], "MAX_LINES"), Class::Number);
        // Ordinary names are left alone.
        assert!(lines[2].iter().any(|(piece, class)| piece.contains("found") && *class == Class::Plain));
        for (line, text) in lines.iter().zip(["const MAX_LINES: usize = 3;"]) {
            assert_eq!(line.iter().map(|(piece, _)| piece.as_str()).collect::<String>(), text);
        }

        // Elsewhere capitals mean nothing in particular.
        let python = colour("a.py", &[('+', "Total = Compute(MAX)")]);
        assert!(python[0].iter().all(|(_, class)| !matches!(class, Class::Type | Class::Number)), "{python:?}");
    }

    #[test]
    fn the_old_side_does_not_leak_into_the_new_side() {
        let lines = colour(
            "lib.rs",
            &[
                (' ', "fn f() {"),
                ('-', "    /* a comment that is never closed"),
                ('+', "    let x = 1;"),
                (' ', "    let y = 2;"),
                ('-', "    still inside the old comment"),
            ],
        );
        assert_eq!(class_of(&lines[1], "/* a comment that is never closed"), Class::Comment);
        assert_eq!(class_of(&lines[2], "let"), Class::Keyword);
        // Context after the change belongs to the new side...
        assert_eq!(class_of(&lines[3], "let"), Class::Keyword);
        // ...while a later removed line still sees the old side's comment.
        assert_eq!(class_of(&lines[4], "still inside the old comment"), Class::Comment);
    }

    #[test]
    fn unknown_and_plain_text_files_stay_plain() {
        for name in ["data.xyz", "notes.txt", "noextension", ""] {
            let lines = colour(name, &[(' ', "fn main() {}"), ('+', "let x = \"1\";")]);
            assert!(lines.iter().all(Vec::is_empty), "{name}: {lines:?}");
        }
    }

    #[test]
    fn file_names_extensions_and_aliases_pick_a_syntax() {
        let mut engine = ENGINE.lock().unwrap_or_else(|e| e.into_inner());
        let mut name = |file: &str| engine.syntax_for(file).map(|grammar| engine.name(grammar).to_owned());
        for (file, syntax) in [
            ("main.rs", "Rust"),
            ("makefile", "Makefile"),
            ("app.ts", "TypeScript"),
            ("lib.mts", "TypeScript"),
            ("app.tsx", "TypeScriptReact"),
            // JSX goes with TSX, which knows its tags.
            ("app.jsx", "TypeScriptReact"),
            ("main.kt", "Kotlin"),
            ("build.gradle.kts", "Kotlin"),
            ("view.swift", "Swift"),
            ("main.dart", "Dart"),
            ("build.ps1", "PowerShell"),
            ("module.psm1", "PowerShell"),
            ("cargo.toml", "TOML"),
            ("cargo.lock", "TOML"),
            ("style.scss", "SCSS"),
            ("dockerfile", "Dockerfile"),
            ("app.vue", "Vue Component"),
            ("setup.cfg", "INI"),
            ("app.csproj", "XML"),
        ] {
            assert_eq!(name(file).as_deref(), Some(syntax), "{file}");
        }
        assert_eq!(name("archive.tar.gz"), None);
    }

    #[test]
    fn newer_languages_are_coloured() {
        let ts = colour("app.ts", &[('+', "interface Point { x: number; label?: string }"), ('+', "const p: Point = { x: 1 };")]);
        assert_eq!(class_of(&ts[0], "interface"), Class::Keyword);
        assert_eq!(class_of(&ts[0], "number"), Class::Type);
        assert_eq!(class_of(&ts[1], "1"), Class::Number);
        let tsx = colour("app.tsx", &[('+', "const view = <div className=\"a\">{n}</div>;")]);
        assert_eq!(class_of(&tsx[0], "div"), Class::Tag);
        let kotlin = colour("main.kt", &[('+', "fun main() { val s = \"hi\" }")]);
        assert_eq!(class_of(&kotlin[0], "fun"), Class::Keyword);
        assert_eq!(class_of(&kotlin[0], "\"hi\""), Class::String);
        let swift = colour("view.swift", &[('+', "func greet() -> String { return \"hi\" }")]);
        assert_eq!(class_of(&swift[0], "func"), Class::Keyword);
        let dart = colour("main.dart", &[('+', "void main() { var n = 42; } // done")]);
        assert_eq!(class_of(&dart[0], "42"), Class::Number);
        assert_eq!(class_of(&dart[0], "// done"), Class::Comment);
    }

    #[test]
    fn powershell_is_coloured() {
        let lines = colour(
            "build.ps1",
            &[
                ('+', "<# Builds it"),
                ('+', "   all #>"),
                ('+', "function Invoke-Build([string]$Path) {"),
                ('+', "    if ($Path -eq $null) { Write-Host \"no $Path here\" -ForegroundColor Red } # check"),
                ('+', "    $n = 0x1F; @'"),
                ('+', "raw $text"),
                ('+', "'@"),
                ('+', "}"),
            ],
        );
        assert_eq!(class_of(&lines[0], "<# Builds it"), Class::Comment);
        assert_eq!(class_of(&lines[1], "all #>"), Class::Comment);
        assert_eq!(class_of(&lines[2], "function"), Class::Keyword);
        assert_eq!(class_of(&lines[2], "Invoke-Build"), Class::Function);
        assert_eq!(class_of(&lines[2], "string"), Class::Type);
        assert_eq!(class_of(&lines[3], "if"), Class::Keyword);
        assert_eq!(class_of(&lines[3], "-eq"), Class::Keyword);
        assert_eq!(class_of(&lines[3], "$null"), Class::Number);
        assert_eq!(class_of(&lines[3], "Write-Host"), Class::Function);
        // A variable outside a string is plain; inside one, it stands out.
        assert_eq!(class_of(&lines[3], "$Path"), Class::Plain);
        assert!(lines[3].iter().any(|(piece, class)| piece == "$Path" && *class == Class::Escape), "{:?}", lines[3]);
        assert_eq!(class_of(&lines[3], "-ForegroundColor"), Class::Property);
        assert_eq!(class_of(&lines[3], "# check"), Class::Comment);
        assert_eq!(class_of(&lines[4], "0x1F"), Class::Number);
        assert_eq!(class_of(&lines[5], "raw $text"), Class::String);
    }

    /// A line of a file, as the whole-file diff shows it.
    fn numbered(origin: char, old: Option<u32>, new: Option<u32>, text: &str) -> DiffLine {
        DiffLine { origin, old_no: old, new_no: new, text: text.to_owned() }
    }

    /// The classes of a stream's line `line` once the worker has done it.
    fn stream_classes(style: &DiffStyle, hunk: usize, line: usize, text: &str) -> Vec<(String, Class)> {
        wait_for("the stream", || !style.hunk(hunk).unwrap().spans(line).is_empty());
        let view = style.hunk(hunk).unwrap();
        let mut start = 0;
        view.spans(line)
            .iter()
            .map(|span| {
                let piece = text[start..span.end as usize].to_owned();
                start = span.end as usize;
                (piece, span.class)
            })
            .collect()
    }

    #[test]
    fn a_hunk_inside_a_block_comment_is_coloured_as_comment() {
        let ctx = egui::Context::default();
        // The whole file shown: a gap opens a comment, the hunk after it is
        // still inside.
        let gap = Hunk {
            lines: vec![
                numbered(' ', Some(1), Some(1), "fn stream_test_whole() {}"),
                numbered(' ', Some(2), Some(2), "/* a comment"),
                numbered(' ', Some(3), Some(3), "   that goes on"),
            ],
            gap: true,
            ..Hunk::default()
        };
        let hunk = Hunk {
            lines: vec![
                numbered('-', Some(4), None, "   let old = 1;"),
                numbered('+', None, Some(4), "   let new = 2;"),
                numbered(' ', Some(5), Some(5), "*/ let after = 3;"),
            ],
            ..Hunk::default()
        };
        let whole = Arc::new(FileDiff { hunks: vec![gap, hunk.clone()], ..Default::default() });
        let style = style_for(&ctx, &whole, "src/stream_test_whole.rs", true);
        assert!(style.stream.as_ref().is_some_and(|link| link.map.is_none()));
        for (line, text) in [(0, "   let old = 1;"), (1, "   let new = 2;")] {
            let classes = stream_classes(&style, 1, line, text);
            assert!(classes.iter().all(|(_, class)| *class == Class::Comment), "{text}: {classes:?}");
        }
        let after = stream_classes(&style, 1, 2, "*/ let after = 3;");
        assert_eq!(class_of(&after, "let"), Class::Keyword);

        // The hunks only, with the whole file beside them: the same colours.
        let mut all = whole.hunks[0].clone();
        all.lines.extend(hunk.lines.iter().cloned());
        let hunks = Arc::new(FileDiff { hunks: vec![hunk], whole: Some(Arc::new(all)), ..Default::default() });
        let style = style_for(&ctx, &hunks, "src/stream_test_hunks.rs", true);
        assert!(style.stream.as_ref().is_some_and(|link| link.map.is_some()));
        wait_for("the stream", || style.stream.as_ref().unwrap().style.syntax.read().unwrap().ends.len() == 6);
        let new = stream_classes(&style, 0, 1, "   let new = 2;");
        assert!(new.iter().all(|(_, class)| *class == Class::Comment), "{new:?}");
    }

    #[test]
    fn shown_lines_are_found_in_the_stream_by_number() {
        let stream = [
            numbered(' ', Some(1), Some(1), "a"),
            numbered('-', Some(2), None, "b"),
            numbered('+', None, Some(2), "B"),
            numbered(' ', Some(3), Some(3), "c"),
            numbered('+', None, Some(4), "d"),
        ];
        let refs: Vec<&DiffLine> = stream.iter().collect();
        assert!(in_file_order(&refs));
        assert!(!in_file_order(&refs[1..]), "a stream starts at the first line");
        let hunk = Hunk { lines: vec![stream[1].clone(), stream[2].clone(), stream[3].clone()], ..Hunk::default() };
        let diff = FileDiff { hunks: vec![hunk.clone()], ..Default::default() };
        assert_eq!(map_shown(&diff, &refs), Some(vec![1, 2, 3]));
        // A shown line the stream has otherwise: no stream at all.
        let mut odd = hunk;
        odd.lines[1].text = "other".to_owned();
        assert_eq!(map_shown(&FileDiff { hunks: vec![odd], ..Default::default() }, &refs), None);

        // An earlier version: "B" was not there, the rest one line up.
        let earlier = [
            numbered(' ', Some(1), Some(1), "a"),
            numbered('-', Some(2), None, "b"),
            numbered(' ', Some(3), Some(2), "c"),
            numbered('+', None, Some(3), "d"),
        ];
        let earlier: Vec<&DiffLine> = earlier.iter().collect();
        // "a" and "b" match by number and text, "c" by its old number, "d"
        // has moved and "B" is new.
        assert_eq!(seed_map(&refs, &earlier), [0, 1, NONE, 2, NONE]);
    }

    #[test]
    fn joined_sides_colour_like_separate_ones() {
        // Removed lines open a comment that the added ones do not: the
        // context after them belongs to the new side, which is not in one.
        let lines = colour(
            "lib.rs",
            &[
                (' ', "fn a() {}"),
                ('-', "/* opened"),
                ('+', "let x = 1;"),
                (' ', "let y = 2;"),
                ('-', "still in the old comment */"),
                (' ', "let z = 3;"),
                ('+', "let w = 4;"),
            ],
        );
        assert_eq!(class_of(&lines[3], "let"), Class::Keyword);
        assert_eq!(class_of(&lines[4], "still in the old comment */"), Class::Comment);
        assert_eq!(class_of(&lines[5], "let"), Class::Keyword);
        assert_eq!(class_of(&lines[6], "let"), Class::Keyword);
    }

    #[test]
    fn toml_and_json_keys_are_told_from_values() {
        let toml = colour(
            "cargo.toml",
            &[(' ', "[package]"), ('+', "name = \"gitkrill\" # the app"), ('+', "edition = 2024"), ('+', "lto = true")],
        );
        assert_eq!(class_of(&toml[0], "package"), Class::Function);
        assert_eq!(class_of(&toml[1], "name"), Class::Property);
        assert_eq!(class_of(&toml[1], "\"gitkrill\""), Class::String);
        assert_eq!(class_of(&toml[1], "# the app"), Class::Comment);
        assert_eq!(class_of(&toml[2], "2024"), Class::Number);
        assert_eq!(class_of(&toml[3], "true"), Class::Number);

        let json = colour("package.json", &[(' ', "{"), ('+', "  \"name\": \"demo\","), (' ', "}")]);
        assert_eq!(class_of(&json[1], "\"name\""), Class::Property);
        assert_eq!(class_of(&json[1], "\"demo\""), Class::String);
    }

    #[test]
    fn overlong_lines_are_skipped_without_losing_the_rest() {
        let long = format!("let s = \"{}\";", "x".repeat(MAX_LINE_BYTES));
        let lines = colour("main.rs", &[('+', long.as_str()), ('+', "let y = 2;")]);
        assert!(lines[0].is_empty());
        assert_eq!(class_of(&lines[1], "let"), Class::Keyword);
    }

    #[test]
    fn layout_follows_the_spans_and_survives_bad_ones() {
        let font = FontId::monospace(12.0);
        let text = "let s = \"héllo → wörld\";";
        let lines = colour("main.rs", &[('+', text)]);
        let spans: Vec<Span> = {
            let mut end = 0;
            lines[0].iter().map(|(piece, class)| { end += piece.len(); Span { end: end as u32, class: *class } }).collect()
        };
        let job = layout(text, &spans, &font);
        assert_eq!(job.text, text);
        assert!(job.sections.len() > 2);
        let string = job.sections.iter().find(|s| job.text[s.byte_range.start.0..].starts_with('"')).unwrap();
        assert_eq!(string.format.color, Class::String.color());

        // No spans: one plain section. Spans that do not fit the text (past
        // the end, inside a character, not ascending): also plain.
        assert_eq!(layout(text, &[], &font).sections.len(), 1);
        for bad in [&[Span { end: 999, class: Class::Keyword }][..], &[Span { end: 11, class: Class::Keyword }], &[
            Span { end: 5, class: Class::Keyword },
            Span { end: 3, class: Class::String },
        ]] {
            let job = layout(text, bad, &font);
            assert_eq!((job.text.as_str(), job.sections.len()), (text, 1));
        }
    }

    fn ranges(pairs: &[(u32, u32)]) -> Vec<Range<u32>> {
        pairs.iter().map(|(a, b)| *a..*b).collect()
    }

    type Marked = Vec<Range<u32>>;

    /// The marked ranges of the old and of the new line.
    fn marks(old: &str, new: &str) -> Option<(Marked, Marked)> {
        emphasize(old, new).map(|change| (change.old, change.new))
    }

    #[test]
    fn emphasis_marks_what_changed_inside_a_line() {
        // One word replaced.
        assert_eq!(
            marks("let x = foo(1);", "let x = bar(1);"),
            Some((ranges(&[(8, 11)]), ranges(&[(8, 11)])))
        );
        // Something appended: nothing to mark on the old side.
        assert_eq!(marks("call(a, b)", "call(a, b, c)"), Some((ranges(&[]), ranges(&[(9, 12)]))));
        // Two separate edits stay separate...
        assert_eq!(
            marks("draw(x, y, red);", "draw(a, y, blue);"),
            Some((ranges(&[(5, 6), (11, 14)]), ranges(&[(5, 6), (11, 15)])))
        );
        // ...unless only whitespace lies between them.
        assert_eq!(
            marks("send(one two, flag);", "send(three four, flag);"),
            Some((ranges(&[(5, 12)]), ranges(&[(5, 15)])))
        );
        // Positions are characters, not bytes.
        assert_eq!(marks("héllo = wörld", "héllo = wörlds"), Some((ranges(&[(8, 13)]), ranges(&[(8, 14)]))));
        // Indentation changes are changes too.
        assert_eq!(marks("  done();", "    done();"), Some((ranges(&[(0, 2)]), ranges(&[(0, 4)]))));
    }

    #[test]
    fn emphasis_is_withheld_when_it_would_say_nothing() {
        assert_eq!(marks("same", "same"), None);
        // Rewritten lines: only punctuation in common.
        assert_eq!(marks("let total = compute(values);", "return Err(Missing);"), None);
        assert_eq!(marks("", "something new"), None);
    }

    #[test]
    fn removed_and_added_lines_are_paired_before_they_are_compared() {
        let paired = hunk(&[
            (' ', "start"),
            ('-', "let a = 1;"),
            ('-', "let b = 2;"),
            ('+', "let a = 10;"),
            ('+', "let b = 20;"),
            (' ', "end"),
        ]);
        assert_eq!(emphasize_hunk(&paired), vec![(1, 8, 9), (2, 8, 9), (3, 8, 10), (4, 8, 10)]);

        // Unequal blocks: each line goes with the one it resembles, in
        // order, and lines that resemble nothing are left alone.
        let grown = hunk(&[
            ('-', "    \"retries\": 3,"),
            ('-', "    \"verbose\": false"),
            ('+', "    \"retries\": 5,"),
            ('+', "    \"timeout\": 30,"),
            ('+', "    \"verbose\": true,"),
            ('+', "    \"tags\": []"),
        ]);
        assert_eq!(
            emphasize_hunk(&grown),
            vec![(0, 15, 16), (1, 15, 20), (2, 15, 16), (4, 15, 20)],
        );
        let shrunk = hunk(&[('-', "let a = 1;"), ('-', "fn unrelated() {}"), ('-', "let b = 2;"), ('+', "let b = 20;")]);
        assert_eq!(emphasize_hunk(&shrunk), vec![(2, 8, 9), (3, 8, 10)]);

        // An addition that follows context is not paired with anything.
        let apart = hunk(&[('-', "let a = 1;"), (' ', "middle"), ('+', "let a = 10;")]);
        assert!(emphasize_hunk(&apart).is_empty());
        // Nor are blocks too large to compare line by line.
        let old: Vec<String> = (0..40).map(|i| format!("let old_{i} = {i};")).collect();
        let new: Vec<String> = (0..41).map(|i| format!("let old_{i} = {i}0;")).collect();
        let lines: Vec<(char, &str)> =
            old.iter().map(|t| ('-', t.as_str())).chain(new.iter().map(|t| ('+', t.as_str()))).collect();
        assert!(emphasize_hunk(&hunk(&lines)).is_empty());
    }

    #[test]
    fn similarity_says_how_much_two_lines_share() {
        let close = emphasize("let total = compute(values);", "let total = compute(inputs);").unwrap();
        let far = emphasize("let total = compute(values);", "let total = other;").unwrap();
        assert!(close.similarity > 0.7 && close.similarity < 1.0, "{}", close.similarity);
        assert!(far.similarity < close.similarity);
    }

    use crate::theme::contrast;

    #[test]
    fn every_token_colour_reads_on_every_line_background() {
        let classes = [
            Class::Plain,
            Class::Comment,
            Class::Keyword,
            Class::String,
            Class::Escape,
            Class::Number,
            Class::Function,
            Class::Type,
            Class::Tag,
            Class::Property,
            Class::Punctuation,
            Class::Inserted,
            Class::Deleted,
            Class::Operator,
            Class::Storage,
        ];
        for light in [false, true] {
            theme::set_light(light);
            colours_read_on_line_backgrounds(&classes);
        }
        theme::set_light(false);
    }

    fn colours_read_on_line_backgrounds(classes: &[Class]) {
        let light = theme::is_light();
        for &class in classes {
            // Ordinary text contrast on the line backgrounds...
            for bg in [theme::bg_app(), theme::diff_add_bg(), theme::diff_del_bg()] {
                let ratio = contrast(class.color(), bg);
                assert!(ratio >= 4.5, "{class:?} on {bg:?} (light: {light}): {ratio:.2}");
            }
            // ...and still clearly legible on the brighter backgrounds that
            // come and go: selected lines and the word emphasis.
            let selected = ['+', '-'].map(|origin| crate::ui::diff::line_background(origin, true).unwrap());
            for bg in selected.into_iter().chain([emphasis_color('+'), emphasis_color('-')]) {
                let ratio = contrast(class.color(), bg);
                assert!(ratio >= 3.0, "{class:?} on {bg:?} (light: {light}): {ratio:.2}");
            }
        }
        // The emphasis itself has to stand out from the line it sits on,
        // also when that line is selected.
        for origin in ['+', '-'] {
            for selected in [false, true] {
                let line = crate::ui::diff::line_background(origin, selected).unwrap();
                let ratio = contrast(emphasis_color(origin), line);
                assert!(ratio >= 1.25, "emphasis on {origin} (selected: {selected}, light: {light}): {ratio:.2}");
            }
        }
    }

    fn file_diff(lines: &[(char, &str)]) -> Arc<FileDiff> {
        Arc::new(FileDiff { hunks: vec![hunk(lines)], ..Default::default() })
    }

    fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
        let started = Instant::now();
        while !ready() {
            assert!(started.elapsed() < Duration::from_secs(120), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn styles_arrive_in_the_background_and_are_shared_by_equal_hunks() {
        let ctx = egui::Context::default();
        let lines = [(' ', "fn shared_by_equal_hunks() {"), ('-', "    let a = 1;"), ('+', "    let a = 2;"), (' ', "}")];
        let first = file_diff(&lines);
        let style = style_for(&ctx, &first, "src/some/file.rs", true);
        // Asking again is a lookup, not new work.
        assert!(Arc::ptr_eq(&style, &style_for(&ctx, &first, "src/some/file.rs", true)));

        wait_for("colours", || !style.hunk(0).unwrap().spans(3).is_empty());
        let view = style.hunk(0).unwrap();
        assert_eq!(view.spans(0)[0], Span { end: 2, class: Class::Keyword });
        for changed in [1, 2] {
            let mut marked = view.emphasis(changed);
            assert_eq!((marked.next(), marked.next()), (Some(12..13), None));
        }
        assert_eq!(view.emphasis(0).count(), 0);
        drop(view);

        // The same change loaded again (as after every refresh) reuses the
        // finished hunk even though the first diff object is gone.
        drop(first);
        let again = file_diff(&lines);
        let reloaded = style_for(&ctx, &again, "src/some/file.rs", true);
        assert!(!Arc::ptr_eq(&style, &reloaded));
        assert!(Arc::ptr_eq(&style.hunks[0], &reloaded.hunks[0]));
        // Under another name it is another language, so it is done afresh.
        let other = style_for(&ctx, &file_diff(&lines), "src/some/file.py", true);
        assert!(!Arc::ptr_eq(&style.hunks[0], &other.hunks[0]));

        // With syntax colours off the changed words are still marked, and
        // nothing is coloured.
        let plain = style_for(&ctx, &again, "src/some/file.rs", false);
        assert!(!Arc::ptr_eq(&plain, &reloaded));
        wait_for("emphasis", || plain.hunk(0).unwrap().emphasis(1).count() == 1);
        assert!((0..4).all(|line| plain.hunk(0).unwrap().spans(line).is_empty()));
    }

    #[test]
    fn rows_of_a_wrapped_line_keep_their_colours() {
        let font = FontId::monospace(12.0);
        let text = "let s = \"héllo\";";
        let spans = [
            Span { end: 3, class: Class::Keyword },
            Span { end: 8, class: Class::Plain },
            Span { end: 16, class: Class::String },
            Span { end: 17, class: Class::Punctuation },
        ];
        // "let s = " / "\"héllo\";": the second row starts inside no span.
        let first = layout_part(text, 0..8, &spans, &font);
        assert_eq!(first.text, "let s = ");
        assert_eq!(first.sections[0].format.color, Class::Keyword.color());
        let second = layout_part(text, 8..text.len(), &spans, &font);
        assert_eq!(second.text, "\"héllo\";");
        assert_eq!(second.sections[0].format.color, Class::String.color());
        assert_eq!(second.sections.last().unwrap().format.color, Class::Punctuation.color());
        // A row cut through a span keeps the span's colour on both sides.
        let middle = layout_part(text, 10..14, &spans, &font);
        assert_eq!((middle.text.as_str(), middle.sections.len()), ("éll", 1));
        assert_eq!(middle.sections[0].format.color, Class::String.color());
    }

    #[test]
    fn huge_diffs_are_coloured_up_to_the_limit_only() {
        let ctx = egui::Context::default();
        let text: Vec<String> = (0..MAX_LINES + 500).map(|i| format!("let limit_test_{i} = {i};")).collect();
        let mut hunks = Vec::new();
        for chunk in text.chunks(MAX_LINES - 100) {
            let lines: Vec<(char, &str)> = chunk.iter().map(|t| ('+', t.as_str())).collect();
            hunks.push(hunk(&lines));
        }
        assert_eq!(hunks.len(), 2);
        let diff = Arc::new(FileDiff { hunks, ..Default::default() });
        let requested = Instant::now();
        let style = style_for(&ctx, &diff, "big.rs", true);

        // A diff opened while that is being worked on does not wait for it.
        let small = file_diff(&[('+', "let opened_meanwhile = true;")]);
        let small_style = style_for(&ctx, &small, "small.rs", true);
        let prompt = requested.elapsed() < Duration::from_millis(250);
        wait_for("the later diff", || !small_style.hunk(0).unwrap().spans(0).is_empty());
        // (Meaningless if this thread was stalled long enough between the
        // two requests for the big one to get far.)
        if prompt {
            assert!(style.hunk(0).unwrap().spans(MAX_LINES - 101).is_empty(), "the big diff finished first");
        }

        // The second hunk crosses the limit: its first 100 lines are done,
        // the remaining 500 are left plain.
        wait_for("the first hunk", || !style.hunk(0).unwrap().spans(MAX_LINES - 101).is_empty());
        wait_for("the second hunk", || !style.hunk(1).unwrap().spans(99).is_empty());
        std::thread::sleep(Duration::from_millis(50));
        assert!(style.hunk(1).unwrap().spans(100).is_empty());
        assert!(style.hunk(1).unwrap().spans(599).is_empty());
    }

    /// A diff of the hunks of a file that comes with its new version, as
    /// the loader gives one for a large file (`FileDiff::new_file`): `lines`
    /// are the new version, `changes` the lines (by index, at least seven
    /// apart) that read otherwise in the old one, with their old text.
    fn carried_diff(lines: &[String], changes: &[(usize, &str)]) -> Arc<FileDiff> {
        let mut hunks = Vec::new();
        for &(at, old) in changes {
            let (from, to) = (at.saturating_sub(3), (at + 4).min(lines.len()));
            let mut shown = Vec::new();
            for (i, text) in lines.iter().enumerate().take(to).skip(from) {
                let n = Some(i as u32 + 1);
                if i == at {
                    shown.push(numbered('-', n, None, old));
                    shown.push(numbered('+', None, n, text));
                } else {
                    shown.push(numbered(' ', n, n, text));
                }
            }
            let count = to - from;
            hunks.push(Hunk { header: format!("@@ -{0},{count} +{0},{count} @@", from + 1), lines: shown, ..Hunk::default() });
        }
        let bytes: Vec<u8> = lines.iter().flat_map(|line| [line.as_bytes(), b"\n"]).flatten().copied().collect();
        let id = crate::git::Oid::hash_object(git2::ObjectType::Blob, &bytes).unwrap();
        Arc::new(FileDiff { hunks, new_file: Some(Arc::new(crate::git::NewFile { bytes, id })), ..Default::default() })
    }

    /// Lines of Rust, one function each, called `name_<n>`.
    fn rust_lines(name: &str, count: usize) -> Vec<String> {
        (0..count).map(|i| format!("fn {name}_{i}() {{ let x = {i}; }}")).collect()
    }

    /// The classes of every line of a hunk as shown.
    fn shown_classes(style: &DiffStyle, diff: &FileDiff, hunk: usize) -> Vec<Vec<Class>> {
        let view = style.hunk(hunk).unwrap();
        (0..diff.hunks[hunk].lines.len()).map(|line| view.spans(line).iter().map(|span| span.class).collect()).collect()
    }

    #[test]
    fn hunks_of_a_large_file_are_coloured_from_where_the_file_is() {
        let ctx = egui::Context::default();
        let mut lines = rust_lines("carried", 300);
        lines[100] = "/* a comment that runs on".to_owned();
        lines[200] = "   and ends here */".to_owned();
        let changes = [(50, "fn before() {}"), (150, "fn was_code() {}"), (250, "fn after() {}")];
        let diff = carried_diff(&lines, &changes);
        let style = style_for(&ctx, &diff, "src/carried_test.rs", true);
        assert!(style.stream.is_none());
        let carried = style.carried.clone().expect("the hunks are carried");
        wait_for("the parser", || carried.reached.load(Ordering::Acquire) == 3);
        assert!(!carried.given_up.load(Ordering::Relaxed));

        // The hunk inside the comment is a comment, its removed line too.
        let inside = shown_classes(&style, &diff, 1);
        assert_eq!(inside.len(), 8);
        assert!(inside.iter().all(|line| !line.is_empty() && line.iter().all(|class| *class == Class::Comment)), "{inside:?}");
        // The others come out as they do on their own, which they keep,
        // without a second copy.
        for hunk in [0, 2] {
            assert!(read(&carried.hunks[hunk]).ends.is_empty(), "hunk {hunk}");
            assert!(shown_classes(&style, &diff, hunk).iter().all(|line| line.first() == Some(&Class::Keyword)), "hunk {hunk}");
        }

        // A reloaded copy of the same diff shares them.
        let again = carried_diff(&lines, &changes);
        let reloaded = style_for(&ctx, &again, "src/carried_test.rs", true);
        assert!(Arc::ptr_eq(reloaded.carried.as_ref().unwrap(), &carried));
        // The next version (one more change, above them) starts from them:
        // the hunk in the comment is one from the first look.
        let mut next = changes.to_vec();
        next.insert(0, (20, "fn first() {}"));
        let next = carried_diff(&lines, &next);
        let next_style = style_for(&ctx, &next, "src/carried_test.rs", true);
        let next_carried = next_style.carried.clone().unwrap();
        assert!(!Arc::ptr_eq(&next_carried, &carried));
        let seeded = read(&next_carried.hunks[2]).clone();
        assert_eq!(seeded, *read(&carried.hunks[1]), "seeded from the version shown before");
        wait_for("the next parser", || next_carried.reached.load(Ordering::Acquire) == 4);
        assert!(shown_classes(&next_style, &next, 2).iter().all(|line| line.iter().all(|class| *class == Class::Comment)));
    }

    #[test]
    fn carried_colours_stop_with_the_diff_and_go_on_for_a_copy() {
        let ctx = egui::Context::default();
        let mut lines = rust_lines("parked", 3000);
        lines[2000] = "/* opened".to_owned();
        let changes = [(100, "fn a() {}"), (2500, "fn b() {}")];
        let diff = carried_diff(&lines, &changes);
        let style = style_for(&ctx, &diff, "src/carried_park.rs", true);
        let carried = style.carried.clone().unwrap();
        // The diff is closed before the parser gets far: its job is set
        // aside, the hunk past the comment not reached.
        drop(diff);
        let parked = || carried.link.lock().unwrap().parked.is_some();
        wait_for("the job to stop", || parked() || carried.reached.load(Ordering::Acquire) == 2);
        if !parked() {
            // This thread was held up long enough for the parser to finish.
            return;
        }
        assert!(carried.reached.load(Ordering::Acquire) < 2);
        std::thread::sleep(Duration::from_millis(50));
        assert!(parked() && carried.reached.load(Ordering::Acquire) < 2, "nothing goes on meanwhile");

        // Shown again: it carries on where it stopped.
        let again = carried_diff(&lines, &changes);
        let shown = style_for(&ctx, &again, "src/carried_park.rs", true);
        assert!(Arc::ptr_eq(shown.carried.as_ref().unwrap(), &carried));
        wait_for("the parser", || carried.reached.load(Ordering::Acquire) == 2);
        assert!(shown_classes(&shown, &again, 1).iter().all(|line| line.iter().all(|class| *class == Class::Comment)));
    }

    #[test]
    fn a_file_that_is_not_the_diffs_keeps_the_hunks_own_colours() {
        let ctx = egui::Context::default();
        let mut lines = rust_lines("mismatch", 100);
        lines[10] = "/* opened".to_owned();
        let diff = carried_diff(&lines, &[(50, "fn old() {}")]);
        // The file on disk changed since: a line of the hunk reads otherwise.
        let mut other = lines.clone();
        other[49] = "fn changed_since() {}".to_owned();
        let bytes: Vec<u8> = other.iter().flat_map(|line| [line.as_bytes(), b"\n"]).flatten().copied().collect();
        let id = crate::git::Oid::hash_object(git2::ObjectType::Blob, &bytes).unwrap();
        let diff = Arc::new(FileDiff { new_file: Some(Arc::new(crate::git::NewFile { bytes, id })), ..(*diff).clone() });
        let style = style_for(&ctx, &diff, "src/carried_mismatch.rs", true);
        let carried = style.carried.clone().unwrap();
        wait_for("the parser to give up", || carried.given_up.load(Ordering::Relaxed));
        assert_eq!(carried.reached.load(Ordering::Acquire), 0);
        wait_for("the hunk's own colours", || !style.hunk(0).unwrap().spans(0).is_empty());
        assert!(shown_classes(&style, &diff, 0).iter().all(|line| line.first() == Some(&Class::Keyword)));
    }

    /// A [`Carried`] for `diff` that compares with no colours of its own.
    fn carried_for(diff: &FileDiff) -> Carried {
        let count = diff.hunks.len();
        Carried {
            hunks: (0..count).map(|_| RwLock::default()).collect(),
            limits: diff.hunks.iter().map(|hunk| hunk.lines.len()).collect(),
            own: (0..count).map(|_| Weak::new()).collect(),
            marks: vec![(0, 0); count],
            reached: AtomicUsize::new(0),
            given_up: AtomicBool::new(false),
            link: Mutex::default(),
        }
    }

    #[test]
    fn carrying_stops_after_any_batch_and_carries_on_the_same() {
        let mut lines = rust_lines("batches", 2000);
        lines[300] = "let s = \"a string".to_owned();
        lines[900] = "   that ends here\";".to_owned();
        let changes = [(10, "fn a() {}"), (600, "fn b() {}"), (1500, "fn c() {}")];
        let diff = carried_diff(&lines, &changes);
        let mut engine = ENGINE.lock().unwrap_or_else(|e| e.into_inner());
        let grammar = engine.syntax_for("lib.rs").unwrap();

        let whole = carried_for(&diff);
        let mut carry = engine.start_carry(grammar).unwrap();
        assert!(engine.carry(&mut carry, &diff, &whole, &mut || true));
        assert_eq!(whole.reached.load(Ordering::Acquire), 3);
        assert!(read(&whole.hunks[1]).spans.iter().all(|span| span.class == Class::String));

        // Stopped at every batch: where it stopped, the hunks it has not
        // reached have nothing yet (their own colours show), and in the end
        // it comes to the same.
        let stepped = carried_for(&diff);
        let mut carry = engine.start_carry(grammar).unwrap();
        let mut stops = 0;
        while !engine.carry(&mut carry, &diff, &stepped, &mut || false) {
            stops += 1;
            let reached = stepped.reached.load(Ordering::Acquire);
            assert!(stepped.hunks[reached..].iter().all(|hunk| read(hunk).ends.is_empty()));
        }
        assert!(stops >= 5, "{stops}");
        for (a, b) in whole.hunks.iter().zip(&stepped.hunks) {
            assert_eq!(*read(a), *read(b));
        }
        assert!(!stepped.given_up.load(Ordering::Relaxed));
    }

    /// Not a check, a measurement: `cargo test carry_speed -- --ignored
    /// --nocapture`. How long carrying the parser through a large file
    /// takes, for [`CARRY_MAX_TIME`].
    #[test]
    #[ignore]
    fn carry_speed() {
        let source = include_str!("highlight.rs");
        let rust: Vec<String> = source.lines().cycle().take(150_000).map(str::to_owned).collect();
        let json: Vec<String> = std::iter::once("[".to_owned())
            .chain((0..150_000).map(|i| format!("  {{\"id\": {i}, \"name\": \"item {i}\", \"tags\": [\"a\", \"b\"], \"ok\": true}},")))
            .collect();
        let mut engine = ENGINE.lock().unwrap();
        for (name, mut lines) in [("big.rs", rust), ("big.json", json)] {
            let count = lines.len();
            // Twenty lines edited, spread through the file.
            let at: Vec<usize> = (1..=20).map(|k| k * count / 21).collect();
            let old: Vec<String> = at.iter().map(|&i| std::mem::take(&mut lines[i])).collect();
            for (&i, old) in at.iter().zip(&old) {
                lines[i] = format!("{old} ");
            }
            let changes: Vec<(usize, &str)> = at.iter().copied().zip(old.iter().map(String::as_str)).collect();
            let diff = carried_diff(&lines, &changes);
            let bytes = diff.new_file.as_ref().unwrap().bytes.len();
            let carried = carried_for(&diff);
            let grammar = engine.syntax_for(name).unwrap();
            let mut carry = engine.start_carry(grammar).unwrap();
            let started = Instant::now();
            assert!(engine.carry(&mut carry, &diff, &carried, &mut || true));
            let secs = started.elapsed().as_secs_f32();
            println!(
                "{name}: {count} lines, {:.1} MB in {secs:.2}s: {:.0} lines/s, {:.2} MB/s",
                bytes as f32 / 1e6,
                count as f32 / secs,
                bytes as f32 / 1e6 / secs
            );
        }
    }

    /// Not a check, a measurement: `cargo test parse_speed -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn parse_speed() {
        let source = include_str!("highlight.rs");
        let lines: Vec<(char, &str)> = source.lines().cycle().take(20_000).map(|l| ('+', l)).collect();
        let hunk = hunk(&lines);
        let mut engine = ENGINE.lock().unwrap();
        let syntax = engine.syntax_for("highlight.rs").unwrap();
        let mut progress = engine.start(syntax).unwrap();
        let started = Instant::now();
        engine.colour_hunk(&mut progress, &hunk, hunk.lines.len(), &HunkStyle::default(), &mut || true);
        let secs = started.elapsed().as_secs_f32();
        println!("{} lines in {secs:.2}s: {:.0} lines/s", lines.len(), lines.len() as f32 / secs);
        let started = Instant::now();
        let mut fresh = Engine::new();
        fresh.syntax_for("highlight.rs");
        println!("loading the grammars: {:.0} ms", started.elapsed().as_secs_f32() * 1000.0);
    }
}
