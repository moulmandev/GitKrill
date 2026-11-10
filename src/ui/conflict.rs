//! Merge-conflict resolver for one file.
//!
//! The two sides sit next to each other with the result underneath. All
//! three panes show the same rows at the same height: text that merges on
//! its own appears once in each, and every conflict is a block with a
//! header strip (the controls) and as many rows as its tallest version. One
//! scroll position therefore serves all three panes, and only the rows in
//! view are laid out.
//!
//! What the user decides lives in a [`Draft`]; the backend in
//! `git::conflict` turns it into bytes. Clicking in the result pane opens a
//! text editor in place over that line or conflict, so anything can also be
//! fixed by typing.
//!
//! A view with unsaved work never disappears silently. Closing it asks
//! first, and if something else replaces the overlay behind its back the
//! view parks itself (see [`PARKED`]) and comes back the next time a
//! conflict is opened.

use std::borrow::Cow;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use eframe::egui::text::{CCursor, CCursorRange, LayoutJob, TextFormat};
use eframe::egui::{
    self, Align, Align2, Button, Color32, CursorIcon, FontId, Frame, Id, Key, Layout, Margin, Modal,
    Modifiers, Painter, Rect, RichText, Sense, Stroke, StrokeKind, TextBuffer, TextEdit, Ui,
    UiBuilder, Vec2, pos2, vec2,
};

use crate::git::conflict::{
    self, Chunk, ConflictFile, Content, Draft, Resolution, Segment, Side, SideInfo, Stage, TextMerge,
    Whole,
};
use crate::git::{ChangeKind, DiffTarget, Oid, text};
use crate::prefs::editor;
use crate::state::{Overlay, RepoTab, Shared, ToastKind};
use crate::theme::{self, icons};
use crate::ui::widgets::{self, icon_button, tint_button, tint_button_at, tint_button_size};

/// Height of a row of text, from the font size in Preferences > Editor.
fn line_h() -> f32 {
    editor::look().line_height()
}
/// Height of the strip of controls above each conflict.
const HEAD_H: f32 = 26.0;
const PANE_HEAD_H: f32 = 30.0;
const TOOLBAR_H: f32 = 38.0;
const NOTICE_H: f32 = 26.0;
/// Least width of the line-number column; long files get a wider one.
const MIN_GUTTER: f32 = 48.0;
/// The margin left of the text when line numbers are off.
const NO_NUMBERS_GUTTER: f32 = 12.0;
const TEXT_PAD: f32 = 8.0;
const SCROLLBAR: f32 = 12.0;
/// Share of the height given to the two sides; the result gets the rest.
const SIDES_SHARE: f32 = 0.45;
/// Lines are cut off after this many characters when painted.
const MAX_SHOWN: usize = 4000;
/// Space kept above a conflict that previous / next scrolls to.
fn jump_margin() -> f32 {
    line_h() * 2.0
}

/// Resolvers that were replaced while they had unsaved work, for instance
/// because another conflicted file was clicked. `Resolver::start` picks
/// them up again, so the work is either saved or knowingly discarded.
static PARKED: Mutex<Vec<Resolver>> = Mutex::new(Vec::new());

fn side_color(side: Side) -> Color32 {
    match side {
        Side::Ours => theme::blue(),
        Side::Theirs => theme::yellow(),
    }
}

/// Colour for text typed by hand.
fn typed() -> Color32 {
    theme::green()
}

enum Phase {
    /// Nothing requested yet.
    Fresh,
    Loading,
    Ready,
    Failed(String),
}

/// What the editor in the result pane is open on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    /// The whole resolution of a conflict.
    Conflict(usize),
    /// One line outside the conflicts.
    Line { seg: usize, line: usize },
}

struct Editing {
    target: Target,
    text: String,
    /// Row and column to put the cursor at when the editor first appears.
    place: Option<(usize, usize)>,
    /// The editor has been on screen for at least one frame.
    shown: bool,
    /// Where the cursor was last, to follow it only when it moves.
    cursor: Option<usize>,
}

/// A save (or whole-file choice) that is running.
struct Saving {
    label: String,
    started: Instant,
    /// The draft as it was written, which becomes the saved state.
    draft: Draft,
    then_close: bool,
    /// Title and body of the toast shown once it has worked.
    done: (String, String),
}

/// Where to go once this view has closed.
enum Then {
    Diff(DiffTarget),
    Conflict(String),
}

/// The overlay's state as the rest of the program sees it. Everything of
/// substance is in the boxed [`Resolver`], which keeps `Overlay` small and
/// lets a resolver outlive its view when it has to be parked.
pub struct ConflictView {
    /// Repository-relative path of the conflicted file.
    pub path: String,
    resolver: Box<Resolver>,
}

impl ConflictView {
    pub fn new(path: &str) -> Self {
        ConflictView { path: path.to_owned(), resolver: Box::new(Resolver::new(path)) }
    }
}

impl Drop for ConflictView {
    fn drop(&mut self) {
        if self.resolver.closing || !self.resolver.is_dirty() {
            return;
        }
        // Replaced from outside with unsaved work: keep it.
        let mut parked = std::mem::replace(&mut self.resolver, Box::new(Resolver::new(&self.path)));
        parked.editing = None;
        parked.asking = false;
        parked.replan = true;
        if let Ok(mut list) = PARKED.lock() {
            list.push(*parked);
        }
    }
}

struct Resolver {
    path: String,
    /// Working directory of the repository the view belongs to.
    repo: PathBuf,
    phase: Phase,
    file: Option<ConflictFile>,
    /// The choices on screen, and the ones the working file reflects.
    draft: Draft,
    saved: Draft,
    plan: Plan,
    replan: bool,
    /// The row height the plan was made for; another font size makes another.
    plan_line_h: f32,
    scroll: Vec2,
    /// The conflict that previous / next start from.
    current: usize,
    /// Conflict to scroll to once the rows are laid out.
    jump: Option<usize>,
    editing: Option<Editing>,
    saving: Option<Saving>,
    /// The "unsaved choices" question is open.
    asking: bool,
    then: Option<Then>,
    /// Identity of the snapshot last checked for this file's state.
    seen_snapshot: usize,
    /// A job is comparing the index stages with the ones that were loaded.
    checking: bool,
    /// Set once leaving is settled, so dropping the view parks nothing.
    closing: bool,
}

impl Resolver {
    fn new(path: &str) -> Self {
        Resolver {
            path: path.to_owned(),
            repo: PathBuf::new(),
            phase: Phase::Fresh,
            file: None,
            draft: Draft::default(),
            saved: Draft::default(),
            plan: Plan::default(),
            replan: true,
            plan_line_h: 0.0,
            scroll: Vec2::ZERO,
            current: 0,
            jump: None,
            editing: None,
            saving: None,
            asking: false,
            then: None,
            seen_snapshot: 0,
            checking: false,
            closing: false,
        }
    }

    fn is_dirty(&self) -> bool {
        matches!(self.phase, Phase::Ready) && self.draft != self.saved
    }

    fn job_key(&self) -> String {
        format!("conflict:{}", self.path)
    }

    fn check_key(&self) -> String {
        format!("conflict-check:{}", self.path)
    }

    fn load(&mut self, tab: &mut RepoTab) {
        let path = self.path.clone();
        tab.spawn_job(self.job_key(), move |loader| conflict::load(loader.repo(), &path));
        self.phase = Phase::Loading;
        self.file = None;
        self.editing = None;
        self.checking = false;
    }

    /// First frame: bring back parked work for this repository, or start
    /// reading the conflict.
    fn start(&mut self, tab: &mut RepoTab) {
        if !matches!(self.phase, Phase::Fresh) {
            return;
        }
        self.repo = tab.path.clone();
        // The resolver takes the place of whatever diff was open.
        tab.diff = None;

        if let Some(mut parked) = take_parked(&self.repo, |path| is_conflicted(tab, path)) {
            if parked.path != self.path {
                // Settle the unsaved file first, then go where the click meant.
                parked.then = Some(Then::Conflict(self.path.clone()));
                parked.asking = true;
            }
            // Have the stages checked again; they may have moved meanwhile.
            parked.seen_snapshot = 0;
            *self = parked;
            return;
        }
        self.load(tab);
    }

    /// Collects finished background work and follows the repository.
    fn poll(&mut self, tab: &mut RepoTab, shared: &mut Shared) {
        if matches!(self.phase, Phase::Loading)
            && let Some(result) = tab.take_result::<Result<ConflictFile, String>>(&self.job_key())
        {
            match result {
                Ok(file) => {
                    self.draft = match &file.content {
                        Content::Text { draft, .. } => draft.clone(),
                        Content::Whole(_) => Draft::default(),
                    };
                    self.saved = self.draft.clone();
                    let first_open = self.draft.choices.iter().position(|c| *c == Resolution::Unresolved);
                    self.current = first_open.unwrap_or(0);
                    self.jump = (!self.draft.choices.is_empty()).then_some(self.current);
                    self.scroll = Vec2::ZERO;
                    self.replan = true;
                    self.file = Some(file);
                    self.phase = Phase::Ready;
                }
                Err(message) => self.phase = Phase::Failed(message),
            }
        }

        // The repository's encoding was changed with nothing decided yet:
        // read the file again in the new one.
        if matches!(self.phase, Phase::Ready)
            && !self.is_dirty()
            && self.saving.is_none()
            && self.file.as_ref().is_some_and(|file| file.default_encoding != text::encoding_for(&self.repo))
        {
            self.load(tab);
        }

        // A save that has finished. Its error, if any, was already shown by
        // the tab as a toast; the choices stay as they are so it can be retried.
        if let Some(saving) = &self.saving
            && let Some(succeeded) = tab.take_outcome(&saving.label, saving.started)
            && let Some(saving) = self.saving.take()
            && succeeded {
                self.saved = saving.draft;
                self.closing |= saving.then_close;
                shared.toast(ToastKind::Success, saving.done.0, saving.done.1);
            }

        // Whenever the repository state is refreshed, see what became of
        // this file. Not while a save is running: the file watcher can show
        // us the index half-way through it, before its outcome is known.
        let snapshot = tab.snap.as_ref().map_or(0, |snap| Arc::as_ptr(snap) as usize);
        if snapshot != self.seen_snapshot && self.saving.is_none() && !matches!(self.phase, Phase::Fresh) {
            self.seen_snapshot = snapshot;
            if !is_conflicted(tab, &self.path) {
                // Resolved (by us or elsewhere) or the operation was aborted.
                self.closing = true;
            } else if matches!(self.phase, Phase::Ready) {
                let path = self.path.clone();
                tab.spawn_job(self.check_key(), move |loader| conflict::stage_ids(loader.repo(), &path));
                self.checking = true;
            }
        }
        if self.checking
            && let Some(result) = tab.take_result::<Result<[Option<Oid>; 3], String>>(&self.check_key())
        {
            self.checking = false;
            let changed = match (&result, &self.file) {
                (Ok(stages), Some(file)) => file.stage_ids() != *stages,
                _ => false,
            };
            // Same path, different conflict (the next commit of a rebase, say).
            if changed {
                if self.is_dirty() {
                    shared.toast(
                        ToastKind::Error,
                        "The conflict changed",
                        format!("{} conflicts with different content now, so the resolver started over.", self.path),
                    );
                }
                self.load(tab);
            }
        }
    }

    /// Asks to close, now or after the user has dealt with unsaved work.
    fn request_close(&mut self, then: Option<Then>) {
        if then.is_some() {
            self.then = then;
        }
        if let Some(saving) = &mut self.saving {
            saving.then_close = true;
        } else if self.is_dirty() {
            self.editing = None;
            self.replan = true;
            self.asking = true;
        } else {
            self.closing = true;
        }
    }

    /// Escape: back out of the innermost thing first. Returns true when the
    /// view may be closed.
    fn escape(&mut self) -> bool {
        if self.asking {
            self.asking = false;
            self.then = None;
        } else if self.editing.take().is_some() {
            self.replan = true;
        } else {
            self.request_close(None);
        }
        self.closing
    }

    fn leave(&mut self, tab: &mut RepoTab) {
        match self.then.take() {
            Some(Then::Diff(target)) => tab.open_diff(target),
            Some(Then::Conflict(path)) => tab.overlay = Some(Overlay::Conflict(ConflictView::new(&path))),
            None => {}
        }
    }

    fn save(
        &mut self,
        tab: &mut RepoTab,
        shared: &mut Shared,
        file: &ConflictFile,
        mark_resolved: bool,
        then_close: bool,
    ) {
        // In the file's encoding, with the line endings Preferences > Editor asks for.
        let Some(bytes) = file.output(&self.draft, shared.settings.editor.eol) else { return };
        let name = file_name(&file.path);
        let done = if mark_resolved {
            (format!("Resolved {name}"), "The result is written and staged.".to_owned())
        } else {
            let left = match self.draft.open_conflicts() {
                0 => "Every conflict in it is resolved, but the file".to_owned(),
                1 => "1 conflict is still unresolved, so the file".to_owned(),
                n => format!("{n} conflicts are still unresolved, so the file"),
            };
            (format!("Saved {name}"), format!("{left} stays marked as conflicted."))
        };
        match conflict::save(file, bytes, mark_resolved) {
            Ok(cmd) => self.run(tab, cmd, then_close, done),
            Err(message) => shared.toast(ToastKind::Error, "Could not save the file", message),
        }
    }

    fn run(&mut self, tab: &mut RepoTab, cmd: crate::git::ops::GitCmd, then_close: bool, done: (String, String)) {
        self.editing = None;
        self.replan = true;
        self.saving = Some(Saving {
            label: cmd.label.clone(),
            started: Instant::now(),
            draft: self.draft.clone(),
            then_close,
            done,
        });
        tab.run(cmd);
    }
}

/// Removes the parked resolvers of `repo` and returns the one that is still
/// worth showing: `conflicted` says whether a path is still in conflict.
fn take_parked(repo: &Path, conflicted: impl Fn(&str) -> bool) -> Option<Resolver> {
    let mut list = PARKED.lock().ok()?;
    let mut keep = None;
    let mut i = 0;
    while i < list.len() {
        if list[i].repo != repo {
            i += 1;
            continue;
        }
        // Work on a file that is no longer in conflict has nowhere to go.
        let parked = list.swap_remove(i);
        if keep.is_none() && conflicted(&parked.path) {
            keep = Some(parked);
        }
    }
    keep
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn is_conflicted(tab: &RepoTab, path: &str) -> bool {
    tab.snap.as_ref().is_some_and(|snap| {
        snap.status.unstaged.iter().any(|f| f.kind == ChangeKind::Conflicted && f.path == path)
    })
}

/// Called for Escape while an overlay is open. Returns whether the overlay
/// may close; a resolver with unsaved choices closes its editor or asks
/// first instead.
pub fn escape(tab: &mut RepoTab) -> bool {
    match &mut tab.overlay {
        Some(Overlay::Conflict(view)) => view.resolver.escape(),
        _ => true,
    }
}

/// Returns false when the overlay should close.
pub fn show(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, view: &mut ConflictView) -> bool {
    let ConflictView { path, resolver: view } = view;
    view.start(tab);
    // Parked work may have taken the place of the file that was asked for.
    path.clone_from(&view.path);
    view.poll(tab, shared);
    ui.spacing_mut().item_spacing.y = 0.0;

    let (close, free) = widgets::overlay_bar(ui, icons::WARNING, "Resolve conflict", &view.path);
    if close {
        view.request_close(None);
    }
    // Another file was opened from the file lists: that means leaving this one.
    if let Some(diff) = tab.diff.take() {
        view.request_close(Some(Then::Diff(diff.target)));
    }

    let failure = match &view.phase {
        Phase::Failed(message) => Some(message.clone()),
        _ => None,
    };
    if let Some(message) = failure {
        widgets::placeholder(ui, &message);
    } else if let Some(file) = view.file.take() {
        match &file.content {
            Content::Text { merge, edited_outside, .. } => {
                text_ui(ui, tab, shared, view, &file, merge, *edited_outside, free);
            }
            Content::Whole(why) => whole_ui(ui, tab, view, &file, *why),
        }
        if view.asking {
            ask(ui.ctx(), tab, shared, view, &file);
        }
        view.file = Some(file);
    } else {
        widgets::placeholder(ui, "Reading both sides of the conflict…");
    }

    if view.closing {
        view.leave(tab);
        return false;
    }
    true
}

// ---- rows ------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Kind {
    /// Lines `from..to` of a stable segment, the same in all three panes.
    Stable { seg: usize, from: usize, to: usize },
    /// A line outside the conflicts that was rewritten by hand (or is being).
    Edited { seg: usize, line: usize },
    Conflict { index: usize },
}

struct Block {
    kind: Kind,
    top: f32,
    bottom: f32,
    /// Rows of text; a conflict has its header strip above them.
    rows: usize,
    /// Lines above this block in the current side, the incoming side and
    /// the result, for the line numbers.
    above: [usize; 3],
}

/// Where every block of rows sits, top to bottom.
#[derive(Default)]
struct Plan {
    blocks: Vec<Block>,
    height: f32,
    /// Longest line in bytes, for the horizontal scroll range.
    longest: usize,
    /// Lines in the longest of the three panes, for the width of the numbers.
    lines: usize,
}

/// Rows a piece of typed text occupies: none when empty.
fn text_rows(text: &str) -> usize {
    if text.is_empty() { 0 } else { text.split('\n').count() }
}

/// Lines a resolution puts into the result.
fn resolved_rows(choice: &Resolution, ours: &Chunk, theirs: &Chunk) -> usize {
    match choice {
        Resolution::Unresolved | Resolution::Neither => 0,
        Resolution::Ours => ours.len(),
        Resolution::Theirs => theirs.len(),
        Resolution::OursTheirs | Resolution::TheirsOurs => ours.len() + theirs.len(),
        Resolution::Custom(text) => text_rows(text),
    }
}

/// Rows `rows` of what a resolution puts into the result, each with the
/// side it comes from (`None` for typed text).
fn resolved_lines<'a>(
    choice: &'a Resolution,
    ours: &'a Chunk,
    theirs: &'a Chunk,
    rows: Range<usize>,
) -> Vec<(Cow<'a, str>, Option<Side>)> {
    let two = |first: (&'a Chunk, Side), second: (&'a Chunk, Side)| {
        rows.clone()
            .map(|row| {
                if row < first.0.len() {
                    (first.0.line(row), Some(first.1))
                } else {
                    (second.0.line(row - first.0.len()), Some(second.1))
                }
            })
            .collect()
    };
    let (mine, yours) = ((ours, Side::Ours), (theirs, Side::Theirs));
    match choice {
        Resolution::Unresolved | Resolution::Neither => Vec::new(),
        Resolution::Ours => rows.clone().map(|row| (ours.line(row), Some(Side::Ours))).collect(),
        Resolution::Theirs => rows.clone().map(|row| (theirs.line(row), Some(Side::Theirs))).collect(),
        Resolution::OursTheirs => two(mine, yours),
        Resolution::TheirsOurs => two(yours, mine),
        Resolution::Custom(text) => text
            .split('\n')
            .skip(rows.start)
            .take(rows.len())
            .map(|line| (Cow::Borrowed(line), None))
            .collect(),
    }
}

/// The text the editor starts with for a conflict.
fn editor_text(choice: &Resolution, ours: &Chunk, theirs: &Chunk) -> String {
    let both = |first: &Chunk, second: &Chunk| {
        [first.text(), second.text()]
            .into_iter()
            .zip([first.is_empty(), second.is_empty()])
            .filter(|(_, empty)| !empty)
            .map(|(text, _)| text)
            .collect::<Vec<_>>()
            .join("\n")
    };
    match choice {
        // Start from everything both sides wrote; trimming is easier than retyping.
        Resolution::Unresolved | Resolution::OursTheirs => both(ours, theirs),
        Resolution::TheirsOurs => both(theirs, ours),
        Resolution::Ours => ours.text(),
        Resolution::Theirs => theirs.text(),
        Resolution::Neither => String::new(),
        Resolution::Custom(text) => text.clone(),
    }
}

impl Plan {
    fn build(merge: &TextMerge, draft: &Draft, editing: Option<&Editing>) -> Plan {
        let mut plan = Plan::default();
        let mut top = 0.0;
        let mut above = [0usize; 3];
        let mut conflict = 0;
        let mut push = |plan: &mut Plan, kind: Kind, rows: usize, head: f32, lines: [usize; 3]| {
            let bottom = top + head + rows as f32 * line_h();
            plan.blocks.push(Block { kind, top, bottom, rows, above });
            top = bottom;
            for (count, add) in above.iter_mut().zip(lines) {
                *count += add;
            }
        };

        for (seg, segment) in merge.segments.iter().enumerate() {
            match segment {
                Segment::Stable(chunk) => {
                    plan.longest = plan.longest.max(chunk.longest_line());
                    // Lines with an edit, plus the one the editor is open on.
                    let mut own: Vec<usize> = draft
                        .edits
                        .range((seg, 0)..(seg + 1, 0))
                        .map(|(key, _)| key.1)
                        .filter(|line| *line < chunk.len())
                        .collect();
                    if let Some(Editing { target: Target::Line { seg: s, line }, .. }) = editing
                        && *s == seg
                        && *line < chunk.len()
                        && !own.contains(line)
                    {
                        own.push(*line);
                        own.sort_unstable();
                    }
                    let mut from = 0;
                    for line in own {
                        if line > from {
                            let n = line - from;
                            push(&mut plan, Kind::Stable { seg, from, to: line }, n, 0.0, [n, n, n]);
                        }
                        let typed = match editing {
                            Some(e) if e.target == (Target::Line { seg, line }) => Some(&e.text),
                            _ => draft.edits.get(&(seg, line)),
                        };
                        let typed_rows = typed.map_or(1, |text| text_rows(text));
                        plan.longest = plan.longest.max(typed.map_or(0, |text| text.len()));
                        push(&mut plan, Kind::Edited { seg, line }, typed_rows.max(1), 0.0, [1, 1, typed_rows]);
                        from = line + 1;
                    }
                    if from < chunk.len() {
                        let n = chunk.len() - from;
                        push(&mut plan, Kind::Stable { seg, from, to: chunk.len() }, n, 0.0, [n, n, n]);
                    }
                }
                Segment::Conflict { ours, theirs } => {
                    plan.longest = plan.longest.max(ours.longest_line()).max(theirs.longest_line());
                    let choice = draft.choices.get(conflict).unwrap_or(&Resolution::Unresolved);
                    let result = match editing {
                        Some(e) if e.target == Target::Conflict(conflict) => text_rows(&e.text).max(1),
                        _ => resolved_rows(choice, ours, theirs),
                    };
                    if let Resolution::Custom(text) = choice {
                        plan.longest = plan.longest.max(text.len());
                    }
                    let rows = ours.len().max(theirs.len()).max(result).max(1);
                    let lines = [ours.len(), theirs.len(), result];
                    push(&mut plan, Kind::Conflict { index: conflict }, rows, HEAD_H, lines);
                    conflict += 1;
                }
            }
        }
        plan.height = top;
        plan.lines = above.into_iter().max().unwrap_or(0);
        plan
    }

    /// The blocks that intersect `height` pixels starting at `offset`.
    fn visible(&self, offset: f32, height: f32) -> &[Block] {
        let first = self.blocks.partition_point(|block| block.bottom <= offset);
        let last = self.blocks.partition_point(|block| block.top < offset + height);
        &self.blocks[first.min(last)..last]
    }

    fn at(&self, y: f32) -> Option<&Block> {
        self.blocks.get(self.blocks.partition_point(|block| block.bottom <= y)).filter(|block| block.top <= y)
    }

    fn conflict(&self, index: usize) -> Option<&Block> {
        self.blocks.iter().find(|block| matches!(block.kind, Kind::Conflict { index: i } if i == index))
    }
}

/// Of `count` rows starting at `top`, the ones inside `clip`.
fn visible_rows(top: f32, count: usize, clip: Rect) -> Range<usize> {
    let first = ((clip.top() - top) / line_h()).floor().max(0.0) as usize;
    let last = (((clip.bottom() - top) / line_h()).ceil().max(0.0) as usize).min(count);
    first.min(last)..last
}

// ---- actions ---------------------------------------------------------------

enum Action {
    /// A side's checkbox was clicked.
    Toggle(usize, Side),
    Set(usize, Resolution),
    /// Open the editor, with the cursor at this row and column.
    Edit(Target, (usize, usize)),
    /// The editor's text changed.
    Typed,
    StopEditing,
    /// Drop the hand edit of a line outside the conflicts.
    Restore(usize, usize),
    Focus(usize),
    TakeAll(Side),
    Reset,
    Step(isize),
    Save { mark_resolved: bool },
}

/// What clicking a side's checkbox makes of a resolution. Picking the
/// second side adds it after the first, which is how "both" gets its order.
fn toggled(choice: &Resolution, side: Side) -> Resolution {
    use Resolution::*;
    match (choice, side) {
        (Ours, Side::Ours) | (Theirs, Side::Theirs) => Unresolved,
        (Ours, Side::Theirs) => OursTheirs,
        (Theirs, Side::Ours) => TheirsOurs,
        (OursTheirs | TheirsOurs, Side::Ours) => Theirs,
        (OursTheirs | TheirsOurs, Side::Theirs) => Ours,
        (_, Side::Ours) => Ours,
        (_, Side::Theirs) => Theirs,
    }
}

/// How a resolution uses one side.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Used {
    No,
    Alone,
    First,
    Second,
}

fn used(choice: &Resolution, side: Side) -> Used {
    use Resolution::*;
    match (choice, side) {
        (Ours, Side::Ours) | (Theirs, Side::Theirs) => Used::Alone,
        (OursTheirs, Side::Ours) | (TheirsOurs, Side::Theirs) => Used::First,
        (OursTheirs, Side::Theirs) | (TheirsOurs, Side::Ours) => Used::Second,
        _ => Used::No,
    }
}

impl Resolver {
    fn apply(
        &mut self,
        action: Action,
        tab: &mut RepoTab,
        shared: &mut Shared,
        file: &ConflictFile,
        merge: &TextMerge,
    ) {
        self.replan = true;
        let count = self.draft.choices.len();
        match action {
            Action::Toggle(index, side) => {
                if let Some(choice) = self.draft.choices.get_mut(index) {
                    *choice = toggled(choice, side);
                    self.current = index;
                    self.editing = None;
                }
            }
            Action::Set(index, resolution) => {
                if let Some(choice) = self.draft.choices.get_mut(index) {
                    *choice = resolution;
                    self.current = index;
                    self.editing = None;
                }
            }
            Action::Edit(target, place) => {
                if self.editing.as_ref().is_some_and(|e| e.target == target) {
                    return;
                }
                let text = match target {
                    Target::Conflict(index) => {
                        let (Some(choice), Some((ours, theirs))) =
                            (self.draft.choices.get(index), merge.conflict(index))
                        else {
                            return;
                        };
                        self.current = index;
                        editor_text(choice, ours, theirs)
                    }
                    Target::Line { seg, line } => {
                        let Some(Segment::Stable(chunk)) = merge.segments.get(seg) else { return };
                        match self.draft.edits.get(&(seg, line)) {
                            Some(text) => text.clone(),
                            None => chunk.line(line).into_owned(),
                        }
                    }
                };
                self.editing = Some(Editing { target, text, place: Some(place), shown: false, cursor: None });
            }
            Action::Typed => {
                let Some(editing) = &self.editing else { return };
                match editing.target {
                    Target::Conflict(index) => {
                        if let Some(choice) = self.draft.choices.get_mut(index) {
                            *choice = Resolution::Custom(editing.text.clone());
                        }
                    }
                    Target::Line { seg, line } => {
                        let Some(Segment::Stable(chunk)) = merge.segments.get(seg) else { return };
                        // Typing the line back to what it was is no edit.
                        if chunk.line(line) == editing.text.as_str() {
                            self.draft.edits.remove(&(seg, line));
                        } else {
                            self.draft.edits.insert((seg, line), editing.text.clone());
                        }
                    }
                }
            }
            Action::StopEditing => self.editing = None,
            Action::Restore(seg, line) => {
                self.draft.edits.remove(&(seg, line));
                self.editing = None;
            }
            Action::Focus(index) => self.current = index,
            Action::TakeAll(side) => {
                let resolution = match side {
                    Side::Ours => Resolution::Ours,
                    Side::Theirs => Resolution::Theirs,
                };
                self.draft.choices.fill(resolution);
                self.editing = None;
            }
            Action::Reset => {
                self.draft = Draft::unresolved(merge);
                self.editing = None;
            }
            Action::Step(by) => {
                if count > 0 {
                    self.current = (self.current as isize + by).rem_euclid(count as isize) as usize;
                    self.jump = Some(self.current);
                }
            }
            Action::Save { mark_resolved } => self.save(tab, shared, file, mark_resolved, false),
        }
    }
}

// ---- the text resolver -----------------------------------------------------

/// What the panes need to paint a frame.
struct Scene<'a> {
    merge: &'a TextMerge,
    draft: &'a Draft,
    plan: &'a Plan,
    scroll: Vec2,
    current: usize,
    font: FontId,
    char_w: f32,
    /// Width of the line-number column.
    gutter: f32,
}

#[allow(clippy::too_many_arguments)]
fn text_ui(
    ui: &mut Ui,
    tab: &mut RepoTab,
    shared: &mut Shared,
    view: &mut Resolver,
    file: &ConflictFile,
    merge: &TextMerge,
    edited_outside: bool,
    free: Rect,
) {
    if view.replan || view.plan_line_h != line_h() {
        view.plan = Plan::build(merge, &view.draft, view.editing.as_ref());
        view.replan = false;
        view.plan_line_h = line_h();
    }
    if let Some(block) = view.jump.take().and_then(|index| view.plan.conflict(index)) {
        view.scroll.y = block.top - jump_margin();
    }

    let mut actions = Vec::new();
    let saving = view.saving.is_some();
    let open = view.draft.open_conflicts();
    let total = view.draft.choices.len();
    title_buttons(ui, free, open, view.is_dirty(), saving, &mut actions);
    toolbar(ui, open, total, saving, &mut actions);
    if edited_outside {
        notice(
            ui,
            "This file was already changed by hand and has no conflict markers left. \
             Saving from here replaces that version.",
        );
    }

    let body = ui.available_rect_before_wrap();
    ui.allocate_rect(body, Sense::hover());

    // Geometry: two sides on top, the result below, one scrollbar for all.
    let content = Rect::from_min_max(body.min, pos2(body.right() - SCROLLBAR, body.bottom()));
    let sides_h = ((content.height() - PANE_HEAD_H * 2.0) * SIDES_SHARE).floor().max(line_h());
    let middle = content.center().x.round();
    let heads = Rect::from_min_max(content.min, pos2(content.right(), content.top() + PANE_HEAD_H));
    let sides = Rect::from_min_max(heads.left_bottom(), pos2(content.right(), heads.bottom() + sides_h));
    let result_head = Rect::from_min_max(sides.left_bottom(), pos2(content.right(), sides.bottom() + PANE_HEAD_H));
    let below = Rect::from_min_max(result_head.left_bottom(), content.max);
    let ours_rect = Rect::from_min_max(sides.min, pos2(middle, sides.bottom()));
    let theirs_rect = Rect::from_min_max(pos2(middle + 1.0, sides.top()), sides.max);

    // Preferences > Editor: the font, its size, line numbers or not.
    let look = editor::look();
    let font = look.font();
    let (char_w, digit_w) =
        ui.painter().fonts_mut(|f| (f.glyph_width(&font, 'm'), f.glyph_width(&number_font(), '0')));
    // Room for the largest line number.
    let digits = view.plan.lines.max(1).ilog10() + 1;
    let gutter = if look.numbers { (digits as f32 * digit_w + 22.0).max(MIN_GUTTER) } else { NO_NUMBERS_GUTTER };
    let text_w = view.plan.longest.min(MAX_SHOWN) as f32 * char_w + 40.0;
    let max_x = (text_w - (ours_rect.width() - gutter - TEXT_PAD)).max(0.0);
    let result_rect = if max_x > 0.0 {
        Rect::from_min_max(below.min, pos2(below.right(), below.bottom() - SCROLLBAR))
    } else {
        below
    };
    let view_h = sides.height().min(result_rect.height());
    // A little slack so the last rows clear the bottom edge, and enough for
    // "next conflict" to bring even the last one to its usual place.
    let to_end = if view.plan.height > view_h { view.plan.height - view_h + line_h() * 2.0 } else { 0.0 };
    let last_conflict = view.plan.blocks.iter().rev().find(|block| matches!(block.kind, Kind::Conflict { .. }));
    let max_y = to_end.max(last_conflict.map_or(0.0, |block| block.top - jump_margin()));

    // Keyboard and wheel. Keys are left alone while a dialog is open or
    // another text field (the commit message, say) is being typed in.
    let typing_elsewhere = view.editing.is_none() && ui.memory(|memory| memory.focused().is_some());
    if !view.asking && shared.dialog.is_none() {
        let key = |modifiers, key| ui.input_mut(|i| i.consume_key(modifiers, key));
        if !typing_elsewhere {
            if key(Modifiers::ALT, Key::ArrowDown) {
                actions.push(Action::Step(1));
            }
            if key(Modifiers::ALT, Key::ArrowUp) {
                actions.push(Action::Step(-1));
            }
        }
        if !typing_elsewhere && view.editing.is_none() {
            let page = view_h - line_h() * 2.0;
            if key(Modifiers::NONE, Key::PageDown) {
                view.scroll.y += page;
            }
            if key(Modifiers::NONE, Key::PageUp) {
                view.scroll.y -= page;
            }
        }
        if ui.rect_contains_pointer(content) {
            view.scroll -= ui.input(|i| i.smooth_scroll_delta);
        }
    }
    view.scroll.y = view.scroll.y.clamp(0.0, max_y);
    view.scroll.x = view.scroll.x.clamp(0.0, max_x);

    // Pane headers.
    let (ours_head, theirs_head) = heads.split_left_right_at_x(middle);
    pane_header(ui, ours_head, &file.ours_info, side_color(Side::Ours));
    pane_header(ui, theirs_head, &file.theirs_info, side_color(Side::Theirs));
    let status = if saving {
        "saving…"
    } else if view.is_dirty() {
        "unsaved choices"
    } else {
        ""
    };
    let result_info = SideInfo {
        role: "Result",
        name: file_name(&file.path).to_owned(),
        detail: "click a line to edit it by hand".to_owned(),
        marker: String::new(),
    };
    pane_header(ui, result_head, &result_info, typed());
    if !status.is_empty() {
        ui.painter().text(
            pos2(result_head.right() - 10.0, result_head.center().y),
            Align2::RIGHT_CENTER,
            status,
            theme::font(12.0),
            theme::yellow(),
        );
    }

    let scene = Scene {
        merge,
        draft: &view.draft,
        plan: &view.plan,
        scroll: view.scroll,
        current: view.current,
        font,
        char_w,
        gutter,
    };
    side_pane(ui, &scene, ours_rect, Side::Ours, &mut actions);
    side_pane(ui, &scene, theirs_rect, Side::Theirs, &mut actions);
    let cursor = result_pane(ui, &scene, result_rect, text_w, &mut view.editing, &mut actions);

    let painter = ui.painter();
    painter.vline(middle + 0.5, sides.y_range(), Stroke::new(1.0, theme::border()));

    // Scrollbars, with a tick for every conflict.
    let marks: Vec<(f32, Color32)> = view
        .plan
        .blocks
        .iter()
        .filter_map(|block| match block.kind {
            Kind::Conflict { index } => {
                let open = view.draft.choices.get(index) == Some(&Resolution::Unresolved);
                Some((block.top, if open { theme::orange() } else { theme::green() }))
            }
            _ => None,
        })
        .collect();
    let bar = Rect::from_min_max(pos2(content.right(), sides.top()), pos2(body.right(), result_rect.bottom()));
    scrollbar(ui, bar, "conflict-vscroll", &mut view.scroll.y, max_y, view_h, true, &marks);
    if max_x > 0.0 {
        let bar = Rect::from_min_max(pos2(below.left() + gutter, result_rect.bottom()), below.max);
        let width = ours_rect.width() - gutter - TEXT_PAD;
        scrollbar(ui, bar, "conflict-hscroll", &mut view.scroll.x, max_x, width, false, &[]);
    }

    // Keep the text cursor in view while typing.
    if let Some(cursor) = cursor {
        if cursor.top() < result_rect.top() {
            view.scroll.y -= result_rect.top() - cursor.top();
        } else if cursor.bottom() > result_rect.bottom() {
            view.scroll.y += cursor.bottom() - result_rect.bottom();
        }
        let left = result_rect.left() + gutter + TEXT_PAD;
        if cursor.left() < left {
            view.scroll.x -= left - cursor.left();
        } else if cursor.right() > result_rect.right() - 12.0 {
            view.scroll.x += cursor.right() - (result_rect.right() - 12.0);
        }
        ui.ctx().request_repaint();
    }

    if !actions.is_empty() {
        ui.ctx().request_repaint();
    }
    for action in actions {
        view.apply(action, tab, shared, file, merge);
    }
}

/// Save buttons, in the free part of the title bar.
fn title_buttons(ui: &mut Ui, free: Rect, open: usize, dirty: bool, saving: bool, actions: &mut Vec<Action>) {
    const RESOLVE: &str = "Save and mark resolved";
    const SAVE: &str = "Save";
    // The path may run under the buttons when the view is narrow.
    let width = tint_button_size(ui, RESOLVE).x + tint_button_size(ui, SAVE).x + 22.0;
    let backing = Rect::from_min_max(
        pos2((free.right() - width).max(free.left()), free.top() + 1.0),
        pos2(free.right(), free.bottom() - 1.0),
    );
    ui.painter().rect_filled(backing, 0.0, theme::bg_header());

    ui.scope_builder(UiBuilder::new().max_rect(free).layout(Layout::right_to_left(Align::Center)), |ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        if saving {
            ui.label(RichText::new("Saving…").color(theme::text_dim()));
            ui.add(egui::Spinner::new().size(13.0).color(theme::accent()));
            return;
        }
        let resolve = ui.add_enabled_ui(open == 0, |ui| tint_button(ui, RESOLVE, theme::green())).inner;
        let hint = match open {
            0 => "Write the result to the file and stage it, which marks the conflict as resolved".to_owned(),
            1 => "1 conflict is still unresolved".to_owned(),
            n => format!("{n} conflicts are still unresolved"),
        };
        if resolve.on_hover_text(&hint).on_disabled_hover_text(&hint).clicked() {
            actions.push(Action::Save { mark_resolved: true });
        }
        let save = ui.add_enabled_ui(dirty, |ui| tint_button(ui, SAVE, theme::accent())).inner;
        let hint = "Write the result to the file but leave it marked as conflicted. \
                    Unresolved regions keep their conflict markers.";
        if save.on_hover_text(hint).on_disabled_hover_text("Nothing new to save").clicked() {
            actions.push(Action::Save { mark_resolved: false });
        }
    });
}

/// Progress, navigation and the bulk choices.
fn toolbar(ui: &mut Ui, open: usize, total: usize, saving: bool, actions: &mut Vec<Action>) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), TOOLBAR_H), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, theme::bg_panel());
    ui.painter().hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::border()));

    let inner = rect.shrink2(vec2(10.0, 0.0));
    ui.scope_builder(UiBuilder::new().max_rect(inner).layout(Layout::left_to_right(Align::Center)), |ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let (icon, color, text) = if total == 0 {
            (icons::CHECK_CIRCLE, theme::green(), "No conflicting lines: the two sides merge cleanly".to_owned())
        } else if open == 0 {
            let all = match total {
                1 => "The conflict is resolved".to_owned(),
                n => format!("All {n} conflicts resolved"),
            };
            (icons::CHECK_CIRCLE, theme::green(), all)
        } else {
            (icons::WARNING, theme::orange(), format!("{} of {total} conflicts resolved", total - open))
        };
        ui.label(RichText::new(icon).size(15.0).color(color));
        ui.label(RichText::new(text).font(theme::bold(13.0)).color(theme::text_bright()));
        if total > 0 {
            ui.add_space(6.0);
            if icon_button(ui, icons::ARROW_UP, 14.0, "Previous conflict (Alt+Up)").clicked() {
                actions.push(Action::Step(-1));
            }
            if icon_button(ui, icons::ARROW_DOWN, 14.0, "Next conflict (Alt+Down)").clicked() {
                actions.push(Action::Step(1));
            }
        }

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if total == 0 {
                return;
            }
            ui.add_enabled_ui(!saving, |ui| {
                if tint_button(ui, "Reset", theme::red())
                    .on_hover_text("Forget every choice and edit, back to all conflicts unresolved")
                    .clicked()
                {
                    actions.push(Action::Reset);
                }
                if tint_button(ui, "Take all incoming", side_color(Side::Theirs)).clicked() {
                    actions.push(Action::TakeAll(Side::Theirs));
                }
                if tint_button(ui, "Take all current", side_color(Side::Ours)).clicked() {
                    actions.push(Action::TakeAll(Side::Ours));
                }
            });
        });
    });
}

fn notice(ui: &mut Ui, text: &str) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), NOTICE_H), Sense::hover());
    let painter = ui.painter().with_clip_rect(rect);
    painter.rect_filled(rect, 0.0, theme::mix(theme::bg_app(), theme::yellow(), 0.14));
    let x = rect.left() + 12.0;
    let w = widgets::text_left(&painter, rect, x, icons::INFO, theme::font(14.0), theme::yellow());
    widgets::text_left(&painter, rect, x + w + 8.0, text, theme::font(12.0), theme::text());
}

/// The strip above a pane saying what it shows: `CURRENT  main  1a2b3c4 Summary`.
fn pane_header(ui: &Ui, rect: Rect, info: &SideInfo, color: Color32) {
    let painter = ui.painter().with_clip_rect(rect);
    painter.rect_filled(rect, 0.0, theme::bg_header());
    painter.hline(rect.x_range(), rect.top() + 0.5, Stroke::new(1.0, theme::border()));
    painter.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::border()));

    let tag = painter.layout_no_wrap(info.role.to_uppercase(), theme::bold(10.5), theme::text_bright());
    let chip = Rect::from_min_size(
        pos2(rect.left() + 10.0, rect.center().y - 9.0),
        vec2(tag.size().x + 12.0, 18.0),
    );
    painter.rect_filled(chip, 3.0, theme::mix(theme::bg_app(), color, 0.45));
    widgets::galley_left(&painter, chip, chip.left() + 6.0, tag);

    let x = chip.right() + 8.0;
    let galley = widgets::elided_pair(
        &painter,
        (&info.name, theme::bold(13.0), theme::text_bright()),
        (&info.detail, theme::font(12.0), theme::text_dim()),
        rect.right() - x - 8.0,
    );
    widgets::galley_left(&painter, rect, x, galley);
}

fn row_job(text: &str, font: &FontId, color: Color32) -> LayoutJob {
    let format = TextFormat { font_id: font.clone(), color, line_height: Some(line_h()), ..Default::default() };
    let mut job = LayoutJob::single_section(text.to_owned(), format);
    job.wrap.max_width = f32::INFINITY;
    job
}

/// One line of file text in a row whose top is `top`. Laid out exactly as
/// the editor lays its text out, so opening the editor moves nothing.
fn draw_line(painter: &Painter, x: f32, top: f32, text: &str, font: &FontId, color: Color32) {
    let shown = match text.char_indices().nth(MAX_SHOWN) {
        Some((end, _)) => &text[..end],
        None => text,
    };
    let galley = painter.layout_job(row_job(shown, font, color));
    painter.galley(pos2(x, top), galley, color);
}

/// The font of line numbers: the code font, a little smaller.
fn number_font() -> FontId {
    theme::code(editor::look().size - 1.5)
}

fn draw_number(painter: &Painter, gutter: f32, pane: Rect, top: f32, number: usize) {
    if !editor::look().numbers {
        return;
    }
    painter.text(
        pos2(pane.left() + gutter - 10.0, top + line_h() / 2.0),
        Align2::RIGHT_CENTER,
        number.to_string(),
        number_font(),
        theme::text_faint(),
    );
}

/// A remark in place of text: "no lines on this side".
fn draw_remark(painter: &Painter, gutter: f32, pane: Rect, top: f32, text: &str, color: Color32) {
    painter.text(
        pos2(pane.left() + gutter + TEXT_PAD, top + line_h() / 2.0),
        Align2::LEFT_CENTER,
        text,
        theme::font(12.0),
        color,
    );
}

/// The part of a row right of the line numbers.
fn row_rect(gutter: f32, pane: Rect, top: f32) -> Rect {
    Rect::from_min_max(pos2(pane.left() + gutter, top), pos2(pane.right(), top + line_h()))
}

/// A child `Ui` clipped to a pane, so neither paint nor clicks leak out of it.
fn pane_ui(ui: &mut Ui, rect: Rect, salt: &str) -> Ui {
    let mut child = ui.new_child(UiBuilder::new().id_salt(salt).max_rect(rect));
    child.set_clip_rect(rect.intersect(ui.clip_rect()));
    child
}

/// Background, gutter and the painter for file text (clipped to the right of
/// the line numbers).
fn pane_backdrop(ui: &Ui, rect: Rect, gutter: f32) -> (Painter, Painter) {
    let painter = ui.painter().clone();
    painter.rect_filled(rect, 0.0, theme::bg_app());
    let numbers = Rect::from_min_max(rect.min, pos2(rect.left() + gutter, rect.bottom()));
    painter.rect_filled(numbers, 0.0, theme::bg_panel());
    let text_clip = Rect::from_min_max(pos2(rect.left() + gutter, rect.top()), rect.max);
    let text_painter = painter.with_clip_rect(text_clip.intersect(painter.clip_rect()));
    (painter, text_painter)
}

fn outline_current(painter: &Painter, pane: Rect, block: &Block, top: f32) {
    let rect = Rect::from_min_max(pos2(pane.left(), top), pos2(pane.right(), top + block.bottom - block.top));
    painter.rect(
        rect,
        0.0,
        Color32::TRANSPARENT,
        Stroke::new(1.0, theme::with_alpha(theme::text_bright(), 70)),
        StrokeKind::Inside,
    );
}

/// One side of the conflict: the merged text, with this side's version of
/// every conflict and a checkbox to take it.
fn side_pane(ui: &mut Ui, scene: &Scene<'_>, rect: Rect, side: Side, actions: &mut Vec<Action>) {
    let (pane, salt, take) = match side {
        Side::Ours => (0, "conflict-ours", "Take current"),
        Side::Theirs => (1, "conflict-theirs", "Take incoming"),
    };
    let color = side_color(side);
    let ui = &mut pane_ui(ui, rect, salt);
    let background = ui.interact(rect, ui.id().with("background"), Sense::click());
    let (painter, text_painter) = pane_backdrop(ui, rect, scene.gutter);
    let text_x = rect.left() + scene.gutter + TEXT_PAD - scene.scroll.x;
    let clicked = background.clicked().then(|| background.interact_pointer_pos()).flatten();

    for block in scene.plan.visible(scene.scroll.y, rect.height()) {
        let top = rect.top() + block.top - scene.scroll.y;
        match block.kind {
            Kind::Stable { seg, from, to } => {
                let Segment::Stable(chunk) = &scene.merge.segments[seg] else { continue };
                for row in visible_rows(top, to - from, rect) {
                    let y = top + row as f32 * line_h();
                    draw_number(&painter, scene.gutter, rect, y, block.above[pane] + row + 1);
                    draw_line(&text_painter, text_x, y, &chunk.line(from + row), &scene.font, theme::text());
                }
            }
            Kind::Edited { seg, line } => {
                // The sides keep showing the line as it was merged.
                let Segment::Stable(chunk) = &scene.merge.segments[seg] else { continue };
                draw_number(&painter, scene.gutter, rect, top, block.above[pane] + 1);
                draw_line(&text_painter, text_x, top, &chunk.line(line), &scene.font, theme::text());
                for row in 1..block.rows {
                    let filler = row_rect(scene.gutter, rect, top + row as f32 * line_h());
                    painter.rect_filled(filler, 0.0, theme::bg_void());
                }
            }
            Kind::Conflict { index } => {
                let Some((ours, theirs)) = scene.merge.conflict(index) else { continue };
                let chunk = match side {
                    Side::Ours => ours,
                    Side::Theirs => theirs,
                };
                let choice = scene.draft.choices.get(index).unwrap_or(&Resolution::Unresolved);
                let used = used(choice, side);
                let open = *choice == Resolution::Unresolved;

                // Header strip: the checkbox that takes this side.
                let head = Rect::from_min_max(pos2(rect.left(), top), pos2(rect.right(), top + HEAD_H));
                let hit = ui
                    .interact(head, ui.id().with(("take", index)), Sense::click())
                    .on_hover_cursor(CursorIcon::PointingHand);
                let strength = match (used != Used::No, hit.hovered()) {
                    (true, _) => 0.36,
                    (false, true) => 0.26,
                    (false, false) => 0.17,
                };
                painter.rect_filled(head, 0.0, theme::mix(theme::bg_app(), color, strength));
                let cy = head.center().y;
                let (icon, icon_color) = match used {
                    Used::No => (icons::SQUARE, theme::text_dim()),
                    _ => (icons::CHECK_SQUARE, theme::text_bright()),
                };
                painter.text(pos2(head.left() + 12.0, cy), Align2::LEFT_CENTER, icon, theme::font(16.0), icon_color);
                let label = match used {
                    Used::First => format!("{take}  ·  placed first"),
                    Used::Second => format!("{take}  ·  placed second"),
                    _ => take.to_owned(),
                };
                let label_color = if used == Used::No { theme::text() } else { theme::text_bright() };
                let at = pos2(head.left() + 36.0, cy);
                painter.text(at, Align2::LEFT_CENTER, label, theme::font(12.5), label_color);
                let lines = match chunk.len() {
                    0 => "no lines".to_owned(),
                    1 => "1 line".to_owned(),
                    n => format!("{n} lines"),
                };
                let at = pos2(head.right() - 10.0, cy);
                painter.text(at, Align2::RIGHT_CENTER, lines, theme::font(11.5), theme::text_dim());
                if hit.clicked() {
                    actions.push(Action::Toggle(index, side));
                }

                // This side's lines.
                let body_top = top + HEAD_H;
                let (tint, text_color) = match (used != Used::No, open) {
                    (true, _) => (0.22, theme::text_bright()),
                    (false, true) => (0.11, theme::text()),
                    // Resolved without this side.
                    (false, false) => (0.04, theme::text_dim()),
                };
                for row in visible_rows(body_top, block.rows, rect) {
                    let y = body_top + row as f32 * line_h();
                    if row < chunk.len() {
                        let fill = theme::mix(theme::bg_app(), color, tint);
                        painter.rect_filled(row_rect(scene.gutter, rect, y), 0.0, fill);
                        draw_number(&painter, scene.gutter, rect, y, block.above[pane] + row + 1);
                        draw_line(&text_painter, text_x, y, &chunk.line(row), &scene.font, text_color);
                    } else {
                        painter.rect_filled(row_rect(scene.gutter, rect, y), 0.0, theme::bg_void());
                        if chunk.is_empty() && row == 0 {
                            draw_remark(&painter, scene.gutter, rect, y, "no lines on this side", theme::text_faint());
                        }
                    }
                }
                if used != Used::No && !chunk.is_empty() {
                    let bar = Rect::from_min_size(
                        pos2(rect.left() + scene.gutter, body_top),
                        vec2(3.0, chunk.len() as f32 * line_h()),
                    );
                    painter.rect_filled(bar, 0.0, color);
                }
                if index == scene.current {
                    outline_current(&painter, rect, block, top);
                }
                // Clicking the lines themselves takes the side too.
                if let Some(pos) = clicked
                    && pos.y >= body_top
                    && pos.y < top + block.bottom - block.top
                {
                    actions.push(Action::Toggle(index, side));
                }
            }
        }
    }
}

fn state_label(choice: &Resolution) -> (&'static str, Color32) {
    match choice {
        Resolution::Unresolved => ("unresolved", theme::orange()),
        Resolution::Ours => ("current taken", side_color(Side::Ours)),
        Resolution::Theirs => ("incoming taken", side_color(Side::Theirs)),
        Resolution::OursTheirs => ("both taken: current, then incoming", theme::text()),
        Resolution::TheirsOurs => ("both taken: incoming, then current", theme::text()),
        Resolution::Neither => ("both sides dropped", theme::text()),
        Resolution::Custom(_) => ("typed by hand", typed()),
    }
}

/// The result: what the file will contain. Returns where the text cursor is
/// when it moved this frame, so the caller can scroll it into view.
fn result_pane(
    ui: &mut Ui,
    scene: &Scene<'_>,
    rect: Rect,
    text_w: f32,
    editing: &mut Option<Editing>,
    actions: &mut Vec<Action>,
) -> Option<Rect> {
    let ui = &mut pane_ui(ui, rect, "conflict-result");
    let background = ui.interact(rect, ui.id().with("background"), Sense::click()).on_hover_cursor(CursorIcon::Text);
    let (painter, text_painter) = pane_backdrop(ui, rect, scene.gutter);
    let text_x = rect.left() + scene.gutter + TEXT_PAD - scene.scroll.x;
    let total = scene.draft.choices.len();
    let mut cursor = None;

    for block in scene.plan.visible(scene.scroll.y, rect.height()) {
        let top = rect.top() + block.top - scene.scroll.y;
        match block.kind {
            Kind::Stable { seg, from, to } => {
                let Segment::Stable(chunk) = &scene.merge.segments[seg] else { continue };
                for row in visible_rows(top, to - from, rect) {
                    let y = top + row as f32 * line_h();
                    draw_number(&painter, scene.gutter, rect, y, block.above[2] + row + 1);
                    draw_line(&text_painter, text_x, y, &chunk.line(from + row), &scene.font, theme::text());
                }
            }
            Kind::Edited { seg, line } => {
                let target = Target::Line { seg, line };
                let area = Rect::from_min_max(
                    pos2(rect.left() + scene.gutter, top),
                    pos2(rect.right(), top + block.rows as f32 * line_h()),
                );
                painter.rect_filled(area, 0.0, theme::mix(theme::bg_app(), typed(), 0.13));
                if let Some(open) = editing.as_mut().filter(|e| e.target == target) {
                    for row in visible_rows(top, block.rows, rect) {
                        let y = top + row as f32 * line_h();
                        draw_number(&painter, scene.gutter, rect, y, block.above[2] + row + 1);
                    }
                    cursor = editor(ui, scene, rect, area, text_x, text_w, open, actions);
                } else {
                    let typed = scene.draft.edits.get(&(seg, line)).map(String::as_str).unwrap_or_default();
                    if typed.is_empty() {
                        draw_remark(&painter, scene.gutter, rect, top, "line removed", theme::text_dim());
                    }
                    for (row, text) in typed.split('\n').enumerate().take(text_rows(typed)) {
                        let y = top + row as f32 * line_h();
                        draw_number(&painter, scene.gutter, rect, y, block.above[2] + row + 1);
                        draw_line(&text_painter, text_x, y, text, &scene.font, theme::text_bright());
                    }
                    let size = tint_button_size(ui, "Restore");
                    let corner = pos2(rect.right() - size.x - 8.0, top + (line_h() - size.y) / 2.0);
                    let button = Rect::from_min_size(corner, size);
                    if ui.rect_contains_pointer(area)
                        && tint_button_at(ui, button, "Restore", theme::red())
                            .on_hover_text("Put the merged line back")
                            .clicked()
                    {
                        actions.push(Action::Restore(seg, line));
                    }
                }
                painter.rect_filled(Rect::from_min_size(area.min, vec2(3.0, area.height())), 0.0, typed());
            }
            Kind::Conflict { index } => {
                let Some((ours, theirs)) = scene.merge.conflict(index) else { continue };
                let choice = scene.draft.choices.get(index).unwrap_or(&Resolution::Unresolved);
                let open = *choice == Resolution::Unresolved;
                let target = Target::Conflict(index);
                let is_editing = editing.as_ref().is_some_and(|e| e.target == target);

                // Header strip: state and the choices that are not a side.
                let head = Rect::from_min_max(pos2(rect.left(), top), pos2(rect.right(), top + HEAD_H));
                let accent = if open { theme::orange() } else { theme::green() };
                painter.rect_filled(head, 0.0, theme::mix(theme::bg_app(), accent, if open { 0.22 } else { 0.13 }));
                let cy = head.center().y;
                let icon = if open { icons::WARNING } else { icons::CHECK_CIRCLE };
                painter.text(pos2(head.left() + 12.0, cy), Align2::LEFT_CENTER, icon, theme::font(15.0), accent);
                let title = format!("Conflict {} of {total}", index + 1);
                let x = head.left() + 36.0;
                let w = widgets::text_left(&painter, head, x, &title, theme::bold(12.5), theme::text_bright());
                let (state, state_color) = state_label(choice);
                widgets::text_left(&painter, head, x + w + 10.0, state, theme::font(12.5), state_color);

                let mut right = head.right() - 8.0;
                let mut place = |ui: &mut Ui, label: &str, tint, hint: &str| {
                    let size = tint_button_size(ui, label);
                    let button = Rect::from_min_size(pos2(right - size.x, cy - size.y / 2.0), size);
                    right = button.left() - 6.0;
                    tint_button_at(ui, button, label, tint).on_hover_text(hint).clicked()
                };
                if !open && place(ui, "Reset", theme::red(), "Back to unresolved") {
                    actions.push(Action::Set(index, Resolution::Unresolved));
                }
                if is_editing {
                    if place(ui, "Done", typed(), "Finish typing (Esc)") {
                        actions.push(Action::StopEditing);
                    }
                } else if place(ui, "Edit", typed(), "Type the result for this conflict by hand") {
                    actions.push(Action::Edit(target, (usize::MAX, 0)));
                }
                // One more button, depending on where the conflict stands.
                let extra = match choice {
                    Resolution::OursTheirs => {
                        Some(("Swap order", "Put the incoming lines first", Resolution::TheirsOurs))
                    }
                    Resolution::TheirsOurs => {
                        Some(("Swap order", "Put the current lines first", Resolution::OursTheirs))
                    }
                    Resolution::Unresolved => {
                        Some(("Take neither", "Resolve by dropping the lines of both sides", Resolution::Neither))
                    }
                    _ => None,
                };
                if let Some((label, hint, becomes)) = extra
                    && place(ui, label, theme::accent(), hint)
                {
                    actions.push(Action::Set(index, becomes));
                }

                // The lines the resolution yields.
                let body_top = top + HEAD_H;
                let area = Rect::from_min_max(
                    pos2(rect.left() + scene.gutter, body_top),
                    pos2(rect.right(), body_top + block.rows as f32 * line_h()),
                );
                if let Some(open) = editing.as_mut().filter(|e| e.target == target) {
                    painter.rect_filled(area, 0.0, theme::mix(theme::bg_app(), typed(), 0.13));
                    for row in visible_rows(body_top, text_rows(&open.text).max(1), rect) {
                        let y = body_top + row as f32 * line_h();
                        draw_number(&painter, scene.gutter, rect, y, block.above[2] + row + 1);
                    }
                    cursor = editor(ui, scene, rect, area, text_x, text_w, open, actions);
                    painter.rect_filled(Rect::from_min_size(area.min, vec2(3.0, area.height())), 0.0, typed());
                } else if open {
                    painter.rect_filled(area, 0.0, theme::mix(theme::bg_app(), theme::orange(), 0.08));
                    draw_remark(
                        &painter,
                        scene.gutter,
                        rect,
                        body_top,
                        "Unresolved: take a side above, or click here to type the result",
                        theme::mix(theme::bg_app(), theme::orange(), 0.8),
                    );
                } else {
                    let count = resolved_rows(choice, ours, theirs);
                    let rows = visible_rows(body_top, block.rows, rect);
                    let shown = rows.start.min(count)..rows.end.min(count);
                    let lines = resolved_lines(choice, ours, theirs, shown.clone());
                    for (row, (text, from)) in shown.zip(lines) {
                        let y = body_top + row as f32 * line_h();
                        let color = from.map_or(typed(), side_color);
                        let row_area = row_rect(scene.gutter, rect, y);
                        painter.rect_filled(row_area, 0.0, theme::mix(theme::bg_app(), color, 0.15));
                        painter.rect_filled(Rect::from_min_size(row_area.min, vec2(3.0, line_h())), 0.0, color);
                        draw_number(&painter, scene.gutter, rect, y, block.above[2] + row + 1);
                        draw_line(&text_painter, text_x, y, &text, &scene.font, theme::text_bright());
                    }
                    for row in rows.start.max(count)..rows.end {
                        let y = body_top + row as f32 * line_h();
                        painter.rect_filled(row_rect(scene.gutter, rect, y), 0.0, theme::bg_void());
                        if count == 0 && row == 0 {
                            let remark = "no lines here in the result";
                            draw_remark(&painter, scene.gutter, rect, y, remark, theme::text_faint());
                        }
                    }
                }
                if index == scene.current {
                    outline_current(&painter, rect, block, top);
                }
            }
        }
    }

    // A click on the text opens the editor there.
    if background.clicked()
        && let Some(pos) = background.interact_pointer_pos()
    {
        let y = pos.y - rect.top() + scene.scroll.y;
        let column = ((pos.x - text_x) / scene.char_w).round().max(0.0) as usize;
        if let Some(block) = scene.plan.at(y) {
            let row = ((y - block.top) / line_h()).max(0.0) as usize;
            match block.kind {
                Kind::Stable { seg, from, .. } => {
                    actions.push(Action::Edit(Target::Line { seg, line: from + row }, (0, column)));
                }
                Kind::Edited { seg, line } => actions.push(Action::Edit(Target::Line { seg, line }, (row, column))),
                Kind::Conflict { index } if y >= block.top + HEAD_H => {
                    let row = ((y - block.top - HEAD_H) / line_h()) as usize;
                    actions.push(Action::Edit(Target::Conflict(index), (row, column)));
                }
                Kind::Conflict { index } => actions.push(Action::Focus(index)),
            }
        }
    }
    cursor
}

/// Character index of `column` in row `row` of `text`, clamped to the text.
fn char_index(text: &str, row: usize, column: usize) -> usize {
    let mut index = 0;
    for (i, line) in text.split('\n').enumerate() {
        let chars = line.chars().count();
        if i == row {
            return index + column.min(chars);
        }
        index += chars + 1;
    }
    text.chars().count()
}

/// The text editor, placed over the rows it edits. Rows have the height and
/// font of the painted ones, so the text does not move when it opens.
#[allow(clippy::too_many_arguments)]
fn editor(
    ui: &mut Ui,
    scene: &Scene<'_>,
    pane: Rect,
    area: Rect,
    text_x: f32,
    text_w: f32,
    editing: &mut Editing,
    actions: &mut Vec<Action>,
) -> Option<Rect> {
    let id = ui.id().with("editor");
    if let Some((row, column)) = editing.place.take() {
        let at = CCursor::new(char_index(&editing.text, row, column));
        let mut state = TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
        state.cursor.set_char_range(Some(CCursorRange::one(at)));
        TextEdit::store_state(ui.ctx(), id, state);
        ui.memory_mut(|memory| memory.request_focus(id));
    }

    // As wide as the longest line, so typing does not hit a wrap; the pane
    // clips it and the shared horizontal scroll moves it.
    let width = text_w.max(editing.text.len() as f32 * scene.char_w + 40.0).max(area.right() - text_x);
    let wide = Rect::from_min_size(pos2(text_x, area.top()), vec2(width, area.height()));
    let builder = UiBuilder::new().id_salt("editor-ui").max_rect(wide).layout(Layout::top_down(Align::Min));
    let mut child = ui.new_child(builder);
    let text_clip = Rect::from_min_max(pos2(pane.left() + scene.gutter, pane.top()), pane.max);
    child.set_clip_rect(text_clip.intersect(ui.clip_rect()));

    let font = scene.font.clone();
    let mut layouter = |ui: &Ui, buffer: &dyn TextBuffer, _wrap: f32| {
        ui.painter().layout_job(row_job(buffer.as_str(), &font, theme::text_bright()))
    };
    let output = TextEdit::multiline(&mut editing.text)
        .id(id)
        .frame(Frame::NONE)
        .margin(Margin::ZERO)
        .desired_width(width)
        .desired_rows(1)
        .lock_focus(true)
        .layouter(&mut layouter)
        .show(&mut child);

    let response = &output.response.response;
    if response.changed() {
        actions.push(Action::Typed);
    }
    // Clicking anywhere else, or Escape, ends the edit; the text stays. Ask
    // the memory rather than the response, which also reports "no focus"
    // whenever the window itself is in the background.
    if editing.shown && !ui.memory(|memory| memory.has_focus(id)) {
        actions.push(Action::StopEditing);
    }
    editing.shown = true;

    let at = output.cursor_range.map(|range| range.primary);
    let moved = at.map(|c| c.index.0) != editing.cursor;
    editing.cursor = at.map(|c| c.index.0);
    match at {
        Some(at) if moved => Some(output.galley.pos_from_cursor(at).translate(output.galley_pos.to_vec2())),
        _ => None,
    }
}

/// A scrollbar for one axis. `marks` are positions in content pixels to flag
/// on the track.
#[allow(clippy::too_many_arguments)]
fn scrollbar(
    ui: &mut Ui,
    rect: Rect,
    salt: &str,
    offset: &mut f32,
    max: f32,
    view: f32,
    vertical: bool,
    marks: &[(f32, Color32)],
) {
    let painter = ui.painter().with_clip_rect(rect);
    painter.rect_filled(rect, 0.0, theme::bg_panel());
    let length = if vertical { rect.height() } else { rect.width() };
    let content = (max + view).max(1.0);

    if max > 0.0 {
        let thumb = (length * view / content).clamp(24.0_f32.min(length), length);
        let travel = (length - thumb).max(1.0);
        let response = ui.interact(rect, ui.id().with(salt), Sense::click_and_drag());
        if response.is_pointer_button_down_on()
            && let Some(pos) = response.interact_pointer_pos()
        {
            let along = if vertical { pos.y - rect.top() } else { pos.x - rect.left() };
            *offset = ((along - thumb / 2.0) / travel).clamp(0.0, 1.0) * max;
        }
        let start = travel * (*offset / max).clamp(0.0, 1.0);
        let thumb_rect = if vertical {
            Rect::from_min_size(pos2(rect.left() + 2.0, rect.top() + start), vec2(rect.width() - 4.0, thumb))
        } else {
            Rect::from_min_size(pos2(rect.left() + start, rect.top() + 2.0), vec2(thumb, rect.height() - 4.0))
        };
        let fill = if response.is_pointer_button_down_on() {
            theme::text_dim()
        } else if response.hovered() {
            theme::text_faint()
        } else {
            theme::border_light()
        };
        painter.rect_filled(thumb_rect, 3.0, fill);
    }

    for (at, color) in marks {
        let y = rect.top() + (at / content) * length;
        let tick = Rect::from_min_size(pos2(rect.left() + 1.0, y), vec2(rect.width() - 2.0, 3.0));
        painter.rect_filled(tick, 1.0, *color);
    }
}

// ---- unsaved choices -------------------------------------------------------

enum Answer {
    Save { mark_resolved: bool },
    Discard,
    Cancel,
}

/// Asks what to do with unsaved choices before the view goes away.
fn ask(ctx: &egui::Context, tab: &mut RepoTab, shared: &mut Shared, view: &mut Resolver, file: &ConflictFile) {
    let open = view.draft.open_conflicts();
    let frame = Frame::new()
        .fill(theme::bg_toolbar())
        .stroke(Stroke::new(1.0, theme::border_light()))
        .corner_radius(8.0)
        .inner_margin(Margin::same(20));
    let modal = Modal::new(Id::new("conflict-unsaved")).frame(frame).show(ctx, |ui| {
        ui.set_width(430.0);
        ui.spacing_mut().item_spacing.y = 8.0;
        ui.label(RichText::new("Unsaved choices").font(theme::bold(16.0)).color(theme::text_bright()));
        ui.label(
            RichText::new(format!("What you chose for \"{}\" has not been saved to the file yet.", view.path))
                .color(theme::text()),
        );
        let remark = match open {
            0 => "Every conflict in it is resolved.".to_owned(),
            1 => "1 conflict is still unresolved; saving keeps it marked as conflicted.".to_owned(),
            n => format!("{n} conflicts are still unresolved; saving keeps it marked as conflicted."),
        };
        ui.label(RichText::new(remark).color(theme::text_dim()));
        ui.add_space(6.0);

        let mut answer = None;
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let label = if open == 0 { "Save and mark resolved" } else { "Save" };
            let primary = Button::new(RichText::new(label).color(theme::on_accent()))
                .fill(theme::solid(theme::green(), 0.6))
                .min_size(vec2(84.0, 28.0));
            if ui.add(primary).clicked() {
                answer = Some(Answer::Save { mark_resolved: open == 0 });
            }
            let discard = Button::new(RichText::new("Discard").color(theme::on_accent()))
                .fill(theme::solid(theme::red(), 0.7))
                .min_size(vec2(84.0, 28.0));
            if ui.add(discard).clicked() {
                answer = Some(Answer::Discard);
            }
            if ui.add(Button::new("Cancel").min_size(vec2(72.0, 28.0))).clicked() {
                answer = Some(Answer::Cancel);
            }
        });
        answer
    });

    let answer = match modal.inner {
        Some(answer) => answer,
        None if modal.backdrop_response.clicked() => Answer::Cancel,
        None => return,
    };
    view.asking = false;
    match answer {
        Answer::Save { mark_resolved } => view.save(tab, shared, file, mark_resolved, true),
        Answer::Discard => view.closing = true,
        Answer::Cancel => view.then = None,
    }
    ctx.request_repaint();
}

// ---- conflicts settled by choosing a whole file -----------------------------

fn file_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} bytes")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f32 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f32 / (1024.0 * 1024.0))
    }
}

/// Binary files, delete/modify and rename leftovers: no editor, just the
/// question of which version stays.
fn whole_ui(ui: &mut Ui, tab: &mut RepoTab, view: &mut Resolver, file: &ConflictFile, why: Whole) {
    let (ours, theirs) = (&file.ours_info, &file.theirs_info);
    let (headline, explanation) = match why {
        Whole::Binary => (
            "Binary file changed on both sides".to_owned(),
            "A binary file cannot be merged line by line. Choose the version to keep.",
        ),
        Whole::DeletedByUs => (
            format!("Deleted in {}, changed in {}", ours.name, theirs.name),
            "Keep the changed file, or let it stay deleted.",
        ),
        Whole::DeletedByThem => (
            format!("Changed in {}, deleted in {}", ours.name, theirs.name),
            "Keep the changed file, or delete it as the other side did.",
        ),
        Whole::BothDeleted => (
            "Both sides removed this path".to_owned(),
            "Usually each side renamed the file differently, and the new names are conflicts of their own. \
             This old path can normally go.",
        ),
        Whole::AddedByUs => (
            format!("Only {} has this path", ours.name),
            "Usually this is the name one side gave a file that the other side renamed or deleted. \
             Keep it under this name, or delete it.",
        ),
        Whole::AddedByThem => (
            format!("Only {} has this path", theirs.name),
            "Usually this is the name one side gave a file that the other side renamed or deleted. \
             Keep it under this name, or delete it.",
        ),
        Whole::TooLarge => (
            "Too large for the line-by-line resolver".to_owned(),
            "Choose the version to keep, or resolve the file in an editor and mark it as resolved \
             from the file list.",
        ),
        Whole::Special => (
            "A symbolic link or submodule is in conflict".to_owned(),
            "It cannot be merged line by line. Choose the version to keep.",
        ),
    };

    let saving = view.saving.is_some();
    let mut run = None;
    let area = ui.available_rect_before_wrap();
    ui.allocate_rect(area, Sense::hover());
    let column = Rect::from_center_size(area.center(), vec2(area.width().min(620.0) - 40.0, area.height() - 40.0));
    ui.scope_builder(UiBuilder::new().max_rect(column).layout(Layout::top_down(Align::Center)), |ui| {
        ui.spacing_mut().item_spacing.y = 8.0;
        ui.add_space((column.height() * 0.16).max(8.0));
        ui.label(RichText::new(icons::WARNING).size(40.0).color(theme::orange()));
        ui.label(RichText::new(headline).font(theme::bold(17.0)).color(theme::text_bright()));
        ui.add(egui::Label::new(RichText::new(explanation).color(theme::text_dim())).wrap());
        ui.add_space(14.0);

        for (side, info, stage) in [(Side::Ours, ours, &file.ours), (Side::Theirs, theirs, &file.theirs)] {
            side_card(ui, info, stage.as_ref(), side_color(side));
        }
        ui.add_space(14.0);

        if saving {
            ui.add(egui::Spinner::new().size(18.0).color(theme::accent()));
            return;
        }
        let wide = |text: String, tint: Color32| {
            Button::new(RichText::new(text).size(13.5).color(theme::text_bright()))
                .fill(theme::mix(theme::bg_app(), tint, 0.3))
                .stroke(Stroke::new(1.0, theme::mix(theme::bg_app(), tint, 0.75)))
                .min_size(vec2(column.width().min(380.0), 32.0))
        };
        for side in [Side::Ours, Side::Theirs] {
            let info = file.info(side);
            let label = format!("Keep the {} version  ({})", info.role.to_lowercase(), info.name);
            if file.stage(side).is_some() && ui.add(wide(label, side_color(side))).clicked() {
                let done = format!("Kept the {} version ({}).", info.role.to_lowercase(), info.name);
                run = Some((conflict::keep(&file.path, side), done));
            }
        }
        if ui.add(wide(format!("{}  Delete the file", icons::TRASH), theme::red())).clicked() {
            run = Some((conflict::delete(&file.path), "The file is deleted.".to_owned()));
        }
    });

    if let Some((cmd, done)) = run {
        view.run(tab, cmd, false, (format!("Resolved {}", file_name(&file.path)), done));
        ui.ctx().request_repaint();
    }
}

/// What one side has at this path.
fn side_card(ui: &mut Ui, info: &SideInfo, stage: Option<&Stage>, color: Color32) {
    let width = ui.available_width().min(520.0);
    let (rect, _) = ui.allocate_exact_size(vec2(width, 48.0), Sense::hover());
    let painter = ui.painter().with_clip_rect(rect);
    painter.rect(
        rect,
        5.0,
        theme::mix(theme::bg_app(), color, 0.10),
        Stroke::new(1.0, theme::mix(theme::bg_app(), color, 0.5)),
        StrokeKind::Inside,
    );
    let top = Rect::from_min_max(rect.min, pos2(rect.right(), rect.center().y + 2.0));
    let bottom = Rect::from_min_max(pos2(rect.left(), rect.center().y - 2.0), rect.max);
    let mut x = rect.left() + 12.0;
    x += widgets::text_left(&painter, top, x, &info.role.to_uppercase(), theme::bold(10.5), color) + 8.0;
    x += widgets::text_left(&painter, top, x, &info.name, theme::bold(13.0), theme::text_bright()) + 10.0;
    let detail = widgets::elided(&painter, &info.detail, theme::font(12.0), theme::text_dim(), rect.right() - x - 10.0);
    widgets::galley_left(&painter, top, x, detail);

    let (text, text_color) = match stage {
        None => ("does not have this file".to_owned(), theme::text_dim()),
        Some(stage) if stage.mode & 0o170000 == 0o160000 => ("a submodule commit".to_owned(), theme::text()),
        Some(stage) if stage.mode & 0o170000 == 0o120000 => ("a symbolic link".to_owned(), theme::text()),
        Some(stage) => {
            let kind = if stage.binary { "binary file" } else { "text file" };
            (format!("{kind}, {}", file_size(stage.size)), theme::text())
        }
    };
    widgets::text_left(&painter, bottom, rect.left() + 12.0, &text, theme::font(12.5), text_color);
}
