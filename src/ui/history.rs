//! History and blame of a single file.
//!
//! One overlay, two modes that share the file being looked at:
//!
//! * **History** lists the commits that changed the file (following renames)
//!   and shows what the selected one did to it.
//! * **Blame** shows the file with the commit each run of lines comes from.
//!
//! Both look at the file as of HEAD by default. "Blame before this commit"
//! and "Blame at this commit" move the view to an older commit (and possibly
//! an older name of the file); Back returns.
//!
//! History pages and commit diffs are read by jobs on the reader thread, in
//! slices short enough not to hold it up. Blame runs `git blame` on a thread
//! of its own: it can take many seconds on a long history, nothing else
//! should wait behind it, and it has to be stoppable when the view closes.

use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Align2, Color32, CursorIcon, Key, Layout, Painter, Pos2, Rect, ScrollArea, Sense, Shape,
    Stroke, Ui, UiBuilder, pos2, vec2,
};

use crate::git::blame::{Blame, BlameCommit, BlameError, Cancel, FileCommit, HistoryCursor};
use crate::git::{ChangeKind, DiffSource, DiffTarget, FileChange, FileDiff, Oid, RepoSnapshot, short};
use crate::state::{RepoTab, Selection, Shared, ToastKind, format_time, relative_time};
use crate::theme::{self, icons};
use crate::ui::textsel::{self, Lines, Pos};
use crate::ui::widgets::{self, elided, galley_left, menu_item, tint_button};

/// Commits asked for at a time; more are fetched as the list is scrolled.
const PAGE: usize = 100;
/// The longest one page request may keep the reader thread. A request that
/// runs out of time returns what it has and the next one carries on.
const PAGE_BUDGET: Duration = Duration::from_millis(60);
/// While the next commit's diff loads, the previous one stays up this long
/// before giving way to a spinner, so stepping through the list does not flicker.
const DIFF_GRACE: Duration = Duration::from_millis(200);

const PAGE_KEY: &str = "history:page";
const DIFF_KEY: &str = "history:diff";

const ROW_H: f32 = 46.0;
const STRIP_H: f32 = 30.0;
const HEADER_H: f32 = 54.0;
const LINE_H: f32 = 19.0;
/// Width of the commit column beside the blamed lines, when there is room.
const ANNOT_W: f32 = 304.0;
/// A history narrower than this puts the list above the diff, not beside it.
const STACK_BELOW: f32 = 640.0;
/// Bars narrower than this show their buttons as icons only: the one above
/// the diff (two buttons) and the one above the blame (two wider ones).
const COMPACT_DIFF_BELOW: f32 = 500.0;
const COMPACT_BLAME_BELOW: f32 = 620.0;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    History,
    Blame,
}

pub struct HistoryView {
    /// Repository-relative path of the file.
    pub path: String,
    pub mode: Mode,
    /// The commit whose version of the file is looked at; `None` is HEAD,
    /// together with the working tree for blame.
    at: Option<Oid>,
    /// Where Back returns to: earlier `(at, path)` pairs, oldest first.
    back: Vec<(Option<Oid>, String)>,
    // Boxed so the view stays small inside `Overlay`, whatever these grow to.
    history: Box<HistoryState>,
    blame: Box<BlameState>,
    /// A commit to show in the graph as soon as the graph has loaded it.
    jump: Option<Oid>,
    list_width: f32,
}

impl HistoryView {
    pub fn history(path: &str) -> Self {
        Self::new(path, Mode::History)
    }

    pub fn blame(path: &str) -> Self {
        Self::new(path, Mode::Blame)
    }

    fn new(path: &str, mode: Mode) -> Self {
        HistoryView {
            path: path.to_owned(),
            mode,
            at: None,
            back: Vec::new(),
            history: Box::default(),
            blame: Box::default(),
            jump: None,
            list_width: 430.0,
        }
    }

    /// Looks at another version of the file, remembering the current one.
    fn go_to(&mut self, at: Option<Oid>, path: String) {
        self.back.push((self.at, std::mem::replace(&mut self.path, path)));
        self.at = at;
        self.reset();
    }

    fn go_back(&mut self) {
        if let Some((at, path)) = self.back.pop() {
            self.at = at;
            self.path = path;
            self.reset();
        }
    }

    fn reset(&mut self) {
        *self.history = HistoryState::default();
        // Dropping the old state stops a blame that is still running.
        *self.blame = BlameState::default();
    }
}

/// Job results are matched to requests by token, because a result can arrive
/// after the view moved on, or after another view took this one's place.
fn next_token() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// What the user asked for during a frame, applied once painting is done.
enum Action {
    Select(usize),
    ShowInGraph(Oid),
    /// Blame this commit's version of the file, under this name.
    BlameAt(Oid, String),
    SelectBlamed(Option<Oid>),
    OpenWorkingTreeDiff,
}

/// Returns false when the overlay should close.
pub fn show(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, view: &mut HistoryView) -> bool {
    ui.spacing_mut().item_spacing.y = 0.0;
    if title_bar(ui, view) {
        return false;
    }
    // Without a working repository there is nobody to ask.
    if let Some(e) = &tab.error {
        widgets::placeholder(ui, e);
        return true;
    }
    let Some(snap) = tab.snap.clone() else {
        widgets::placeholder(ui, "Loading repository…");
        return true;
    };
    if let Some(oid) = view.jump {
        // Still here, so the graph has not loaded that commit yet.
        let strip = strip(ui, theme::mix(theme::bg_header(), theme::accent(), 0.12));
        widgets::spinner(ui.painter(), pos2(strip.left() + 18.0, strip.center().y), 6.0, theme::accent());
        let text = format!("Loading older commits into the graph to reach {}…", short(oid));
        widgets::text_left(ui.painter(), strip, strip.left() + 34.0, &text, theme::font(12.0), theme::text());
        ui.ctx().request_repaint();
    }

    let mut action = None;
    match view.mode {
        Mode::History => history_mode(ui, tab, shared, &snap, view, &mut action),
        Mode::Blame => blame_mode(ui, &snap, tab, view, &mut action),
    }

    match action {
        Some(Action::Select(index)) => view.select(tab, index),
        Some(Action::ShowInGraph(oid)) => view.jump = Some(oid),
        Some(Action::BlameAt(oid, path)) => {
            view.go_to(Some(oid), path);
            view.mode = Mode::Blame;
        }
        Some(Action::SelectBlamed(oid)) => view.blame.selected = oid,
        Some(Action::OpenWorkingTreeDiff) => {
            if let Some((source, file)) = working_change(&snap, &view.path) {
                tab.select(Selection::Wip);
                tab.open_diff(DiffTarget::new(source, file));
                return false;
            }
        }
        None => {}
    }
    // Last, so the view is complete on the frame it closes.
    !finish_jump(tab, shared, &snap, view)
}

/// The title bar with the mode toggle. Returns true when close was clicked.
fn title_bar(ui: &mut Ui, view: &mut HistoryView) -> bool {
    let (icon, title) = match view.mode {
        Mode::History => (icons::CLOCK_COUNTER_CLOCKWISE, "File history"),
        Mode::Blame => (icons::USER_LIST, "Blame"),
    };
    let subtitle = match view.at {
        Some(oid) => format!("{}   as of commit {}", view.path, short(oid)),
        None => view.path.clone(),
    };
    let (close, free) = widgets::overlay_bar(ui, icon, title, &subtitle);

    // The controls keep their place at the right even when a long path runs
    // into them: they are laid out against the close button and painted over
    // a patch of bar that hides whatever text is underneath.
    let area = Rect::from_min_max(
        pos2(free.right() - 400.0, free.top() + 1.0),
        pos2(free.right(), free.bottom() - 1.0),
    );
    let patch = ui.painter().add(Shape::Noop);
    let mut blame = view.mode == Mode::Blame;
    let mut back = false;
    let used = ui
        .scope_builder(
            UiBuilder::new().max_rect(area).layout(Layout::right_to_left(Align::Center)),
            |ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                widgets::segmented(
                    ui,
                    &mut blame,
                    (icons::CLOCK_COUNTER_CLOCKWISE, "History"),
                    (icons::USER_LIST, "Blame"),
                );
                if !view.back.is_empty() {
                    let label = format!("{}  Back", icons::ARROW_LEFT);
                    back = tint_button(ui, &label, theme::accent())
                        .on_hover_text("Return to the version of the file shown before")
                        .clicked();
                }
                ui.min_rect()
            },
        )
        .inner;
    let cover = Rect::from_min_max(pos2(used.left() - 12.0, area.top()), area.max);
    ui.painter().set(patch, Shape::rect_filled(cover, 0.0, theme::bg_header()));

    view.mode = if blame { Mode::Blame } else { Mode::History };
    if back {
        view.go_back();
    }
    close
}

/// Carries out a pending "Show in graph". Returns true once the graph is on
/// the commit and the overlay should close.
///
/// The graph loads history in pages, so an old commit may not be in it yet:
/// more pages are requested until it turns up.
fn finish_jump(tab: &mut RepoTab, shared: &mut Shared, snap: &RepoSnapshot, view: &mut HistoryView) -> bool {
    let Some(oid) = view.jump else { return false };
    if snap.row_of(oid).is_some() {
        view.jump = None;
        tab.jump_to(oid);
        return true;
    }
    if !snap.history.has_more {
        view.jump = None;
        shared.toast(
            ToastKind::Error,
            "Cannot show the commit in the graph",
            format!("{} is not reachable from any branch or tag the graph shows.", short(oid)),
        );
        return false;
    }
    // A no-op while a bigger page is already on its way.
    tab.load_more();
    false
}

/// The file's uncommitted change, if it has one (unstaged first).
fn working_change<'a>(snap: &'a RepoSnapshot, path: &str) -> Option<(DiffSource, &'a FileChange)> {
    let find = |files: &'a [FileChange]| files.iter().find(|file| file.path == path);
    find(&snap.status.unstaged)
        .map(|file| (DiffSource::Unstaged, file))
        .or_else(|| find(&snap.status.staged).map(|file| (DiffSource::Staged, file)))
}

/// The nearest position that falls on a physical pixel.
fn to_pixel(ui: &Ui, x: f32) -> f32 {
    let scale = ui.pixels_per_point();
    (x * scale).round() / scale
}

/// A full-width band for a one-line notice. Returns its rectangle.
fn strip(ui: &mut Ui, fill: Color32) -> Rect {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), STRIP_H), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, fill);
    ui.painter().hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::border()));
    rect
}

/// A one-line notice with a leading icon, cut off if it does not fit.
fn notice(ui: &mut Ui, icon: &str, color: Color32, text: &str) -> Rect {
    let rect = strip(ui, theme::mix(theme::bg_panel(), color, 0.10));
    let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
    painter.text(pos2(rect.left() + 12.0, rect.center().y), Align2::LEFT_CENTER, icon, theme::font(14.0), color);
    let galley = elided(&painter, text, theme::font(12.0), theme::text(), rect.width() - 44.0);
    galley_left(&painter, rect, rect.left() + 34.0, galley);
    rect
}

/// A button of the bars above the diff and the blame: icon and label, or only
/// the icon (the label moves into the tooltip) when the bar is short of room.
fn action_button(ui: &mut Ui, compact: bool, icon: &str, label: &str, hint: &str, tint: Color32) -> bool {
    if compact {
        widgets::icon_button(ui, icon, 15.0, &format!("{label}\n{hint}")).clicked()
    } else {
        tint_button(ui, &format!("{icon}  {label}"), tint).on_hover_text(hint).clicked()
    }
}

/// A spinner with a caption, filling the remaining space.
fn busy(ui: &mut Ui, text: &str) {
    let rect = ui.available_rect_before_wrap();
    widgets::spinner(ui.painter(), rect.center() - vec2(0.0, 14.0), 10.0, theme::accent());
    ui.painter().text(
        rect.center() + vec2(0.0, 14.0),
        Align2::CENTER_CENTER,
        text,
        theme::font(13.0),
        theme::text_dim(),
    );
    ui.allocate_rect(rect, Sense::hover());
    ui.ctx().request_repaint();
}

/// `Oct 12, 2026` (or the date format chosen in preferences), or `3 days
/// ago` while that says more.
fn short_date(secs: i64) -> String {
    use chrono::{Local, TimeZone};
    let ago = relative_time(secs);
    if !ago.is_empty() {
        return ago;
    }
    match Local.timestamp_opt(secs, 0).single() {
        Some(time) => crate::prefs::appearance::custom_date(&time, false)
            .unwrap_or_else(|| time.format("%b %-d, %Y").to_string()),
        None => String::new(),
    }
}

// ---- history ----------------------------------------------------------------

/// The result of one page request.
struct Page {
    token: u64,
    result: Result<(HistoryCursor, Vec<FileCommit>), String>,
}

struct DiffLoaded {
    token: u64,
    result: Result<FileDiff, String>,
}

#[derive(Default)]
struct HistoryState {
    /// The HEAD the walk started from (`None` inside when the view is pinned
    /// to a commit); `None` outside before the first request.
    loaded_for: Option<Option<Oid>>,
    /// Token of the page request in flight; 0 when there is none.
    token: u64,
    /// The walk, parked here between requests. `None` while a request has it.
    cursor: Option<HistoryCursor>,
    commits: Vec<FileCommit>,
    /// Nothing older is left to find.
    done: bool,
    error: Option<String>,
    /// Whether the file exists in the commit the walk started from.
    exists: bool,
    /// The file's committed name when it differs from the one on disk (a
    /// rename that is staged but not committed).
    committed_as: Option<String>,
    selected: Option<Oid>,
    /// Scroll the selected row into view on the next frame.
    reveal: bool,
    diff: DiffSlot,
}

#[derive(Default)]
struct DiffSlot {
    target: Option<DiffTarget>,
    token: u64,
    /// The diff for `target`, once loaded.
    data: Option<Result<Arc<FileDiff>, String>>,
    requested: Option<Instant>,
    /// The last diff that was shown, kept up briefly while the next one loads.
    previous: Option<(DiffTarget, Arc<FileDiff>)>,
}

impl HistoryView {
    /// Starts the walk (again when HEAD moved) and collects finished requests.
    fn sync_history(&mut self, tab: &mut RepoTab, snap: &RepoSnapshot) {
        let head = if self.at.is_none() { snap.head.oid } else { None };
        if self.history.loaded_for != Some(head) {
            let committed_as = match self.at {
                Some(_) => None,
                None => working_change(snap, &self.path).and_then(|(_, file)| file.old_path.clone()),
            };
            let token = next_token();
            *self.history = HistoryState {
                loaded_for: Some(head),
                token,
                selected: self.history.selected,
                committed_as: committed_as.clone(),
                ..Default::default()
            };
            let at = self.at;
            let path = committed_as.unwrap_or_else(|| self.path.clone());
            tab.spawn_job(PAGE_KEY, move |loader| {
                let repo = loader.repo();
                let result = HistoryCursor::new(repo, at, &path).and_then(|mut cursor| {
                    let commits = cursor.advance(repo, PAGE, PAGE_BUDGET)?;
                    Ok((cursor, commits))
                });
                Page { token, result }
            });
        }

        if let Some(page) = tab.take_result::<Page>(PAGE_KEY)
            && page.token == self.history.token
        {
            let history = &mut self.history;
            history.token = 0;
            match page.result {
                Ok((cursor, commits)) => {
                    history.exists = cursor.exists_at_start;
                    history.done = cursor.is_done();
                    history.cursor = Some(cursor);
                    history.commits.extend(commits);
                }
                Err(e) => {
                    history.error = Some(e);
                    history.done = true;
                }
            }
            // Something is always selected once there is something to select.
            if !self.history.commits.is_empty() && self.history.diff.target.is_none() {
                let selected = self.history.selected;
                let index = self.history.commits.iter().position(|c| Some(c.oid) == selected).unwrap_or(0);
                self.select(tab, index);
            }
        }

        if let Some(loaded) = tab.take_result::<DiffLoaded>(DIFF_KEY)
            && loaded.token == self.history.diff.token
        {
            let slot = &mut self.history.diff;
            let result = loaded.result.map(Arc::new);
            if let (Ok(diff), Some(target)) = (&result, &slot.target) {
                slot.previous = Some((target.clone(), diff.clone()));
            }
            slot.data = Some(result);
        }
    }

    /// Asks for the next page, unless one is on its way or there is none.
    fn load_more(&mut self, tab: &mut RepoTab) {
        let history = &mut self.history;
        if history.token != 0 || history.done {
            return;
        }
        let Some(mut cursor) = history.cursor.take() else { return };
        let token = next_token();
        history.token = token;
        tab.spawn_job(PAGE_KEY, move |loader| {
            let result = cursor.advance(loader.repo(), PAGE, PAGE_BUDGET).map(|commits| (cursor, commits));
            Page { token, result }
        });
    }

    /// Selects a commit of the list and loads what it did to the file.
    fn select(&mut self, tab: &mut RepoTab, index: usize) {
        let Some(commit) = self.history.commits.get(index) else { return };
        self.history.selected = Some(commit.oid);
        self.history.reveal = true;
        let target = DiffTarget {
            source: DiffSource::Commit(commit.oid),
            path: commit.path.clone(),
            old_path: commit.old_path.clone(),
        };
        let slot = &mut self.history.diff;
        if slot.target.as_ref() == Some(&target) {
            return;
        }
        let token = next_token();
        slot.token = token;
        slot.target = Some(target.clone());
        slot.data = None;
        slot.requested = Some(Instant::now());
        tab.spawn_job(DIFF_KEY, move |loader| DiffLoaded { token, result: loader.diff(&target) });
    }
}

fn history_mode(
    ui: &mut Ui,
    tab: &mut RepoTab,
    shared: &mut Shared,
    snap: &RepoSnapshot,
    view: &mut HistoryView,
    action: &mut Option<Action>,
) {
    view.sync_history(tab, snap);

    if view.history.commits.is_empty() {
        let history = &view.history;
        let place = match view.at {
            Some(oid) => format!("in commit {}", short(oid)),
            None => "at HEAD".to_owned(),
        };
        if let Some(e) = &history.error {
            widgets::placeholder(ui, e);
        } else if !history.done {
            // An empty page is not the end: keep the walk going.
            view.load_more(tab);
            busy(ui, "Reading history…");
        } else if view.at.is_none() && working_change(snap, &view.path).is_some() {
            widgets::placeholder(ui, &format!("{} has not been committed yet, so it has no history", view.path));
        } else {
            widgets::placeholder(
                ui,
                &format!("{} does not exist {place}, and no commit leading up to it ever contained it", view.path),
            );
        }
        return;
    }

    // Arrow keys walk the list unless a text field has the keyboard.
    if shared.dialog.is_none() && !ui.ctx().egui_wants_keyboard_input() {
        let step = ui.input(|i| i.key_pressed(Key::ArrowDown) as i64 - i.key_pressed(Key::ArrowUp) as i64);
        if step != 0 {
            let selected = view.history.selected;
            let current = view.history.commits.iter().position(|c| Some(c.oid) == selected);
            let last = view.history.commits.len() as i64 - 1;
            let next = match current {
                Some(current) => (current as i64 + step).clamp(0, last),
                None => 0,
            };
            *action = Some(Action::Select(next as usize));
        }
    }

    // The list beside the diff, or above it when there is no room for both.
    let avail = ui.available_rect_before_wrap();
    ui.allocate_rect(avail, Sense::hover());
    let stacked = avail.width() < STACK_BELOW;
    let list_width = view.list_width.clamp(280.0, (avail.width() - 320.0).max(280.0));
    // The diff's edges sit on whole pixels. A scroll area snaps its content
    // to pixels, and one that starts between two can end up a fraction larger
    // than its frame, which shows as a scroll bar with nothing to scroll.
    let (left, top) = if stacked {
        let list_height = (avail.height() * 0.4).max(ROW_H * 2.5).min(avail.height());
        (avail.left(), avail.top() + list_height + 1.0)
    } else {
        (avail.left() + list_width + 1.0, avail.top())
    };
    let detail = Rect::from_min_max(
        pos2(to_pixel(ui, left), to_pixel(ui, top)),
        pos2(to_pixel(ui, avail.right()), to_pixel(ui, avail.bottom())),
    );
    let list = if stacked {
        Rect::from_min_max(avail.min, pos2(avail.right(), detail.top() - 1.0))
    } else {
        Rect::from_min_max(avail.min, pos2(detail.left() - 1.0, avail.bottom()))
    };

    ui.painter().rect_filled(list, 0.0, theme::bg_panel());
    ui.scope_builder(UiBuilder::new().max_rect(list), |ui| {
        ui.set_clip_rect(list.intersect(ui.clip_rect()));
        history_list(ui, tab, snap, view, action);
    });
    ui.scope_builder(UiBuilder::new().max_rect(detail), |ui| {
        ui.set_clip_rect(detail.intersect(ui.clip_rect()));
        history_detail(ui, tab, shared, view, action);
    });

    if stacked {
        ui.painter().hline(avail.x_range(), list.bottom(), Stroke::new(1.0, theme::border()));
        return;
    }
    // The edge between the two can be dragged.
    let edge = Rect::from_center_size(pos2(list.right(), avail.center().y), vec2(9.0, avail.height()));
    let response = ui
        .interact(edge, ui.id().with("file-history-edge"), Sense::drag())
        .on_hover_and_drag_cursor(CursorIcon::ResizeHorizontal);
    if response.dragged() {
        view.list_width = list_width + response.drag_delta().x;
    }
    let active = response.hovered() || response.dragged();
    ui.painter().vline(
        list.right(),
        avail.y_range(),
        Stroke::new(1.0, if active { theme::accent() } else { theme::border() }),
    );
}

fn history_list(
    ui: &mut Ui,
    tab: &mut RepoTab,
    snap: &RepoSnapshot,
    view: &mut HistoryView,
    action: &mut Option<Action>,
) {
    // What the list cannot show by itself is said above it.
    if view.at.is_none()
        && let Some((_, file)) = working_change(snap, &view.path)
    {
        let text = match &view.history.committed_as {
            Some(old) => format!("Renamed from {old}, not committed yet."),
            None => "This file has uncommitted changes, which are not listed.".to_owned(),
        };
        let rect = notice(ui, icons::PENCIL_SIMPLE, theme::yellow(), &text);
        if file.kind != ChangeKind::Conflicted {
            let size = widgets::tint_button_size(ui, "View");
            let button = Rect::from_min_size(pos2(rect.right() - size.x - 8.0, rect.center().y - size.y / 2.0), size);
            ui.painter().rect_filled(button.expand2(vec2(8.0, 2.0)), 0.0, theme::mix(theme::bg_panel(), theme::yellow(), 0.10));
            if widgets::tint_button_at(ui, button, "View", theme::yellow())
                .on_hover_text("Close this view and open the uncommitted changes")
                .clicked()
            {
                *action = Some(Action::OpenWorkingTreeDiff);
            }
        }
    }
    if !view.history.exists {
        let place = match view.at {
            Some(oid) => format!("in commit {}", short(oid)),
            None => "at HEAD".to_owned(),
        };
        notice(
            ui,
            icons::INFO,
            theme::blue(),
            &format!("The file does not exist {place}. This is its history up to the deletion."),
        );
    }

    let history = &view.history;
    let rows = history.commits.len();
    let selected = history.selected;
    let reveal = history.reveal;
    let mut want_more = false;

    ScrollArea::vertical().id_salt("file-history-rows").auto_shrink([false, false]).show_viewport(
        ui,
        |ui, viewport| {
            // One extra row says whether there is more to come.
            let (content, _) = ui.allocate_exact_size(
                vec2(ui.available_width(), (rows + 1) as f32 * ROW_H),
                Sense::hover(),
            );
            let row_rect = |i: usize| {
                Rect::from_min_size(
                    pos2(content.left(), content.top() + i as f32 * ROW_H),
                    vec2(content.width(), ROW_H),
                )
            };

            if reveal
                && let Some(row) = history.commits.iter().position(|c| Some(c.oid) == selected)
            {
                let top = row as f32 * ROW_H;
                if top < viewport.min.y || top + ROW_H > viewport.max.y {
                    ui.scroll_to_rect(row_rect(row), None);
                }
            }

            let first = (viewport.min.y / ROW_H).floor().max(0.0) as usize;
            let last = ((viewport.max.y / ROW_H).ceil().max(0.0) as usize + 1).min(rows);
            let painter = ui.painter().clone();

            for (i, commit) in history.commits.iter().enumerate().take(last).skip(first) {
                let rect = row_rect(i);
                let response = ui.interact(rect, ui.id().with(("file-commit", commit.oid)), Sense::click());
                let is_selected = selected == Some(commit.oid);
                paint_commit_row(&painter, rect, commit, &view.path, is_selected, response.hovered());

                if response.double_clicked() {
                    *action = Some(Action::ShowInGraph(commit.oid));
                } else if response.clicked() {
                    *action = Some(Action::Select(i));
                }
                response.context_menu(|ui| {
                    ui.set_min_width(190.0);
                    if menu_item(ui, icons::GIT_COMMIT, "Show in graph") {
                        *action = Some(Action::ShowInGraph(commit.oid));
                    }
                    if commit.kind != ChangeKind::Deleted
                        && menu_item(ui, icons::USER_LIST, "Blame at this commit")
                    {
                        *action = Some(Action::BlameAt(commit.oid, commit.path.clone()));
                    }
                    if menu_item(ui, icons::COPY, "Copy SHA") {
                        ui.ctx().copy_text(commit.oid.to_string());
                    }
                });
            }

            let footer = row_rect(rows);
            if last >= rows && ui.is_rect_visible(footer) {
                if let Some(e) = &history.error {
                    let galley = elided(&painter, e, theme::font(12.0), theme::red(), footer.width() - 24.0);
                    galley_left(&painter, footer, footer.left() + 12.0, galley);
                } else if history.done {
                    let text = if rows == 1 { "1 commit".to_owned() } else { format!("{rows} commits") };
                    painter.text(footer.center(), Align2::CENTER_CENTER, text, theme::font(12.0), theme::text_faint());
                } else {
                    widgets::spinner(&painter, pos2(footer.center().x - 86.0, footer.center().y), 6.0, theme::text_dim());
                    painter.text(
                        footer.center() + vec2(8.0, 0.0),
                        Align2::CENTER_CENTER,
                        "Looking further back…",
                        theme::font(12.0),
                        theme::text_dim(),
                    );
                    want_more = true;
                    ui.ctx().request_repaint();
                }
            }
        },
    );

    view.history.reveal = false;
    if want_more {
        view.load_more(tab);
    }
}

fn paint_commit_row(
    painter: &Painter,
    rect: Rect,
    commit: &FileCommit,
    current_path: &str,
    selected: bool,
    hovered: bool,
) {
    widgets::row_background(painter, rect, selected, hovered);
    painter.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::with_alpha(theme::border(), 110)));
    let painter = painter.with_clip_rect(rect.intersect(painter.clip_rect()));

    let upper = Rect::from_min_max(rect.min + vec2(0.0, 5.0), pos2(rect.right(), rect.center().y + 1.0));
    let lower = Rect::from_min_max(pos2(rect.left(), rect.center().y), rect.max - vec2(0.0, 5.0));
    let left = rect.left() + 46.0;
    let right = rect.right() - 10.0;

    widgets::avatar(&painter, pos2(rect.left() + 24.0, rect.center().y), 13.0, &commit.author, &commit.email, None);
    // The selection fill needs brighter text than the panel does.
    let (dim, faint) = if selected { (theme::text(), theme::text()) } else { (theme::text_dim(), theme::text_faint()) };

    // First line: summary, then what happened to the file and the SHA.
    let sha = painter.layout_no_wrap(short(commit.oid), theme::mono(11.5), dim);
    let mut summary_right = right - sha.size().x - 10.0;
    galley_left(&painter, upper, right - sha.size().x, sha);
    if commit.kind != ChangeKind::Modified {
        let (icon, color) = widgets::kind_icon(commit.kind);
        painter.text(pos2(summary_right, upper.center().y), Align2::RIGHT_CENTER, icon, theme::font(13.0), color);
        summary_right -= 22.0;
    }
    let color = if selected { theme::text_bright() } else { theme::text() };
    let summary = elided(&painter, &commit.summary, theme::font(13.0), color, summary_right - left);
    galley_left(&painter, upper, left, summary);

    // Second line: who and when, then the file's name at the time when it
    // was not today's.
    let was = match &commit.old_path {
        Some(old) => Some((format!("{old}  {}  {}", icons::ARROW_RIGHT, commit.path), theme::blue())),
        None if commit.path != current_path => Some((commit.path.clone(), dim)),
        None => None,
    };
    let mut who_right = right;
    if let Some((text, color)) = was {
        let galley = elided(&painter, &text, theme::font(11.5), color, (right - left) * 0.6);
        who_right = right - galley.size().x - 12.0;
        galley_left(&painter, lower, right - galley.size().x, galley);
    }
    let who = widgets::elided_pair(
        &painter,
        (&commit.author, theme::font(11.5), dim),
        (&short_date(commit.time), theme::font(11.5), faint),
        who_right - left,
    );
    galley_left(&painter, lower, left, who);
}

fn history_detail(
    ui: &mut Ui,
    tab: &mut RepoTab,
    shared: &mut Shared,
    view: &HistoryView,
    action: &mut Option<Action>,
) {
    let history = &view.history;
    let Some(commit) = history.commits.iter().find(|c| Some(c.oid) == history.selected) else {
        widgets::placeholder(ui, "Select a commit to see what it did to the file");
        return;
    };

    // What the selected commit is, and where to go from it. The diff below
    // starts on a whole pixel (see `history_mode`).
    let top = ui.available_rect_before_wrap().top();
    let height = to_pixel(ui, top + HEADER_H) - top;
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, theme::bg_header());
    ui.painter().hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::border()));

    let compact = rect.width() < COMPACT_DIFF_BELOW;
    let mut buttons_left = rect.right();
    ui.scope_builder(
        UiBuilder::new()
            .max_rect(rect.shrink2(vec2(10.0, 0.0)))
            .layout(Layout::right_to_left(Align::Center)),
        |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            if action_button(
                ui,
                compact,
                icons::GIT_COMMIT,
                "Show in graph",
                "Close this view and select the commit in the graph (double-click)",
                theme::accent(),
            ) {
                *action = Some(Action::ShowInGraph(commit.oid));
            }
            if commit.kind != ChangeKind::Deleted
                && action_button(
                    ui,
                    compact,
                    icons::USER_LIST,
                    "Blame at this commit",
                    "Blame the file as this commit left it",
                    theme::blue(),
                )
            {
                *action = Some(Action::BlameAt(commit.oid, commit.path.clone()));
            }
            buttons_left = ui.min_rect().left();
        },
    );

    let text_clip = Rect::from_min_max(rect.min, pos2(buttons_left - 12.0, rect.bottom()));
    let painter = ui.painter().with_clip_rect(text_clip.intersect(ui.clip_rect()));
    let left = rect.left() + 14.0;
    let width = text_clip.right() - left;
    let upper = Rect::from_min_max(rect.min + vec2(0.0, 7.0), pos2(rect.right(), rect.center().y + 1.0));
    let lower = Rect::from_min_max(pos2(rect.left(), rect.center().y), rect.max - vec2(0.0, 7.0));
    let summary = elided(&painter, &commit.summary, theme::bold(13.5), theme::text_bright(), width);
    galley_left(&painter, upper, left, summary);

    let what = format!("{}   {}", commit.author, format_time(commit.time));
    let mut change = short(commit.oid);
    match (&commit.old_path, commit.kind) {
        (Some(old), _) => change.push_str(&format!("   renamed from {old}")),
        (None, ChangeKind::Modified) => {}
        (None, kind) => change.push_str(&format!("   {}", widgets::kind_name(kind))),
    }
    let line = widgets::elided_pair(
        &painter,
        (&what, theme::font(12.0), theme::text_dim()),
        (&change, theme::mono(11.5), theme::text_faint()),
        width,
    );
    galley_left(&painter, lower, left, line);

    // And what it did to the file.
    let slot = &history.diff;
    let shown = match (&slot.data, &slot.target) {
        (Some(Ok(diff)), Some(target)) => Some((target.clone(), diff.clone())),
        (Some(Err(e)), _) => {
            widgets::placeholder(ui, e);
            return;
        }
        // Still loading: the previous diff stays for a moment.
        _ => {
            let waited = slot.requested.map(|at| at.elapsed()).unwrap_or(DIFF_GRACE);
            match &slot.previous {
                Some(previous) if waited < DIFF_GRACE => {
                    ui.ctx().request_repaint_after(DIFF_GRACE - waited);
                    Some(previous.clone())
                }
                _ => None,
            }
        }
    };
    match shown {
        None => busy(ui, "Loading diff…"),
        Some((_, diff)) if diff.binary => match &diff.images {
            Some(images) => crate::ui::image_diff::show(ui, images),
            None => widgets::placeholder(ui, "Binary file - no text diff to show"),
        },
        Some((_, diff)) if diff.hunks.is_empty() => {
            widgets::placeholder(ui, "No content changes (the file mode or name changed)")
        }
        Some((target, diff)) => {
            // A scroll position per commit: each diff opens at its top.
            ui.push_id(target.source, |ui| {
                crate::ui::diff::read_only(ui, tab, shared, &target, &diff);
            });
        }
    }
}

// ---- blame ------------------------------------------------------------------

/// What a blame was computed from, to notice when it has gone stale.
struct Stamp {
    /// HEAD at the time, when following HEAD.
    head: Option<Oid>,
    /// The snapshot at the time, while the file has uncommitted changes:
    /// every refresh may then mean the file was edited again. Holding on to
    /// it is what makes comparing identities sound.
    dirty: Option<Arc<RepoSnapshot>>,
}

impl Stamp {
    fn matches(&self, head: Option<Oid>, dirty: Option<&Arc<RepoSnapshot>>) -> bool {
        self.head == head
            && match (&self.dirty, dirty) {
                (Some(then), Some(now)) => Arc::ptr_eq(then, now),
                (None, None) => true,
                _ => false,
            }
    }
}

/// A `git blame` running on its own thread. Dropping it stops git.
struct BlameTask {
    rx: Receiver<Result<Blame, BlameError>>,
    cancel: Cancel,
}

impl Drop for BlameTask {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn start_blame(ctx: &egui::Context, workdir: PathBuf, at: Option<Oid>, path: String) -> BlameTask {
    let (tx, rx) = channel();
    let cancel = Cancel::default();
    let spawned = {
        let (ctx, cancel, tx) = (ctx.clone(), cancel.clone(), tx.clone());
        std::thread::Builder::new().name("git-blame".into()).spawn(move || {
            let result = match git2::Repository::open(&workdir) {
                Ok(repo) => crate::git::blame::blame(&repo, at, &path, &cancel),
                Err(e) => Err(BlameError::Git(e.message().to_owned())),
            };
            // Nobody is listening any more if the view has closed.
            let _ = tx.send(result);
            ctx.request_repaint();
        })
    };
    if let Err(e) = spawned {
        let _ = tx.send(Err(BlameError::Git(format!("Could not start a thread for git blame: {e}"))));
    }
    BlameTask { rx, cancel }
}

#[derive(Default)]
struct BlameState {
    /// What the blame on screen, or the one being computed, was started for.
    stamp: Option<Stamp>,
    task: Option<BlameTask>,
    data: Option<Result<Arc<Blame>, BlameError>>,
    /// For each commit of `data`, how recent it is among them: 0 for the
    /// oldest, 1 for the newest.
    heat: Vec<f32>,
    /// The commit whose blocks are highlighted.
    selected: Option<Oid>,
}

impl BlameState {
    fn set(&mut self, result: Result<Blame, BlameError>) {
        self.heat.clear();
        if let Ok(blame) = &result {
            // Ranked rather than scaled by date: one change from last week in
            // a file from ten years ago would otherwise leave everything else
            // looking equally old.
            let mut times: Vec<i64> = blame.commits.iter().filter(|c| !c.is_uncommitted()).map(|c| c.time).collect();
            times.sort_unstable();
            times.dedup();
            let span = times.len().saturating_sub(1).max(1) as f32;
            self.heat = blame
                .commits
                .iter()
                .map(|c| times.binary_search(&c.time).map_or(1.0, |rank| rank as f32 / span))
                .collect();
            if times.len() == 1 {
                self.heat.fill(1.0);
            }
        }
        self.data = Some(result.map(Arc::new));
    }
}

impl HistoryView {
    /// Starts the blame, restarts it when the file or HEAD changed under it,
    /// and collects the result.
    fn sync_blame(&mut self, ctx: &egui::Context, tab: &RepoTab, snap: &Arc<RepoSnapshot>) {
        let state = &mut self.blame;
        if let Some(task) = &state.task {
            match task.rx.try_recv() {
                Ok(result) => {
                    state.task = None;
                    state.set(result);
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    state.task = None;
                    state.set(Err(BlameError::Git("git blame stopped unexpectedly".to_owned())));
                }
            }
        }

        let following = self.at.is_none();
        let head = if following { snap.head.oid } else { None };
        let dirty = (following && working_change(snap, &self.path).is_some()).then_some(snap);
        let current = state.stamp.as_ref().is_some_and(|stamp| stamp.matches(head, dirty));
        // One at a time: a blame that is still running is left to finish, and
        // what it shows is brought up to date afterwards.
        if !current && state.task.is_none() {
            state.stamp = Some(Stamp { head, dirty: dirty.cloned() });
            state.task = Some(start_blame(ctx, tab.path.clone(), self.at, self.path.clone()));
        }
    }
}

fn blame_mode(
    ui: &mut Ui,
    snap: &Arc<RepoSnapshot>,
    tab: &RepoTab,
    view: &mut HistoryView,
    action: &mut Option<Action>,
) {
    view.sync_blame(ui.ctx(), tab, snap);

    let state = &view.blame;
    let blame = match &state.data {
        // The first blame of this file version: nothing to show yet.
        None => return busy(ui, "Computing blame…"),
        Some(Err(e)) => {
            let mut text = e.message(&view.path, view.at);
            if *e == BlameError::Missing && view.at.is_none() {
                text.push_str(". To blame an older version, pick a commit in History.");
            }
            return widgets::placeholder(ui, &text);
        }
        Some(Ok(blame)) => blame.clone(),
    };
    if blame.lines.is_empty() {
        return widgets::placeholder(ui, &format!("{} is empty", view.path));
    }
    if view.at.is_none() && blame.rev.is_some() {
        notice(
            ui,
            icons::INFO,
            theme::blue(),
            "The file is deleted in the working tree. This is the version at HEAD.",
        );
    }

    let selected = state
        .selected
        .and_then(|oid| blame.commits.iter().position(|c| c.oid == oid));
    blame_strip(ui, &blame, selected, action);
    blame_body(ui, &blame, &state.heat, selected, action);
}

/// From cool for the oldest lines to warm for the newest.
fn heat_color(heat: f32) -> Color32 {
    theme::mix(theme::blue(), theme::orange(), heat.clamp(0.0, 1.0))
}

/// The colour that stands for a commit's age in the blame.
fn age_color(commit: &BlameCommit, heat: f32) -> Color32 {
    if commit.is_uncommitted() { theme::green() } else { heat_color(heat) }
}

/// The band above the blamed file: the selected commit and what can be done
/// with it, or how to read the view when nothing is selected.
fn blame_strip(ui: &mut Ui, blame: &Blame, selected: Option<usize>, action: &mut Option<Action>) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 38.0), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, theme::bg_panel());
    ui.painter().hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::border()));
    let left = rect.left() + 14.0;

    let Some(commit) = selected.map(|index| &blame.commits[index]) else {
        let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
        // A legend for the colours, and beside it how to use the view.
        let mut x = rect.right() - 14.0;
        x -= painter
            .text(pos2(x, rect.center().y), Align2::RIGHT_CENTER, "newer", theme::font(11.5), theme::text_dim())
            .width()
            + 8.0;
        const STEPS: usize = 8;
        for step in (0..STEPS).rev() {
            let swatch = Rect::from_min_size(pos2(x - 14.0, rect.center().y - 5.0), vec2(14.0, 10.0));
            painter.rect_filled(swatch, 0.0, heat_color(step as f32 / (STEPS - 1) as f32));
            x -= 14.0;
        }
        x -= painter
            .text(pos2(x - 8.0, rect.center().y), Align2::RIGHT_CENTER, "older", theme::font(11.5), theme::text_dim())
            .width()
            + 24.0;
        let hint = elided(
            &painter,
            "Click a commit to select its lines. Hover it for the full summary.",
            theme::font(12.0),
            theme::text_dim(),
            x - left,
        );
        galley_left(&painter, rect, left, hint);
        return;
    };

    let compact = rect.width() < COMPACT_BLAME_BELOW;
    let mut buttons_left = rect.right();
    ui.scope_builder(
        UiBuilder::new()
            .max_rect(rect.shrink2(vec2(10.0, 0.0)))
            .layout(Layout::right_to_left(Align::Center)),
        |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            if widgets::icon_button(ui, icons::X, 12.0, "Clear the selection").clicked() {
                *action = Some(Action::SelectBlamed(None));
            }
            match &commit.previous {
                Some((parent, path)) => {
                    let (label, hint) = if commit.is_uncommitted() {
                        ("Blame without uncommitted changes", "Blame the file as it is at HEAD")
                    } else {
                        (
                            "Blame before this commit",
                            "Blame the file as it was just before this commit, to see where these lines came from",
                        )
                    };
                    if action_button(ui, compact, icons::ARROW_BEND_UP_LEFT, label, hint, theme::blue()) {
                        *action = Some(Action::BlameAt(*parent, path.clone()));
                    }
                }
                None if compact => {}
                None => {
                    ui.label(
                        egui::RichText::new("The file starts with this commit")
                            .size(11.5)
                            .color(theme::text_faint()),
                    );
                }
            }
            if !commit.is_uncommitted()
                && action_button(
                    ui,
                    compact,
                    icons::GIT_COMMIT,
                    "Show in graph",
                    "Close this view and select the commit in the graph",
                    theme::accent(),
                )
            {
                *action = Some(Action::ShowInGraph(commit.oid));
            }
            buttons_left = ui.min_rect().left();
        },
    );

    let clip = Rect::from_min_max(rect.min, pos2(buttons_left - 12.0, rect.bottom()));
    let painter = ui.painter().with_clip_rect(clip.intersect(ui.clip_rect()));
    let mut x = left;
    if commit.is_uncommitted() {
        widgets::text_left(&painter, rect, x, "Uncommitted changes", theme::bold(13.0), theme::text_bright());
        return;
    }
    x += widgets::text_left(&painter, rect, x, &short(commit.oid), theme::mono(12.0), theme::accent()) + 10.0;
    let who = format!("{}   {}", commit.author, format_time(commit.time));
    let line = widgets::elided_pair(
        &painter,
        (&commit.summary, theme::bold(13.0), theme::text_bright()),
        (&who, theme::font(12.0), theme::text_dim()),
        clip.right() - x,
    );
    galley_left(&painter, rect, x, line);
}

/// The blamed file as numbered lines, for selecting and copying text.
struct BlameText<'a>(&'a Blame);

impl Lines for BlameText<'_> {
    fn count(&self) -> usize {
        self.0.lines.len()
    }

    fn shown(&self, line: usize) -> &str {
        &self.0.lines[line]
    }

    fn source(&self, line: usize) -> Option<&str> {
        self.0.originals.get(&line).map(String::as_str)
    }
}

/// Text selected in the blame, kept from frame to frame.
#[derive(Default)]
struct BlameSelection {
    text: textsel::Selection,
    /// The line the text's context menu was opened on.
    menu_line: Option<usize>,
}

/// What can be done with the commit of a block, as context menu entries.
fn commit_menu(ui: &mut Ui, commit: &BlameCommit, action: &mut Option<Action>) {
    if !commit.is_uncommitted() && menu_item(ui, icons::GIT_COMMIT, "Show in graph") {
        *action = Some(Action::ShowInGraph(commit.oid));
    }
    if let Some((parent, path)) = &commit.previous {
        let label = if commit.is_uncommitted() {
            "Blame without uncommitted changes"
        } else {
            "Blame before this commit"
        };
        if menu_item(ui, icons::ARROW_BEND_UP_LEFT, label) {
            *action = Some(Action::BlameAt(*parent, path.clone()));
        }
    }
    if !commit.is_uncommitted() && menu_item(ui, icons::COPY, "Copy SHA") {
        ui.ctx().copy_text(commit.oid.to_string());
    }
}

fn blame_body(ui: &mut Ui, blame: &Arc<Blame>, heat: &[f32], selected: Option<usize>, action: &mut Option<Action>) {
    let font = theme::mono(12.5);
    let small = theme::mono(11.5);
    let (char_w, digit_w) = ui.painter().fonts_mut(|f| (f.glyph_width(&font, 'm'), f.glyph_width(&small, '0')));
    let lines = blame.lines.len();
    let number_w = lines.to_string().len().max(3) as f32 * digit_w + 20.0;
    // In a narrow view the commit column gives way to the code: first it
    // shrinks, then it drops the date.
    let annot_w = (ui.available_width() * 0.4).clamp(150.0, ANNOT_W).round();
    let with_date = annot_w >= 230.0;
    let gutter = annot_w + number_w;
    let text_w = gutter + 10.0 + blame.max_chars as f32 * char_w + 40.0;
    let height = lines as f32 * LINE_H;

    // Selected text. It is dropped when another blame is shown, but not when
    // the same file is blamed again with the same result.
    let text = BlameText(blame);
    let state_id = ui.id().with("blame-text-selection");
    let state = ui.data_mut(|data| data.get_temp_mut_or_default::<Arc<Mutex<BlameSelection>>>(state_id).clone());
    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
    let state = &mut *state;
    state.text.follow(blame, ui.ctx().cumulative_frame_nr(), || {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (blame.rev, &blame.lines).hash(&mut hasher);
        hasher.finish()
    });
    let view = ui.available_rect_before_wrap();

    ScrollArea::both().id_salt("blame-body").auto_shrink([false, false]).show_viewport(ui, |ui, viewport| {
        // Exactly the size of the file, not stretched to fill the view: a
        // scroll area whose content is "as large as the space" shows a scroll
        // bar whenever rounding makes the two differ by a hair. What lies
        // beyond a short file is painted all the same.
        let (content, _) = ui.allocate_exact_size(vec2(text_w, height), Sense::hover());
        let painter = ui.painter().clone();
        let clip = painter.clip_rect();

        // The commit column and the line numbers stay put while the text
        // scrolls sideways underneath.
        let left = content.left() + viewport.min.x;
        let right = content.left() + viewport.max.x;
        let (view_top, view_bottom) = (content.top() + viewport.min.y, content.top() + viewport.max.y);
        // Where every line's text starts, scrolled sideways.
        let text_left = content.left() + gutter + 10.0;
        let text_painter =
            painter.with_clip_rect(Rect::from_min_max(pos2(left + gutter, clip.top()), clip.max).intersect(clip));
        painter.rect_filled(
            Rect::from_min_max(pos2(left, view_top), pos2(left + gutter, view_bottom)),
            0.0,
            theme::bg_panel(),
        );

        // The code is for selecting text; the commit column for the commit.
        let text_rect = Rect::from_min_max(pos2(left + gutter, view_top), pos2(right, view_bottom));
        let hit = |pos: Pos2| {
            let y = pos.y - content.top();
            if y < 0.0 {
                return Pos::new(0, 0);
            }
            let line = (y / LINE_H) as usize;
            if line >= lines {
                return Pos::new(lines - 1, blame.lines[lines - 1].chars().count());
            }
            let galley = painter.layout_no_wrap(blame.lines[line].clone(), font.clone(), theme::text());
            Pos::new(line, textsel::column_at(&galley, pos.x - text_left))
        };
        let text_response = state.text.interact(ui, ui.id().with("text"), text_rect, view, &text, hit);
        if text_response.secondary_clicked() {
            state.menu_line = text_response.interact_pointer_pos().map(|pos| hit(pos).line);
        }
        text_response.context_menu(|ui| {
            ui.set_min_width(210.0);
            state.text.menu(ui, &text);
            // And what the line's commit offers, as on the commit column.
            let block = state.menu_line.and_then(|line| blame.block_of(line));
            if let Some(commit) = block.map(|block| &blame.commits[blame.blocks[block].commit])
                && (!commit.is_uncommitted() || commit.previous.is_some())
            {
                ui.separator();
                commit_menu(ui, commit, action);
            }
        });

        let first = (viewport.min.y / LINE_H).floor().max(0.0) as usize;
        let last = ((viewport.max.y / LINE_H).ceil().max(0.0) as usize).min(lines);

        // Blocks first: tint, separator and the commit they come from.
        let mut index = blame.block_of(first).unwrap_or(blame.blocks.len());
        while let Some(block) = blame.blocks.get(index).filter(|block| block.start < last) {
            let commit = &blame.commits[block.commit];
            let warmth = heat.get(block.commit).copied().unwrap_or(0.0);
            let color = age_color(commit, warmth);
            let top = content.top() + block.start as f32 * LINE_H;
            let bottom = top + block.len as f32 * LINE_H;
            // Only the part in view is laid out, however long the block is.
            let seen = Rect::from_min_max(pos2(left, top.max(view_top)), pos2(right, bottom.min(view_bottom)));
            let annot = Rect::from_min_max(seen.min, pos2(left + annot_w, seen.bottom()));

            let margin = Rect::from_min_max(seen.min, pos2(left + gutter, seen.bottom()));
            let response = ui.interact(margin, ui.id().with(("blame-block", index)), Sense::click());
            let is_selected = selected == Some(block.commit);
            let strength = if commit.is_uncommitted() { 0.16 } else { 0.05 + 0.13 * warmth };
            let mut fill = theme::mix(theme::bg_panel(), color, strength);
            if is_selected {
                fill = theme::bg_selected();
                painter.rect_filled(
                    Rect::from_min_max(pos2(left + gutter, seen.top()), seen.max),
                    0.0,
                    theme::mix(theme::bg_app(), theme::accent(), 0.09),
                );
            } else if response.hovered() {
                fill = theme::mix(fill, theme::text(), 0.08);
            }
            painter.rect_filled(annot, 0.0, fill);
            painter.rect_filled(Rect::from_min_max(annot.min, pos2(left + 4.0, annot.bottom())), 0.0, color);
            if block.start > 0 && top >= view_top {
                painter.hline(left..=left + gutter, top, Stroke::new(1.0, theme::border()));
                painter.hline(left + gutter..=right, top, Stroke::new(1.0, theme::with_alpha(theme::border_light(), 70)));
            }

            // The commit is named on the block's first line that is in view,
            // so it stays readable while a long block scrolls past.
            let label_top = top.max(view_top).min(bottom - LINE_H);
            let label = Rect::from_min_max(pos2(left, label_top), pos2(left + annot_w, label_top + LINE_H));
            let label_painter = painter.with_clip_rect(annot.intersect(clip));
            let bright = if is_selected { theme::text_bright() } else { theme::text() };
            if commit.is_uncommitted() {
                widgets::text_left(&label_painter, label, left + 12.0, "Not committed yet", theme::font(12.0), bright);
            } else {
                let mut date_x = label.right() - 2.0;
                if with_date {
                    let date = label_painter.layout_no_wrap(short_date(commit.time), theme::font(11.5), theme::text_dim());
                    date_x = label.right() - 10.0 - date.size().x;
                    galley_left(&label_painter, label, date_x, date);
                }
                let sha_color = if is_selected { theme::text_bright() } else { theme::text_dim() };
                widgets::text_left(&label_painter, label, left + 12.0, &short(commit.oid), small.clone(), sha_color);
                let author_x = left + 12.0 + digit_w * 7.0 + 10.0;
                let author = elided(&label_painter, &commit.author, theme::font(12.0), bright, date_x - author_x - 8.0);
                galley_left(&label_painter, label, author_x, author);
            }

            if response.clicked() {
                *action = Some(Action::SelectBlamed((!is_selected).then_some(commit.oid)));
            }
            response.context_menu(|ui| {
                ui.set_min_width(210.0);
                commit_menu(ui, commit, action);
            });
            response.on_hover_ui_at_pointer(|ui| {
                ui.set_max_width(460.0);
                if commit.is_uncommitted() {
                    ui.label(egui::RichText::new("Not committed yet").font(theme::bold(13.0)).color(theme::text_bright()));
                    ui.label(egui::RichText::new("These lines exist only in the working tree or the index.").color(theme::text_dim()));
                    return;
                }
                ui.label(egui::RichText::new(&commit.summary).font(theme::bold(13.0)).color(theme::text_bright()));
                ui.label(egui::RichText::new(format!("{} <{}>", commit.author, commit.email)).color(theme::text()));
                let mut when = format_time(commit.time);
                let ago = relative_time(commit.time);
                if !ago.is_empty() {
                    when.push_str(&format!(" ({ago})"));
                }
                ui.label(egui::RichText::new(when).size(11.5).color(theme::text_dim()));
                let lines = if block.len == 1 { "1 line".to_owned() } else { format!("{} lines", block.len) };
                ui.label(
                    egui::RichText::new(format!("{}   {lines} in this block", short(commit.oid)))
                        .font(theme::mono(11.5))
                        .color(theme::text_dim()),
                );
            });
            index += 1;
        }

        // Then the file itself, over the highlight of selected text.
        for (i, line) in blame.lines.iter().enumerate().take(last).skip(first) {
            let top = content.top() + i as f32 * LINE_H;
            let y = top + 0.5 * LINE_H;
            painter.text(
                pos2(left + gutter - 10.0, y),
                Align2::RIGHT_CENTER,
                (i + 1).to_string(),
                small.clone(),
                theme::text_faint(),
            );
            let galley = text_painter.layout_no_wrap(line.clone(), font.clone(), theme::text());
            let origin = pos2(text_left, y - galley.size().y / 2.0);
            let row = Rect::from_min_max(pos2(left + gutter, top), pos2(right, top + LINE_H));
            state.text.paint(&text_painter, i, &galley, origin.x, row, char_w);
            text_painter.galley(origin, galley, theme::text());
        }

        painter.vline(left + annot_w, clip.y_range(), Stroke::new(1.0, theme::border()));
        painter.vline(left + gutter, clip.y_range(), Stroke::new(1.0, theme::with_alpha(theme::border_light(), 90)));
    });
}
