//! Merge-conflict resolver for one file.
//!
//! The two sides sit next to each other with the result underneath, and the
//! common ancestor between the sides when it is asked for. All panes show
//! the same rows at the same height: text that merges on its own appears
//! once in each, and every conflict is a block with a header strip (the
//! controls) and as many rows as its tallest version. With word wrap every
//! pane wraps at the width of the narrowest, so a line takes as many rows
//! wherever it is. One scroll position therefore serves all panes, and only
//! the rows in view are laid out.
//!
//! The panes' text is kept as shown (tabs at the tab stops of Preferences >
//! Editor) and syntax-coloured in the background; see `ui::conflict_text`.
//! The text of the sides and the ancestor can be selected and copied
//! (`ui::textsel`); a click on a conflict's strip or on its line numbers
//! takes that side.
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

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use eframe::egui::text::{CCursor, CCursorRange};
use eframe::egui::{
    self, Align, Align2, Button, Color32, CursorIcon, FontId, Frame, Id, Key, Layout, Margin, Modal,
    Modifiers, Painter, Pos2, Rangef, Rect, RichText, Sense, Stroke, StrokeKind, TextBuffer, TextEdit, Ui,
    UiBuilder, Vec2, pos2, vec2,
};

use crate::git::conflict::{
    self, Chunk, ConflictFile, Content, Draft, Resolution, Segment, Side, SideInfo, Stage, TextMerge,
    Whole,
};
use crate::git::{ChangeKind, DiffLine, DiffTarget, FileDiff, Hunk, Oid, text};
use crate::prefs::editor;
use crate::state::{Overlay, RepoTab, Settings, Shared, ToastKind};
use crate::theme::{self, icons};
use crate::ui::conflict_text::{self, EditorLook, PaneText, Version, typed_hunk};
use crate::ui::diff;
use crate::ui::highlight::{self, DiffStyle, HunkView, Span};
use crate::ui::palette::{Entry, Env, Kind as EntryKind};
use crate::ui::textsel::{self, Pos, Selection};
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
/// Share of the height given to the panes on top; the result gets the rest.
const SIDES_SHARE: f32 = 0.45;
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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
    /// Rows the text took when last laid out, wrapped lines included.
    rows: Option<usize>,
    /// The colours each line was last shown with, kept for a line while it
    /// is being typed in and the highlighter has not caught up.
    spans: Vec<Vec<Span>>,
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
    /// What the plan was made for; anything else makes another.
    plan_for: PlanFor,
    /// The panes' texts, made once the file is read (and again for a new
    /// tab width).
    panes: Option<Box<Panes>>,
    /// Counts the texts made, see [`Panes::serial`].
    serial: u64,
    typed: Typed,
    styles: Styles,
    /// What is selected in each pane with a text, by [`Version`].
    selections: [Selection; 3],
    /// The wrapped rows the plan was made with.
    wrapped: Option<[Arc<Vec<u32>>; 3]>,
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
            plan_for: PlanFor::default(),
            panes: None,
            serial: 0,
            typed: Typed::default(),
            styles: Styles::default(),
            selections: Default::default(),
            wrapped: None,
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
        let encoding = text::encoding_for(&tab.path);
        tab.spawn_job(self.job_key(), move |loader| conflict::load(loader.repo(), &path, encoding));
        self.phase = Phase::Loading;
        self.file = None;
        self.panes = None;
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

/// Command palette entries for the resolver's switches, while the
/// repository in front has conflicted files. (Word wrap has its own entry.)
pub fn palette_entries(list: &mut Vec<Entry>, snap: Option<&crate::git::RepoSnapshot>, settings: &Settings) {
    let conflicted = snap.is_some_and(|snap| snap.status.unstaged.iter().any(|f| f.kind == ChangeKind::Conflicted));
    if !conflicted {
        return;
    }
    let on = settings.editor.conflict_ancestor;
    let label = if on { "Hide the common ancestor of conflicts" } else { "Show the common ancestor of conflicts" };
    list.push(
        Entry::new(EntryKind::Action, "conflict-ancestor", icons::GIT_FORK, label, move |env: &mut Env<'_>| {
            env.shared.settings.editor.conflict_ancestor = !on;
        })
        .keywords("conflict resolver ancestor base merge base stage 1 diff3 three-way"),
    );
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
            Content::Whole(why) => whole_ui(ui, tab, shared, view, &file, *why),
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
    /// Lines `from..to` of a stable segment, the same in all panes.
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
    /// Lines above this block in the current side, the incoming side, the
    /// result and the ancestor ([`RESULT`], or a [`Version`] as index), for
    /// the line numbers.
    above: [usize; 4],
}

/// Index of the result in [`Block::above`], after the three [`Version`]s.
const RESULT: usize = 3;

/// Where every block of rows sits, top to bottom.
#[derive(Default)]
struct Plan {
    blocks: Vec<Block>,
    height: f32,
    /// Longest line in columns, for the horizontal scroll range.
    longest: usize,
    /// Lines in the longest of the panes, for the width of the numbers.
    lines: usize,
}

/// What a plan was made for; another font size, wrap width or set of panes
/// makes another.
#[derive(Clone, Copy, Default, PartialEq)]
struct PlanFor {
    line_h: f32,
    columns: Option<usize>,
    ancestor: bool,
    texts: u64,
}

/// Rows a piece of typed text occupies: none when empty.
fn text_rows(text: &str) -> usize {
    if text.is_empty() { 0 } else { text.split('\n').count() }
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

/// Some lines of a text and the row each starts at.
#[derive(Clone, Copy)]
struct Rows<'a> {
    lines: &'a [DiffLine],
    /// The first row of every line, then the total, when lines wrap; each
    /// line is one row without.
    first: Option<&'a [u32]>,
}

impl Rows<'_> {
    /// The row line `line` starts at (or, for one past the last, the total).
    fn start(&self, line: usize) -> usize {
        self.first.map_or(line, |first| first.get(line).map_or(0, |row| *row as usize))
    }

    fn count(&self, lines: Range<usize>) -> usize {
        self.start(lines.end) - self.start(lines.start)
    }

    fn all(&self) -> usize {
        self.count(0..self.lines.len())
    }

    /// The line row `row` is part of.
    fn line_at(&self, row: usize) -> usize {
        match self.first {
            None => row,
            Some(first) => first.partition_point(|start| *start as usize <= row).saturating_sub(1),
        }
    }
}

/// The texts of the panes that have one, made with the tab width in effect.
struct Panes {
    /// Indexed by [`Version`].
    texts: [PaneText; 3],
    /// `text::generation` when they were made.
    generation: u64,
    /// Goes up every time texts are made, so plans and selections know.
    serial: u64,
}

impl Panes {
    fn new(merge: &TextMerge, serial: u64) -> Self {
        let generation = text::generation();
        let width = text::tab_width();
        let texts = [Version::Ours, Version::Theirs, Version::Base].map(|version| PaneText::new(merge, version, width));
        Panes { texts, generation, serial }
    }
}

/// The text typed by hand, kept as shown for painting and colouring.
#[derive(Default)]
struct Typed {
    /// Every piece: what it was typed for, and the text.
    pieces: Vec<(Target, String)>,
    index: HashMap<Target, usize>,
    /// The pieces as shown, one hunk each.
    diff: Arc<FileDiff>,
    /// Where each line of each piece starts when wrapping (see [`Rows`]).
    rows: Vec<Vec<u32>>,
    columns: Option<usize>,
    tab_width: usize,
    longest: usize,
}

impl Typed {
    /// Brings the pieces up to date with `draft` and the text being typed.
    fn update(&mut self, draft: &Draft, editing: Option<&Editing>, columns: Option<usize>) {
        let mut pieces = Vec::new();
        for (index, choice) in draft.choices.iter().enumerate() {
            if let Resolution::Custom(text) = choice {
                pieces.push((Target::Conflict(index), text.clone()));
            }
        }
        for (&(seg, line), text) in &draft.edits {
            pieces.push((Target::Line { seg, line }, text.clone()));
        }
        if let Some(editing) = editing {
            match pieces.iter_mut().find(|(target, _)| *target == editing.target) {
                Some(piece) => piece.1.clone_from(&editing.text),
                None => pieces.push((editing.target, editing.text.clone())),
            }
        }
        let tab_width = text::tab_width();
        let changed = pieces != self.pieces || tab_width != self.tab_width;
        if changed {
            let hunks: Vec<Hunk> = pieces.iter().map(|(_, text)| typed_hunk(text, tab_width)).collect();
            self.longest = hunks.iter().flat_map(|h| &h.lines).map(|l| l.text.chars().count()).max().unwrap_or(0);
            self.index = pieces.iter().enumerate().map(|(i, (target, _))| (*target, i)).collect();
            self.diff = Arc::new(FileDiff { hunks, ..Default::default() });
            self.pieces = pieces;
            self.tab_width = tab_width;
        }
        if changed || columns != self.columns {
            self.columns = columns;
            self.rows = match columns {
                Some(columns) => {
                    let rows = |hunk: &Hunk| diff::first_rows(hunk.lines.iter().map(|l| l.text.as_str()), columns);
                    self.diff.hunks.iter().map(rows).collect()
                }
                None => Vec::new(),
            };
        }
    }
}

/// The texts as the plan and the panes need them in one frame.
struct Texts<'a> {
    panes: &'a [PaneText; 3],
    /// First rows of every line of each pane text when wrapping.
    wrapped: Option<&'a [Arc<Vec<u32>>; 3]>,
    typed: &'a Typed,
    /// Columns lines wrap at, the same in every pane so that the same text
    /// takes the same rows wherever it is.
    columns: Option<usize>,
}

impl<'a> Texts<'a> {
    fn pane(&self, version: Version) -> Rows<'a> {
        let first = self.wrapped.map(|rows| rows[version as usize].as_slice());
        Rows { lines: self.panes[version as usize].lines(), first }
    }

    /// The hunk of the text typed for `target`, and its rows.
    fn typed(&self, target: Target) -> Option<(usize, Rows<'a>)> {
        let hunk = *self.typed.index.get(&target)?;
        let lines = &self.typed.diff.hunks.get(hunk)?.lines;
        let first = self.columns.and_then(|_| self.typed.rows.get(hunk)).map(Vec::as_slice);
        Some((hunk, Rows { lines, first }))
    }

    /// The lines of `version`'s text that `block` shows.
    fn block_lines(&self, merge: &TextMerge, version: Version, block: &Block) -> Range<usize> {
        let pane = &self.panes[version as usize];
        match block.kind {
            Kind::Stable { seg, from, to } => {
                let start = pane.segment(seg).start;
                start + from..start + to
            }
            Kind::Edited { seg, line } => {
                let start = pane.segment(seg).start + line;
                start..start + 1
            }
            Kind::Conflict { index } => merge.conflicts.get(index).map_or(0..0, |seg| pane.segment(*seg)),
        }
    }

    /// What a resolution of conflict `index` (segment `seg`) puts into the
    /// result when it takes sides: lines of the side texts, in order.
    fn taken(&self, choice: &Resolution, seg: usize) -> Vec<(Side, Range<usize>)> {
        let ours = (Side::Ours, self.panes[Version::Ours as usize].segment(seg));
        let theirs = (Side::Theirs, self.panes[Version::Theirs as usize].segment(seg));
        match choice {
            Resolution::Ours => vec![ours],
            Resolution::Theirs => vec![theirs],
            Resolution::OursTheirs => vec![ours, theirs],
            Resolution::TheirsOurs => vec![theirs, ours],
            Resolution::Unresolved | Resolution::Neither | Resolution::Custom(_) => Vec::new(),
        }
    }

    /// Lines and rows a resolution puts into the result.
    fn resolved(&self, choice: &Resolution, index: usize, seg: usize) -> (usize, usize) {
        if let Resolution::Custom(_) = choice {
            return self.typed(Target::Conflict(index)).map_or((0, 0), |(_, rows)| (rows.lines.len(), rows.all()));
        }
        self.taken(choice, seg).into_iter().fold((0, 0), |(lines, rows), (side, range)| {
            (lines + range.len(), rows + self.pane(version_of(side)).count(range))
        })
    }
}

fn version_of(side: Side) -> Version {
    match side {
        Side::Ours => Version::Ours,
        Side::Theirs => Version::Theirs,
    }
}

impl Plan {
    fn build(merge: &TextMerge, draft: &Draft, editing: Option<&Editing>, texts: &Texts<'_>, ancestor: bool) -> Plan {
        let mut plan = Plan::default();
        let mut top = 0.0;
        let mut above = [0usize; 4];
        let mut conflict = 0;
        let line_h = line_h();
        let mut push = |plan: &mut Plan, kind: Kind, rows: usize, head: f32, lines: [usize; 4]| {
            let bottom = top + head + rows as f32 * line_h;
            plan.blocks.push(Block { kind, top, bottom, rows, above });
            top = bottom;
            for (count, add) in above.iter_mut().zip(lines) {
                *count += add;
            }
        };
        let ours = texts.pane(Version::Ours);
        // The text being typed: its rows as laid out last frame, once known.
        let typing = |target: Target, rows: usize| match editing {
            Some(e) if e.target == target => e.rows.unwrap_or(rows).max(1),
            _ => rows,
        };

        for (seg, segment) in merge.segments.iter().enumerate() {
            match segment {
                Segment::Stable(chunk) => {
                    let start = texts.panes[Version::Ours as usize].segment(seg).start;
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
                            let rows = ours.count(start + from..start + line);
                            push(&mut plan, Kind::Stable { seg, from, to: line }, rows, 0.0, [n, n, n, n]);
                        }
                        let target = Target::Line { seg, line };
                        let (typed_lines, typed_rows) =
                            texts.typed(target).map_or((1, 1), |(_, rows)| (rows.lines.len(), rows.all()));
                        let rows = ours.count(start + line..start + line + 1).max(typing(target, typed_rows)).max(1);
                        push(&mut plan, Kind::Edited { seg, line }, rows, 0.0, [1, 1, 1, typed_lines]);
                        from = line + 1;
                    }
                    if from < chunk.len() {
                        let n = chunk.len() - from;
                        let rows = ours.count(start + from..start + chunk.len());
                        push(&mut plan, Kind::Stable { seg, from, to: chunk.len() }, rows, 0.0, [n, n, n, n]);
                    }
                }
                Segment::Conflict { ours: mine, theirs: yours, base } => {
                    let choice = draft.choices.get(conflict).unwrap_or(&Resolution::Unresolved);
                    let target = Target::Conflict(conflict);
                    let (result_lines, result_rows) = match editing {
                        Some(e) if e.target == target => {
                            let rows = texts.typed(target).map_or(1, |(_, rows)| rows.all());
                            (text_rows(&e.text), typing(target, rows))
                        }
                        _ => texts.resolved(choice, conflict, seg),
                    };
                    let side_rows = |version: Version| texts.pane(version).count(texts.panes[version as usize].segment(seg));
                    let base_rows = if ancestor { side_rows(Version::Base) } else { 0 };
                    let rows = side_rows(Version::Ours)
                        .max(side_rows(Version::Theirs))
                        .max(base_rows)
                        .max(result_rows)
                        .max(1);
                    let lines = [mine.len(), yours.len(), base.len(), result_lines];
                    push(&mut plan, Kind::Conflict { index: conflict }, rows, HEAD_H, lines);
                    conflict += 1;
                }
            }
        }
        plan.height = top;
        plan.lines = above.into_iter().max().unwrap_or(0);
        let [mine, yours, base] = texts.panes.each_ref().map(|pane| pane.longest);
        plan.longest = mine.max(yours).max(if ancestor { base } else { 0 }).max(texts.typed.longest);
        plan
    }

    /// The blocks that intersect `height` pixels starting at `offset`.
    fn visible(&self, offset: f32, height: f32) -> &[Block] {
        let first = self.blocks.partition_point(|block| block.bottom <= offset);
        let last = self.blocks.partition_point(|block| block.top < offset + height);
        &self.blocks[first.min(last)..last]
    }

    /// The block at `y`, or the nearest one above or below the content.
    fn nearest(&self, y: f32) -> Option<&Block> {
        let at = self.blocks.partition_point(|block| block.bottom <= y);
        self.blocks.get(at.min(self.blocks.len().saturating_sub(1)))
    }

    fn at(&self, y: f32) -> Option<&Block> {
        self.blocks.get(self.blocks.partition_point(|block| block.bottom <= y)).filter(|block| block.top <= y)
    }

    fn conflict(&self, index: usize) -> Option<&Block> {
        self.blocks.iter().find(|block| matches!(block.kind, Kind::Conflict { index: i } if i == index))
    }
}

/// A place in the view, kept while the rows move under it.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Anchor {
    /// The block it is in.
    block: usize,
    /// In a stretch of merged lines, the line (of the current side's text),
    /// else nothing more precise than the block.
    line: Option<usize>,
    /// Points below the top of that line, or of the block.
    offset: f32,
}

impl Anchor {
    /// Where `y` is in `plan`, made from `texts` with rows `line_h` high.
    fn at(plan: &Plan, texts: &Texts<'_>, merge: &TextMerge, y: f32, line_h: f32) -> Option<Anchor> {
        let index = plan.blocks.partition_point(|block| block.bottom <= y);
        let block = plan.blocks.get(index)?;
        let rows = texts.pane(Version::Ours);
        let lines = texts.block_lines(merge, Version::Ours, block);
        if let Kind::Stable { .. } = block.kind
            && !lines.is_empty()
        {
            let first = rows.start(lines.start);
            let row = ((y - block.top) / line_h).max(0.0) as usize;
            let line = rows.line_at(first + row).clamp(lines.start, lines.end - 1);
            let offset = y - (block.top + (rows.start(line) - first) as f32 * line_h);
            return Some(Anchor { block: index, line: Some(line), offset: offset / line_h });
        }
        Some(Anchor { block: index, line: None, offset: (y - block.top) / line_h })
    }

    /// Where the place is in `plan`, made from `texts` for the same blocks.
    fn find(self, plan: &Plan, texts: &Texts<'_>, merge: &TextMerge) -> Option<f32> {
        let block = plan.blocks.get(self.block)?;
        let line_h = line_h();
        match self.line {
            Some(line) => {
                let rows = texts.pane(Version::Ours);
                let lines = texts.block_lines(merge, Version::Ours, block);
                lines.contains(&line).then(|| {
                    block.top + (rows.start(line) - rows.start(lines.start)) as f32 * line_h + self.offset * line_h
                })
            }
            None => Some(block.top + (self.offset * line_h).min(block.bottom - block.top)),
        }
    }
}

/// Height of the strip above a block's rows.
fn head_h(block: &Block) -> f32 {
    match block.kind {
        Kind::Conflict { .. } => HEAD_H,
        _ => 0.0,
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
    /// Hand the file to the external merge tool (Preferences > External Tools).
    MergeTool,
    /// Show the ancestor pane, or not.
    ShowAncestor(bool),
    /// Wrap long lines, or not.
    Wrap(bool),
    /// The editor's text takes another number of rows: lay out again.
    Relayout,
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
                self.editing = Some(Editing {
                    target,
                    text,
                    place: Some(place),
                    shown: false,
                    cursor: None,
                    rows: None,
                    spans: Vec::new(),
                });
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
            Action::MergeTool => {
                if let Some(cmd) = crate::external::merge(&shared.settings.tools, &file.path) {
                    tab.run(cmd);
                }
            }
            // Both are Preferences > Editor settings, so they are remembered.
            Action::ShowAncestor(on) => shared.settings.editor.conflict_ancestor = on,
            Action::Wrap(on) => shared.settings.editor.word_wrap = on,
            Action::Relayout => {}
        }
    }
}

// ---- the text resolver -----------------------------------------------------

/// Colour of the ancestor, which is neither side.
fn ancestor_color() -> Color32 {
    theme::text_dim()
}

/// Syntax colours of the pane texts and of the typed text (indexed by
/// [`Version`], then [`TYPED`]), kept while the text they colour stays.
#[derive(Default)]
struct Styles {
    known: [Option<Styled>; 4],
}

/// A text, whether colours were asked for, and its styling.
type Styled = (Arc<FileDiff>, bool, Arc<DiffStyle>);

/// Index of the typed text in [`Styles`].
const TYPED: usize = 3;

impl Styles {
    /// The styling of `diff`, asked for once per text: it fills in from the
    /// highlighting thread while it is held.
    fn get(&mut self, slot: usize, ctx: &egui::Context, diff: &Arc<FileDiff>, path: &str, colours: bool) -> Arc<DiffStyle> {
        if let Some((known, on, style)) = &self.known[slot]
            && Arc::ptr_eq(known, diff)
            && *on == colours
        {
            return style.clone();
        }
        let style = highlight::style_for(ctx, diff, path, colours);
        self.known[slot] = Some((diff.clone(), colours, style.clone()));
        style
    }
}

/// What the panes need to paint a frame.
struct Scene<'a> {
    merge: &'a TextMerge,
    draft: &'a Draft,
    plan: &'a Plan,
    texts: Texts<'a>,
    /// Indexed by [`Version`], then [`TYPED`].
    styles: [Arc<DiffStyle>; 4],
    scroll: Vec2,
    current: usize,
    font: FontId,
    char_w: f32,
    /// What the editor needs to put tabs at the tab stops.
    space_w: f32,
    tab_w: f32,
    tab_width: usize,
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
    // Preferences > Editor: the font, its size, line numbers, wrapping,
    // colours; and whether the ancestor is shown.
    let look = editor::look();
    let ancestor = shared.settings.editor.conflict_ancestor;
    // The pane texts are made with the tab width in effect.
    if view.panes.as_ref().is_none_or(|panes| panes.generation != text::generation()) {
        view.serial += 1;
        view.panes = Some(Box::new(Panes::new(merge, view.serial)));
    }

    let mut actions = Vec::new();
    let saving = view.saving.is_some();
    let open = view.draft.open_conflicts();
    let total = view.draft.choices.len();
    let merging = tab.is_running(&crate::external::merge_label(&shared.settings.tools, &file.path));
    title_buttons(ui, free, open, view.is_dirty(), (saving, merging), &mut actions);
    toolbar(ui, open, total, saving, (ancestor, look.wrap), &mut actions);
    if edited_outside {
        notice(
            ui,
            "This file was already changed by hand and has no conflict markers left. \
             Saving from here replaces that version.",
        );
    }

    let body = ui.available_rect_before_wrap();
    ui.allocate_rect(body, Sense::hover());

    // Geometry: the sides on top (the ancestor between them when shown),
    // the result below, one scrollbar for all.
    let content = Rect::from_min_max(body.min, pos2(body.right() - SCROLLBAR, body.bottom()));
    let sides_h = ((content.height() - PANE_HEAD_H * 2.0) * SIDES_SHARE).floor().max(line_h());
    let heads = Rect::from_min_max(content.min, pos2(content.right(), content.top() + PANE_HEAD_H));
    let sides = Rect::from_min_max(heads.left_bottom(), pos2(content.right(), heads.bottom() + sides_h));
    let result_head = Rect::from_min_max(sides.left_bottom(), pos2(content.right(), sides.bottom() + PANE_HEAD_H));
    let below = Rect::from_min_max(result_head.left_bottom(), content.max);
    let columns_on_top = if ancestor { 3 } else { 2 };
    let edge = |i: usize| (content.left() + content.width() * i as f32 / columns_on_top as f32).round();
    let top_pane = |i: usize, y: Rangef| {
        Rect::from_x_y_ranges((edge(i) + if i > 0 { 1.0 } else { 0.0 })..=edge(i + 1), y)
    };
    let ours_rect = top_pane(0, sides.y_range());
    let base_rect = ancestor.then(|| top_pane(1, sides.y_range()));
    let theirs_rect = top_pane(columns_on_top - 1, sides.y_range());
    let narrow = ours_rect.width().min(theirs_rect.width());

    let font = look.font();
    let (char_w, space_w, digit_w) = ui.painter().fonts_mut(|f| {
        (f.glyph_width(&font, 'm'), f.glyph_width(&font, ' '), f.glyph_width(&number_font(), '0'))
    });
    let tab_w = ui.painter().layout_no_wrap("\t".to_owned(), font.clone(), theme::text()).size().x;
    // Room for the largest line number, as far as it is known.
    let panes = view.panes.as_deref().expect("made above");
    let most = view.plan.lines.max(panes.texts.iter().map(|pane| pane.lines().len()).max().unwrap_or(0));
    let digits = most.max(1).ilog10() + 1;
    let gutter = if look.numbers { (digits as f32 * digit_w + 22.0).max(MIN_GUTTER) } else { NO_NUMBERS_GUTTER };
    // Every pane wraps at the width of the narrowest, so a line takes as
    // many rows in each and the panes stay level.
    let columns = look.wrap.then(|| {
        let width = narrow - gutter - TEXT_PAD - diff::WRAP_MARGIN;
        ((width / char_w).floor().max(0.0) as usize).max(diff::MIN_WRAP_COLUMNS)
    });
    let wrapped = columns.map(|columns| {
        [0, 1, 2].map(|i| {
            let pane = &panes.texts[i];
            let lines = || pane.lines().iter().map(|line| line.text.as_str());
            diff::wrap_rows(ui, ui.id().with(("conflict-wrap", i)), &pane.diff, columns, || diff::first_rows(lines(), columns))
        })
    });

    let plan_for = PlanFor { line_h: line_h(), columns, ancestor, texts: panes.serial };
    if view.replan || view.plan_for != plan_for {
        // Another wrap width or font size moves every row: keep the line at
        // the top of the view where it is.
        let anchor = (view.plan_for != plan_for && view.plan_for.texts == plan_for.texts).then(|| {
            let old = Texts { panes: &panes.texts, wrapped: view.wrapped.as_ref(), typed: &view.typed, columns: view.plan_for.columns };
            Anchor::at(&view.plan, &old, merge, view.scroll.y, view.plan_for.line_h)
        });
        let blocks = view.plan.blocks.len();
        view.typed.update(&view.draft, view.editing.as_ref(), columns);
        let texts = Texts { panes: &panes.texts, wrapped: wrapped.as_ref(), typed: &view.typed, columns };
        view.plan = Plan::build(merge, &view.draft, view.editing.as_ref(), &texts, ancestor);
        if let Some(y) = anchor.flatten().filter(|_| view.plan.blocks.len() == blocks).and_then(|a| a.find(&view.plan, &texts, merge)) {
            view.scroll.y = y;
        }
        view.wrapped.clone_from(&wrapped);
        view.replan = false;
        view.plan_for = plan_for;
        // More lines than the numbers had room for: lay out again.
        if view.plan.lines.max(1).ilog10() + 1 != digits {
            view.replan = true;
            ui.ctx().request_repaint();
        }
    }
    if let Some(block) = view.jump.take().and_then(|index| view.plan.conflict(index)) {
        view.scroll.y = block.top - jump_margin();
    }

    let text_w = view.plan.longest as f32 * char_w + 40.0;
    let max_x = if columns.is_some() { 0.0 } else { (text_w - (narrow - gutter - TEXT_PAD)).max(0.0) };
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
    pane_header(ui, Rect::from_x_y_ranges(ours_rect.x_range(), heads.y_range()), &file.ours_info, side_color(Side::Ours));
    if let Some(base) = base_rect {
        pane_header(ui, Rect::from_x_y_ranges(base.x_range(), heads.y_range()), &file.base_info, ancestor_color());
    }
    let theirs_head = Rect::from_x_y_ranges(theirs_rect.x_range(), heads.y_range());
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

    let path = file.path.as_str();
    let ctx = ui.ctx().clone();
    let styles = [0, 1, 2].map(|i| view.styles.get(i, &ctx, &panes.texts[i].diff, path, look.highlight));
    let typed_style = view.styles.get(TYPED, &ctx, &view.typed.diff, path, look.highlight);
    let [ours_style, theirs_style, base_style] = styles;
    let scene = Scene {
        merge,
        draft: &view.draft,
        plan: &view.plan,
        texts: Texts { panes: &panes.texts, wrapped: wrapped.as_ref(), typed: &view.typed, columns },
        styles: [ours_style, theirs_style, base_style, typed_style],
        scroll: view.scroll,
        current: view.current,
        font,
        char_w,
        space_w,
        tab_w,
        tab_width: text::tab_width(),
        gutter,
    };
    let [ours_sel, theirs_sel, base_sel] = &mut view.selections;
    let fingerprint = |version: Version| panes.serial << 2 | version as u64;
    let mut scrolled = Vec2::ZERO;
    scrolled += version_pane(ui, &scene, ours_rect, Version::Ours, ours_sel, fingerprint(Version::Ours), &mut actions);
    scrolled += version_pane(ui, &scene, theirs_rect, Version::Theirs, theirs_sel, fingerprint(Version::Theirs), &mut actions);
    if let Some(base) = base_rect {
        if file.base.is_some() {
            scrolled += version_pane(ui, &scene, base, Version::Base, base_sel, fingerprint(Version::Base), &mut actions);
        } else {
            no_ancestor(ui, base);
        }
    }
    let cursor = result_pane(ui, &scene, result_rect, text_w, &mut view.editing, &mut actions);
    drop(scene);

    let painter = ui.painter();
    for i in 1..columns_on_top {
        painter.vline(edge(i) + 0.5, sides.y_range(), Stroke::new(1.0, theme::border()));
    }

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
        let width = narrow - gutter - TEXT_PAD;
        scrollbar(ui, bar, "conflict-hscroll", &mut view.scroll.x, max_x, width, false, &[]);
    }

    // A selection dragged past the edge of its pane scrolls them all.
    if scrolled != Vec2::ZERO {
        view.scroll += scrolled;
        ui.ctx().request_repaint();
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
fn title_buttons(ui: &mut Ui, free: Rect, open: usize, dirty: bool, (saving, merging): (bool, bool), actions: &mut Vec<Action>) {
    const RESOLVE: &str = "Save and mark resolved";
    const SAVE: &str = "Save";
    const MERGE_TOOL: &str = "Open in merge tool";
    // The path may run under the buttons when the view is narrow.
    let width = tint_button_size(ui, RESOLVE).x + tint_button_size(ui, SAVE).x + tint_button_size(ui, MERGE_TOOL).x + 28.0;
    let backing = Rect::from_min_max(
        pos2((free.right() - width).max(free.left()), free.top() + 1.0),
        pos2(free.right(), free.bottom() - 1.0),
    );
    ui.painter().rect_filled(backing, 0.0, theme::bg_header());

    ui.scope_builder(UiBuilder::new().max_rect(free).layout(Layout::right_to_left(Align::Center)), |ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        if saving || merging {
            let what = if merging { "Waiting for the merge tool…" } else { "Saving…" };
            ui.label(RichText::new(what).color(theme::text_dim()));
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
        // The tool works on the file as it is on disk: not with choices
        // made here and not saved yet.
        let merge_tool = ui.add_enabled_ui(!dirty, |ui| tint_button(ui, MERGE_TOOL, theme::text_dim())).inner;
        let hint = "Resolve the file in the merge tool chosen in Preferences > External Tools.                     It is marked as resolved once the tool has saved it.";
        if merge_tool.on_hover_text(hint).on_disabled_hover_text("Save or reset your choices first").clicked() {
            actions.push(Action::MergeTool);
        }
    });
}

/// A labelled switch of the toolbar that stays pressed while `on`; true
/// when it was clicked.
fn switch(ui: &mut Ui, icon: &str, label: &str, on: bool, tip: &str) -> bool {
    let font = theme::font(12.0);
    let text = format!("{icon} {label}");
    let galley = ui.painter().layout_no_wrap(text, font, theme::text());
    let (rect, response) = ui.allocate_exact_size(vec2(galley.size().x + 16.0, 24.0), Sense::click());
    let response = response.on_hover_cursor(CursorIcon::PointingHand).on_hover_text(tip);
    let fill = if on {
        theme::bg_selected()
    } else if response.hovered() {
        theme::bg_raised()
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect(rect, 3.0, fill, Stroke::new(1.0, theme::border()), StrokeKind::Inside);
    let color = if on { theme::text_bright() } else { theme::text_dim() };
    let at = pos2(rect.left() + 8.0, rect.center().y - galley.size().y / 2.0);
    ui.painter().galley(at, galley, color);
    response.clicked()
}

/// Progress, navigation, what is shown, and the bulk choices.
fn toolbar(ui: &mut Ui, open: usize, total: usize, saving: bool, (ancestor, wrap): (bool, bool), actions: &mut Vec<Action>) {
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
        ui.add_space(10.0);
        let tip = if ancestor {
            "Hide the common ancestor's version of each conflict"
        } else {
            "Show what the common ancestor had in place of each conflict, between the two sides"
        };
        if switch(ui, icons::GIT_FORK, "Ancestor", ancestor, tip) {
            actions.push(Action::ShowAncestor(!ancestor));
        }
        let tip = if wrap {
            "Keep long lines on one row and scroll sideways (Preferences > Editor > Word Wrap)"
        } else {
            "Let long lines continue on the next row (Preferences > Editor > Word Wrap)"
        };
        if switch(ui, icons::TEXT_INDENT, "Wrap", wrap, tip) {
            actions.push(Action::Wrap(!wrap));
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

/// How a run of lines is painted.
#[derive(Clone, Copy)]
struct LineLook {
    /// Behind each row, right of the numbers.
    fill: Option<Color32>,
    /// Text without a syntax colour.
    plain: Color32,
    /// How far every colour fades towards the background (0: not at all).
    fade: f32,
}

impl LineLook {
    fn plain(color: Color32) -> Self {
        LineLook { fill: None, plain: color, fade: 0.0 }
    }
}

/// Paints lines `lines` of `rows` one under the other from `top` in `pane`:
/// numbered from `number`, coloured by `spans` (the styling of the hunk
/// `rows` are from), each wrapped line on as many rows as it takes, with the
/// part of `selection` (a selection in these very lines) behind the text.
#[allow(clippy::too_many_arguments)]
fn paint_lines(
    (painter, text_painter): (&Painter, &Painter),
    scene: &Scene<'_>,
    pane: Rect,
    top: f32,
    rows: Rows<'_>,
    spans: Option<&HunkView<'_>>,
    lines: Range<usize>,
    number: usize,
    look: LineLook,
    selection: Option<&Selection>,
) {
    let line_h = line_h();
    let clip = painter.clip_rect();
    let first = rows.start(lines.start);
    let shown = visible_rows(top, rows.count(lines.clone()), clip);
    if shown.is_empty() {
        return;
    }
    let text_x = pane.left() + scene.gutter + TEXT_PAD - scene.scroll.x;
    let mut line = rows.line_at(first + shown.start).max(lines.start);
    while line < lines.end {
        let y = top + (rows.start(line) - first) as f32 * line_h;
        if y >= clip.bottom() {
            break;
        }
        let text = &rows.lines[line].text;
        let colours = spans.map_or(&[][..], |view| view.spans(line));
        let parts = diff::segments(text, scene.texts.columns);
        // Only a wrapped line needs its length for the selection.
        let len = if parts.len() > 1 { parts.last().map_or(0, |(cols, _)| cols.end) } else { 0 };
        for (k, (cols, bytes)) in parts.into_iter().enumerate() {
            let row_top = y + k as f32 * line_h;
            if row_top + line_h <= clip.top() {
                continue;
            }
            if row_top >= clip.bottom() {
                break;
            }
            let row = row_rect(scene.gutter, pane, row_top);
            if let Some(fill) = look.fill {
                painter.rect_filled(row, 0.0, fill);
            }
            if k == 0 {
                draw_number(painter, scene.gutter, pane, row_top, number + line - lines.start);
            }
            let job = conflict_text::row_job(text, bytes, colours, &scene.font, look.plain, look.fade, line_h);
            let galley = text_painter.layout_job(job);
            if let Some(selection) = selection {
                if len == 0 {
                    selection.paint(text_painter, line, &galley, text_x, row, scene.char_w);
                } else {
                    selection.paint_part(text_painter, line, &galley, text_x, row, scene.char_w, cols, len);
                }
            }
            text_painter.galley(pos2(text_x, row_top), galley, look.plain);
        }
        line += 1;
    }
}

/// The shown column of `text` nearest to `x` (from where the text starts)
/// in row `row` of it when wrapped.
fn column_in(painter: &Painter, scene: &Scene<'_>, text: &str, row: usize, x: f32) -> usize {
    let parts = diff::segments(text, scene.texts.columns);
    let (cols, bytes) = parts[row.min(parts.len() - 1)].clone();
    let galley = painter.layout_no_wrap(text[bytes].to_owned(), scene.font.clone(), theme::text());
    cols.start + textsel::column_at(&galley, x)
}

/// Of lines `lines` of `rows`, the one row `row` (counted from the first of
/// them) is on, and the shown column nearest to `x` there. Past the last
/// row, the end of the last line.
fn line_and_column(
    painter: &Painter,
    scene: &Scene<'_>,
    rows: Rows<'_>,
    lines: Range<usize>,
    row: usize,
    x: f32,
) -> Option<(usize, usize)> {
    let last = lines.end.checked_sub(1).filter(|last| *last >= lines.start)?;
    let first = rows.start(lines.start);
    if row >= rows.count(lines.clone()) {
        return Some((last, rows.lines[last].text.chars().count()));
    }
    let line = rows.line_at(first + row).clamp(lines.start, last);
    let column = column_in(painter, scene, &rows.lines[line].text, first + row - rows.start(line), x);
    Some((line, column))
}

/// The place in `version`'s text nearest to the screen point `pos`, for
/// selecting there.
fn pane_pos(painter: &Painter, scene: &Scene<'_>, version: Version, pane: Rect, pos: Pos2) -> Pos {
    let rows = scene.texts.pane(version);
    let y = pos.y - pane.top() + scene.scroll.y;
    let Some(block) = scene.plan.nearest(y) else { return Pos::default() };
    let lines = scene.texts.block_lines(scene.merge, version, block);
    let body = block.top + head_h(block);
    let text_x = pane.left() + scene.gutter + TEXT_PAD - scene.scroll.x;
    let start = Pos::new(lines.start.min(rows.lines.len().saturating_sub(1)), 0);
    if y < body {
        return start;
    }
    let row = ((y - body) / line_h()) as usize;
    match line_and_column(painter, scene, rows, lines, row, pos.x - text_x) {
        Some((line, column)) => Pos::new(line, column),
        // A side with no lines here: where the next line begins.
        None => start,
    }
}

/// The strip above a side's version of a conflict: the box that takes it.
#[allow(clippy::too_many_arguments)]
fn take_strip(ui: &Ui, painter: &Painter, head: Rect, index: usize, side: Side, used: Used, lines: usize, actions: &mut Vec<Action>) {
    let take = match side {
        Side::Ours => "Take current",
        Side::Theirs => "Take incoming",
    };
    let color = side_color(side);
    let hit = ui.interact(head, ui.id().with(("take", index)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
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
    painter.text(pos2(head.left() + 36.0, cy), Align2::LEFT_CENTER, label, theme::font(12.5), label_color);
    painter.text(pos2(head.right() - 10.0, cy), Align2::RIGHT_CENTER, count_lines(lines), theme::font(11.5), theme::text_dim());
    if hit.clicked() {
        actions.push(Action::Toggle(index, side));
    }
}

/// The strip above the ancestor's version of a conflict.
fn ancestor_strip(painter: &Painter, head: Rect, lines: usize) {
    painter.rect_filled(head, 0.0, theme::mix(theme::bg_app(), ancestor_color(), 0.17));
    let cy = head.center().y;
    painter.text(pos2(head.left() + 12.0, cy), Align2::LEFT_CENTER, icons::GIT_FORK, theme::font(15.0), theme::text_dim());
    painter.text(pos2(head.left() + 36.0, cy), Align2::LEFT_CENTER, "Before either side", theme::font(12.5), theme::text());
    painter.text(pos2(head.right() - 10.0, cy), Align2::RIGHT_CENTER, count_lines(lines), theme::font(11.5), theme::text_dim());
}

fn count_lines(lines: usize) -> String {
    match lines {
        0 => "no lines".to_owned(),
        1 => "1 line".to_owned(),
        n => format!("{n} lines"),
    }
}

/// A pane with a text of its own: one side of the conflict with a box to
/// take its version of each conflict, or the ancestor. Its text can be
/// selected and copied. Returns how far a selection dragged past the edge
/// scrolls the view.
#[allow(clippy::too_many_arguments)]
fn version_pane(
    ui: &mut Ui,
    scene: &Scene<'_>,
    rect: Rect,
    version: Version,
    selection: &mut Selection,
    fingerprint: u64,
    actions: &mut Vec<Action>,
) -> Vec2 {
    let (salt, side) = match version {
        Version::Ours => ("conflict-ours", Some(Side::Ours)),
        Version::Theirs => ("conflict-theirs", Some(Side::Theirs)),
        Version::Base => ("conflict-base", None),
    };
    let color = side.map_or(ancestor_color(), side_color);
    let ui = &mut pane_ui(ui, rect, salt);
    let (painter, text_painter) = pane_backdrop(ui, rect, scene.gutter);
    let painters = (&painter, &text_painter);
    let pane = &scene.texts.panes[version as usize];
    let rows = scene.texts.pane(version);
    let style = scene.styles[version as usize].hunk(0);
    let line_h = line_h();

    // Selecting first: the strips and line numbers on top take their own clicks.
    let text_rect = Rect::from_min_max(pos2(rect.left() + scene.gutter, rect.top()), rect.max);
    selection.follow(&pane.diff, ui.ctx().cumulative_frame_nr(), || fingerprint);
    let mut scrolled = Vec2::ZERO;
    let hit = |pos: Pos2| pane_pos(&painter, scene, version, rect, pos);
    let response = selection.interact_scrolling(ui, ui.id().with("text"), text_rect, rect, pane, hit, |by| scrolled += by);
    response.context_menu(|ui| selection.menu(ui, pane));
    let selection = &*selection;

    for block in scene.plan.visible(scene.scroll.y, rect.height()) {
        let top = rect.top() + block.top - scene.scroll.y;
        let lines = scene.texts.block_lines(scene.merge, version, block);
        let number = block.above[version as usize] + 1;
        let used_rows = rows.count(lines.clone());
        match block.kind {
            Kind::Stable { .. } => {
                let look = LineLook::plain(theme::text());
                paint_lines(painters, scene, rect, top, rows, style.as_ref(), lines, number, look, Some(selection));
            }
            Kind::Edited { .. } => {
                // The panes keep showing the line as it was merged.
                let look = LineLook::plain(theme::text());
                paint_lines(painters, scene, rect, top, rows, style.as_ref(), lines, number, look, Some(selection));
                for row in visible_rows(top, block.rows, painter.clip_rect()).filter(|row| *row >= used_rows) {
                    painter.rect_filled(row_rect(scene.gutter, rect, top + row as f32 * line_h), 0.0, theme::bg_void());
                }
            }
            Kind::Conflict { index } => {
                let choice = scene.draft.choices.get(index).unwrap_or(&Resolution::Unresolved);
                let used = side.map_or(Used::No, |side| used(choice, side));
                let open = *choice == Resolution::Unresolved;
                let head = Rect::from_min_max(pos2(rect.left(), top), pos2(rect.right(), top + HEAD_H));
                match side {
                    Some(side) => take_strip(ui, &painter, head, index, side, used, lines.len(), actions),
                    None => ancestor_strip(&painter, head, lines.len()),
                }

                // This version's lines. A click on their numbers takes the side.
                let body_top = top + HEAD_H;
                let numbers = Rect::from_min_max(
                    pos2(rect.left(), body_top),
                    pos2(rect.left() + scene.gutter, body_top + used_rows as f32 * line_h),
                );
                if let Some(side) = side
                    && !lines.is_empty()
                {
                    let hint = match side {
                        Side::Ours => "Take the current side's lines",
                        Side::Theirs => "Take the incoming side's lines",
                    };
                    let hit = ui
                        .interact(numbers, ui.id().with(("take-lines", index)), Sense::click())
                        .on_hover_cursor(CursorIcon::PointingHand)
                        .on_hover_text(hint);
                    if hit.hovered() {
                        painter.rect_filled(numbers, 0.0, theme::mix(theme::bg_panel(), color, 0.3));
                    }
                    if hit.clicked() {
                        actions.push(Action::Toggle(index, side));
                    }
                }
                let (tint, plain, fade) = match (side, used != Used::No, open) {
                    (None, _, _) => (0.08, theme::text(), 0.0),
                    (Some(_), true, _) => (0.22, theme::text_bright(), 0.0),
                    (Some(_), false, true) => (0.11, theme::text(), 0.0),
                    // Resolved without this side.
                    (Some(_), false, false) => (0.04, theme::text(), 0.45),
                };
                let look = LineLook { fill: Some(theme::mix(theme::bg_app(), color, tint)), plain, fade };
                paint_lines(painters, scene, rect, body_top, rows, style.as_ref(), lines.clone(), number, look, Some(selection));
                for row in visible_rows(body_top, block.rows, painter.clip_rect()).filter(|row| *row >= used_rows) {
                    let y = body_top + row as f32 * line_h;
                    painter.rect_filled(row_rect(scene.gutter, rect, y), 0.0, theme::bg_void());
                    if lines.is_empty() && row == 0 {
                        let remark = if side.is_some() { "no lines on this side" } else { "the ancestor had no lines here" };
                        draw_remark(&painter, scene.gutter, rect, y, remark, theme::text_faint());
                    }
                }
                if used != Used::No && !lines.is_empty() {
                    let bar = Rect::from_min_size(pos2(rect.left() + scene.gutter, body_top), vec2(3.0, used_rows as f32 * line_h));
                    painter.rect_filled(bar, 0.0, color);
                }
                if index == scene.current {
                    outline_current(&painter, rect, block, top);
                }
            }
        }
    }
    scrolled
}

/// The ancestor's pane when there is no ancestor.
fn no_ancestor(ui: &Ui, rect: Rect) {
    let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
    painter.rect_filled(rect, 0.0, theme::bg_app());
    let center = rect.center();
    painter.text(center - vec2(0.0, 34.0), Align2::CENTER_CENTER, icons::GIT_FORK, theme::font(28.0), theme::text_faint());
    painter.text(center - vec2(0.0, 8.0), Align2::CENTER_CENTER, "No common ancestor", theme::bold(13.5), theme::text_dim());
    let text = "Both sides added this file, so there is no earlier version to compare them with.";
    let galley = painter.layout(text.to_owned(), theme::font(12.0), theme::text_faint(), (rect.width() - 32.0).max(40.0));
    let at = pos2(center.x - galley.size().x / 2.0, center.y + 6.0);
    painter.galley(at, galley, theme::text_faint());
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

/// The line (counted in the editor's text) and character to put the text
/// cursor at for a click `row` rows into the result of a conflict, `x`
/// points into the text.
fn conflict_place(painter: &Painter, scene: &Scene<'_>, index: usize, row: usize, x: f32) -> (usize, usize) {
    let choice = scene.draft.choices.get(index).unwrap_or(&Resolution::Unresolved);
    if let Resolution::Custom(_) = choice {
        return typed_place(painter, scene, Target::Conflict(index), row, x);
    }
    let Some(&seg) = scene.merge.conflicts.get(index) else { return (0, 0) };
    let (mut above_rows, mut above_lines) = (0, 0);
    for (side, range) in scene.texts.taken(choice, seg) {
        let version = version_of(side);
        let rows = scene.texts.pane(version);
        let count = rows.count(range.clone());
        if row < above_rows + count {
            let Some((line, column)) = line_and_column(painter, scene, rows, range.clone(), row - above_rows, x) else { break };
            let pane = &scene.texts.panes[version as usize];
            let source = textsel::Lines::source(pane, line).unwrap_or(&rows.lines[line].text);
            return (above_lines + line - range.start, conflict_text::source_char(source, column, scene.tab_width));
        }
        above_rows += count;
        above_lines += range.len();
    }
    (0, 0)
}

/// [`conflict_place`] for text typed by hand.
fn typed_place(painter: &Painter, scene: &Scene<'_>, target: Target, row: usize, x: f32) -> (usize, usize) {
    let Some((hunk, rows)) = scene.texts.typed(target) else { return (0, 0) };
    let Some((line, column)) = line_and_column(painter, scene, rows, 0..rows.lines.len(), row, x) else { return (0, 0) };
    let source = scene.texts.typed.pieces[hunk].1.split('\n').nth(line).unwrap_or_default();
    (line, conflict_text::source_char(source, column, scene.tab_width))
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
    let painters = (&painter, &text_painter);
    let text_x = rect.left() + scene.gutter + TEXT_PAD - scene.scroll.x;
    let total = scene.draft.choices.len();
    let line_h = line_h();
    let ours = scene.texts.pane(Version::Ours);
    let side_styles = [scene.styles[0].hunk(0), scene.styles[1].hunk(0)];
    let side_style = |side: Side| side_styles[version_of(side) as usize].as_ref();
    let mut cursor = None;

    for block in scene.plan.visible(scene.scroll.y, rect.height()) {
        let top = rect.top() + block.top - scene.scroll.y;
        let number = block.above[RESULT] + 1;
        match block.kind {
            Kind::Stable { .. } => {
                let lines = scene.texts.block_lines(scene.merge, Version::Ours, block);
                let look = LineLook::plain(theme::text());
                paint_lines(painters, scene, rect, top, ours, side_style(Side::Ours), lines, number, look, None);
            }
            Kind::Edited { seg, line } => {
                let target = Target::Line { seg, line };
                let area = Rect::from_min_max(
                    pos2(rect.left() + scene.gutter, top),
                    pos2(rect.right(), top + block.rows as f32 * line_h),
                );
                painter.rect_filled(area, 0.0, theme::mix(theme::bg_app(), typed(), 0.13));
                let piece = scene.texts.typed(target);
                if let Some(open) = editing.as_mut().filter(|e| e.target == target) {
                    let out = editor(ui, scene, rect, area, text_x, text_w, open, piece.map(|(hunk, _)| hunk), actions);
                    for (k, y) in out.line_tops.iter().enumerate() {
                        draw_number(&painter, scene.gutter, rect, *y, number + k);
                    }
                    cursor = out.cursor;
                } else {
                    match piece {
                        Some((hunk, rows)) if !rows.lines.is_empty() => {
                            let view = scene.styles[TYPED].hunk(hunk);
                            let look = LineLook::plain(theme::text_bright());
                            paint_lines(painters, scene, rect, top, rows, view.as_ref(), 0..rows.lines.len(), number, look, None);
                        }
                        _ => draw_remark(&painter, scene.gutter, rect, top, "line removed", theme::text_dim()),
                    }
                    let size = tint_button_size(ui, "Restore");
                    let corner = pos2(rect.right() - size.x - 8.0, top + (line_h - size.y) / 2.0);
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
                let Some(&seg) = scene.merge.conflicts.get(index) else { continue };
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
                    pos2(rect.right(), body_top + block.rows as f32 * line_h),
                );
                let piece = scene.texts.typed(target);
                if let Some(open) = editing.as_mut().filter(|e| e.target == target) {
                    painter.rect_filled(area, 0.0, theme::mix(theme::bg_app(), typed(), 0.13));
                    let out = editor(ui, scene, rect, area, text_x, text_w, open, piece.map(|(hunk, _)| hunk), actions);
                    for (k, y) in out.line_tops.iter().enumerate() {
                        draw_number(&painter, scene.gutter, rect, *y, number + k);
                    }
                    cursor = out.cursor;
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
                    // Each part of the result in the colour of where it comes
                    // from. (The sides' styling is already held: a second
                    // read lock on it could wait behind the highlighter.)
                    let custom = matches!(choice, Resolution::Custom(_));
                    let typed_view = piece.filter(|_| custom).and_then(|(hunk, _)| scene.styles[TYPED].hunk(hunk));
                    let mut parts: Vec<(Rows<'_>, Option<&HunkView<'_>>, Range<usize>, Color32)> = Vec::new();
                    match piece.filter(|_| custom) {
                        Some((_, rows)) => parts.push((rows, typed_view.as_ref(), 0..rows.lines.len(), typed())),
                        None => {
                            for (side, range) in scene.texts.taken(choice, seg) {
                                parts.push((scene.texts.pane(version_of(side)), side_style(side), range, side_color(side)));
                            }
                        }
                    }
                    let (mut row, mut line) = (0, 0);
                    for (rows, view, range, color) in &parts {
                        let part_top = body_top + row as f32 * line_h;
                        let fill = Some(theme::mix(theme::bg_app(), *color, 0.15));
                        let look = LineLook { fill, plain: theme::text_bright(), fade: 0.0 };
                        paint_lines(painters, scene, rect, part_top, *rows, *view, range.clone(), number + line, look, None);
                        let count = rows.count(range.clone());
                        let bar = Rect::from_min_size(pos2(rect.left() + scene.gutter, part_top), vec2(3.0, count as f32 * line_h));
                        painter.rect_filled(bar, 0.0, *color);
                        row += count;
                        line += range.len();
                    }
                    for filler in visible_rows(body_top, block.rows, painter.clip_rect()).filter(|r| *r >= row) {
                        let y = body_top + filler as f32 * line_h;
                        painter.rect_filled(row_rect(scene.gutter, rect, y), 0.0, theme::bg_void());
                        if line == 0 && filler == 0 {
                            draw_remark(&painter, scene.gutter, rect, y, "no lines here in the result", theme::text_faint());
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
        let x = pos.x - text_x;
        if let Some(block) = scene.plan.at(y) {
            let row = ((y - block.top - head_h(block)) / line_h).max(0.0) as usize;
            match block.kind {
                Kind::Stable { seg, from, .. } => {
                    let lines = scene.texts.block_lines(scene.merge, Version::Ours, block);
                    if let Some((line, column)) = line_and_column(&painter, scene, ours, lines.clone(), row, x) {
                        let pane = &scene.texts.panes[Version::Ours as usize];
                        let source = textsel::Lines::source(pane, line).unwrap_or(&ours.lines[line].text);
                        let column = conflict_text::source_char(source, column, scene.tab_width);
                        let target = Target::Line { seg, line: from + line - lines.start };
                        actions.push(Action::Edit(target, (0, column)));
                    }
                }
                Kind::Edited { seg, line } => {
                    let target = Target::Line { seg, line };
                    actions.push(Action::Edit(target, typed_place(&painter, scene, target, row, x)));
                }
                Kind::Conflict { index } if y >= block.top + HEAD_H => {
                    actions.push(Action::Edit(Target::Conflict(index), conflict_place(&painter, scene, index, row, x)));
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

/// What the editor did this frame.
struct EditorOut {
    /// Where the text cursor is, when it moved.
    cursor: Option<Rect>,
    /// Where each line of the text starts on screen, for its number.
    line_tops: Vec<f32>,
}

/// The text editor, placed over the rows it edits. Rows have the height,
/// font, tab stops, colours and wrapping of the painted ones, so the text
/// does not move when it opens. `piece` is the hunk of the typed text that
/// was made from this editor's text, which the colours come from.
#[allow(clippy::too_many_arguments)]
fn editor(
    ui: &mut Ui,
    scene: &Scene<'_>,
    pane: Rect,
    area: Rect,
    text_x: f32,
    text_w: f32,
    editing: &mut Editing,
    piece: Option<usize>,
    actions: &mut Vec<Action>,
) -> EditorOut {
    let id = ui.id().with("editor");
    if let Some((row, column)) = editing.place.take() {
        let at = CCursor::new(char_index(&editing.text, row, column));
        let mut state = TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
        state.cursor.set_char_range(Some(CCursorRange::one(at)));
        TextEdit::store_state(ui.ctx(), id, state);
        ui.memory_mut(|memory| memory.request_focus(id));
    }

    // Colours of each line, from the highlighter once it has been through
    // the text; a line typed since keeps the colours it had.
    let tab_width = scene.tab_width;
    let view = piece.and_then(|hunk| scene.styles[TYPED].hunk(hunk));
    let known: Vec<&str> = piece.map_or_else(Vec::new, |hunk| scene.texts.typed.pieces[hunk].1.split('\n').collect());
    let spans: Vec<Vec<Span>> = editing
        .text
        .split('\n')
        .enumerate()
        .map(|(i, line)| {
            let fresh = view.as_ref().filter(|_| known.get(i) == Some(&line)).map(|view| view.spans(i));
            match fresh {
                Some(spans) if !spans.is_empty() => conflict_text::source_spans(line, spans, tab_width),
                _ => editing.spans.get(i).cloned().unwrap_or_default(),
            }
        })
        .collect();
    drop(view);

    // Wrapped, as wide as the panes wrap; else as wide as the longest line,
    // so typing does not hit a wrap, and the pane clips it and the shared
    // horizontal scroll moves it. The painted rows keep the space after a
    // word that ends at the wrap width, where egui would carry the word over:
    // one column more makes egui break where they do.
    let width = match scene.texts.columns {
        Some(columns) => (columns + 1) as f32 * scene.space_w + 0.5,
        None => text_w.max(editing.text.len() as f32 * scene.char_w + 40.0).max(area.right() - text_x),
    };
    let wide = Rect::from_min_size(pos2(text_x, area.top()), vec2(width, area.height()));
    let builder = UiBuilder::new().id_salt("editor-ui").max_rect(wide).layout(Layout::top_down(Align::Min));
    let mut child = ui.new_child(builder);
    let text_clip = Rect::from_min_max(pos2(pane.left() + scene.gutter, pane.top()), pane.max);
    child.set_clip_rect(text_clip.intersect(ui.clip_rect()));

    let look = EditorLook {
        font: scene.font.clone(),
        line_h: line_h(),
        color: theme::text_bright(),
        space_w: scene.space_w,
        tab_w: scene.tab_w,
        tab_width,
        wrap: scene.texts.columns.map(|_| width),
    };
    let mut layouter = |ui: &Ui, buffer: &dyn TextBuffer, _wrap: f32| {
        ui.painter().layout_job(conflict_text::editor_job(buffer.as_str(), &look, &spans))
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
    editing.spans = spans;

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

    // Wrapped rows the plan should make room for.
    let rows = output.galley.rows.len().max(1);
    if editing.rows != Some(rows) {
        editing.rows = Some(rows);
        actions.push(Action::Relayout);
    }
    let mut line_tops = Vec::new();
    let mut starts_line = true;
    for placed in &output.galley.rows {
        if starts_line {
            line_tops.push(output.galley_pos.y + placed.pos.y);
        }
        starts_line = placed.ends_with_newline;
    }

    let at = output.cursor_range.map(|range| range.primary);
    let moved = at.map(|c| c.index.0) != editing.cursor;
    editing.cursor = at.map(|c| c.index.0);
    let cursor = match at {
        Some(at) if moved => Some(output.galley.pos_from_cursor(at).translate(output.galley_pos.to_vec2())),
        _ => None,
    };
    EditorOut { cursor, line_tops }
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
fn whole_ui(ui: &mut Ui, tab: &mut RepoTab, shared: &Shared, view: &mut Resolver, file: &ConflictFile, why: Whole) {
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
        // A merge tool can take what is too large or binary for this view.
        let tools = &shared.settings.tools;
        if matches!(why, Whole::Binary | Whole::TooLarge) {
            let merging = tab.is_running(&crate::external::merge_label(tools, &file.path));
            let button = wide(format!("{}  Open in merge tool", icons::GIT_MERGE), theme::text_dim());
            if ui.add_enabled(!merging, button).on_disabled_hover_text("Waiting for the merge tool…").clicked()
                && let Some(cmd) = crate::external::merge(tools, &file.path)
            {
                tab.run(cmd);
            }
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::git::conflict::merge_text;

    fn near(found: Option<f32>, expected: f32) -> bool {
        found.is_some_and(|y| (y - expected).abs() < 0.01)
    }

    const LONG: &str = "word word word word word word word word word word word word word word word word word word word word";
    const THEIRS_LONG: &str = "theirs line which is long enough to wrap at twenty columns";

    /// One conflict between two stretches of merged lines: one line on our
    /// side, two on theirs, five in the ancestor.
    fn merge() -> TextMerge {
        let base = format!("start\n{LONG}\nb1\nb2\nb3\nb4\nb5\nend\n");
        let ours = format!("start\n{LONG}\nours line\nend\n");
        let theirs = format!("start\n{LONG}\n{THEIRS_LONG}\nmore theirs\nend\n");
        let merge = merge_text(base.as_bytes(), ours.as_bytes(), theirs.as_bytes()).unwrap();
        assert_eq!(merge.conflicts.len(), 1);
        merge
    }

    fn panes(merge: &TextMerge) -> [PaneText; 3] {
        [Version::Ours, Version::Theirs, Version::Base].map(|version| PaneText::new(merge, version, 4))
    }

    fn wrapped(panes: &[PaneText; 3], columns: usize) -> [Arc<Vec<u32>>; 3] {
        panes.each_ref().map(|pane| Arc::new(diff::first_rows(pane.lines().iter().map(|l| l.text.as_str()), columns)))
    }

    fn rows(plan: &Plan) -> Vec<usize> {
        plan.blocks.iter().map(|block| block.rows).collect()
    }

    #[test]
    fn the_panes_stay_level_block_by_block() {
        let merge = merge();
        let panes = panes(&merge);
        let typed = Typed::default();
        let open = Draft::unresolved(&merge);

        // A row per line; a conflict as tall as its tallest version, the
        // ancestor's counting only while it is shown.
        let plain = Texts { panes: &panes, wrapped: None, typed: &typed, columns: None };
        assert_eq!(rows(&Plan::build(&merge, &open, None, &plain, false)), [2, 2, 1]);
        let with_base = Plan::build(&merge, &open, None, &plain, true);
        assert_eq!(rows(&with_base), [2, 5, 1]);
        assert_eq!(with_base.blocks[2].above, [3, 4, 7, 2]);

        // Wrapped, every pane at the same width, so the same line takes the
        // same rows in each.
        let columns = 20;
        let wrap = wrapped(&panes, columns);
        let long_rows = text::wrapped_rows(LONG, columns);
        let theirs_rows = text::wrapped_rows(THEIRS_LONG, columns) + 1;
        assert!(long_rows > 1 && theirs_rows > 2);
        let texts = Texts { panes: &panes, wrapped: Some(&wrap), typed: &typed, columns: Some(columns) };
        let plan = Plan::build(&merge, &open, None, &texts, false);
        assert_eq!(rows(&plan), [1 + long_rows, theirs_rows, 1]);
        assert_eq!(plan.blocks[1].top, (1 + long_rows) as f32 * line_h());
        assert_eq!(plan.height, plan.blocks[2].bottom);

        // Both sides taken: the result holds both, one after the other.
        let both = Draft { choices: vec![Resolution::OursTheirs], edits: BTreeMap::new() };
        let plan = Plan::build(&merge, &both, None, &texts, true);
        assert_eq!(rows(&plan), [1 + long_rows, (1 + theirs_rows).max(5), 1]);
        assert_eq!(plan.blocks[2].above[RESULT], 2 + 3);
        assert_eq!(texts.resolved(&Resolution::OursTheirs, 0, 1), (3, 1 + theirs_rows));
        assert_eq!(texts.resolved(&Resolution::Neither, 0, 1), (0, 0));
    }

    #[test]
    fn typed_text_has_rows_of_its_own() {
        let merge = merge();
        let panes = panes(&merge);
        let mut draft = Draft::unresolved(&merge);
        draft.choices[0] = Resolution::Custom(format!("typed\n{THEIRS_LONG}"));
        draft.edits.insert((0, 0), String::new());
        let mut typed = Typed::default();
        typed.update(&draft, None, Some(20));
        let texts = Texts { panes: &panes, wrapped: None, typed: &typed, columns: Some(20) };

        let (_, conflict) = texts.typed(Target::Conflict(0)).expect("the typed resolution");
        assert_eq!(conflict.lines.len(), 2);
        assert_eq!(conflict.all(), 1 + text::wrapped_rows(THEIRS_LONG, 20));
        // A line typed away has no lines, and its block keeps a row.
        let (_, removed) = texts.typed(Target::Line { seg: 0, line: 0 }).expect("the edited line");
        assert!(removed.lines.is_empty());
        let plan = Plan::build(&merge, &draft, None, &texts, false);
        assert!(matches!(plan.blocks[0].kind, Kind::Edited { seg: 0, line: 0 }));
        assert_eq!(plan.blocks[0].rows, 1);
        assert_eq!(plan.blocks[1].above[RESULT], 0);

        // The same pieces again make nothing new, so their colours stay.
        let before = Arc::as_ptr(&typed.diff);
        typed.update(&draft, None, Some(20));
        assert_eq!(Arc::as_ptr(&typed.diff), before);
        typed.update(&draft, None, None);
        assert_eq!(Arc::as_ptr(&typed.diff), before);
        assert!(typed.rows.is_empty());
    }

    #[test]
    fn the_line_at_the_top_stays_when_lines_wrap() {
        let merge = merge();
        let panes = panes(&merge);
        let typed = Typed::default();
        let open = Draft::unresolved(&merge);
        let plain = Texts { panes: &panes, wrapped: None, typed: &typed, columns: None };
        let before = Plan::build(&merge, &open, None, &plain, false);
        let wrap = wrapped(&panes, 20);
        let texts = Texts { panes: &panes, wrapped: Some(&wrap), typed: &typed, columns: Some(20) };
        let after = Plan::build(&merge, &open, None, &texts, false);
        let h = line_h();

        // Half-way down the long line, which becomes many rows.
        let anchor = Anchor::at(&before, &plain, &merge, 1.5 * h, h).unwrap();
        assert_eq!(anchor, Anchor { block: 0, line: Some(1), offset: 0.5 });
        assert_eq!(anchor.find(&after, &texts, &merge), Some(1.5 * h));
        // The last line comes after all the rows the long one took.
        let last = Anchor::at(&before, &plain, &merge, before.blocks[2].top + 2.0, h).unwrap();
        assert!(near(last.find(&after, &texts, &merge), after.blocks[2].top + 2.0));
        // In a conflict, the same distance below its top, as far as it goes.
        let inside = Anchor::at(&before, &plain, &merge, before.blocks[1].top + HEAD_H, h).unwrap();
        assert_eq!(inside.line, None);
        assert!(near(inside.find(&after, &texts, &merge), after.blocks[1].top + HEAD_H));
    }

    #[test]
    fn rows_find_their_lines() {
        let lines: Vec<DiffLine> =
            ["a", "b", "c"].iter().map(|t| DiffLine { origin: ' ', old_no: None, new_no: None, text: (*t).to_owned() }).collect();
        let first = [0u32, 1, 4, 5];
        let wrapped = Rows { lines: &lines, first: Some(&first) };
        assert_eq!((wrapped.start(1), wrapped.start(3), wrapped.all()), (1, 5, 5));
        assert_eq!((0..5).map(|row| wrapped.line_at(row)).collect::<Vec<_>>(), [0, 1, 1, 1, 2]);
        assert_eq!(wrapped.count(1..2), 3);
        let flat = Rows { lines: &lines, first: None };
        assert_eq!((flat.start(2), flat.all(), flat.line_at(2)), (2, 3, 2));
    }
}
