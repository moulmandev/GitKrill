//! Interactive rebase: choose what happens to each commit, then run it.
//!
//! The list reads like the graph: newest commit on top, oldest at the bottom,
//! with the base commit (which is not touched) underneath. The plan itself
//! lives in `git::rebase` and is kept oldest first, the order git works in;
//! this module only turns row positions around when it draws.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use eframe::egui::text::{LayoutJob, TextFormat, TextWrapping};
use eframe::egui::{
    self, Align, Align2, Button, Color32, CursorIcon, FontId, Frame, Galley, Id, Key, Label,
    LayerId, Layout, Margin, Modifiers, Order, Painter, Popup, Rect, RichText, ScrollArea, Sense,
    Stroke, StrokeKind, TextEdit, Ui, UiBuilder, pos2, vec2,
};

use crate::git::ops::{self, GitCmd};
use crate::git::rebase::{self, Action, Group, Item, Plan, Range};
use crate::git::{Oid, short};
use crate::state::{RepoTab, Shared};
use crate::theme::{self, icons};
use crate::ui::widgets;

const ROW_H: f32 = 30.0;
/// Height of the message editor that opens under a row.
const EDITOR_H: f32 = 156.0;
const CAPTION_H: f32 = 24.0;

// Columns, from the left edge of a row.
const HANDLE_W: f32 = 30.0;
const CHIP_W: f32 = 104.0;
const SHA_W: f32 = 68.0;
// ... and from the right edge.
const BUTTONS_W: f32 = 86.0;
const AUTHOR_W: f32 = 150.0;

/// Gives every overlay its own job keys, so an answer meant for one that was
/// closed in the meantime is never taken for the current one's.
static NEXT_VIEW: AtomicU64 = AtomicU64::new(0);

enum State {
    Loading,
    /// The range could not be read at all.
    Failed(String),
    Ready(Plan),
}

/// A row being dragged to a new place.
struct Drag {
    /// Range index of the commit.
    source: usize,
    /// Where inside the row it was grabbed, so the ghost does not jump.
    grab: f32,
}

/// Everything about the overlay that is not the plan itself.
#[derive(Default)]
struct Controls {
    /// Selected commit, by range index (stable while rows move around).
    selected: Option<usize>,
    /// Commits whose message editor is folded away.
    folded: HashSet<usize>,
    /// What git proposed for a squash group when its message was last
    /// edited, by the leading commit's range index. When the proposal
    /// changes afterwards, the typed message may be missing something.
    edited_from: HashMap<usize, String>,
    /// Editor to hand the keyboard to once it is on screen.
    focus: Option<usize>,
    /// Commit to scroll into view.
    reveal: Option<usize>,
    drag: Option<Drag>,
    /// The plan is being checked and written out on the reader thread.
    starting: bool,
    /// Why the last attempt to start was refused.
    error: Option<String>,
    /// The plan was picked up from an overlay that was closed with Escape.
    restored: bool,
    /// Fingerprint of the plan as last saved to the draft slot.
    saved: u64,
}

pub struct RebaseView {
    /// The commits after this one, up to HEAD, are the ones being rebased.
    pub base: Oid,
    base_label: String,
    /// Which overlay this is, for its job keys.
    serial: u64,
    requested: bool,
    state: State,
    // Boxed to keep the overlay enum's variants a similar size.
    controls: Box<Controls>,
}

impl RebaseView {
    pub fn new(base: Oid) -> Self {
        RebaseView {
            base,
            base_label: short(base),
            serial: NEXT_VIEW.fetch_add(1, Ordering::Relaxed),
            requested: false,
            state: State::Loading,
            controls: Box::default(),
        }
    }

    /// Key of the job that reads the range.
    fn range_job(&self) -> String {
        format!("rebase:range:{}", self.serial)
    }

    /// Key of the job that checks the plan and writes it out.
    fn prepare_job(&self) -> String {
        format!("rebase:prepare:{}", self.serial)
    }
}

/// A plan in the making, remembered outside the overlay.
///
/// Escape closes any overlay from the application shell, before this module
/// gets a say, and it is also what people press to leave a text field. The
/// draft lets "Interactive rebase from here" on the same commit pick up
/// where an overlay closed that way left off, instead of losing every
/// message that was typed.
#[derive(Clone)]
struct Draft {
    base: Oid,
    head: Option<Oid>,
    items: Vec<Item>,
    folded: HashSet<usize>,
    edited_from: HashMap<usize, String>,
}

fn draft_id(tab: &RepoTab) -> Id {
    Id::new(("rebase-draft", &tab.path))
}

fn forget_draft(ctx: &egui::Context, tab: &RepoTab) {
    ctx.data_mut(|data| data.remove::<Draft>(draft_id(tab)));
}

fn fingerprint(plan: &Plan) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for item in &plan.items {
        item.source.hash(&mut hasher);
        (item.action as u8).hash(&mut hasher);
        item.reword.hash(&mut hasher);
        item.combined.hash(&mut hasher);
    }
    hasher.finish()
}

/// Whether `items` still describes `range`: every commit exactly once.
fn fits(items: &[Item], range: &Range) -> bool {
    let mut seen = vec![false; range.commits.len()];
    items.len() == seen.len()
        && items.iter().all(|item| {
            seen.get_mut(item.source).is_some_and(|slot| !std::mem::replace(slot, true))
        })
}

enum Outcome {
    Stay,
    Close,
    /// Start again from another base (or the same one, to reload).
    Restart(Oid),
    /// Hand the plan to git.
    Start,
}

/// Returns false when the overlay should close.
pub fn show(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, view: &mut RebaseView) -> bool {
    let ctx = ui.ctx().clone();

    if !view.requested {
        view.requested = true;
        let base = view.base;
        tab.spawn_job(view.range_job(), move |loader| rebase::load_range(loader.repo(), base));
    }
    if matches!(view.state, State::Loading)
        && let Some(result) = tab.take_result::<Result<Range, String>>(&view.range_job())
    {
        view.state = match result {
            Ok(range) => {
                let mut plan = Plan::new(Arc::new(range));
                let untouched = fingerprint(&plan);
                view.controls.saved = untouched;
                let draft = ctx.data(|data| data.get_temp::<Draft>(draft_id(tab)));
                if let Some(draft) = draft
                    && draft.base == plan.range.base
                    && draft.head == plan.range.head.oid
                    && fits(&draft.items, &plan.range)
                {
                    plan.items = draft.items;
                    view.controls.saved = fingerprint(&plan);
                    if view.controls.saved != untouched {
                        view.controls.folded = draft.folded;
                        view.controls.edited_from = draft.edited_from;
                        view.controls.restored = true;
                    }
                }
                State::Ready(plan)
            }
            Err(e) => State::Failed(e),
        };
    }
    if view.controls.starting
        && let Some(result) = tab.take_result::<Result<GitCmd, String>>(&view.prepare_job())
    {
        view.controls.starting = false;
        match result {
            Ok(cmd) => {
                // From here on the toasts and the "Rebase in progress"
                // banner report what happens.
                tab.run(cmd);
                forget_draft(&ctx, tab);
                return false;
            }
            Err(e) => view.controls.error = Some(e),
        }
    }

    let subtitle = match &view.state {
        State::Ready(plan) => {
            let n = plan.items.len();
            let branch = plan.range.head.branch.as_deref().unwrap_or("HEAD");
            format!("{branch}  ·  {n} commit{} after {}", if n == 1 { "" } else { "s" }, view.base_label)
        }
        _ => format!("from {}", view.base_label),
    };
    let (close, _) = widgets::overlay_bar(ui, icons::LIST_NUMBERS, "Interactive rebase", &subtitle);
    if close {
        forget_draft(&ctx, tab);
        return false;
    }

    let RebaseView { state, controls, .. } = view;
    let outcome = match state {
        State::Loading => {
            widgets::placeholder(ui, "Loading commits…");
            Outcome::Stay
        }
        State::Failed(error) => {
            notice(ui, error);
            Outcome::Stay
        }
        State::Ready(plan) => body(ui, tab, shared, plan, controls),
    };
    match outcome {
        Outcome::Stay => true,
        Outcome::Close => {
            forget_draft(&ctx, tab);
            false
        }
        Outcome::Restart(base) => {
            forget_draft(&ctx, tab);
            *view = RebaseView::new(base);
            true
        }
        Outcome::Start => {
            if let State::Ready(plan) = &view.state {
                // Checked against the repository as it is now and written
                // out on the reader thread; the command to run comes back
                // through `take_result` above.
                let plan = plan.clone();
                tab.spawn_job(view.prepare_job(), move |loader| plan.prepare(loader.repo()));
                view.controls.starting = true;
                view.controls.error = None;
            }
            true
        }
    }
}

/// A message in the middle of the overlay, wrapped (git errors can be long).
fn notice(ui: &mut Ui, text: &str) {
    let rect = ui.available_rect_before_wrap();
    let width = (rect.width() - 80.0).clamp(120.0, 620.0);
    let galley = ui.painter().layout(
        format!("{}  {text}", icons::WARNING),
        theme::font(14.0),
        theme::TEXT_DIM,
        width,
    );
    let pos = rect.center() - galley.size() / 2.0;
    ui.painter().galley(pos2(pos.x.round(), pos.y.round()), galley, theme::TEXT_DIM);
    ui.allocate_rect(rect, Sense::hover());
}

fn action_style(action: Action) -> (&'static str, Color32, Key) {
    match action {
        Action::Pick => (icons::CHECK, theme::TEXT_DIM, Key::P),
        Action::Reword => (icons::PENCIL_SIMPLE, theme::BLUE, Key::R),
        Action::Squash => (icons::ARROWS_MERGE, theme::ACCENT, Key::S),
        Action::Fixup => (icons::WRENCH, theme::YELLOW, Key::F),
        Action::Drop => (icons::TRASH, theme::RED, Key::D),
    }
}

/// Which message editor a row carries.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Editor {
    /// The commit's own new message.
    Reword,
    /// One message for the commit and everything squashed into it.
    Combined,
}

/// What the list needs to know about one plan position, worked out once per
/// frame from the plan's groups.
#[derive(Clone, Default)]
struct RowInfo {
    /// Index into the frame's group list, for commits that lead a group.
    group: Option<usize>,
    /// Plan position of the commit this one is folded into.
    into: Option<usize>,
    editor: Option<Editor>,
}

fn row_infos(plan: &Plan, groups: &[Group]) -> Vec<RowInfo> {
    let mut rows = vec![RowInfo::default(); plan.items.len()];
    for (index, group) in groups.iter().enumerate() {
        let leader = &mut rows[group.leader];
        leader.group = Some(index);
        leader.editor = if plan.has_squash(group) {
            Some(Editor::Combined)
        } else if plan.items[group.leader].action == Action::Reword {
            Some(Editor::Reword)
        } else {
            None
        };
        for member in &group.members {
            rows[*member].into = Some(group.leader);
        }
    }
    rows
}

/// A change to the plan asked for while the list was being drawn. Applied
/// afterwards, so the rows of one frame all describe the same plan.
enum Edit {
    Action(usize, Action),
    /// Move the commit at one plan position to another.
    Move { from: usize, to: usize },
    ToggleFold(usize),
}

fn set_action(plan: &mut Plan, controls: &mut Controls, pos: usize, action: Action) {
    plan.items[pos].action = action;
    let source = plan.items[pos].source;
    controls.selected = Some(source);
    match action {
        Action::Reword => {
            controls.folded.remove(&source);
            controls.focus = Some(source);
            controls.reveal = Some(source);
        }
        // Bring the editor for the combined message into the open.
        Action::Squash => {
            let leader = plan.groups().into_iter().find(|g| g.members.contains(&pos)).map(|g| g.leader);
            if let Some(leader) = leader {
                controls.folded.remove(&plan.items[leader].source);
            }
        }
        _ => {}
    }
}

/// Shows or hides the message editor of the commit with range index `source`.
fn toggle_fold(controls: &mut Controls, source: usize) {
    if !controls.folded.remove(&source) {
        controls.folded.insert(source);
    }
}

fn move_item(plan: &mut Plan, controls: &mut Controls, from: usize, to: usize) {
    if from == to || from >= plan.items.len() || to >= plan.items.len() {
        return;
    }
    let item = plan.items.remove(from);
    controls.selected = Some(item.source);
    controls.reveal = Some(item.source);
    plan.items.insert(to, item);
}

fn keyboard(ui: &Ui, plan: &mut Plan, controls: &mut Controls) {
    let n = plan.items.len();
    if n == 0 {
        return;
    }
    // Alt first: a pattern without modifiers also matches Alt+arrow.
    let (shift, step, action) = ui.input_mut(|i| {
        let shift = i.consume_key(Modifiers::ALT, Key::ArrowUp) as i64
            - i.consume_key(Modifiers::ALT, Key::ArrowDown) as i64;
        let step = i.consume_key(Modifiers::NONE, Key::ArrowUp) as i64
            - i.consume_key(Modifiers::NONE, Key::ArrowDown) as i64;
        let mut action = Action::ALL
            .into_iter()
            .find(|action| i.consume_key(Modifiers::NONE, action_style(*action).2));
        if i.consume_key(Modifiers::NONE, Key::Delete) {
            action = Some(Action::Drop);
        }
        (shift, step, action)
    });

    let pos = controls.selected.and_then(|s| plan.items.iter().position(|item| item.source == s));
    // Rows are drawn newest first: "up" is towards the end of the plan.
    if step != 0 {
        let next = match pos {
            Some(pos) => (pos as i64 + step).clamp(0, n as i64 - 1) as usize,
            None => n - 1,
        };
        controls.selected = Some(plan.items[next].source);
        controls.reveal = controls.selected;
    }
    let Some(pos) = pos else { return };
    if shift != 0 {
        let to = (pos as i64 + shift).clamp(0, n as i64 - 1) as usize;
        move_item(plan, controls, pos, to);
    } else if let Some(action) = action {
        set_action(plan, controls, pos, action);
    }
}

fn body(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, plan: &mut Plan, controls: &mut Controls) -> Outcome {
    let ctx = ui.ctx().clone();
    let snap = tab.snap.clone();

    let blocked = plan.range.blocker.is_some();
    let stale = snap.as_ref().is_some_and(|snap| snap.head != plan.range.head);
    // Nothing can be edited while the plan is being handed to git.
    let locked = blocked || controls.starting;

    if !locked
        && shared.dialog.is_none()
        && !ctx.egui_wants_keyboard_input()
        && !Popup::is_any_open(&ctx)
        && controls.drag.is_none()
    {
        keyboard(ui, plan, controls);
    }

    let mut outcome = Outcome::Stay;

    // ---- footer: what will happen, and the buttons --------------------------
    let summary = plan.summary();
    let mut problems = plan.problems();
    if stale && !blocked {
        problems.insert(0, "The branch has moved since these commits were listed.".to_owned());
    }
    // One reason at a time: with merges in the way the tree does not matter yet.
    let tree = snap.as_ref().filter(|_| !blocked).and_then(|snap| rebase::tree_blocker(snap));
    let running = tab.is_running(rebase::LABEL);
    let can_start = problems.is_empty()
        && tree.is_none()
        && !summary.is_noop()
        && !controls.starting
        && !running;
    let mut start = false;

    egui::Panel::bottom("rebase-footer")
        .resizable(false)
        .show_separator_line(false)
        .frame(Frame::new().fill(theme::BG_HEADER).inner_margin(Margin::symmetric(12, 9)))
        .show(ui, |ui| {
            let top = ui.max_rect().top() - 9.0;
            ui.painter().hline(ui.max_rect().x_range().expand(12.0), top, Stroke::new(1.0, theme::BORDER));
            ui.horizontal(|ui| {
                let buttons_w = 230.0;
                let text_w = (ui.available_width() - buttons_w).max(160.0);
                ui.allocate_ui_with_layout(vec2(text_w, 0.0), Layout::top_down(Align::LEFT), |ui| {
                    ui.spacing_mut().item_spacing.y = 5.0;
                    let line = |ui: &mut Ui, icon: &str, color: Color32, text: &str| {
                        ui.add(Label::new(RichText::new(format!("{icon}  {text}")).color(color)).wrap());
                    };

                    if let Some(error) = &controls.error {
                        line(ui, icons::WARNING_CIRCLE, theme::RED, error);
                    }
                    for problem in &problems {
                        line(ui, icons::WARNING, theme::ORANGE, problem);
                    }
                    if stale && !blocked && widgets::tint_button(ui, "Reload the commits", theme::ACCENT).clicked() {
                        outcome = Outcome::Restart(plan.range.base);
                    }
                    if let Some(base) = plan.range.suggested_base {
                        let label = format!("Start from {} instead", short(base));
                        if widgets::tint_button(ui, &label, theme::ACCENT).clicked() {
                            outcome = Outcome::Restart(base);
                        }
                    }
                    if let Some(tree) = &tree {
                        line(ui, icons::WARNING, theme::ORANGE, tree);
                        let stashing = tab.is_running("Stash");
                        let can_stash = snap.as_ref().is_some_and(|s| s.state == crate::git::RepoState::Clean);
                        if can_stash
                            && !stashing
                            && widgets::tint_button(ui, "Stash the changes", theme::ACCENT)
                                .on_hover_text("Saves them as a stash; pop it again after the rebase")
                                .clicked()
                        {
                            tab.run(ops::stash_push("Before interactive rebase"));
                        }
                    }

                    if problems.is_empty() {
                        if summary.is_noop() {
                            line(
                                ui,
                                icons::INFO,
                                theme::TEXT_DIM,
                                "Nothing to do yet. Change an action, edit a message or move a commit.",
                            );
                        } else {
                            let n = plan.items.len();
                            let mut text = format!("{}.", summary.text());
                            if summary.result() != n {
                                let branch = plan.range.head.branch.as_deref().unwrap_or("HEAD");
                                let result = summary.result();
                                text.push_str(&format!(
                                    "  {branch} will have {result} commit{} after {} instead of {n}.",
                                    if result == 1 { "" } else { "s" },
                                    short(plan.range.base),
                                ));
                            }
                            line(ui, icons::LIST_NUMBERS, theme::TEXT_BRIGHT, &text);
                        }
                    }
                    if controls.restored {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("Picked up the plan you had open for these commits.")
                                    .size(12.0)
                                    .color(theme::TEXT_DIM),
                            );
                            if widgets::tint_button(ui, "Start over", theme::TEXT_DIM).clicked() {
                                outcome = Outcome::Restart(plan.range.base);
                            }
                        });
                    }
                });

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let label = if controls.starting || running { "Starting…" } else { "Start rebase" };
                    let fill = if can_start { theme::mix(theme::BG_APP, theme::ACCENT, 0.8) } else { theme::BG_RAISED };
                    let text = RichText::new(format!("{}  {label}", icons::PLAY))
                        .color(if can_start { Color32::WHITE } else { theme::TEXT_DIM });
                    let button = Button::new(text).fill(fill).min_size(vec2(124.0, 30.0));
                    let response = ui
                        .add_enabled(can_start, button)
                        .on_hover_text("Run the rebase (Ctrl+Enter)")
                        .on_disabled_hover_text("Nothing to run yet, or one of the problems listed here is in the way");
                    if response.clicked() {
                        start = true;
                    }
                    if ui.add(Button::new("Cancel").min_size(vec2(76.0, 30.0))).clicked() {
                        outcome = Outcome::Close;
                    }
                });
            });
        });
    if can_start
        && shared.dialog.is_none()
        && ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter))
    {
        start = true;
    }

    // ---- which way is up -----------------------------------------------------
    ui.spacing_mut().item_spacing.y = 0.0;
    let (hint, _) = ui.allocate_exact_size(vec2(ui.available_width(), 46.0), Sense::hover());
    {
        let painter = ui.painter().with_clip_rect(hint);
        painter.rect_filled(hint, 0.0, theme::BG_PANEL);
        let order = format!(
            "{}  Newest commit on top, oldest at the bottom, like the graph. Git applies them from \
             the bottom up, onto {}.",
            icons::INFO,
            short(plan.range.base),
        );
        let keys = "Drag a row, or press Alt+↑ / Alt+↓, to move it.  ↑ / ↓ select a row;  P  R  S  F  D \
                    set its action (pick, reword, squash, fixup, drop).";
        let width = hint.width() - 24.0;
        let (upper, lower) = hint.split_top_bottom_at_y(hint.center().y);
        let galley = widgets::elided(&painter, &order, theme::font(12.0), theme::TEXT, width);
        widgets::galley_left(&painter, upper.translate(vec2(0.0, 3.0)), hint.left() + 12.0, galley);
        let galley = widgets::elided(&painter, keys, theme::font(12.0), theme::TEXT_DIM, width - 20.0);
        widgets::galley_left(&painter, lower.translate(vec2(0.0, -3.0)), hint.left() + 32.0, galley);
    }
    let (caption, _) = ui.allocate_exact_size(vec2(ui.available_width(), CAPTION_H), Sense::hover());
    {
        let painter = ui.painter().with_clip_rect(caption);
        painter.rect_filled(caption, 0.0, theme::BG_HEADER);
        painter.hline(caption.x_range(), caption.bottom() - 0.5, Stroke::new(1.0, theme::BORDER));
        let bar = ui.spacing().scroll.bar_width;
        let label = |x: f32, text: &str| {
            painter.text(pos2(x, caption.center().y), Align2::LEFT_CENTER, text, theme::font(10.5), theme::TEXT_DIM);
        };
        label(caption.left() + HANDLE_W, "ACTION");
        label(caption.left() + HANDLE_W + CHIP_W + 12.0, "SHA");
        label(caption.left() + HANDLE_W + CHIP_W + 12.0 + SHA_W, "COMMIT MESSAGE");
        label(caption.right() - bar - BUTTONS_W - AUTHOR_W, "AUTHOR");
    }

    // ---- the commits ----------------------------------------------------------
    let edit = list(ui, plan, controls, locked);
    match edit {
        Some(Edit::Action(pos, action)) => set_action(plan, controls, pos, action),
        Some(Edit::Move { from, to }) => move_item(plan, controls, from, to),
        Some(Edit::ToggleFold(source)) => toggle_fold(controls, source),
        None => {}
    }

    // Keep the draft in step with the plan (see `Draft`).
    let stamp = fingerprint(plan);
    if stamp != controls.saved {
        controls.saved = stamp;
        controls.error = None;
        let draft = Draft {
            base: plan.range.base,
            head: plan.range.head.oid,
            items: plan.items.clone(),
            folded: controls.folded.clone(),
            edited_from: controls.edited_from.clone(),
        };
        ctx.data_mut(|data| data.insert_temp(draft_id(tab), draft));
    }

    if start && matches!(outcome, Outcome::Stay) {
        outcome = Outcome::Start;
    }
    outcome
}

fn subject(message: &str) -> &str {
    message.lines().next().unwrap_or_default()
}

/// One elided line, optionally struck through.
fn line(painter: &Painter, text: &str, font: FontId, color: Color32, strike: bool, max_width: f32) -> Arc<Galley> {
    let mut format = TextFormat::simple(font, color);
    if strike {
        format.strikethrough = Stroke::new(1.0, color);
    }
    let mut job = LayoutJob::default();
    job.append(text, 0.0, format);
    job.wrap = TextWrapping { max_width: max_width.max(0.0), max_rows: 1, break_anywhere: true, ..Default::default() };
    painter.layout_job(job)
}

/// A small icon button at a fixed place inside a custom-painted row.
fn icon_at(ui: &mut Ui, rect: Rect, salt: (&str, usize), icon: &str, enabled: bool, tooltip: &str) -> bool {
    let response = ui.interact(rect, ui.id().with(salt), if enabled { Sense::click() } else { Sense::hover() });
    let color = if !enabled {
        theme::TEXT_FAINT
    } else if response.hovered() {
        theme::TEXT_BRIGHT
    } else {
        theme::TEXT_DIM
    };
    if enabled && response.is_pointer_button_down_on() {
        ui.painter().rect_filled(rect, 4.0, theme::BG_PRESSED);
    } else if enabled && response.hovered() {
        ui.painter().rect_filled(rect, 4.0, theme::BG_RAISED);
    }
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, icon, theme::font(14.0), color);
    enabled && response.on_hover_text(tooltip).clicked()
}

/// The scrolling list of commits, newest first, with the base underneath.
fn list(ui: &mut Ui, plan: &mut Plan, controls: &mut Controls, locked: bool) -> Option<Edit> {
    let n = plan.items.len();
    let groups = plan.groups();
    let rows = row_infos(plan, &groups);
    let mut edit = None;

    ScrollArea::vertical().id_salt("rebase-rows").auto_shrink([false, false]).show_viewport(ui, |ui, viewport| {
        // Display row `d` shows plan position `n - 1 - d`. Rows differ in
        // height (an open editor hangs below its row), so their offsets are
        // summed up once and only the visible ones are laid out.
        let open: Vec<bool> = (0..n)
            .map(|pos| rows[pos].editor.is_some() && !controls.folded.contains(&plan.items[pos].source))
            .collect();
        let mut tops = Vec::with_capacity(n + 1);
        let mut y = 0.0;
        for d in 0..n {
            tops.push(y);
            y += ROW_H + if open[n - 1 - d] { EDITOR_H } else { 0.0 };
        }
        tops.push(y);

        let (content, _) =
            ui.allocate_exact_size(vec2(ui.available_width(), y + ROW_H + 10.0), Sense::hover());
        let row_rect = |d: usize| {
            Rect::from_min_size(pos2(content.left(), content.top() + tops[d]), vec2(content.width(), ROW_H))
        };

        if let Some(source) = controls.reveal.take()
            && let Some(pos) = plan.items.iter().position(|item| item.source == source)
        {
            let d = n - 1 - pos;
            let block = Rect::from_min_max(row_rect(d).min, pos2(content.right(), content.top() + tops[d + 1]));
            if block.top() < ui.clip_rect().top() || block.bottom() > ui.clip_rect().bottom() {
                ui.scroll_to_rect(block, None);
            }
        }

        let first = tops.partition_point(|top| *top <= viewport.min.y).saturating_sub(1);
        let last = tops[..n].partition_point(|top| *top < viewport.max.y);
        let painter = ui.painter().clone();
        let pointer = ui.input(|i| i.pointer.latest_pos());

        for d in first..last {
            let pos = n - 1 - d;
            let info = &rows[pos];
            let source = plan.items[pos].source;
            let action = plan.items[pos].action;
            let rect = row_rect(d);
            let dragging = controls.drag.as_ref().is_some_and(|drag| drag.source == source);

            let response = ui.interact(rect, ui.id().with(("row", source)), Sense::click_and_drag());
            if response.clicked() {
                controls.selected = Some(source);
            }
            if !locked && response.drag_started() {
                // Where the button went down, not where the pointer has got to
                // by the time the movement counts as a drag.
                let origin = ui.input(|i| i.pointer.press_origin()).or(pointer);
                let grab = origin.map_or(ROW_H / 2.0, |p| (p.y - rect.top()).clamp(0.0, ROW_H));
                controls.drag = Some(Drag { source, grab });
                controls.selected = Some(source);
            }
            let response = if locked { response } else { response.on_hover_cursor(CursorIcon::Grab) };

            widgets::row_background(&painter, rect, controls.selected == Some(source), response.hovered());
            painter.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::BG_PANEL));

            // A squash or fixup with no kept commit below it has nowhere to go.
            let orphan = action.folds() && info.into.is_none();
            let (icon, color, _) = action_style(action);
            let fade = |c: Color32| if dragging { theme::mix(c, theme::BG_APP, 0.6) } else { c };
            let mid = rect.center().y;

            painter.text(
                pos2(rect.left() + 15.0, mid),
                Align2::CENTER_CENTER,
                icons::DOTS_SIX_VERTICAL,
                theme::font(14.0),
                if locked { theme::BG_RAISED } else { theme::TEXT_FAINT },
            );

            // ---- action chip, with its menu ----
            let chip = Rect::from_min_size(pos2(rect.left() + HANDLE_W, mid - 11.0), vec2(CHIP_W, 22.0));
            let chip_response = ui.interact(chip, ui.id().with(("action", source)), Sense::click());
            let strength = if chip_response.hovered() && !locked { 0.34 } else { 0.18 };
            let outline = if orphan { theme::RED } else { theme::mix(theme::BG_APP, color, 0.6) };
            painter.rect(
                chip,
                4.0,
                theme::mix(theme::BG_APP, color, strength),
                Stroke::new(1.0, outline),
                StrokeKind::Inside,
            );
            painter.text(pos2(chip.left() + 8.0, mid), Align2::LEFT_CENTER, icon, theme::font(13.0), fade(color));
            painter.text(
                pos2(chip.left() + 28.0, mid),
                Align2::LEFT_CENTER,
                action.label(),
                theme::font(12.5),
                fade(theme::TEXT_BRIGHT),
            );
            if !locked {
                painter.text(
                    pos2(chip.right() - 12.0, mid),
                    Align2::CENTER_CENTER,
                    icons::CARET_DOWN,
                    theme::font(10.0),
                    theme::TEXT_DIM,
                );
                Popup::menu(&chip_response).show(|ui| {
                    ui.set_min_width(300.0);
                    for choice in Action::ALL {
                        let (icon, _, key) = action_style(choice);
                        let button = Button::new(format!("{icon}  {}  -  {}", choice.label(), choice.explain()))
                            .shortcut_text(key.name());
                        if ui.add(button).clicked() {
                            edit = Some(Edit::Action(pos, choice));
                        }
                    }
                });
            }

            // ---- sha, message, author ----
            let commit = plan.commit(pos);
            let mut x = chip.right() + 12.0;
            painter.text(pos2(x, mid), Align2::LEFT_CENTER, short(commit.oid), theme::mono(11.5), fade(theme::TEXT_DIM));
            x += SHA_W;

            let bar = ui.spacing().scroll.bar_width;
            let buttons_left = rect.right() - bar - BUTTONS_W;
            let author_left = buttons_left - AUTHOR_W;

            if info.into.is_some() || orphan {
                let (arrow, tint) = if orphan { (icons::WARNING, theme::RED) } else { (icons::ARROW_BEND_LEFT_DOWN, color) };
                painter.text(pos2(x, mid), Align2::LEFT_CENTER, arrow, theme::font(13.0), fade(tint));
                x += 20.0;
            } else if commit.is_merge {
                // The reason the range cannot be rebased here.
                painter.text(pos2(x, mid), Align2::LEFT_CENTER, icons::GIT_MERGE, theme::font(13.0), theme::ORANGE);
                x += 20.0;
            }
            // A commit that gets a new message shows that message.
            let new_message = info.group.map(|g| plan.message(&groups[g])).filter(|m| *m != rebase::normalize(&commit.message));
            let (text, text_color, strike) = match (&new_message, action) {
                (Some(message), _) => (subject(message).to_owned(), theme::TEXT_BRIGHT, false),
                (None, Action::Drop) => (commit.summary.clone(), theme::TEXT_FAINT, true),
                (None, Action::Fixup) => (commit.summary.clone(), theme::TEXT_DIM, true),
                (None, Action::Squash) => (commit.summary.clone(), theme::TEXT_DIM, false),
                (None, _) => (commit.summary.clone(), theme::TEXT, false),
            };
            let mut note = String::new();
            if let Some(g) = info.group
                && !groups[g].members.is_empty()
            {
                note = format!("{} commits become this one", groups[g].members.len() + 1);
            }
            if d == 0 {
                note = if note.is_empty() { "newest".to_owned() } else { format!("{note}  ·  newest") };
            } else if d == n - 1 {
                note = if note.is_empty() { "oldest".to_owned() } else { format!("{note}  ·  oldest") };
            }
            let note_color = if controls.selected == Some(source) { theme::TEXT_DIM } else { theme::TEXT_FAINT };
            let note_galley = line(&painter, &note, theme::font(11.0), fade(note_color), false, 220.0);
            let note_w = if note.is_empty() { 0.0 } else { note_galley.size().x + 14.0 };
            let room = (author_left - 10.0 - x - note_w).max(40.0);
            let galley = line(&painter, &text, theme::font(13.0), fade(text_color), strike, room);
            let used = widgets::galley_left(&painter, rect, x, galley);
            if !note.is_empty() {
                widgets::galley_left(&painter, rect, x + used + 14.0, note_galley);
            }

            widgets::avatar(&painter, pos2(author_left + 9.0, mid), 9.0, &commit.author, &commit.email, None);
            let author = line(&painter, &commit.author, theme::font(12.0), fade(theme::TEXT_DIM), false, AUTHOR_W - 34.0);
            widgets::galley_left(&painter, rect, author_left + 26.0, author);

            // ---- fold toggle and move buttons ----
            let slot = |k: f32| Rect::from_center_size(pos2(buttons_left + 14.0 + k * 27.0, mid), vec2(24.0, 24.0));
            if info.editor.is_some() {
                let folded = controls.folded.contains(&source);
                let tip = if folded { "Show the message editor" } else { "Hide the message editor" };
                if icon_at(ui, slot(0.0), ("fold", source), icons::NOTE_PENCIL, true, tip) {
                    edit = Some(Edit::ToggleFold(source));
                }
            }
            if !locked {
                if icon_at(ui, slot(1.0), ("up", source), icons::ARROW_UP, d > 0, "Move up (Alt+↑)") {
                    edit = Some(Edit::Move { from: pos, to: pos + 1 });
                }
                if icon_at(ui, slot(2.0), ("down", source), icons::ARROW_DOWN, d + 1 < n, "Move down (Alt+↓)") {
                    edit = Some(Edit::Move { from: pos, to: pos.saturating_sub(1) });
                }
            }

            // ---- message editor ----
            if let Some(kind) = info.editor
                && open[pos]
                && let Some(g) = info.group
            {
                let block = Rect::from_min_max(rect.left_bottom(), pos2(rect.right() - bar, content.top() + tops[d + 1]));
                editor(ui, plan, controls, &groups[g], kind, block, locked);
            }
        }

        // ---- the base, which stays as it is ----
        let base = Rect::from_min_size(pos2(content.left(), content.top() + tops[n]), vec2(content.width(), ROW_H));
        if ui.is_rect_visible(base) {
            let mid = base.center().y;
            painter.rect_filled(base, 0.0, theme::BG_PANEL);
            painter.text(
                pos2(base.left() + HANDLE_W + 8.0, mid),
                Align2::LEFT_CENTER,
                format!("{}  Base", icons::LOCK_SIMPLE),
                theme::font(12.5),
                theme::TEXT_FAINT,
            );
            let x = base.left() + HANDLE_W + CHIP_W + 12.0;
            painter.text(pos2(x, mid), Align2::LEFT_CENTER, short(plan.range.base), theme::mono(11.5), theme::TEXT_FAINT);
            let text = format!("{}   (not changed; the commits above are replayed onto it)", plan.range.base_summary);
            let galley = line(&painter, &text, theme::font(13.0), theme::TEXT_FAINT, false, base.right() - x - SHA_W - 20.0);
            widgets::galley_left(&painter, base, x + SHA_W, galley);
        }

        // ---- a row on the move ----
        if let Some(drag) = &controls.drag {
            let down = ui.input(|i| i.pointer.primary_down());
            let from = plan.items.iter().position(|item| item.source == drag.source);
            match (from, pointer) {
                (Some(from), Some(pointer)) => {
                    let d = n - 1 - from;
                    // The gap the pointer is closest to: above display row `slot`.
                    let y = pointer.y - content.top();
                    let slot = (0..n).filter(|k| (tops[*k] + tops[*k + 1]) / 2.0 < y).count();
                    let target = if slot > d { slot - 1 } else { slot };
                    if down {
                        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
                        if target != d {
                            let at = content.top() + tops[slot];
                            painter.hline(content.x_range(), at, Stroke::new(2.0, theme::ACCENT));
                            painter.circle_filled(pos2(content.left() + 6.0, at), 4.0, theme::ACCENT);
                        }
                        // The row itself follows the pointer, above everything.
                        let ghost = Rect::from_min_size(
                            pos2(content.left() + 6.0, pointer.y - drag.grab),
                            vec2(content.width() - 12.0 - ui.spacing().scroll.bar_width, ROW_H),
                        );
                        let top = ui.ctx().layer_painter(LayerId::new(Order::Tooltip, Id::new("rebase-drag")));
                        top.rect(
                            ghost,
                            4.0,
                            theme::BG_RAISED,
                            Stroke::new(1.0, theme::ACCENT),
                            StrokeKind::Inside,
                        );
                        let commit = plan.commit(from);
                        let text = format!("{}   {}", short(commit.oid), commit.summary);
                        let galley = line(&top, &text, theme::font(13.0), theme::TEXT_BRIGHT, false, ghost.width() - 24.0);
                        widgets::galley_left(&top, ghost, ghost.left() + 12.0, galley);

                        // Near an edge, scroll so far-away places can be reached.
                        let view = ui.clip_rect();
                        let edge = 36.0;
                        let push = if pointer.y < view.top() + edge {
                            (view.top() + edge - pointer.y).min(edge)
                        } else if pointer.y > view.bottom() - edge {
                            -(pointer.y - (view.bottom() - edge)).min(edge)
                        } else {
                            0.0
                        };
                        if push != 0.0 {
                            ui.scroll_with_delta(vec2(0.0, push * 0.5));
                            ui.ctx().request_repaint();
                        }
                    } else {
                        if target != d {
                            edit = Some(Edit::Move { from, to: n - 1 - target });
                        }
                        controls.drag = None;
                    }
                }
                _ => {
                    if !down {
                        controls.drag = None;
                    }
                }
            }
        }
    });
    edit
}

/// The message editor under a row: the commit's new message, or the one
/// message for a squash group.
fn editor(ui: &mut Ui, plan: &mut Plan, controls: &mut Controls, group: &Group, kind: Editor, block: Rect, locked: bool) {
    let pos = group.leader;
    let source = plan.items[pos].source;
    let (_, color, _) = action_style(if kind == Editor::Combined { Action::Squash } else { Action::Reword });

    let painter = ui.painter().clone();
    painter.rect_filled(block, 0.0, theme::BG_PANEL);
    painter.rect_filled(Rect::from_min_max(block.min, pos2(block.left() + 3.0, block.bottom())), 0.0, color);
    painter.hline(block.x_range(), block.bottom() - 0.5, Stroke::new(1.0, theme::BORDER));

    // What the editor shows until the user types something else.
    let (default, title) = match kind {
        Editor::Reword => (
            rebase::normalize(&plan.commit(pos).message),
            format!("New message for {}", short(plan.commit(pos).oid)),
        ),
        Editor::Combined => (
            plan.default_combined(group),
            format!(
                "One message for the {} commits squashed together (proposed: their messages, as git would)",
                group.members.len() + 1
            ),
        ),
    };
    let default = default.trim_end().to_owned();
    let typed = match kind {
        Editor::Reword => plan.items[pos].reword.clone(),
        Editor::Combined => plan.items[pos].combined.clone(),
    };
    let edited = typed.as_ref().is_some_and(|text| *text != default);
    // The group changed after its message was written by hand.
    let outdated = kind == Editor::Combined
        && edited
        && controls.edited_from.get(&source).is_some_and(|proposal| *proposal != default);
    let mut text = typed.unwrap_or_else(|| default.clone());
    let mut reset = false;

    let inner = Rect::from_min_max(pos2(block.left() + HANDLE_W, block.top() + 6.0), pos2(block.right() - 12.0, block.bottom() - 8.0));
    ui.scope_builder(UiBuilder::new().max_rect(inner).layout(Layout::top_down(Align::LEFT)), |ui| {
        ui.spacing_mut().item_spacing = vec2(8.0, 4.0);
        ui.horizontal(|ui| {
            ui.set_height(22.0);
            ui.label(RichText::new(title).size(12.0).color(theme::TEXT_DIM));
            if outdated {
                ui.label(
                    RichText::new(format!("{}  The commits in this group changed after you wrote this.", icons::WARNING))
                        .size(12.0)
                        .color(theme::ORANGE),
                );
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let label = if kind == Editor::Combined { "Use the proposal" } else { "Original message" };
                if edited && !locked && widgets::tint_button(ui, label, theme::TEXT_DIM).clicked() {
                    reset = true;
                }
            });
        });
        ScrollArea::vertical().id_salt(("rebase-editor", source)).auto_shrink([false, false]).show(ui, |ui| {
            // At least as tall as the block, so a click anywhere in it types.
            let fill = vec2(ui.available_width(), ui.available_height() - 2.0);
            let response = ui.add_enabled(
                !locked,
                TextEdit::multiline(&mut text)
                    .id(Id::new(("rebase-message", source)))
                    .hint_text("Commit message")
                    .desired_rows(4)
                    .desired_width(f32::INFINITY)
                    .min_size(fill),
            );
            // Asked for after the widget ran, so the key that opened the
            // editor ("R") is not typed into it.
            if controls.focus == Some(source) {
                controls.focus = None;
                response.request_focus();
            }
            if response.changed() {
                let item = &mut plan.items[pos];
                match kind {
                    Editor::Reword => item.reword = Some(text.clone()),
                    Editor::Combined => {
                        item.combined = Some(text.clone());
                        controls.edited_from.insert(source, default.clone());
                    }
                }
            }
        });
    });

    if reset {
        let item = &mut plan.items[pos];
        match kind {
            Editor::Reword => item.reword = None,
            Editor::Combined => {
                item.combined = None;
                controls.edited_from.remove(&source);
            }
        }
    }
}
