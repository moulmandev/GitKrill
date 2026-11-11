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
//! A hunk is all the parser gets to see, so it starts every hunk in the
//! syntax's initial state: a hunk that begins inside a block comment or a
//! multi-line string is coloured as if it did not. (The unchanged stretches
//! of a whole-file diff are hunks of their own, `Hunk::gap`.) The old side (context and
//! removed lines) and the new side (context and added lines) are parsed as
//! two separate streams, so a string opened on a removed line cannot leak
//! into the added lines after it.

use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::sync::{Arc, Condvar, LazyLock, Mutex, OnceLock, RwLock, RwLockReadGuard, Weak};
use std::time::{Duration, Instant};

use eframe::egui::text::{LayoutJob, TextFormat};
use eframe::egui::{self, Color32, FontId};
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxDefinition, SyntaxSet};

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
    ("ts", "js"),
    ("mts", "js"),
    ("cts", "js"),
    ("tsx", "js"),
    ("jsx", "js"),
    ("mjs", "js"),
    ("cjs", "js"),
    ("jsonc", "json"),
    ("json5", "json"),
    ("webmanifest", "json"),
    ("scss", "css"),
    ("less", "css"),
    ("vue", "html"),
    ("svelte", "html"),
    ("kt", "java"),
    ("kts", "java"),
    ("pyi", "py"),
    ("mdx", "md"),
    ("zsh", "sh"),
    ("csproj", "xml"),
    ("props", "xml"),
    ("targets", "xml"),
    ("xaml", "xml"),
    ("lock", "toml"),
    ("ini", "toml"),
    ("cfg", "toml"),
    ("conf", "toml"),
    ("editorconfig", "toml"),
    ("gitconfig", "toml"),
];

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
#[derive(Default)]
struct Lines {
    /// Per finished line, where its runs end in `spans`.
    ends: Vec<u32>,
    spans: Vec<Span>,
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
}

/// Read access to one hunk's styling while its rows are painted.
pub struct HunkView<'a> {
    syntax: RwLockReadGuard<'a, Lines>,
    emphasis: &'a [Stretch],
}

impl DiffStyle {
    pub fn hunk(&self, index: usize) -> Option<HunkView<'_>> {
        let hunk = self.hunks.get(index)?;
        Some(HunkView {
            // A poisoned lock only means the worker died mid-append; what is
            // in there is still a consistent prefix.
            syntax: hunk.syntax.read().unwrap_or_else(|e| e.into_inner()),
            emphasis: hunk.emphasis.get().map_or(&[], Vec::as_slice),
        })
    }
}

impl HunkView<'_> {
    /// Coloured runs of a line; empty while (or if) it is not highlighted.
    pub fn spans(&self, line: usize) -> &[Span] {
        let Some(end) = self.syntax.ends.get(line) else { return &[] };
        let start = if line == 0 { 0 } else { self.syntax.ends[line - 1] };
        self.syntax.spans.get(start as usize..*end as usize).unwrap_or(&[])
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

struct Job {
    /// Kept alive so the work stays valid if the view moves on to a reloaded
    /// copy of the same diff, which shares the output below.
    diff: Arc<FileDiff>,
    hunk: usize,
    /// How many of the hunk's lines to parse (0: emphasis only).
    limit: usize,
    file_name: Arc<str>,
    /// Dead once no diff shows this hunk any more; the job is then skipped.
    out: Weak<HunkStyle>,
    ctx: egui::Context,
    /// Which request the job came with; later requests have higher numbers
    /// and go first.
    request: u64,
    /// Where parsing stopped, if the job stepped aside for a later request.
    progress: Option<Box<Progress>>,
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
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Mutex::default);

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
    // Without syntax colours nothing is parsed: every hunk gets the word
    // emphasis only.
    let (mut lines_left, mut bytes_left) = if syntax { (MAX_LINES, MAX_BYTES) } else { (0, 0) };
    let request = state.requests + 1;
    let mut jobs = Vec::new();
    let mut hunks = Vec::with_capacity(diff.hunks.len());
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
                jobs.push(Job {
                    diff: diff.clone(),
                    hunk: index,
                    limit,
                    file_name: file_name.clone(),
                    out: Arc::downgrade(&style),
                    ctx: ctx.clone(),
                    request,
                    progress: None,
                });
                style
            }
        };
        hunks.push(style);
    }

    let style = Arc::new(DiffStyle { hunks });
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
        let Some(out) = job.out.upgrade() else { continue };
        let diff = job.diff.clone();
        let Some(hunk) = diff.hunks.get(job.hunk) else { continue };
        let ctx = job.ctx.clone();

        let mut repaint = |force: bool| {
            // Often enough to look live, not so often that a diff of a
            // thousand small hunks asks for a thousand frames.
            if force || last_repaint.elapsed() > Duration::from_millis(40) {
                ctx.request_repaint();
                last_repaint = Instant::now();
            }
        };
        if job.progress.is_none() {
            let _ = out.emphasis.set(emphasize_hunk(hunk));
            repaint(false);
        }

        let mut finished = true;
        if job.limit > 0 {
            // A later request is for what the user is looking at now.
            let request = job.request;
            let overtaken = || {
                let jobs = queue.jobs.lock().unwrap_or_else(|e| e.into_inner());
                jobs.front().is_some_and(|next| next.request > request)
            };
            // A grammar that trips over some input must not take the thread
            // (and with it all later highlighting) down.
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let engine = engine.get_or_insert_with(Engine::new);
                if job.progress.is_none() {
                    let Some(syntax) = engine.syntax_for(&job.file_name) else { return true };
                    job.progress = engine.start(syntax).map(Box::new);
                }
                let Some(progress) = job.progress.as_deref_mut() else { return true };
                engine.colour_hunk(progress, hunk, job.limit, &out, &mut || {
                    repaint(false);
                    !overtaken()
                })
            }));
            match outcome {
                Ok(done) => finished = done,
                Err(_) => engine = None,
            }
        }

        let mut jobs = queue.jobs.lock().unwrap_or_else(|e| e.into_inner());
        if !finished {
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

/// The loaded grammars plus what is needed to turn scopes into classes.
struct Engine {
    set: SyntaxSet,
    selectors: Vec<(Scope, Class)>,
    /// Class of a single scope, as resolved so far.
    classes: HashMap<Scope, Option<Class>>,
    comment: Option<Scope>,
    /// Scopes that mark the key of a key/value pair.
    key_scopes: Vec<Scope>,
    /// Syntax index per lower-case file name.
    by_name: HashMap<String, Option<usize>>,
}

impl Engine {
    fn new() -> Self {
        let mut builder = SyntaxSet::load_defaults_newlines().into_builder();
        if let Ok(toml) = SyntaxDefinition::load_from_str(TOML_SYNTAX, true, None) {
            builder.add(toml);
        }
        Engine {
            set: builder.build(),
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

    /// Index of the syntax for a (lower-case) file name, if there is one
    /// worth parsing with.
    fn syntax_for(&mut self, file_name: &str) -> Option<usize> {
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
            .find_map(|token| self.set.find_syntax_by_extension(token))
            .filter(|syntax| syntax.name != "Plain Text")
            .and_then(|syntax| self.set.syntaxes().iter().position(|s| std::ptr::eq(s, syntax)));
        self.by_name.insert(file_name.to_owned(), found);
        found
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
    fn start(&self, syntax: usize) -> Option<Progress> {
        let syntax = self.set.syntaxes().get(syntax)?;
        let names_tell = syntax.name == "Rust";
        Some(Progress {
            old: Stream::new(ParseState::new(syntax), names_tell),
            new: Stream::new(ParseState::new(syntax), names_tell),
            next: 0,
        })
    }

    /// Parses the first `limit` lines of a hunk from where `progress` says,
    /// appending their coloured runs to `out` a batch at a time. `go_on` is
    /// asked after every batch; when it says no, the rest is left for a
    /// later call and the result is `false`.
    fn colour_hunk(
        &mut self,
        progress: &mut Progress,
        hunk: &Hunk,
        limit: usize,
        out: &HunkStyle,
        go_on: &mut dyn FnMut() -> bool,
    ) -> bool {
        let lines = &hunk.lines[..limit.min(hunk.lines.len())];
        let Progress { old, new, next } = progress;

        // Context lines belong to both sides. They are coloured from the new
        // side, and fed to the old side only while it still has removed
        // lines coming that depend on them.
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
            match line.origin {
                '-' => old.line(self, &line.text, Some(&mut batch.spans)),
                '+' => new.line(self, &line.text, Some(&mut batch.spans)),
                _ if any_added => {
                    new.line(self, &line.text, Some(&mut batch.spans));
                    if last_removed.is_some_and(|last| *next < last) {
                        old.line(self, &line.text, None);
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
}

/// One side of a hunk being parsed line by line.
struct Stream {
    state: ParseState,
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
    fn new(state: ParseState, names_tell: bool) -> Self {
        Stream { state, stack: ScopeStack::new(), failed: false, names_tell, buffer: String::new() }
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
        let Ok(ops) = self.state.parse_line(&self.buffer, &engine.set) else {
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
        let diff = DiffStyle { hunks: vec![Arc::new(style)] };
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
        let mut name = |file: &str| engine.syntax_for(file).map(|i| engine.set.syntaxes()[i].name.clone());
        assert_eq!(name("main.rs").as_deref(), Some("Rust"));
        assert_eq!(name("makefile").as_deref(), Some("Makefile"));
        assert_eq!(name("app.tsx").as_deref(), Some("JavaScript"));
        assert_eq!(name("cargo.toml").as_deref(), Some("TOML"));
        assert_eq!(name("cargo.lock").as_deref(), Some("TOML"));
        assert_eq!(name("style.scss").as_deref(), Some("CSS"));
        assert_eq!(name("archive.tar.gz"), None);
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
