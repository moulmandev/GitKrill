//! The diff view that replaces the graph while a file is open.

use std::any::Any;
use std::borrow::Cow;
use std::cell::OnceCell;
use std::collections::BTreeSet;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::sync::{Arc, Mutex, Weak};

use eframe::egui::{
    self, Align, Align2, Color32, CursorIcon, FontId, Layout, Painter, Pos2, Rect, ScrollArea, Sense,
    Stroke, Ui, pos2, vec2,
};

use crate::git::diff::LineApply;
use crate::git::{ChangeKind, DiffLine, DiffSource, DiffTarget, FileChange, FileDiff, ops, short, text};
use crate::prefs::editor;
use crate::state::{Dialog, RepoTab, Shared, ToastKind};
use crate::theme::{self, icons};
use crate::ui::highlight;
use crate::ui::textsel::{self, Lines, Pos};
use crate::ui::widgets::{self, icon_button, kind_icon, tint_button};

const HUNK_H: f32 = 30.0;
const NUM_W: f32 = 46.0;
const BAR_H: f32 = 38.0;
const NOTICE_H: f32 = 30.0;
/// A title bar narrower than this shows the mode controls without labels.
const COMPACT_BELOW: f32 = 1000.0;

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

    // Worked out in another mode than the one now set (it was just
    // switched): ask again, and show none of it meanwhile, so that nothing
    // is shown or staged as what it is not.
    let wanted = crate::git::diff::mode();
    if let Some(Ok(diff)) = &data
        && diff.mode != wanted
    {
        let asked = egui::Id::new(("diff-mode-reload", &tab.path));
        if ui.data(|d| d.get_temp::<crate::git::diff::Mode>(asked)) != Some(wanted) {
            ui.data_mut(|d| d.insert_temp(asked, wanted));
            tab.reload_diff();
        }
    }

    match data {
        None => widgets::placeholder(ui, "Loading diff…"),
        Some(Err(e)) => widgets::placeholder(ui, &e),
        Some(Ok(diff)) if diff.mode != wanted => widgets::placeholder(ui, "Loading diff…"),
        Some(Ok(diff)) if diff.submodule.is_some() => {
            if let Some(change) = &diff.submodule {
                crate::ui::submodules::diff_view(ui, tab, shared, &target, change);
            }
        }
        Some(Ok(diff)) if diff.binary => match &diff.images {
            Some(images) => crate::ui::image_diff::show(ui, images),
            None => widgets::placeholder(ui, "Binary file - no text diff to show"),
        },
        Some(Ok(diff)) if diff.hunks.is_empty() && diff.only_whitespace => only_whitespace(ui, shared),
        Some(Ok(diff)) if diff.hunks.is_empty() => {
            widgets::placeholder(ui, "No content changes (the file mode or name changed)")
        }
        Some(Ok(diff)) => body(ui, tab, shared, &target, &diff),
    }
}

/// Paints a diff with no staging controls, for views that only look at
/// history. `target` must have a commit as its source. A diff that
/// ignoring whitespace left empty gets the placeholder that says so.
pub fn read_only(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, target: &DiffTarget, diff: &Arc<FileDiff>) {
    if let Some(change) = &diff.submodule {
        return crate::ui::submodules::diff_view(ui, tab, shared, target, change);
    }
    if diff.hunks.is_empty() && diff.only_whitespace {
        return only_whitespace(ui, shared);
    }
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
                        && !diff.is_some_and(|diff| diff.submodule.is_some())
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
            if let Some(diff) = diff.filter(|diff| !diff.binary && diff.submodule.is_none()) {
                ui.add_space(6.0);
                ui.label(egui::RichText::new(format!("-{}", diff.deletions)).color(theme::red()).font(theme::mono(12.0)));
                ui.label(egui::RichText::new(format!("+{}", diff.additions)).color(theme::green()).font(theme::mono(12.0)));
            }
            // Images have modes of their own.
            if !diff.is_some_and(|diff| diff.binary || diff.submodule.is_some()) {
                ui.add_space(8.0);
                crate::ui::diff_mode::controls(ui, &mut shared.settings.editor, rect.width() < COMPACT_BELOW);
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
pub(super) type LineRef = (usize, usize);

/// The lines picked for line-level staging, and the diff they were picked in.
#[derive(Default)]
pub(super) struct LineSelection {
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
    pub(super) lines: BTreeSet<LineRef>,
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
    pub(super) fn click(&mut self, diff: &FileDiff, at: LineRef, extend: bool) {
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
pub(super) fn fingerprint(target: &DiffTarget, diff: &FileDiff) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (target.source, &target.path).hash(&mut hasher);
    for hunk in &diff.hunks {
        hunk.raw.hash(&mut hasher);
    }
    hasher.finish()
}

/// The lines of a diff one after the other, hunk after hunk, for selecting
/// and copying text. Hunk headers are not part of it.
pub(super) struct DiffText<'a> {
    diff: &'a FileDiff,
    /// Where each hunk's first line is among all of them.
    starts: Vec<usize>,
    total: usize,
    /// Each hunk's lines as in the file, read back from the patch when first
    /// asked for, which is when copying.
    sources: OnceCell<Vec<Option<Vec<Cow<'a, str>>>>>,
}

impl<'a> DiffText<'a> {
    pub(super) fn new(diff: &'a FileDiff) -> Self {
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

    /// Line `index` of hunk `hunk` as it is in the file, when that is not
    /// what is shown.
    pub(super) fn source_at(&self, hunk: usize, index: usize) -> Option<&str> {
        let encoding = self.diff.format.encoding;
        let sources = self.sources.get_or_init(|| self.diff.hunks.iter().map(|hunk| hunk.sources(encoding)).collect());
        sources.get(hunk)?.as_ref()?.get(index).map(|text| text.as_ref())
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
        self.source_at(hunk, index)
    }
}

/// Where hunks and their items are in a diff's content, in points from its
/// top. An item is a line in the inline layout and a row of the two sides in
/// the split one. With word wrap an item takes one row or more; without,
/// exactly one.
pub(super) struct Geometry {
    line_h: f32,
    hunk_h: f32,
    /// Where each hunk's first item is among all of them, then the total.
    starts: Vec<usize>,
    /// Whether each hunk has a header bar: the unchanged stretches of a
    /// whole file have none.
    headed: Vec<bool>,
    /// Top of every hunk.
    tops: Vec<f32>,
    height: f32,
    /// With word wrap: the first row of every item (the items of all hunks
    /// one after the other), then the total. `None`: a row per item.
    rows: Option<Arc<Vec<u32>>>,
}

impl Geometry {
    /// `hunks` gives the number of items of each hunk and whether it has a
    /// header; a truncated diff ends in a note as tall as a header.
    pub(super) fn new(
        hunks: impl IntoIterator<Item = (usize, bool)>,
        truncated: bool,
        line_h: f32,
        hunk_h: f32,
        rows: Option<Arc<Vec<u32>>>,
    ) -> Self {
        let mut geometry =
            Geometry { line_h, hunk_h, starts: Vec::new(), headed: Vec::new(), tops: Vec::new(), height: 0.0, rows };
        let mut total = 0;
        for (items, headed) in hunks {
            geometry.starts.push(total);
            geometry.headed.push(headed);
            total += items;
        }
        geometry.starts.push(total);
        let mut height = 0.0;
        for h in 0..geometry.headed.len() {
            geometry.tops.push(height);
            height += geometry.header(h) + geometry.hunk_rows(h) as f32 * line_h;
        }
        if truncated {
            height += hunk_h;
        }
        geometry.height = height;
        geometry
    }

    /// The inline layout of `diff`: an item per line.
    fn inline(diff: &FileDiff, line_h: f32, hunk_h: f32, rows: Option<Arc<Vec<u32>>>) -> Self {
        Self::new(diff.hunks.iter().map(|hunk| (hunk.lines.len(), !hunk.gap)), diff.truncated, line_h, hunk_h, rows)
    }

    pub(super) fn height(&self) -> f32 {
        self.height
    }

    /// Top of hunk `h`'s header (or of its first item, without one).
    pub(super) fn top(&self, h: usize) -> f32 {
        self.tops[h]
    }

    /// Height of hunk `h`'s header bar.
    pub(super) fn header(&self, h: usize) -> f32 {
        if self.headed[h] { self.hunk_h } else { 0.0 }
    }

    /// Bottom of hunk `h`.
    pub(super) fn bottom(&self, h: usize) -> f32 {
        self.tops[h] + self.header(h) + self.hunk_rows(h) as f32 * self.line_h
    }

    /// The first row of item `flat`.
    fn row(&self, flat: usize) -> usize {
        self.rows.as_ref().map_or(flat, |rows| rows[flat] as usize)
    }

    /// Rows taken by the items of hunk `h`.
    pub(super) fn hunk_rows(&self, h: usize) -> usize {
        self.row(self.starts[h + 1]) - self.row(self.starts[h])
    }

    /// Rows taken by item `flat`.
    pub(super) fn item_rows(&self, flat: usize) -> usize {
        self.row(flat + 1) - self.row(flat)
    }

    /// Where hunk `h`'s items start among all of them.
    pub(super) fn first(&self, h: usize) -> usize {
        self.starts[h]
    }

    /// The hunk item `flat` is in.
    pub(super) fn hunk_of(&self, flat: usize) -> usize {
        // The last hunk starting at or before the item: an empty hunk shares
        // its start with the next one.
        self.starts.partition_point(|start| *start <= flat).saturating_sub(1).min(self.headed.len().saturating_sub(1))
    }

    /// Top of item `i` of hunk `h`.
    pub(super) fn item_top(&self, h: usize, i: usize) -> f32 {
        let start = self.starts[h];
        self.tops[h] + self.header(h) + (self.row(start + i) - self.row(start)) as f32 * self.line_h
    }

    /// The item of hunk `h` that row `row` of the hunk belongs to.
    fn item_of_row(&self, h: usize, row: usize) -> usize {
        let start = self.starts[h];
        let len = self.starts[h + 1] - start;
        match &self.rows {
            None => row.min(len.saturating_sub(1)),
            Some(rows) => {
                let base = rows[start] as usize;
                rows[start..start + len].partition_point(|first| *first as usize - base <= row).saturating_sub(1)
            }
        }
    }

    /// The hunks with something between heights `from` and `to`.
    pub(super) fn hunks_between(&self, from: f32, to: f32) -> Range<usize> {
        let first = self.tops.partition_point(|top| *top <= from).saturating_sub(1);
        let last = self.tops.partition_point(|top| *top < to);
        first..last.max(first)
    }

    /// The items of hunk `h` with a row between heights `from` and `to`.
    pub(super) fn items_between(&self, h: usize, from: f32, to: f32) -> Range<usize> {
        let len = self.starts[h + 1] - self.starts[h];
        let items_top = self.tops[h] + self.header(h);
        let first = ((from - items_top) / self.line_h).floor().max(0.0) as usize;
        let last = ((to - items_top) / self.line_h).ceil().max(0.0) as usize;
        if first >= self.hunk_rows(h) || last == 0 {
            return 0..0;
        }
        self.item_of_row(h, first)..(self.item_of_row(h, last - 1) + 1).min(len)
    }

    /// What is at height `y` of the content: an item, which of its rows, and
    /// the column when it is not "wherever x points". A point on a hunk
    /// header is at the start of the hunk's first item, one below the last
    /// item at the end of it (`usize::MAX`).
    pub(super) fn locate(&self, y: f32) -> (usize, usize, Option<usize>) {
        let total = self.starts.last().copied().unwrap_or(0);
        let Some(last) = total.checked_sub(1) else { return (0, 0, Some(0)) };
        let Some(h) = self.tops.partition_point(|top| *top <= y).checked_sub(1) else {
            return (0, 0, Some(0));
        };
        let start = self.starts[h];
        let offset = y - self.tops[h] - self.header(h);
        if offset < 0.0 {
            return (start.min(last), 0, Some(0));
        }
        let row = (offset / self.line_h) as usize;
        if row >= self.hunk_rows(h) {
            return (self.starts[h + 1].saturating_sub(1).min(last), 0, Some(usize::MAX));
        }
        let i = self.item_of_row(h, row);
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

/// Sizes and fonts of code as Preferences > Editor sets them, for one frame.
pub(super) struct Metrics {
    pub look: editor::Look,
    pub font: FontId,
    pub small: FontId,
    pub line_h: f32,
    pub hunk_h: f32,
    /// One column of line numbers; 0 without them.
    pub num_w: f32,
    /// The column of +/- signs.
    pub sign_w: f32,
    pub char_w: f32,
}

impl Metrics {
    fn new(ui: &Ui) -> Self {
        let look = editor::look();
        let font = look.font();
        let line_h = look.line_height();
        let scale = look.size / editor::DEFAULT_FONT_SIZE;
        Metrics {
            small: look.small(),
            hunk_h: HUNK_H.max(line_h + 10.0),
            num_w: if look.numbers { (NUM_W * scale).round() } else { 0.0 },
            sign_w: (22.0 * scale).round(),
            char_w: ui.painter().fonts_mut(|f| f.glyph_width(&font, 'm')),
            look,
            font,
            line_h,
        }
    }

    /// With word wrap, the columns of code that fit in `width` points.
    pub(super) fn columns(&self, width: f32) -> Option<usize> {
        self.look.wrap.then(|| ((width / self.char_w).floor().max(0.0) as usize).max(MIN_WRAP_COLUMNS))
    }
}

/// What both layouts paint a diff from.
pub(super) struct View<'a> {
    pub target: &'a DiffTarget,
    pub diff: &'a Arc<FileDiff>,
    /// Sizes and fonts.
    pub m: Metrics,
    pub style: Arc<highlight::DiffStyle>,
    /// The lines picked for staging; `None` where lines cannot be staged.
    pub picking: Option<&'a mut LineSelection>,
}

/// Whether lines of `diff` can be picked to stage, unstage or discard:
/// working tree diffs of files that are not in the middle of a merge, with
/// whitespace counted. With whitespace ignored the lines no longer match the
/// files byte for byte, and `git apply` would refuse them or, worse, apply
/// them somewhere else; the hunk buttons are left out then too.
pub fn can_apply_parts(target: &DiffTarget, diff: &FileDiff) -> bool {
    !target.source.is_committed()
        && !diff.file_header.is_empty()
        && diff.kind != Some(ChangeKind::Conflicted)
        && !diff.mode.ignore_whitespace
}

fn body(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, target: &DiffTarget, diff: &Arc<FileDiff>) {
    notices(ui, shared, target, diff);
    // Preferences > Editor: font, size, line numbers, wrapping, colours.
    let m = Metrics::new(ui);
    // Colours arrive from a background thread; until then lines are plain.
    let style = highlight::style_for(ui.ctx(), diff, &target.path, m.look.highlight);

    let picking = can_apply_parts(target, diff).then(|| {
        ui.data_mut(|data| {
            data.get_temp_mut_or_default::<Arc<Mutex<LineSelection>>>(egui::Id::new("diff-line-selection")).clone()
        })
    });
    let mut picking = picking.as_ref().map(|s| s.lock().unwrap_or_else(|e| e.into_inner()));
    if let Some(picking) = &mut picking {
        picking.follow(target, diff, ui.ctx().cumulative_frame_nr());
    }
    let view = View { target, diff, m, style, picking: picking.as_deref_mut() };
    if view.m.look.split {
        crate::ui::diff_split::show(ui, tab, shared, view);
    } else {
        inline(ui, tab, shared, view);
    }
}

/// Strips under the title saying what is left out of this diff, and why.
fn notices(ui: &mut Ui, shared: &mut Shared, target: &DiffTarget, diff: &FileDiff) {
    if diff.mode.ignore_whitespace
        && !target.source.is_committed()
        && !diff.file_header.is_empty()
        && diff.kind != Some(ChangeKind::Conflicted)
    {
        let text = "Whitespace changes are hidden, so hunks and lines cannot be staged or discarded one by one. \
                    The whole file still can.";
        if notice(ui, icons::PARAGRAPH, text, Some("Show Whitespace")) {
            shared.settings.editor.ignore_whitespace = false;
        }
    }
    if diff.too_long_for_whole_file
        && notice(ui, icons::FILE_TEXT, "This file is too long to show whole: only its hunks are shown.", Some("Show Hunks"))
    {
        shared.settings.editor.diff_context = editor::DiffContext::Hunks;
    }
}

/// One strip of [`notices`]; true when its button was clicked.
fn notice(ui: &mut Ui, icon: &str, text: &str, action: Option<&str>) -> bool {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), NOTICE_H), Sense::hover());
    let painter = ui.painter().with_clip_rect(rect);
    painter.rect_filled(rect, 0.0, theme::mix(theme::bg_header(), theme::accent(), 0.1));
    painter.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::border()));
    let mut clicked = false;
    let mut right = rect.right() - 10.0;
    if let Some(action) = action {
        let size = widgets::tint_button_size(ui, action);
        let button = Rect::from_min_size(pos2(right - size.x, rect.center().y - size.y / 2.0), size);
        clicked = widgets::tint_button_at(ui, button, action, theme::accent()).clicked();
        right = button.left() - 10.0;
    }
    let x = rect.left() + 12.0;
    let x = x + widgets::text_left(&painter, rect, x, icon, theme::font(14.0), theme::accent()) + 8.0;
    let galley = widgets::elided(&painter, text, theme::font(12.0), theme::text(), (right - x).max(0.0));
    widgets::galley_left(&painter, rect, x, galley);
    clicked
}

/// The placeholder of a diff that ignoring whitespace left empty.
fn only_whitespace(ui: &mut Ui, shared: &mut Shared) {
    let rect = ui.available_rect_before_wrap();
    ui.allocate_rect(rect, Sense::hover());
    let center = rect.center();
    ui.painter().text(
        center - vec2(0.0, 16.0),
        Align2::CENTER_CENTER,
        "Only whitespace changed in this file, and whitespace changes are hidden",
        theme::font(14.0),
        theme::text_faint(),
    );
    let label = "Show Whitespace Changes";
    let size = widgets::tint_button_size(ui, label);
    let button = Rect::from_center_size(center + vec2(0.0, 14.0), size);
    if widgets::tint_button_at(ui, button, label, theme::accent()).clicked() {
        shared.settings.editor.ignore_whitespace = false;
    }
}

/// The line at the top of a diff, kept there when the layout or the mode
/// changes under it: inline and split, hunks and the whole file, wrapping.
#[derive(Clone)]
struct Anchor {
    target: DiffTarget,
    layout: AnchorLayout,
    /// The line's number in the new version, or in the old one (`old`) for
    /// a removed line.
    number: u32,
    old: bool,
    /// How far the top of the view was below the top of the line.
    offset: f32,
}

/// What an [`Anchor`] is kept across: split, wrapped, and how the diff was
/// worked out.
type AnchorLayout = (bool, bool, crate::git::diff::Mode);

/// Keeps the line at the top of a diff in view when its layout changes.
pub(super) struct Anchoring {
    id: egui::Id,
    target: DiffTarget,
    layout: AnchorLayout,
}

impl Anchoring {
    pub(super) fn new(ui: &Ui, view: &View<'_>) -> Self {
        Anchoring {
            id: ui.id().with("diff-anchor"),
            target: view.target.clone(),
            layout: (view.m.look.split, view.m.look.wrap, view.diff.mode),
        }
    }

    /// Where to scroll to after a change of layout: the top of the first
    /// item whose line (`numbers` gives an item's old and new line numbers)
    /// is the one that was at the top, or the first after it.
    pub(super) fn restore(
        &self,
        ui: &Ui,
        geometry: &Geometry,
        hunks: usize,
        numbers: impl Fn(usize, usize) -> Option<(Option<u32>, Option<u32>)>,
    ) -> Option<f32> {
        let anchor = ui.data(|d| d.get_temp::<Anchor>(self.id))?;
        if anchor.target != self.target || anchor.layout == self.layout {
            return None;
        }
        for h in 0..hunks {
            let mut i = 0;
            while let Some((old, new)) = numbers(h, i) {
                let number = if anchor.old { old } else { new };
                if number.is_some_and(|n| n >= anchor.number) {
                    return Some(geometry.item_top(h, i) + anchor.offset);
                }
                i += 1;
            }
        }
        None
    }

    /// Notes the line at `top`, the top of the view: `number` gives its
    /// numbers in the new version and the old one.
    pub(super) fn keep(&self, ui: &Ui, geometry: &Geometry, top: f32, number: impl Fn(usize) -> Option<(u32, bool)>) {
        let (flat, _, col) = geometry.locate(top);
        if col == Some(usize::MAX) {
            return;
        }
        let Some((number, old)) = number(flat) else { return };
        let h = geometry.hunk_of(flat);
        let offset = top - geometry.item_top(h, flat - geometry.first(h));
        let anchor = Anchor { target: self.target.clone(), layout: self.layout, number, old, offset };
        ui.data_mut(|d| d.insert_temp(self.id, anchor));
    }
}

/// The number that finds a line again in another layout: its new one, or
/// the old one of a removed line.
pub(super) fn anchor_number(line: &DiffLine) -> Option<(u32, bool)> {
    match line.origin {
        '-' => line.old_no.map(|n| (n, true)),
        _ => line.new_no.map(|n| (n, false)),
    }
}

/// Paints the text of `line`, line `i` of a hunk styled as `styled`, with its
/// first row at `top` and its text starting at `text_left`: colours, the
/// changed words, and the selected part, the selection being of a text in
/// which this is line `text_line`. `x` spans the line's width on screen.
#[allow(clippy::too_many_arguments)]
pub(super) fn paint_code(
    painter: &Painter,
    m: &Metrics,
    line: &DiffLine,
    styled: Option<&highlight::HunkView<'_>>,
    i: usize,
    text_left: f32,
    top: f32,
    x: (f32, f32),
    columns: Option<usize>,
    selection: &textsel::Selection,
    text_line: usize,
) {
    let spans = styled.map_or(&[][..], |s| s.spans(i));
    let parts = segments(&line.text, columns);
    // Only a wrapped line needs its length for the selection.
    let len = if parts.len() > 1 { parts.last().map_or(0, |(cols, _)| cols.end) } else { 0 };
    for (k, (cols, bytes)) in parts.into_iter().enumerate() {
        let top = top + k as f32 * m.line_h;
        let galley = painter.layout_job(highlight::layout_part(&line.text, bytes, spans, &m.font));
        let origin = pos2(text_left, top + m.line_h / 2.0 - galley.size().y / 2.0);
        let part = Rect::from_min_max(pos2(x.0, top), pos2(x.1, top + m.line_h));
        // The part of the line that differs from its counterpart, behind the
        // text. Positions come from the laid-out glyphs, as wide characters
        // do not keep to the monospace grid.
        if let (Some(styled), Some(placed)) = (styled, galley.rows.first()) {
            let glyphs = &placed.row.glyphs;
            for range in styled.emphasis(i) {
                let (start, end) = (range.start.max(cols.start), range.end.min(cols.end));
                if start >= end {
                    continue;
                }
                let (Some(from), Some(to)) = (glyphs.get(start - cols.start), glyphs.get(end - cols.start - 1)) else {
                    continue;
                };
                let left = origin.x + placed.pos.x;
                let mark = Rect::from_min_max(
                    pos2(left + from.pos.x, part.top() + 1.0),
                    pos2(left + to.pos.x + to.advance_width, part.bottom() - 1.0),
                );
                painter.rect_filled(mark, 2.0, highlight::emphasis_color(line.origin));
            }
        }
        if len == 0 {
            selection.paint(painter, text_line, &galley, origin.x, part, m.char_w);
        } else {
            selection.paint_part(painter, text_line, &galley, origin.x, part, m.char_w, cols, len);
        }
        painter.galley(origin, galley, theme::text());
    }
}

/// The column of `shown` (a line of code, row `row` of it when wrapped)
/// nearest to `x`, its text starting at `text_left`.
pub(super) fn column_at(painter: &Painter, m: &Metrics, shown: &str, row: usize, x: f32, text_left: f32, columns: Option<usize>) -> usize {
    let parts = segments(shown, columns);
    let (cols, bytes) = parts[row.min(parts.len() - 1)].clone();
    let galley = painter.layout_no_wrap(shown[bytes].to_owned(), m.font.clone(), theme::text());
    cols.start + textsel::column_at(&galley, x - text_left)
}

/// The line-number gutter of a changed line: marked when the line is
/// picked for staging, hinted when a click would pick it.
pub(super) fn paint_pick(painter: &Painter, numbers: Rect, selected: bool, hovered: bool) {
    if selected {
        widgets::row_background(painter, numbers, true, false);
    } else if hovered {
        painter.rect_filled(numbers, 0.0, theme::with_alpha(theme::accent(), 46));
    }
}

/// A line number right-aligned at `right`, beside a row centred on `cy`.
pub(super) fn paint_number(painter: &Painter, m: &Metrics, n: Option<u32>, right: f32, cy: f32, selected: bool) {
    if let Some(n) = n {
        let color = if selected { theme::text_bright() } else { theme::text_faint() };
        painter.text(pos2(right, cy), Align2::RIGHT_CENTER, n.to_string(), m.small.clone(), color);
    }
}

/// The +/- of a changed line, centred on `x`.
pub(super) fn paint_sign(painter: &Painter, m: &Metrics, origin: char, x: f32, cy: f32) {
    let color = match origin {
        '+' => theme::green(),
        '-' => theme::red(),
        _ => return,
    };
    painter.text(pos2(x, cy), Align2::CENTER_CENTER, origin, m.font.clone(), color);
}

/// A hunk's header bar, `bar` on screen, with its buttons.
#[allow(clippy::too_many_arguments)]
pub(super) fn paint_hunk_bar(
    ui: &mut Ui,
    painter: &Painter,
    tab: &mut RepoTab,
    shared: &mut Shared,
    view: &mut View<'_>,
    h: usize,
    bar: Rect,
) {
    painter.rect_filled(bar, 0.0, theme::diff_hunk_bg());
    painter.hline(bar.x_range(), bar.top(), Stroke::new(1.0, theme::border()));
    let header = &view.diff.hunks[h].header;
    painter.text(pos2(bar.left() + 12.0, bar.center().y), Align2::LEFT_CENTER, header, view.m.small.clone(), theme::text_dim());
    hunk_buttons(ui, tab, shared, view.target, view.diff, h, bar, view.picking.as_deref_mut());
}

/// The note that ends a diff cut short.
pub(super) fn paint_truncated(painter: &Painter, m: &Metrics, x: (f32, f32), bottom: f32) {
    painter.text(
        pos2((x.0 + x.1) / 2.0, bottom - m.hunk_h / 2.0),
        Align2::CENTER_CENTER,
        "This diff is very large; the rest is not shown.",
        theme::font(12.0),
        theme::text_dim(),
    );
}

/// The changed line of `diff` at `(h, i)`, if it is one.
pub(super) fn changed(diff: &FileDiff, h: usize, i: usize) -> Option<LineRef> {
    (diff.hunks.get(h)?.lines.get(i)?.origin != ' ').then_some((h, i))
}

/// Selected text, kept per place a diff is shown in (this view, the file
/// history) under `key`.
pub(super) fn text_selection(ui: &Ui, key: &str) -> Arc<Mutex<textsel::Selection>> {
    let id = ui.id().with(key);
    ui.data_mut(|data| data.get_temp_mut_or_default::<Arc<Mutex<textsel::Selection>>>(id).clone())
}

/// The inline layout: removed and added lines one under the other.
fn inline(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, mut view: View<'_>) {
    let (target, diff) = (view.target, view.diff);
    let gutter = view.m.num_w * 2.0 + view.m.sign_w;
    let text = DiffText::new(diff);
    let area = ui.available_rect_before_wrap();
    // Wrapped, the code fits the view and only scrolls up and down.
    let columns = view.m.columns(area.width() - gutter - WRAP_MARGIN);
    let rows = columns.map(|columns| {
        let lines = || diff.hunks.iter().flat_map(|hunk| &hunk.lines).map(|line| line.text.as_str());
        wrap_rows(ui, ui.id().with("diff-wrap"), diff, columns, || first_rows(lines(), columns))
    });
    let geometry = Geometry::inline(diff, view.m.line_h, view.m.hunk_h, rows);
    let height = geometry.height();
    let text_w = if columns.is_some() { 0.0 } else { gutter + diff.max_chars as f32 * view.m.char_w + 40.0 };

    let text_selection = text_selection(ui, "diff-text-selection");
    let mut text_selection = text_selection.lock().unwrap_or_else(|e| e.into_inner());
    text_selection.follow(diff, ui.ctx().cumulative_frame_nr(), || fingerprint(target, diff));

    let anchoring = Anchoring::new(ui, &view);
    let line_numbers = |h: usize, i: usize| diff.hunks.get(h)?.lines.get(i).map(|l| (l.old_no, l.new_no));
    let restore = anchoring.restore(ui, &geometry, diff.hunks.len(), line_numbers);

    let mut scroll = if columns.is_some() { ScrollArea::vertical() } else { ScrollArea::both() };
    if let Some(y) = restore {
        scroll = scroll.vertical_scroll_offset(y.max(0.0));
    }
    let stageable = view.picking.is_some();
    scroll.id_salt("diff-body").auto_shrink([false, false]).show_viewport(ui, |ui, viewport| {
        let m = &view.m;
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
            let (line, row, col) = geometry.locate(pos.y - content.top());
            let shown = text.shown(line);
            let col = match col {
                Some(usize::MAX) => shown.chars().count(),
                Some(col) => col,
                None => column_at(&painter, m, shown, row, pos.x, text_left, columns),
            };
            Pos::new(line, col)
        };
        let text_response = text_selection.interact(ui, ui.id().with("text"), text_rect, area, &text, hit);
        text_response.context_menu(|ui| text_selection.menu(ui, &text));

        // The changed line under a point in the gutter, if any. Hunk
        // headers, context lines and the space below the diff give `None`.
        let line_at = |pos: Pos2| -> Option<LineRef> {
            if pos.x >= left + gutter || text.count() == 0 {
                return None;
            }
            let (flat, _, col) = geometry.locate(pos.y - content.top());
            if col.is_some() {
                return None;
            }
            let (h, i) = text.locate(flat);
            changed(diff, h, i)
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

        let text_clip = Rect::from_min_max(pos2(left + gutter, painter.clip_rect().top()), painter.clip_rect().max);
        let text_painter = painter.with_clip_rect(text_clip.intersect(painter.clip_rect()));
        // The line numbers, or without them the column of +/- signs, are
        // where a picked line is marked.
        let numbers_right = if m.look.numbers { left + m.num_w * 2.0 - 1.0 } else { left + m.sign_w - 1.0 };

        for h in geometry.hunks_between(viewport.min.y, viewport.max.y) {
            let hunk = &diff.hunks[h];
            if geometry.bottom(h) < viewport.min.y {
                continue;
            }
            if !hunk.gap {
                let y = content.top() + geometry.top(h);
                let bar = Rect::from_min_max(pos2(left, y), pos2(right, y + view.m.hunk_h));
                paint_hunk_bar(ui, &painter, tab, shared, &mut view, h, bar);
            }
            let m = &view.m;

            // Lines.
            let styled = view.style.hunk(h);
            for i in geometry.items_between(h, viewport.min.y, viewport.max.y) {
                let line = &hunk.lines[i];
                let flat = text.starts[h] + i;
                let y = content.top() + geometry.item_top(h, i);
                let row = Rect::from_min_max(pos2(left, y), pos2(right, y + geometry.item_rows(flat) as f32 * m.line_h));
                let selected = view.picking.as_ref().is_some_and(|s| s.lines.contains(&(h, i)));
                if let Some(bg) = line_background(line.origin, selected) {
                    painter.rect_filled(row, 0.0, bg);
                }
                // A picked line is marked in the gutter the way selected rows
                // are everywhere else; a line that a click would pick gets a
                // hint of it.
                paint_pick(&painter, Rect::from_min_max(row.min, pos2(numbers_right, row.bottom())), selected, hovered == Some((h, i)));

                // Numbers and sign go with the line's first row.
                let cy = y + m.line_h / 2.0;
                if m.look.numbers {
                    paint_number(&painter, m, line.old_no, left + m.num_w - 6.0, cy, selected);
                    paint_number(&painter, m, line.new_no, left + m.num_w * 2.0 - 6.0, cy, selected);
                }
                paint_sign(&painter, m, line.origin, left + m.num_w * 2.0 + (m.sign_w * 0.36).round(), cy);
                paint_code(&text_painter, m, line, styled.as_ref(), i, text_left, y, (left, right), columns, &text_selection, flat);
            }
        }

        let m = &view.m;
        if diff.truncated {
            paint_truncated(&painter, m, (left, right), content.top() + height);
        }

        // Separator between the line numbers and the code.
        painter.vline(
            numbers_right,
            painter.clip_rect().y_range(),
            Stroke::new(1.0, theme::with_alpha(theme::border_light(), 90)),
        );

        anchoring.keep(ui, &geometry, viewport.min.y, |flat| {
            let (h, i) = text.locate(flat);
            anchor_number(diff.hunks.get(h)?.lines.get(i)?)
        });
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
    // Whitespace ignored, the hunks do not match the files: see
    // `can_apply_parts`.
    if target.source.is_committed() || diff.mode.ignore_whitespace {
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
                gap: false,
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
            Hunk { header: header.to_owned(), raw, lines, gap: false }
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
        let geometry = Geometry::inline(&diff, LINE_H, HUNK_H, None);
        let tops = &geometry.tops;
        assert_eq!(tops, &[0.0, HUNK_H + 3.0 * LINE_H]);
        assert_eq!(geometry.height(), HUNK_H * 2.0 + 4.0 * LINE_H);
        assert_eq!(geometry.locate(-3.0), (0, 0, Some(0)));
        assert_eq!(geometry.locate(5.0), (0, 0, Some(0)));
        assert_eq!(geometry.locate(HUNK_H + LINE_H * 1.5), (1, 0, None));
        assert_eq!(geometry.locate(tops[1] + 4.0), (3, 0, Some(0)));
        assert_eq!(geometry.locate(tops[1] + HUNK_H + LINE_H * 5.0), (3, 0, Some(usize::MAX)));
        assert_eq!(geometry.items_between(0, 0.0, HUNK_H + LINE_H * 1.5), 0..2);
        assert_eq!(geometry.items_between(1, 0.0, tops[1] + 2.0), 0..0);

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
                Hunk { lines: vec![line("short"), line(&long), line("end")], ..Hunk::default() },
                Hunk { lines: vec![line(&long)], ..Hunk::default() },
            ],
            ..Default::default()
        };
        let rows = first_rows(diff.hunks.iter().flat_map(|hunk| &hunk.lines).map(|line| line.text.as_str()), 20);
        assert_eq!(rows, [0, 1, 4, 5, 8]);

        let (line_h, hunk_h) = (10.0, 30.0);
        let geometry = Geometry::inline(&diff, line_h, hunk_h, Some(Arc::new(rows)));
        // The first hunk: a header and five rows; the second: a header and three.
        assert_eq!(geometry.tops, [0.0, 80.0]);
        assert_eq!(geometry.height(), 80.0 + 30.0 + 30.0);
        assert_eq!(geometry.item_top(0, 2), 30.0 + 40.0);
        assert_eq!(geometry.item_rows(1), 3);
        // Each row of the long line, and the line after it.
        assert_eq!(geometry.locate(35.0), (0, 0, None));
        assert_eq!(geometry.locate(45.0), (1, 0, None));
        assert_eq!(geometry.locate(65.0), (1, 2, None));
        assert_eq!(geometry.locate(75.0), (2, 0, None));
        assert_eq!(geometry.locate(80.0 + 30.0 + 15.0), (3, 1, None));
        // Only the lines with a row in view are painted.
        assert_eq!(geometry.items_between(0, 50.0, 62.0), 1..2);
        assert_eq!(geometry.items_between(0, 0.0, 1000.0), 0..3);

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
