//! The diff view that replaces the graph while a file is open.

use std::any::Any;
use std::borrow::Cow;
use std::cell::OnceCell;
use std::collections::BTreeSet;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::sync::{Arc, Mutex, Weak};

use eframe::egui::{
    self, Align, Align2, Color32, CursorIcon, Layout, Pos2, Rect, ScrollArea, Sense, Stroke, Ui,
    pos2, vec2,
};

use crate::git::diff::LineApply;
use crate::git::{ChangeKind, DiffSource, DiffTarget, FileChange, FileDiff, ops, short, text};
use crate::prefs::editor;
use crate::state::{Dialog, RepoTab, Shared, ToastKind};
use crate::theme::{self, icons};
use crate::ui::highlight;
use crate::ui::textsel::{self, Lines, Pos};
use crate::ui::widgets::{self, icon_button, kind_icon, tint_button};

const HUNK_H: f32 = 30.0;
const NUM_W: f32 = 46.0;
const BAR_H: f32 = 38.0;

// Operation labels: shown in error toasts, and used to tell that one of
// these is still running.
const STAGE_LINES: &str = "Stage lines";
const UNSTAGE_LINES: &str = "Unstage lines";
const DISCARD_LINES: &str = "Discard lines";

pub fn show(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared) {
    let Some(view) = &tab.diff else { return };
    let target = view.target.clone();
    let data = view.data.clone();

    ui.spacing_mut().item_spacing.y = 0.0;
    let loaded = match &data {
        Some(Ok(diff)) => Some(diff.clone()),
        _ => None,
    };
    if title_bar(ui, tab, shared, &target, loaded.as_deref()) {
        tab.diff = None;
        return;
    }

    // Read before the encoding or tab width changed: read it again. The old
    // lines stay on screen until the new ones are in.
    if let Some(Ok(diff)) = &data
        && diff.format.generation != text::generation()
    {
        let asked = egui::Id::new(("diff-reload", &tab.path));
        if ui.data(|d| d.get_temp::<u64>(asked)) != Some(text::generation()) {
            ui.data_mut(|d| d.insert_temp(asked, text::generation()));
            tab.reload_diff();
        }
    }

    match data {
        None => widgets::placeholder(ui, "Loading diff…"),
        Some(Err(e)) => widgets::placeholder(ui, &e),
        Some(Ok(diff)) if diff.binary => match &diff.images {
            Some(images) => crate::ui::image_diff::show(ui, images),
            None => widgets::placeholder(ui, "Binary file - no text diff to show"),
        },
        Some(Ok(diff)) if diff.hunks.is_empty() => {
            widgets::placeholder(ui, "No content changes (the file mode or name changed)")
        }
        Some(Ok(diff)) => body(ui, tab, shared, &target, &diff),
    }
}

/// Paints a diff with no staging controls, for views that only look at
/// history. `target` must have a commit as its source.
pub fn read_only(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, target: &DiffTarget, diff: &Arc<FileDiff>) {
    body(ui, tab, shared, target, diff);
}

/// Returns true when the view should close.
fn title_bar(
    ui: &mut Ui,
    tab: &mut RepoTab,
    shared: &mut Shared,
    target: &DiffTarget,
    diff: Option<&FileDiff>,
) -> bool {
    let mut close = false;
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), BAR_H), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, theme::bg_header());
    ui.painter().hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::border()));

    let unborn = tab.snap.as_ref().is_some_and(|s| s.head.oid.is_none());
    let kind = diff.and_then(|d| d.kind);
    let file = FileChange {
        path: target.path.clone(),
        old_path: target.old_path.clone(),
        kind: kind.unwrap_or(ChangeKind::Modified),
    };

    // Right-hand controls first so the path knows how much room is left.
    let mut controls_left = rect.right();
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(vec2(8.0, 0.0)))
            .layout(Layout::right_to_left(Align::Center)),
        |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            if icon_button(ui, icons::X, 14.0, "Close (Esc)").clicked() {
                close = true;
            }
            match target.source {
                DiffSource::Unstaged => {
                    if tint_button(ui, "Stage File", theme::green()).clicked() {
                        tab.run(ops::stage(std::slice::from_ref(&target.path)));
                    }
                    if kind != Some(ChangeKind::Conflicted)
                        && tint_button(ui, "Discard", theme::red()).clicked()
                    {
                        shared.show(Dialog::confirm(
                            "Discard changes",
                            format!("Discard all unstaged changes to \"{}\"? This cannot be undone.", target.path),
                            "Discard",
                            ops::discard(std::slice::from_ref(&file)),
                        ));
                    }
                }
                DiffSource::Staged => {
                    if tint_button(ui, "Unstage File", theme::red()).clicked() {
                        tab.run(ops::unstage(std::slice::from_ref(&target.path), unborn));
                    }
                }
                DiffSource::Commit(_) | DiffSource::Compare { .. } => {}
            }
            if let Some(diff) = diff.filter(|diff| !diff.binary) {
                ui.add_space(6.0);
                ui.label(egui::RichText::new(format!("-{}", diff.deletions)).color(theme::red()).font(theme::mono(12.0)));
                ui.label(egui::RichText::new(format!("+{}", diff.additions)).color(theme::green()).font(theme::mono(12.0)));
            }
            controls_left = ui.min_rect().left();
        },
    );

    let painter = ui.painter().with_clip_rect(Rect::from_min_max(rect.min, pos2(controls_left - 10.0, rect.bottom())));
    let mut x = rect.left() + 12.0;
    if let Some(kind) = kind {
        let (icon, color) = kind_icon(kind);
        x += widgets::text_left(&painter, rect, x, icon, theme::font(15.0), color) + 8.0;
    }
    let (dir, name) = match target.path.rfind('/') {
        Some(i) => (&target.path[..=i], &target.path[i + 1..]),
        None => ("", target.path.as_str()),
    };
    x += widgets::text_left(&painter, rect, x, dir, theme::font(13.5), theme::text_dim());
    x += widgets::text_left(&painter, rect, x, name, theme::bold(13.5), theme::text_bright()) + 12.0;

    let origin = match target.source {
        DiffSource::Unstaged => "unstaged changes".to_owned(),
        DiffSource::Staged => "staged changes".to_owned(),
        DiffSource::Commit(oid) => format!("in commit {}", short(oid)),
        DiffSource::Compare { from, to } => format!("{} → {}", short(from), short(to)),
    };
    x += widgets::text_left(&painter, rect, x, &origin, theme::font(12.0), theme::text_dim()) + 12.0;
    if let Some(old) = diff.and_then(|d| d.old_path.as_deref()).or(target.old_path.as_deref()) {
        widgets::text_left(&painter, rect, x, &format!("renamed from {old}"), theme::font(12.0), theme::text_faint());
    }

    close
}

/// A changed line of the open diff: `(hunk index, line index in the hunk)`.
type LineRef = (usize, usize);

/// The lines picked for line-level staging, and the diff they were picked in.
#[derive(Default)]
struct LineSelection {
    /// Held weakly so the address stays taken: "same address" then reliably
    /// means "same diff object".
    diff: Weak<FileDiff>,
    /// Which side of the working tree that diff was shown for. The view can
    /// briefly show one object under a new source: when the last unstaged
    /// lines of a file are staged, the target changes before the staged diff
    /// has loaded.
    source: Option<DiffSource>,
    /// What that diff shows. A reload that changed nothing (another file was
    /// saved, a fetch finished) produces a new object with the same
    /// fingerprint, and the selection carries over; any other diff clears it.
    fingerprint: u64,
    lines: BTreeSet<LineRef>,
    /// The line last clicked, where a shift-click range starts.
    anchor: Option<LineRef>,
    /// The frame in which the diff was last on screen.
    seen: u64,
}

impl LineSelection {
    /// Points the selection at `diff`, shown in frame number `frame`. The
    /// selection is dropped if that is a different change from the one it
    /// was made in, or if the diff has been off screen in between: lines
    /// picked before the view was closed should not come back, ready to be
    /// staged, when the same file is opened again later.
    fn follow(&mut self, target: &DiffTarget, diff: &Arc<FileDiff>, frame: u64) {
        if frame > self.seen + 1 {
            self.clear();
        }
        self.seen = frame;
        if std::ptr::eq(self.diff.as_ptr(), Arc::as_ptr(diff)) && self.source == Some(target.source) {
            return;
        }
        let fingerprint = fingerprint(target, diff);
        if fingerprint != self.fingerprint {
            self.clear();
        }
        self.fingerprint = fingerprint;
        self.diff = Arc::downgrade(diff);
        self.source = Some(target.source);
    }

    fn clear(&mut self) {
        self.lines.clear();
        self.anchor = None;
    }

    fn in_hunk(&self, hunk: usize) -> bool {
        self.lines.range((hunk, 0)..(hunk + 1, 0)).next().is_some()
    }

    /// A click on a changed line: toggles it, or with `extend` (shift held)
    /// selects every changed line from the previous click up to this one.
    fn click(&mut self, diff: &FileDiff, at: LineRef, extend: bool) {
        match self.anchor {
            Some(anchor) if extend => {
                let (from, to) = if anchor <= at { (anchor, at) } else { (at, anchor) };
                for h in from.0..=to.0 {
                    let Some(hunk) = diff.hunks.get(h) else { break };
                    let first = if h == from.0 { from.1 } else { 0 };
                    let last = if h == to.0 { to.1 } else { usize::MAX };
                    for (i, line) in hunk.lines.iter().enumerate().skip(first) {
                        if i > last {
                            break;
                        }
                        if line.origin != ' ' {
                            self.lines.insert((h, i));
                        }
                    }
                }
            }
            _ => {
                if !self.lines.remove(&at) {
                    self.lines.insert(at);
                }
                self.anchor = Some(at);
            }
        }
    }
}

/// What a diff shows, to tell a reload that changed nothing from another diff.
fn fingerprint(target: &DiffTarget, diff: &FileDiff) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (target.source, &target.path).hash(&mut hasher);
    for hunk in &diff.hunks {
        hunk.raw.hash(&mut hasher);
    }
    hasher.finish()
}

/// The lines of a diff one after the other, hunk after hunk, for selecting
/// and copying text. Hunk headers are not part of it.
struct DiffText<'a> {
    diff: &'a FileDiff,
    /// Where each hunk's first line is among all of them.
    starts: Vec<usize>,
    total: usize,
    /// Each hunk's lines as in the file, read back from the patch when first
    /// asked for, which is when copying.
    sources: OnceCell<Vec<Option<Vec<Cow<'a, str>>>>>,
}

impl<'a> DiffText<'a> {
    fn new(diff: &'a FileDiff) -> Self {
        let mut starts = Vec::with_capacity(diff.hunks.len());
        let mut total = 0;
        for hunk in &diff.hunks {
            starts.push(total);
            total += hunk.lines.len();
        }
        DiffText { diff, starts, total, sources: OnceCell::new() }
    }

    /// Hunk, and line in that hunk, of a line of the text.
    fn locate(&self, line: usize) -> (usize, usize) {
        // The last hunk starting at or before the line: an empty hunk shares
        // its start with the next one.
        let hunk = self.starts.partition_point(|start| *start <= line).saturating_sub(1);
        (hunk, line - self.starts[hunk])
    }
}

impl Lines for DiffText<'_> {
    fn count(&self) -> usize {
        self.total
    }

    fn shown(&self, line: usize) -> &str {
        let (hunk, index) = self.locate(line);
        &self.diff.hunks[hunk].lines[index].text
    }

    fn source(&self, line: usize) -> Option<&str> {
        let (hunk, index) = self.locate(line);
        let encoding = self.diff.format.encoding;
        let sources = self.sources.get_or_init(|| self.diff.hunks.iter().map(|hunk| hunk.sources(encoding)).collect());
        sources.get(hunk)?.as_ref()?.get(index).map(|text| text.as_ref())
    }
}

/// Where hunks and lines are in a diff's content, in points from its top.
/// With word wrap a line takes one row or more; without, exactly one.
struct Geometry {
    line_h: f32,
    hunk_h: f32,
    /// Top of every hunk's header.
    tops: Vec<f32>,
    height: f32,
    /// With word wrap: the first row of every line (the lines of all hunks
    /// one after the other), then the total. `None`: a row per line.
    rows: Option<Arc<Vec<u32>>>,
}

impl Geometry {
    fn new(text: &DiffText<'_>, line_h: f32, hunk_h: f32, rows: Option<Arc<Vec<u32>>>) -> Self {
        let mut geometry = Geometry { line_h, hunk_h, tops: Vec::with_capacity(text.starts.len()), height: 0.0, rows };
        let mut height = 0.0;
        for h in 0..text.starts.len() {
            geometry.tops.push(height);
            height += hunk_h + geometry.hunk_rows(text, h) as f32 * line_h;
        }
        if text.diff.truncated {
            height += hunk_h;
        }
        geometry.height = height;
        geometry
    }

    /// The first row of line `flat` of the text.
    fn row(&self, flat: usize) -> usize {
        self.rows.as_ref().map_or(flat, |rows| rows[flat] as usize)
    }

    /// Rows taken by the lines of hunk `h`.
    fn hunk_rows(&self, text: &DiffText<'_>, h: usize) -> usize {
        let start = text.starts[h];
        self.row(start + text.diff.hunks[h].lines.len()) - self.row(start)
    }

    /// Rows taken by line `flat` of the text.
    fn line_rows(&self, flat: usize) -> usize {
        self.row(flat + 1) - self.row(flat)
    }

    /// Top of line `i` of hunk `h`.
    fn line_top(&self, text: &DiffText<'_>, h: usize, i: usize) -> f32 {
        let start = text.starts[h];
        self.tops[h] + self.hunk_h + (self.row(start + i) - self.row(start)) as f32 * self.line_h
    }

    /// The line of hunk `h` that row `row` of the hunk belongs to.
    fn line_of_row(&self, text: &DiffText<'_>, h: usize, row: usize) -> usize {
        let start = text.starts[h];
        let len = text.diff.hunks[h].lines.len();
        match &self.rows {
            None => row.min(len.saturating_sub(1)),
            Some(rows) => {
                let base = rows[start] as usize;
                rows[start..start + len].partition_point(|first| *first as usize - base <= row).saturating_sub(1)
            }
        }
    }

    /// The lines of hunk `h` with a row between heights `from` and `to`.
    fn lines_between(&self, text: &DiffText<'_>, h: usize, from: f32, to: f32) -> Range<usize> {
        let len = text.diff.hunks[h].lines.len();
        let lines_top = self.tops[h] + self.hunk_h;
        let first = ((from - lines_top) / self.line_h).floor().max(0.0) as usize;
        let last = ((to - lines_top) / self.line_h).ceil().max(0.0) as usize;
        if first >= self.hunk_rows(text, h) || last == 0 {
            return 0..0;
        }
        self.line_of_row(text, h, first)..(self.line_of_row(text, h, last - 1) + 1).min(len)
    }

    /// What is at height `y` of the content: a line of the text, which of
    /// its rows, and the column when it is not "wherever x points". A point
    /// on a hunk header is at the start of the hunk's first line, one below
    /// the last line at the end of it (`usize::MAX`).
    fn locate(&self, text: &DiffText<'_>, y: f32) -> (usize, usize, Option<usize>) {
        let Some(last) = text.total.checked_sub(1) else { return (0, 0, Some(0)) };
        let Some(h) = self.tops.partition_point(|top| *top <= y).checked_sub(1) else {
            return (0, 0, Some(0));
        };
        let offset = y - self.tops[h] - self.hunk_h;
        if offset < 0.0 {
            return (text.starts[h].min(last), 0, Some(0));
        }
        let row = (offset / self.line_h) as usize;
        let start = text.starts[h];
        if row >= self.hunk_rows(text, h) {
            let len = text.diff.hunks[h].lines.len();
            return ((start + len).saturating_sub(1).min(last), 0, Some(usize::MAX));
        }
        let i = self.line_of_row(text, h, row);
        (start + i, row - (self.row(start + i) - self.row(start)), None)
    }
}

/// Row counts of the lines of a text (a diff, a blame) wrapped at some
/// width, kept from frame to frame: worked out again only for another
/// text, width or font.
#[derive(Clone)]
struct WrapCache {
    /// Held weakly so the address stays taken: "same address" then reliably
    /// means "same object".
    owner: Weak<dyn Any + Send + Sync>,
    columns: usize,
    rows: Arc<Vec<u32>>,
}

/// The first row of every line of `owner` (and the total) wrapped at
/// `columns`, from `count` (see [`first_rows`]) when not known yet. Kept
/// under `id`.
pub(super) fn wrap_rows<T: Any + Send + Sync>(
    ui: &Ui,
    id: egui::Id,
    owner: &Arc<T>,
    columns: usize,
    count: impl FnOnce() -> Vec<u32>,
) -> Arc<Vec<u32>> {
    if let Some(cache) = ui.data(|data| data.get_temp::<WrapCache>(id))
        && cache.columns == columns
        && std::ptr::addr_eq(cache.owner.as_ptr(), Arc::as_ptr(owner))
    {
        return cache.rows;
    }
    let rows = Arc::new(count());
    let owner: Weak<dyn Any + Send + Sync> = Arc::downgrade(owner) as Weak<dyn Any + Send + Sync>;
    ui.data_mut(|data| data.insert_temp(id, WrapCache { owner, columns, rows: rows.clone() }));
    rows
}

/// The first row of every one of `lines` wrapped at `columns`, then the
/// total. Goes through every line once; only lines longer than a row are
/// looked into.
pub(super) fn first_rows<'a>(lines: impl IntoIterator<Item = &'a str>, columns: usize) -> Vec<u32> {
    let mut rows = Vec::new();
    let mut at = 0u32;
    for line in lines {
        rows.push(at);
        at += text::wrapped_rows(line, columns) as u32;
    }
    rows.push(at);
    rows
}

/// The rows of a shown line wrapped at `columns` (or all of it on one row
/// without): the columns and the bytes of each.
pub(super) fn segments(shown: &str, columns: Option<usize>) -> Vec<(Range<usize>, Range<usize>)> {
    let breaks = columns.map(|columns| text::wrap_points(shown, columns)).unwrap_or_default();
    if breaks.is_empty() {
        return vec![(0..usize::MAX, 0..shown.len())];
    }
    let mut out = Vec::with_capacity(breaks.len() + 1);
    let (mut col, mut byte) = (0, 0);
    let mut next = breaks.iter().copied().peekable();
    for (c, (at, _)) in shown.char_indices().enumerate() {
        if next.peek() == Some(&c) {
            next.next();
            out.push((col..c, byte..at));
            (col, byte) = (c, at);
        }
    }
    out.push((col..shown.chars().count(), byte..shown.len()));
    out
}

/// Background of an added / removed line. One picked for staging keeps its
/// colour, only stronger: mixing in the selection blue would turn a removed
/// line grey. The gutter is what says "selected".
pub(super) fn line_background(origin: char, selected: bool) -> Option<Color32> {
    let (base, full) = match origin {
        '+' => (theme::diff_add_bg(), theme::green()),
        '-' => (theme::diff_del_bg(), theme::red()),
        _ => return None,
    };
    Some(if selected { theme::mix(base, full, 0.16) } else { base })
}

/// Room kept free right of wrapped text, so a row never runs under the
/// scroll bar.
pub(super) const WRAP_MARGIN: f32 = 24.0;
/// Narrower than this, wrapping would make a column of letters.
pub(super) const MIN_WRAP_COLUMNS: usize = 20;

fn body(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, target: &DiffTarget, diff: &Arc<FileDiff>) {
    // Preferences > Editor: font, size, line numbers, wrapping, colours.
    let look = editor::look();
    let font = look.font();
    let small = look.small();
    let line_h = look.line_height();
    let hunk_h = HUNK_H.max(line_h + 10.0);
    let scale = look.size / editor::DEFAULT_FONT_SIZE;
    let num_w = if look.numbers { (NUM_W * scale).round() } else { 0.0 };
    let sign_w = (22.0 * scale).round();
    let gutter = num_w * 2.0 + sign_w;
    let char_w = ui.painter().fonts_mut(|f| f.glyph_width(&font, 'm'));

    let text = DiffText::new(diff);
    let view = ui.available_rect_before_wrap();
    // Wrapped, the code fits the view and only scrolls up and down.
    let columns = look.wrap.then(|| {
        let room = view.width() - gutter - WRAP_MARGIN;
        ((room / char_w).floor().max(0.0) as usize).max(MIN_WRAP_COLUMNS)
    });
    let rows = columns.map(|columns| {
        let lines = || diff.hunks.iter().flat_map(|hunk| &hunk.lines).map(|line| line.text.as_str());
        wrap_rows(ui, ui.id().with("diff-wrap"), diff, columns, || first_rows(lines(), columns))
    });
    let geometry = Geometry::new(&text, line_h, hunk_h, rows);
    let height = geometry.height;
    let text_w = if columns.is_some() { 0.0 } else { gutter + diff.max_chars as f32 * char_w + 40.0 };

    // Colours arrive from a background thread; until then lines are plain.
    let style = highlight::style_for(ui.ctx(), diff, &target.path, look.highlight);

    // Lines can be picked wherever they can be staged: working tree diffs of
    // files that are not in the middle of a merge.
    let stageable = !target.source.is_committed()
        && !diff.file_header.is_empty()
        && diff.kind != Some(ChangeKind::Conflicted);
    let selection = stageable.then(|| {
        ui.data_mut(|data| {
            data.get_temp_mut_or_default::<Arc<Mutex<LineSelection>>>(egui::Id::new("diff-line-selection")).clone()
        })
    });
    let mut selection = selection.as_ref().map(|s| s.lock().unwrap_or_else(|e| e.into_inner()));
    if let Some(selection) = &mut selection {
        selection.follow(target, diff, ui.ctx().cumulative_frame_nr());
    }

    // Selected text, kept per place a diff is shown in (this view, the file
    // history).
    let text_id = ui.id().with("diff-text-selection");
    let text_selection =
        ui.data_mut(|data| data.get_temp_mut_or_default::<Arc<Mutex<textsel::Selection>>>(text_id).clone());
    let mut text_selection = text_selection.lock().unwrap_or_else(|e| e.into_inner());
    text_selection.follow(diff, ui.ctx().cumulative_frame_nr(), || fingerprint(target, diff));

    let scroll = if columns.is_some() { ScrollArea::vertical() } else { ScrollArea::both() };
    scroll.id_salt("diff-body").auto_shrink([false, false]).show_viewport(ui, |ui, viewport| {
        let size = vec2(text_w.max(ui.available_width()), height.max(ui.available_height()));
        let sense = if stageable { Sense::click() } else { Sense::hover() };
        let (content, response) = ui.allocate_exact_size(size, sense);
        let painter = ui.painter().clone();

        // Gutter and hunk controls stay put while the text scrolls sideways.
        let left = content.left() + viewport.min.x;
        let right = content.left() + viewport.max.x;
        // Where every line's text starts, scrolled sideways.
        let text_left = content.left() + gutter;

        // The code, right of the gutter, is for selecting text; the gutter
        // for picking lines to stage.
        let text_rect = Rect::from_min_max(
            pos2(left + gutter, content.top() + viewport.min.y),
            pos2(right, content.top() + viewport.max.y),
        );
        let hit = |pos: Pos2| {
            if text.count() == 0 {
                return Pos::default();
            }
            let (line, row, col) = geometry.locate(&text, pos.y - content.top());
            let shown = text.shown(line);
            let col = match col {
                Some(usize::MAX) => shown.chars().count(),
                Some(col) => col,
                None => {
                    let parts = segments(shown, columns);
                    let (cols, bytes) = parts[row.min(parts.len() - 1)].clone();
                    let galley = painter.layout_no_wrap(shown[bytes].to_owned(), font.clone(), theme::text());
                    cols.start + textsel::column_at(&galley, pos.x - text_left)
                }
            };
            Pos::new(line, col)
        };
        let text_response = text_selection.interact(ui, ui.id().with("text"), text_rect, view, &text, hit);
        text_response.context_menu(|ui| text_selection.menu(ui, &text));

        // The changed line under a point in the gutter, if any. Hunk
        // headers, context lines and the space below the diff give `None`.
        let line_at = |pos: Pos2| -> Option<LineRef> {
            if pos.x >= left + gutter || text.count() == 0 {
                return None;
            }
            let (flat, _, col) = geometry.locate(&text, pos.y - content.top());
            if col.is_some() {
                return None;
            }
            let (h, i) = text.locate(flat);
            (diff.hunks[h].lines.get(i)?.origin != ' ').then_some((h, i))
        };
        let mut hovered = None;
        if let Some(selection) = &mut selection {
            hovered = response.hover_pos().and_then(line_at);
            if hovered.is_some() {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            }
            if response.clicked()
                && let Some(at) = response.interact_pointer_pos().and_then(line_at)
            {
                selection.click(diff, at, ui.input(|i| i.modifiers.shift));
            }
        }

        let text_clip = Rect::from_min_max(pos2(left + gutter, painter.clip_rect().top()), painter.clip_rect().max);
        let text_painter = painter.with_clip_rect(text_clip.intersect(painter.clip_rect()));
        // The line numbers, or without them the column of +/- signs, are
        // where a picked line is marked.
        let numbers_right = if look.numbers { left + num_w * 2.0 - 1.0 } else { left + sign_w - 1.0 };

        for (h, hunk) in diff.hunks.iter().enumerate() {
            let top = geometry.tops[h];
            let bottom = top + hunk_h + geometry.hunk_rows(&text, h) as f32 * line_h;
            if bottom < viewport.min.y || top > viewport.max.y {
                continue;
            }

            // Hunk header.
            let y = content.top() + top;
            let bar = Rect::from_min_max(pos2(left, y), pos2(right, y + hunk_h));
            painter.rect_filled(bar, 0.0, theme::diff_hunk_bg());
            painter.hline(bar.x_range(), bar.top(), Stroke::new(1.0, theme::border()));
            painter.text(pos2(left + 12.0, bar.center().y), Align2::LEFT_CENTER, &hunk.header, small.clone(), theme::text_dim());
            hunk_buttons(ui, tab, shared, target, diff, h, bar, selection.as_deref_mut());

            // Lines.
            let styled = style.hunk(h);
            for i in geometry.lines_between(&text, h, viewport.min.y, viewport.max.y) {
                let line = &hunk.lines[i];
                let flat = text.starts[h] + i;
                let y = content.top() + geometry.line_top(&text, h, i);
                let row = Rect::from_min_max(pos2(left, y), pos2(right, y + geometry.line_rows(flat) as f32 * line_h));
                let selected = selection.as_ref().is_some_and(|s| s.lines.contains(&(h, i)));
                let sign_color = match line.origin {
                    '+' => theme::green(),
                    '-' => theme::red(),
                    _ => theme::text_faint(),
                };
                if let Some(bg) = line_background(line.origin, selected) {
                    painter.rect_filled(row, 0.0, bg);
                }
                // A picked line is marked in the gutter the way selected rows
                // are everywhere else; a line that a click would pick gets a
                // hint of it.
                let numbers = Rect::from_min_max(row.min, pos2(numbers_right, row.bottom()));
                if selected {
                    widgets::row_background(&painter, numbers, true, false);
                } else if hovered == Some((h, i)) {
                    painter.rect_filled(numbers, 0.0, theme::with_alpha(theme::accent(), 46));
                }

                // Numbers and sign go with the line's first row.
                let cy = y + line_h / 2.0;
                if look.numbers {
                    let number_color = if selected { theme::text_bright() } else { theme::text_faint() };
                    let number = |n: Option<u32>, x: f32| {
                        if let Some(n) = n {
                            painter.text(pos2(x, cy), Align2::RIGHT_CENTER, n.to_string(), small.clone(), number_color);
                        }
                    };
                    number(line.old_no, left + num_w - 6.0);
                    number(line.new_no, left + num_w * 2.0 - 6.0);
                }
                if line.origin != ' ' {
                    let x = left + num_w * 2.0 + (sign_w * 0.36).round();
                    painter.text(pos2(x, cy), Align2::CENTER_CENTER, line.origin, font.clone(), sign_color);
                }

                let spans = styled.as_ref().map_or(&[][..], |s| s.spans(i));
                let parts = segments(&line.text, columns);
                // Only a wrapped line needs its length for the selection.
                let len = if parts.len() > 1 { parts.last().map_or(0, |(cols, _)| cols.end) } else { 0 };
                for (k, (cols, bytes)) in parts.into_iter().enumerate() {
                    let top = y + k as f32 * line_h;
                    let galley = text_painter.layout_job(highlight::layout_part(&line.text, bytes, spans, &font));
                    let origin = pos2(text_left, top + line_h / 2.0 - galley.size().y / 2.0);
                    let part = Rect::from_min_max(pos2(left, top), pos2(right, top + line_h));
                    // The part of the line that differs from its counterpart,
                    // behind the text. Positions come from the laid-out glyphs,
                    // as wide characters do not keep to the monospace grid.
                    if let (Some(styled), Some(placed)) = (&styled, galley.rows.first()) {
                        let glyphs = &placed.row.glyphs;
                        for range in styled.emphasis(i) {
                            let (start, end) = (range.start.max(cols.start), range.end.min(cols.end));
                            if start >= end {
                                continue;
                            }
                            let (Some(from), Some(to)) = (glyphs.get(start - cols.start), glyphs.get(end - cols.start - 1))
                            else {
                                continue;
                            };
                            let x = origin.x + placed.pos.x;
                            let mark = Rect::from_min_max(
                                pos2(x + from.pos.x, part.top() + 1.0),
                                pos2(x + to.pos.x + to.advance_width, part.bottom() - 1.0),
                            );
                            text_painter.rect_filled(mark, 2.0, highlight::emphasis_color(line.origin));
                        }
                    }
                    if len == 0 {
                        text_selection.paint(&text_painter, flat, &galley, origin.x, part, char_w);
                    } else {
                        text_selection.paint_part(&text_painter, flat, &galley, origin.x, part, char_w, cols, len);
                    }
                    text_painter.galley(origin, galley, theme::text());
                }
            }
        }

        if diff.truncated {
            let y = content.top() + height - hunk_h;
            painter.text(
                pos2((left + right) / 2.0, y + hunk_h / 2.0),
                Align2::CENTER_CENTER,
                "This diff is very large; the rest is not shown.",
                theme::font(12.0),
                theme::text_dim(),
            );
        }

        // Separator between the line numbers and the code.
        painter.vline(
            numbers_right,
            painter.clip_rect().y_range(),
            Stroke::new(1.0, theme::with_alpha(theme::border_light(), 90)),
        );
    });
}

/// "1 line" / "3 lines".
fn count_lines(n: usize) -> String {
    if n == 1 { "1 line".to_owned() } else { format!("{n} lines") }
}

// One argument more than clippy likes, but they are the same eight things at
// the single call site; a struct to carry them there would only rename them.
#[allow(clippy::too_many_arguments)]
fn hunk_buttons(
    ui: &mut Ui,
    tab: &mut RepoTab,
    shared: &mut Shared,
    target: &DiffTarget,
    diff: &FileDiff,
    index: usize,
    bar: Rect,
    selection: Option<&mut LineSelection>,
) {
    if target.source.is_committed() {
        return;
    }
    let Some(patch) = diff.hunk_patch(index) else { return };

    let mut right = bar.right() - 10.0;
    let mut place = |ui: &mut Ui, label: &str, tint| {
        let size = widgets::tint_button_size(ui, label);
        let rect = Rect::from_min_size(pos2(right - size.x, bar.center().y - size.y / 2.0), size);
        right = rect.left() - 6.0;
        widgets::tint_button_at(ui, rect, label, tint).clicked()
    };

    // With lines picked in this hunk, its buttons act on the picked lines
    // (of every hunk: the count says how many) instead of on the hunk.
    if let Some(selection) = selection
        && selection.in_hunk(index)
    {
        // While the patch is being applied the lines stay marked, and the
        // buttons are gone so it cannot be sent twice. The refreshed diff
        // then no longer has those lines, which drops the selection.
        if [STAGE_LINES, UNSTAGE_LINES, DISCARD_LINES].iter().any(|label| tab.is_running(label)) {
            return;
        }
        let count = selection.lines.len();
        let lines = count_lines(count);
        let patch = |apply| diff.lines_patch(&selection.lines.iter().copied().collect::<Vec<LineRef>>(), apply);
        let stale = |shared: &mut Shared| {
            shared.toast(
                ToastKind::Error,
                "Could not build a patch from the selected lines",
                "The diff may have changed. Select the lines again.",
            );
        };
        match target.source {
            DiffSource::Unstaged => {
                if place(ui, &format!("Stage {lines}"), theme::green()) {
                    match patch(LineApply::Forward) {
                        Some(patch) => tab.run(ops::apply_patch(STAGE_LINES, patch, true, false)),
                        None => stale(shared),
                    }
                }
                if place(ui, &format!("Discard {lines}"), theme::red()) {
                    match patch(LineApply::Reverse) {
                        Some(patch) => shared.show(Dialog::confirm(
                            "Discard lines",
                            format!(
                                "Discard the selected {lines} from the working tree? This cannot be undone.",
                                lines = if count == 1 { "line".to_owned() } else { lines.clone() },
                            ),
                            "Discard",
                            ops::apply_patch(DISCARD_LINES, patch, false, true),
                        )),
                        None => stale(shared),
                    }
                }
            }
            DiffSource::Staged => {
                if place(ui, &format!("Unstage {lines}"), theme::red()) {
                    match patch(LineApply::Reverse) {
                        Some(patch) => tab.run(ops::apply_patch(UNSTAGE_LINES, patch, true, true)),
                        None => stale(shared),
                    }
                }
            }
            DiffSource::Commit(_) | DiffSource::Compare { .. } => {}
        }
        if place(ui, "Clear selection", theme::text_dim()) {
            selection.clear();
        }
        return;
    }

    match target.source {
        DiffSource::Unstaged => {
            if place(ui, "Stage Hunk", theme::green()) {
                tab.run(ops::apply_patch("Stage hunk", patch.clone(), true, false));
            }
            if place(ui, "Discard Hunk", theme::red()) {
                shared.show(Dialog::confirm(
                    "Discard hunk",
                    "Discard this hunk from the working tree? This cannot be undone.",
                    "Discard",
                    ops::apply_patch("Discard hunk", patch, false, true),
                ));
            }
        }
        DiffSource::Staged => {
            if place(ui, "Unstage Hunk", theme::red()) {
                tab.run(ops::apply_patch("Unstage hunk", patch, true, true));
            }
        }
        DiffSource::Commit(_) | DiffSource::Compare { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{DiffLine, Hunk};

    /// A diff with one hunk per string, one line per character: `" -+ "` is
    /// context, a removal, an addition, context.
    fn diff(hunks: &[&str]) -> Arc<FileDiff> {
        let hunks = hunks
            .iter()
            .map(|origins| Hunk {
                header: String::new(),
                raw: origins.as_bytes().to_vec(),
                lines: origins
                    .chars()
                    .map(|origin| DiffLine { origin, old_no: None, new_no: None, text: String::new() })
                    .collect(),
            })
            .collect();
        Arc::new(FileDiff { hunks, ..Default::default() })
    }

    fn target(source: DiffSource) -> DiffTarget {
        DiffTarget { source, path: "src/file.rs".to_owned(), old_path: None }
    }

    fn picked(selection: &LineSelection) -> Vec<LineRef> {
        selection.lines.iter().copied().collect()
    }

    #[test]
    fn a_click_toggles_one_line() {
        let diff = diff(&[" -+ "]);
        let mut selection = LineSelection::default();
        selection.click(&diff, (0, 1), false);
        selection.click(&diff, (0, 2), false);
        assert_eq!(picked(&selection), [(0, 1), (0, 2)]);
        selection.click(&diff, (0, 1), false);
        assert_eq!(picked(&selection), [(0, 2)]);
        assert!(selection.in_hunk(0));
        assert!(!selection.in_hunk(1));
    }

    #[test]
    fn a_shift_click_selects_the_changed_lines_in_between() {
        let diff = diff(&[" -+ +", " - ", "++ -"]);
        let mut selection = LineSelection::default();

        // Within a hunk, skipping the context line in the middle.
        selection.click(&diff, (0, 1), false);
        selection.click(&diff, (0, 4), true);
        assert_eq!(picked(&selection), [(0, 1), (0, 2), (0, 4)]);

        // Across hunks, and upwards: the range starts at the last plain
        // click, wherever the shift-click lands relative to it.
        selection.clear();
        selection.click(&diff, (2, 1), false);
        selection.click(&diff, (0, 2), true);
        assert_eq!(picked(&selection), [(0, 2), (0, 4), (1, 1), (2, 0), (2, 1)]);
        // A second shift-click extends from the same starting point.
        selection.click(&diff, (2, 3), true);
        assert_eq!(picked(&selection), [(0, 2), (0, 4), (1, 1), (2, 0), (2, 1), (2, 3)]);

        // With nothing clicked before, shift-click is a plain click.
        selection.clear();
        selection.click(&diff, (1, 1), true);
        assert_eq!(picked(&selection), [(1, 1)]);
    }

    #[test]
    fn the_selection_survives_a_reload_of_the_same_change_only() {
        let unstaged = target(DiffSource::Unstaged);
        let first = diff(&[" -+ ", " + "]);
        let mut selection = LineSelection::default();
        selection.follow(&unstaged, &first, 100);
        selection.click(&first, (0, 1), false);

        // The same object, frame after frame.
        selection.follow(&unstaged, &first, 101);
        assert_eq!(picked(&selection), [(0, 1)]);
        // The same change loaded again after a refresh.
        let reloaded = diff(&[" -+ ", " + "]);
        drop(first);
        selection.follow(&unstaged, &reloaded, 102);
        assert_eq!(picked(&selection), [(0, 1)]);

        // The same lines shown as staged are another diff...
        selection.follow(&target(DiffSource::Staged), &reloaded, 103);
        assert!(selection.lines.is_empty() && selection.anchor.is_none());
        // ...and so is a diff whose content changed.
        selection.click(&reloaded, (0, 1), false);
        selection.follow(&target(DiffSource::Staged), &diff(&[" -+ ", " - "]), 104);
        assert!(selection.lines.is_empty());
    }

    #[test]
    fn the_selection_does_not_outlive_the_view() {
        let unstaged = target(DiffSource::Unstaged);
        let shown = diff(&[" -+ "]);
        let mut selection = LineSelection::default();
        selection.follow(&unstaged, &shown, 7);
        selection.click(&shown, (0, 1), false);
        selection.follow(&unstaged, &shown, 8);
        assert_eq!(picked(&selection), [(0, 1)]);

        // The view was closed for a few frames and the same file reopened.
        selection.follow(&unstaged, &shown, 12);
        assert!(selection.lines.is_empty() && selection.anchor.is_none());
    }

    #[test]
    fn the_text_runs_through_the_hunks_and_copies_what_is_in_the_file() {
        // A hunk made the way the loader makes them: the patch bytes, and the
        // lines prepared for display.
        let hunk = |header: &str, lines: &[(char, &str)]| {
            let mut raw = format!("{header}\n").into_bytes();
            for (origin, text) in lines {
                raw.push(*origin as u8);
                raw.extend_from_slice(text.as_bytes());
                raw.push(b'\n');
            }
            let lines = lines
                .iter()
                .map(|(origin, text)| DiffLine {
                    origin: *origin,
                    old_no: None,
                    new_no: None,
                    text: crate::git::text::display_text(text.as_bytes()),
                })
                .collect();
            Hunk { header: header.to_owned(), raw, lines }
        };
        let diff = FileDiff {
            hunks: vec![
                hunk("@@ -1,2 +1,2 @@", &[(' ', "fn main() {"), ('-', "\told();"), ('+', "\tnew();\r")]),
                hunk("@@ -9 +9 @@", &[(' ', "}")]),
            ],
            ..Default::default()
        };
        let text = DiffText::new(&diff);
        assert_eq!(text.count(), 4);
        assert_eq!(text.shown(2), "    new();");
        assert_eq!(text.locate(3), (1, 0));

        // Where points fall: hunk headers lead to the first line after them.
        const LINE_H: f32 = 19.0;
        let geometry = Geometry::new(&text, LINE_H, HUNK_H, None);
        let tops = &geometry.tops;
        assert_eq!(tops, &[0.0, HUNK_H + 3.0 * LINE_H]);
        assert_eq!(geometry.height, HUNK_H * 2.0 + 4.0 * LINE_H);
        assert_eq!(geometry.locate(&text, -3.0), (0, 0, Some(0)));
        assert_eq!(geometry.locate(&text, 5.0), (0, 0, Some(0)));
        assert_eq!(geometry.locate(&text, HUNK_H + LINE_H * 1.5), (1, 0, None));
        assert_eq!(geometry.locate(&text, tops[1] + 4.0), (3, 0, Some(0)));
        assert_eq!(geometry.locate(&text, tops[1] + HUNK_H + LINE_H * 5.0), (3, 0, Some(usize::MAX)));
        assert_eq!(geometry.lines_between(&text, 0, 0.0, HUNK_H + LINE_H * 1.5), 0..2);
        assert_eq!(geometry.lines_between(&text, 1, 0.0, tops[1] + 2.0), 0..0);

        // Copied: no headers, no markers, tabs as tabs, LF line endings.
        let mut selection = textsel::Selection::default();
        selection.select_all(&text);
        assert_eq!(selection.text(&text).as_deref(), Some("fn main() {\n\told();\n\tnew();\n}"));
    }

    #[test]
    fn wrapped_lines_take_rows_and_points_find_them() {
        let line = |text: &str| DiffLine { origin: '+', old_no: None, new_no: Some(1), text: text.to_owned() };
        let long = "word ".repeat(10); // 50 columns: three rows of 20
        let diff = FileDiff {
            hunks: vec![
                Hunk { header: String::new(), raw: Vec::new(), lines: vec![line("short"), line(&long), line("end")] },
                Hunk { header: String::new(), raw: Vec::new(), lines: vec![line(&long)] },
            ],
            ..Default::default()
        };
        let text = DiffText::new(&diff);
        let rows = first_rows(diff.hunks.iter().flat_map(|hunk| &hunk.lines).map(|line| line.text.as_str()), 20);
        assert_eq!(rows, [0, 1, 4, 5, 8]);

        let (line_h, hunk_h) = (10.0, 30.0);
        let geometry = Geometry::new(&text, line_h, hunk_h, Some(Arc::new(rows)));
        // The first hunk: a header and five rows; the second: a header and three.
        assert_eq!(geometry.tops, [0.0, 80.0]);
        assert_eq!(geometry.height, 80.0 + 30.0 + 30.0);
        assert_eq!(geometry.line_top(&text, 0, 2), 30.0 + 40.0);
        assert_eq!(geometry.line_rows(1), 3);
        // Each row of the long line, and the line after it.
        assert_eq!(geometry.locate(&text, 35.0), (0, 0, None));
        assert_eq!(geometry.locate(&text, 45.0), (1, 0, None));
        assert_eq!(geometry.locate(&text, 65.0), (1, 2, None));
        assert_eq!(geometry.locate(&text, 75.0), (2, 0, None));
        assert_eq!(geometry.locate(&text, 80.0 + 30.0 + 15.0), (3, 1, None));
        // Only the lines with a row in view are painted.
        assert_eq!(geometry.lines_between(&text, 0, 50.0, 62.0), 1..2);
        assert_eq!(geometry.lines_between(&text, 0, 0.0, 1000.0), 0..3);

        // The rows of a line as painted: columns and bytes of each.
        let parts = segments(&long, Some(20));
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], (0..20, 0..20));
        assert_eq!(parts[2], (40..50, 40..50));
        assert_eq!(segments("é wrapped", None), [(0..usize::MAX, 0.."é wrapped".len())]);
        let wide = segments("éééé éééé", Some(5));
        assert_eq!(wide, [(0..5, 0..9), (5..9, 9..17)]);
    }

    #[test]
    fn line_counts_read_naturally() {
        assert_eq!(count_lines(1), "1 line");
        assert_eq!(count_lines(12), "12 lines");
    }
}
