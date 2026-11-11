//! Interactive rebase: choose what happens to each commit, then run it.
//!
//! The list reads like the graph: newest commit on top, oldest at the bottom,
//! with the base commit (which is not touched) underneath. The plan itself
//! lives in `git::rebase` and is kept oldest first, the order git works in;
//! this module only turns row positions around when it draws.
//!
//! Merge commits are rows like the others. Under each one, indented, are the
//! commits it brings in, one group per side (a line of the plan): they take
//! actions like any commit and move within their side. A merge's caret folds
//! its sides away; big ones start folded. While a merge is dropped, the rows
//! under it are dropped with it and cannot be changed; while it is folded
//! into the commit below it, they are replayed for their changes only.
//! Actions a row does not allow are disabled, with the reason in the menu
//! (`Plan::refusal`).

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
use crate::git::rebase::{self, Action, Fate, Group, Item, Plan, Range};
use crate::git::{Oid, short};
use crate::state::{RepoTab, Shared};
use crate::theme::{self, icons};
use crate::ui::widgets;

const ROW_H: f32 = 30.0;
/// Height of the heading over the commits a merge brings in.
const SIDE_H: f32 = 24.0;
/// Height of the message editor that opens under a row.
const EDITOR_H: f32 = 156.0;
const CAPTION_H: f32 = 24.0;
/// How far the commits a merge brings in are indented, per level.
const INDENT: f32 = 22.0;
/// Merges that bring in more commits than this start folded away.
const FOLDED_OVER: usize = 12;

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
    /// Merges whose sides are hidden, by range index.
    collapsed: HashSet<usize>,
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
    /// The plan's problems, worked out when its fingerprint last changed.
    problems: Option<(u64, Vec<String>)>,
    /// Commits to set to squash together once the range is loaded, oldest
    /// first (see [`RebaseView::squash`]).
    squash: Option<Vec<Oid>>,
    /// Said above the buttons whatever the plan (commits already pushed).
    warning: Option<String>,
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

    /// The planner with `run` (oldest first, from `git::multi::check_squash`)
    /// set to be squashed together and everything else picked, the combined
    /// message ready to edit, and `warning` shown above its buttons.
    pub fn squash(base: Oid, run: Vec<Oid>, warning: Option<String>) -> Self {
        let mut view = RebaseView::new(base);
        view.controls.squash = Some(run);
        view.controls.warning = warning;
        view
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
    lines: Vec<Vec<Item>>,
    folded: HashSet<usize>,
    collapsed: HashSet<usize>,
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
    for (line, items) in plan.lines.iter().enumerate() {
        line.hash(&mut hasher);
        for item in items {
            item.source.hash(&mut hasher);
            (item.action as u8).hash(&mut hasher);
            item.reword.hash(&mut hasher);
            item.combined.hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// Whether `lines` still describes `range`: each line's commits exactly
/// once, on that line.
fn fits(lines: &[Vec<Item>], range: &Range) -> bool {
    lines.len() == range.lines.len()
        && lines.iter().zip(&range.lines).all(|(items, line)| {
            let mut planned: Vec<usize> = items.iter().map(|item| item.source).collect();
            planned.sort_unstable();
            let mut there = line.commits.clone();
            there.sort_unstable();
            planned == there
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
                // Big merges start folded away.
                view.controls.collapsed = plan
                    .range
                    .sides
                    .keys()
                    .copied()
                    .filter(|merge| plan.range.brought_in(*merge) > FOLDED_OVER)
                    .collect();
                // A squash asked for from the graph starts afresh.
                let draft = ctx
                    .data(|data| data.get_temp::<Draft>(draft_id(tab)))
                    .filter(|_| view.controls.squash.is_none());
                if let Some(draft) = draft
                    && draft.base == plan.range.base
                    && draft.head == plan.range.head.oid
                    && fits(&draft.lines, &plan.range)
                {
                    plan.lines = draft.lines;
                    view.controls.saved = fingerprint(&plan);
                    view.controls.collapsed = draft.collapsed;
                    if view.controls.saved != untouched {
                        view.controls.folded = draft.folded;
                        view.controls.edited_from = draft.edited_from;
                        view.controls.restored = true;
                    }
                }
                if let Some(run) = view.controls.squash.take()
                    && let Some((line, pos)) = crate::git::multi::squash_plan(&mut plan, &run)
                {
                    let source = plan.lines[line][pos].source;
                    view.controls.selected = Some(source);
                    view.controls.reveal = Some(source);
                    view.controls.focus = Some(source);
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
            let n = plan.range.commits.len();
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
        theme::text_dim(),
        width,
    );
    let pos = rect.center() - galley.size() / 2.0;
    ui.painter().galley(pos2(pos.x.round(), pos.y.round()), galley, theme::text_dim());
    ui.allocate_rect(rect, Sense::hover());
}

fn action_style(action: Action) -> (&'static str, Color32, Key) {
    match action {
        Action::Pick => (icons::CHECK, theme::text_dim(), Key::P),
        Action::Reword => (icons::PENCIL_SIMPLE, theme::blue(), Key::R),
        Action::Squash => (icons::ARROWS_MERGE, theme::accent(), Key::S),
        Action::Fixup => (icons::WRENCH, theme::yellow(), Key::F),
        Action::Drop => (icons::TRASH, theme::red(), Key::D),
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
    /// Index into the line's group list, for commits that lead a group.
    group: Option<usize>,
    /// Position in the line of the commit this one is folded into.
    into: Option<usize>,
    editor: Option<Editor>,
}

fn row_infos(plan: &Plan, line: usize, groups: &[Group], fate: Fate) -> Vec<RowInfo> {
    let mut rows = vec![RowInfo::default(); plan.lines[line].len()];
    for (index, group) in groups.iter().enumerate() {
        let leader = &mut rows[group.leader];
        leader.group = Some(index);
        // Messages of commits that leave the history with a folded merge
        // are not kept, so they get no editor.
        leader.editor = if fate != Fate::Kept {
            None
        } else if plan.has_squash(group) {
            Some(Editor::Combined)
        } else if plan.lines[line][group.leader].action == Action::Reword {
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

/// One row of the list, newest first.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Row {
    /// A commit: its line and position in the plan, and how deep it is
    /// nested (0 for the branch's own line).
    Item { line: usize, pos: usize, depth: usize },
    /// The heading over the commits a merge brings in through one parent.
    Side { line: usize, depth: usize },
}

impl Row {
    fn depth(self) -> usize {
        match self {
            Row::Item { depth, .. } | Row::Side { depth, .. } => depth,
        }
    }
}

/// The rows of the list: the branch's own line, newest first, each merge
/// followed by its sides unless it is folded away.
fn display_rows(plan: &Plan, collapsed: &HashSet<usize>) -> Vec<Row> {
    let mut rows = Vec::new();
    push_line(plan, 0, 0, collapsed, &mut rows);
    rows
}

fn push_line(plan: &Plan, line: usize, depth: usize, collapsed: &HashSet<usize>, rows: &mut Vec<Row>) {
    for pos in (0..plan.lines[line].len()).rev() {
        rows.push(Row::Item { line, pos, depth });
        let source = plan.lines[line][pos].source;
        if collapsed.contains(&source) {
            continue;
        }
        for side in plan.range.sides.get(&source).into_iter().flatten() {
            rows.push(Row::Side { line: *side, depth: depth + 1 });
            push_line(plan, *side, depth + 1, collapsed, rows);
        }
    }
}

/// A change to the plan asked for while the list was being drawn. Applied
/// afterwards, so the rows of one frame all describe the same plan.
enum Edit {
    Action(usize, usize, Action),
    /// Move the commit at one position of a line to another.
    Move { line: usize, from: usize, to: usize },
    ToggleFold(usize),
    /// Show or hide the sides of a merge, by range index.
    ToggleSides(usize),
}

/// Why the commits of `line` cannot be changed while their merge is
/// dropped, if it is.
fn dropped_with_merge(plan: &Plan, line: usize) -> Option<String> {
    (plan.fate(line) == Fate::Dropped).then(|| {
        "It is dropped with the merge that brings it in. Pick that merge again to change it.".to_owned()
    })
}

fn set_action(plan: &mut Plan, controls: &mut Controls, line: usize, pos: usize, action: Action) {
    let source = plan.lines[line][pos].source;
    controls.selected = Some(source);
    // Only reachable from the keyboard: the menu disables these.
    let commit = plan.commit(line, pos);
    let why = dropped_with_merge(plan, line).or_else(|| plan.refusal(line, action).map(capitalised));
    if let Some(reason) = why {
        controls.error = Some(format!("{} \"{}\": {reason}", short(commit.oid), commit.summary));
        return;
    }
    plan.lines[line][pos].action = action;
    match action {
        Action::Reword => {
            controls.folded.remove(&source);
            controls.focus = Some(source);
            controls.reveal = Some(source);
        }
        // Bring the editor for the combined message into the open.
        Action::Squash => {
            let leader = plan.groups(line).into_iter().find(|g| g.members.contains(&pos)).map(|g| g.leader);
            if let Some(leader) = leader {
                controls.folded.remove(&plan.lines[line][leader].source);
            }
        }
        _ => {}
    }
}

/// Shows or hides the message editor of the commit with range index `source`.
fn toggle(set: &mut HashSet<usize>, source: usize) {
    if !set.remove(&source) {
        set.insert(source);
    }
}

fn move_item(plan: &mut Plan, controls: &mut Controls, line: usize, from: usize, to: usize) {
    let items = &mut plan.lines[line];
    if from == to || from >= items.len() || to >= items.len() {
        return;
    }
    let item = items.remove(from);
    controls.selected = Some(item.source);
    controls.reveal = Some(item.source);
    items.insert(to, item);
}

fn keyboard(ui: &Ui, plan: &mut Plan, controls: &mut Controls, rows: &[Row]) {
    // The commits as listed, newest first.
    let listed: Vec<(usize, usize)> = rows
        .iter()
        .filter_map(|row| match *row {
            Row::Item { line, pos, .. } => Some((line, pos)),
            Row::Side { .. } => None,
        })
        .collect();
    if listed.is_empty() {
        return;
    }
    // Alt first: a pattern without modifiers also matches Alt+arrow.
    let (shift, step, sides, action) = ui.input_mut(|i| {
        let shift = i.consume_key(Modifiers::ALT, Key::ArrowUp) as i64
            - i.consume_key(Modifiers::ALT, Key::ArrowDown) as i64;
        let step = i.consume_key(Modifiers::NONE, Key::ArrowUp) as i64
            - i.consume_key(Modifiers::NONE, Key::ArrowDown) as i64;
        let sides = i.consume_key(Modifiers::NONE, Key::ArrowRight) as i64
            - i.consume_key(Modifiers::NONE, Key::ArrowLeft) as i64;
        let mut action = Action::ALL
            .into_iter()
            .find(|action| i.consume_key(Modifiers::NONE, action_style(*action).2));
        if i.consume_key(Modifiers::NONE, Key::Delete) {
            action = Some(Action::Drop);
        }
        (shift, step, sides, action)
    });

    let at = controls.selected.and_then(|s| listed.iter().position(|(line, pos)| plan.lines[*line][*pos].source == s));
    if step != 0 {
        // Rows are listed newest first: "up" is towards the start.
        let next = match at {
            Some(at) => (at as i64 - step).clamp(0, listed.len() as i64 - 1) as usize,
            None => 0,
        };
        let (line, pos) = listed[next];
        controls.selected = Some(plan.lines[line][pos].source);
        controls.reveal = controls.selected;
    }
    let Some((line, pos)) = at.map(|at| listed[at]) else { return };
    let source = plan.lines[line][pos].source;
    if sides != 0 && plan.range.sides.contains_key(&source) {
        // Right shows what a merge brings in, left hides it.
        if sides > 0 {
            controls.collapsed.remove(&source);
        } else {
            controls.collapsed.insert(source);
        }
    }
    if shift != 0 {
        if dropped_with_merge(plan, line).is_none() {
            let to = (pos as i64 + shift).clamp(0, plan.lines[line].len() as i64 - 1) as usize;
            move_item(plan, controls, line, pos, to);
        }
    } else if let Some(action) = action {
        set_action(plan, controls, line, pos, action);
    }
}

fn body(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, plan: &mut Plan, controls: &mut Controls) -> Outcome {
    let ctx = ui.ctx().clone();
    let snap = tab.snap.clone();

    let stale = snap.as_ref().is_some_and(|snap| snap.head != plan.range.head);
    // Nothing can be edited while the plan is being handed to git.
    let locked = controls.starting;

    let rows = display_rows(plan, &controls.collapsed);
    if !locked
        && shared.dialog.is_none()
        && !ctx.egui_wants_keyboard_input()
        && !Popup::is_any_open(&ctx)
        && controls.drag.is_none()
    {
        keyboard(ui, plan, controls, &rows);
    }
    // The keys may have changed the plan or what is folded away.
    let rows = display_rows(plan, &controls.collapsed);

    let mut outcome = Outcome::Stay;

    // ---- footer: what will happen, and the buttons --------------------------
    let summary = plan.summary();
    // Working the problems out writes the whole todo list: only when the
    // plan changed.
    let stamp = fingerprint(plan);
    if controls.problems.as_ref().is_none_or(|(at, _)| *at != stamp) {
        controls.problems = Some((stamp, plan.problems()));
    }
    let mut problems = controls.problems.as_ref().map(|(_, p)| p.clone()).unwrap_or_default();
    if stale {
        problems.insert(0, "The branch has moved since these commits were listed.".to_owned());
    }
    let tree = snap.as_ref().and_then(|snap| rebase::tree_blocker(snap));
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
        .frame(Frame::new().fill(theme::bg_header()).inner_margin(Margin::symmetric(12, 9)))
        .show(ui, |ui| {
            let top = ui.max_rect().top() - 9.0;
            ui.painter().hline(ui.max_rect().x_range().expand(12.0), top, Stroke::new(1.0, theme::border()));
            ui.horizontal(|ui| {
                let buttons_w = 230.0;
                let text_w = (ui.available_width() - buttons_w).max(160.0);
                ui.allocate_ui_with_layout(vec2(text_w, 0.0), Layout::top_down(Align::LEFT), |ui| {
                    ui.spacing_mut().item_spacing.y = 5.0;
                    let line = |ui: &mut Ui, icon: &str, color: Color32, text: &str| {
                        ui.add(Label::new(RichText::new(format!("{icon}  {text}")).color(color)).wrap());
                    };

                    if let Some(error) = &controls.error {
                        line(ui, icons::WARNING_CIRCLE, theme::red(), error);
                    }
                    if let Some(warning) = &controls.warning {
                        line(ui, icons::WARNING, theme::orange(), warning);
                    }
                    for problem in &problems {
                        line(ui, icons::WARNING, theme::orange(), problem);
                    }
                    if stale && widgets::tint_button(ui, "Reload the commits", theme::accent()).clicked() {
                        outcome = Outcome::Restart(plan.range.base);
                    }
                    if let Some(tree) = &tree {
                        line(ui, icons::WARNING, theme::orange(), tree);
                        let stashing = tab.is_running("Stash");
                        let can_stash = snap.as_ref().is_some_and(|s| s.state == crate::git::RepoState::Clean);
                        if can_stash
                            && !stashing
                            && widgets::tint_button(ui, "Stash the changes", theme::accent())
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
                                theme::text_dim(),
                                "Nothing to do yet. Change an action, edit a message or move a commit.",
                            );
                        } else {
                            let n = plan.range.commits.len();
                            let mut text = format!("{}.", summary.text());
                            let result = summary.result();
                            if result != n {
                                text.push_str(&format!("  The {n} commits listed become {result}."));
                            }
                            line(ui, icons::LIST_NUMBERS, theme::text_bright(), &text);
                            let remade = plan.remade_merges();
                            if remade > 0 {
                                let what = if remade == 1 {
                                    "1 merge is made again because what it merges changes".to_owned()
                                } else {
                                    format!("{remade} merges are made again because what they merge changes")
                                };
                                line(
                                    ui,
                                    icons::GIT_MERGE,
                                    theme::text_dim(),
                                    &format!("{what}. A merge that needed conflicts resolved stops for them again."),
                                );
                            }
                        }
                    }
                    if controls.restored {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("Picked up the plan you had open for these commits.")
                                    .size(12.0)
                                    .color(theme::text_dim()),
                            );
                            if widgets::tint_button(ui, "Start over", theme::text_dim()).clicked() {
                                outcome = Outcome::Restart(plan.range.base);
                            }
                        });
                    }
                });

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let label = if controls.starting || running { "Starting…" } else { "Start rebase" };
                    let fill = if can_start { theme::solid(theme::accent(), 0.8) } else { theme::bg_raised() };
                    let text = RichText::new(format!("{}  {label}", icons::PLAY))
                        .color(if can_start { theme::on_accent() } else { theme::text_dim() });
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
        painter.rect_filled(hint, 0.0, theme::bg_panel());
        let mut order = format!(
            "{}  Newest commit on top, oldest at the bottom, like the graph. Git applies them from \
             the bottom up, onto {}.",
            icons::INFO,
            short(plan.range.base),
        );
        if plan.range.has_merges() {
            order.push_str(" Under each merge, the commits it brings in.");
        }
        let keys = "Drag a row, or press Alt+↑ / Alt+↓, to move it.  ↑ / ↓ select a row;  P  R  S  F  D \
                    set its action (pick, reword, squash, fixup, drop);  ← / → hide or show what a merge brings in.";
        let width = hint.width() - 24.0;
        let (upper, lower) = hint.split_top_bottom_at_y(hint.center().y);
        let galley = widgets::elided(&painter, &order, theme::font(12.0), theme::text(), width);
        widgets::galley_left(&painter, upper.translate(vec2(0.0, 3.0)), hint.left() + 12.0, galley);
        let galley = widgets::elided(&painter, keys, theme::font(12.0), theme::text_dim(), width - 20.0);
        widgets::galley_left(&painter, lower.translate(vec2(0.0, -3.0)), hint.left() + 32.0, galley);
    }
    let (caption, _) = ui.allocate_exact_size(vec2(ui.available_width(), CAPTION_H), Sense::hover());
    {
        let painter = ui.painter().with_clip_rect(caption);
        painter.rect_filled(caption, 0.0, theme::bg_header());
        painter.hline(caption.x_range(), caption.bottom() - 0.5, Stroke::new(1.0, theme::border()));
        let bar = ui.spacing().scroll.bar_width;
        let label = |x: f32, text: &str| {
            painter.text(pos2(x, caption.center().y), Align2::LEFT_CENTER, text, theme::font(10.5), theme::text_dim());
        };
        label(caption.left() + HANDLE_W, "ACTION");
        label(caption.left() + HANDLE_W + CHIP_W + 12.0, "SHA");
        label(caption.left() + HANDLE_W + CHIP_W + 12.0 + SHA_W, "COMMIT MESSAGE");
        label(caption.right() - bar - BUTTONS_W - AUTHOR_W, "AUTHOR");
    }

    // ---- the commits ----------------------------------------------------------
    let edit = list(ui, plan, controls, &rows, locked);
    match edit {
        Some(Edit::Action(line, pos, action)) => set_action(plan, controls, line, pos, action),
        Some(Edit::Move { line, from, to }) => move_item(plan, controls, line, from, to),
        Some(Edit::ToggleFold(source)) => toggle(&mut controls.folded, source),
        Some(Edit::ToggleSides(source)) => toggle(&mut controls.collapsed, source),
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
            lines: plan.lines.clone(),
            folded: controls.folded.clone(),
            collapsed: controls.collapsed.clone(),
            edited_from: controls.edited_from.clone(),
        };
        ctx.data_mut(|data| data.insert_temp(draft_id(tab), draft));
    }

    if start && matches!(outcome, Outcome::Stay) {
        outcome = Outcome::Start;
    }
    outcome
}

/// The action menu's explanations, for a merge commit.
fn merge_explain(action: Action) -> &'static str {
    match action {
        Action::Pick => "Keep the merge and what it brings in",
        Action::Reword => "Keep the merge, change its message",
        Action::Squash => "Fold into the older commit below, combine the messages; no longer a merge",
        Action::Fixup => "Fold into the older commit below, discard this message; no longer a merge",
        Action::Drop => "Remove the merge and the commits it brings in",
    }
}

fn capitalised(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|first| first.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

fn subject(message: &str) -> &str {
    message.lines().next().unwrap_or_default()
}

fn commits(n: usize) -> String {
    if n == 1 { "1 commit".to_owned() } else { format!("{n} commits") }
}

/// One elided line, optionally struck through.
fn one_line(painter: &Painter, text: &str, font: FontId, color: Color32, strike: bool, max_width: f32) -> Arc<Galley> {
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
        theme::text_faint()
    } else if response.hovered() {
        theme::text_bright()
    } else {
        theme::text_dim()
    };
    if enabled && response.is_pointer_button_down_on() {
        ui.painter().rect_filled(rect, 4.0, theme::bg_pressed());
    } else if enabled && response.hovered() {
        ui.painter().rect_filled(rect, 4.0, theme::bg_raised());
    }
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, icon, theme::font(14.0), color);
    enabled && response.on_hover_text(tooltip).clicked()
}

/// The guides that tie the commits a merge brings in to it: one line per
/// level of nesting, down the left of the row.
fn guides(painter: &Painter, rect: Rect, depth: usize) {
    for level in 1..=depth {
        let x = rect.left() + 15.0 + (level - 1) as f32 * INDENT + INDENT / 2.0;
        painter.vline(x, rect.y_range(), Stroke::new(1.0, theme::mix(theme::bg_app(), theme::accent(), 0.45)));
    }
}

/// How a side's heading names where it starts.
fn start_text(range: &Range, line: usize) -> String {
    match range.lines[line].start {
        None => "with a history of its own".to_owned(),
        Some(oid) if oid == range.base => format!("branching off from {} \"{}\"", short(oid), range.base_summary),
        Some(oid) => match range.index_of(oid) {
            Some(i) => format!("branching off from {} \"{}\"", short(oid), range.commits[i].summary),
            None => format!("branching off from {}", short(oid)),
        },
    }
}

/// The scrolling list of commits, newest first, with the base underneath.
fn list(ui: &mut Ui, plan: &mut Plan, controls: &mut Controls, rows: &[Row], locked: bool) -> Option<Edit> {
    let lines = plan.lines.len();
    let fates: Vec<Fate> = (0..lines).map(|line| plan.fate(line)).collect();
    let groups: Vec<Vec<Group>> = (0..lines).map(|line| plan.groups(line)).collect();
    let infos: Vec<Vec<RowInfo>> = (0..lines).map(|line| row_infos(plan, line, &groups[line], fates[line])).collect();
    let n = rows.len();
    let mut edit = None;

    ScrollArea::vertical().id_salt("rebase-rows").auto_shrink([false, false]).show_viewport(ui, |ui, viewport| {
        // Rows differ in height (an open editor hangs below its row), so
        // their offsets are summed up once and only the visible ones are
        // laid out.
        let sources: Vec<Option<usize>> = rows
            .iter()
            .map(|row| match *row {
                Row::Item { line, pos, .. } => Some(plan.lines[line][pos].source),
                Row::Side { .. } => None,
            })
            .collect();
        let open: Vec<bool> = rows
            .iter()
            .zip(&sources)
            .map(|(row, source)| match (*row, source) {
                (Row::Item { line, pos, .. }, Some(source)) => {
                    infos[line][pos].editor.is_some() && !controls.folded.contains(source)
                }
                _ => false,
            })
            .collect();
        let mut tops = Vec::with_capacity(n + 1);
        let mut y = 0.0;
        for (d, row) in rows.iter().enumerate() {
            tops.push(y);
            y += match row {
                Row::Item { .. } => ROW_H + if open[d] { EDITOR_H } else { 0.0 },
                Row::Side { .. } => SIDE_H,
            };
        }
        tops.push(y);
        // Where the rows that belong to row `d` (its editor, and for a merge
        // the commits it brings in) end.
        let block_end = |d: usize| {
            let depth = rows[d].depth();
            tops[(d + 1..n).find(|k| rows[*k].depth() <= depth).unwrap_or(n)]
        };

        let (content, _) =
            ui.allocate_exact_size(vec2(ui.available_width(), y + ROW_H + 10.0), Sense::hover());
        let row_rect = |d: usize, height: f32| {
            Rect::from_min_size(pos2(content.left(), content.top() + tops[d]), vec2(content.width(), height))
        };

        if let Some(source) = controls.reveal.take()
            && let Some(d) = sources.iter().position(|s| *s == Some(source))
        {
            let block = Rect::from_min_max(row_rect(d, ROW_H).min, pos2(content.right(), content.top() + tops[d + 1]));
            if block.top() < ui.clip_rect().top() || block.bottom() > ui.clip_rect().bottom() {
                ui.scroll_to_rect(block, None);
            }
        }

        let first = tops.partition_point(|top| *top <= viewport.min.y).saturating_sub(1);
        let last = tops[..n].partition_point(|top| *top < viewport.max.y);
        let painter = ui.painter().clone();
        let pointer = ui.input(|i| i.pointer.latest_pos());
        let bar = ui.spacing().scroll.bar_width;
        let main_len = plan.lines[0].len();

        for d in first..last {
            let row = rows[d];
            let depth = row.depth();
            let indent = depth as f32 * INDENT;
            let (line, pos) = match row {
                Row::Side { line, .. } => {
                    side_heading(&painter, plan, row_rect(d, SIDE_H), line, depth, fates[line]);
                    continue;
                }
                Row::Item { line, pos, .. } => (line, pos),
            };
            let info = &infos[line][pos];
            let fate = fates[line];
            let item = &plan.lines[line][pos];
            let (source, action) = (item.source, item.action);
            let gone = fate == Fate::Dropped;
            // Rows dropped with their merge keep their actions for when it
            // is picked again, but cannot be changed meanwhile.
            let frozen = locked || gone;
            let rect = row_rect(d, ROW_H);
            let dragging = controls.drag.as_ref().is_some_and(|drag| drag.source == source);

            let response = ui.interact(rect, ui.id().with(("row", source)), Sense::click_and_drag());
            if response.clicked() {
                controls.selected = Some(source);
            }
            if !frozen && response.drag_started() {
                // Where the button went down, not where the pointer has got to
                // by the time the movement counts as a drag.
                let origin = ui.input(|i| i.pointer.press_origin()).or(pointer);
                let grab = origin.map_or(ROW_H / 2.0, |p| (p.y - rect.top()).clamp(0.0, ROW_H));
                controls.drag = Some(Drag { source, grab });
                controls.selected = Some(source);
            }
            let response = if frozen { response } else { response.on_hover_cursor(CursorIcon::Grab) };
            let response = match dropped_with_merge(plan, line) {
                Some(why) if controls.drag.is_none() => response.on_hover_text(why),
                _ => response,
            };

            widgets::row_background(&painter, rect, controls.selected == Some(source), response.hovered());
            painter.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::bg_panel()));
            guides(&painter, rect, depth);

            // A squash or fixup with no kept commit below it has nowhere to go.
            let orphan = action.folds() && info.into.is_none() && !gone;
            let (icon, color, _) = action_style(if gone { Action::Drop } else { action });
            let fade = |c: Color32| if dragging || gone { theme::mix(c, theme::bg_app(), 0.6) } else { c };
            let mid = rect.center().y;
            let left = rect.left() + indent;

            painter.text(
                pos2(left + 15.0, mid),
                Align2::CENTER_CENTER,
                icons::DOTS_SIX_VERTICAL,
                theme::font(14.0),
                if frozen { theme::bg_raised() } else { theme::text_faint() },
            );

            // ---- action chip, with its menu ----
            let chip = Rect::from_min_size(pos2(left + HANDLE_W, mid - 11.0), vec2(CHIP_W, 22.0));
            let chip_response = ui.interact(chip, ui.id().with(("action", source)), Sense::click());
            let strength = if chip_response.hovered() && !frozen { 0.34 } else { 0.18 };
            let outline = if orphan { theme::red() } else { theme::mix(theme::bg_app(), color, 0.6) };
            painter.rect(
                chip,
                4.0,
                theme::mix(theme::bg_app(), color, strength),
                Stroke::new(1.0, outline),
                StrokeKind::Inside,
            );
            painter.text(pos2(chip.left() + 8.0, mid), Align2::LEFT_CENTER, icon, theme::font(13.0), fade(color));
            painter.text(
                pos2(chip.left() + 28.0, mid),
                Align2::LEFT_CENTER,
                if gone { Action::Drop.label() } else { action.label() },
                theme::font(12.5),
                fade(theme::text_bright()),
            );
            let commit = plan.commit(line, pos);
            if !frozen {
                painter.text(
                    pos2(chip.right() - 12.0, mid),
                    Align2::CENTER_CENTER,
                    icons::CARET_DOWN,
                    theme::font(10.0),
                    theme::text_dim(),
                );
                let is_merge = commit.is_merge;
                Popup::menu(&chip_response).show(|ui| {
                    ui.set_min_width(300.0);
                    for choice in Action::ALL {
                        let (icon, _, key) = action_style(choice);
                        let explain = if is_merge { merge_explain(choice) } else { choice.explain() };
                        let button = Button::new(format!("{icon}  {}  -  {explain}", choice.label()))
                            .shortcut_text(key.name());
                        let refusal = plan.refusal(line, choice);
                        let response = ui.add_enabled(refusal.is_none(), button);
                        if let Some(reason) = refusal {
                            response.on_disabled_hover_text(capitalised(reason));
                        } else if response.clicked() {
                            edit = Some(Edit::Action(line, pos, choice));
                        }
                    }
                });
            }

            // ---- sha, message, author ----
            let mut x = chip.right() + 12.0;
            painter.text(pos2(x, mid), Align2::LEFT_CENTER, short(commit.oid), theme::mono(11.5), fade(theme::text_dim()));
            x += SHA_W;

            let buttons_left = rect.right() - bar - BUTTONS_W;
            let author_left = buttons_left - AUTHOR_W;
            let has_sides = plan.range.sides.contains_key(&source);

            if has_sides {
                // The caret shows or hides what the merge brings in.
                let collapsed = controls.collapsed.contains(&source);
                let caret = if collapsed { icons::CARET_RIGHT } else { icons::CARET_DOWN };
                let tip = if collapsed { "Show the commits it brings in (→)" } else { "Hide the commits it brings in (←)" };
                let slot = Rect::from_center_size(pos2(x + 8.0, mid), vec2(18.0, 20.0));
                if icon_at(ui, slot, ("sides", source), caret, true, tip) {
                    edit = Some(Edit::ToggleSides(source));
                }
                x += 20.0;
            }
            if info.into.is_some() || orphan {
                let (arrow, tint) = if orphan { (icons::WARNING, theme::red()) } else { (icons::ARROW_BEND_LEFT_DOWN, color) };
                painter.text(pos2(x, mid), Align2::LEFT_CENTER, arrow, theme::font(13.0), fade(tint));
                x += 20.0;
            } else if commit.is_merge {
                painter.text(pos2(x, mid), Align2::LEFT_CENTER, icons::GIT_MERGE, theme::font(13.0), fade(theme::accent()));
                x += 20.0;
            }
            // A commit that gets a new message shows that message.
            let new_message = info
                .group
                .filter(|_| fate == Fate::Kept)
                .map(|g| plan.message(&groups[line][g]))
                .filter(|m| *m != rebase::normalize(&commit.message));
            let (text, text_color, strike) = match (&new_message, action) {
                _ if gone => (commit.summary.clone(), theme::text_faint(), true),
                (Some(message), _) => (subject(message).to_owned(), theme::text_bright(), false),
                (None, Action::Drop) => (commit.summary.clone(), theme::text_faint(), true),
                (None, Action::Fixup) => (commit.summary.clone(), theme::text_dim(), true),
                (None, Action::Squash) => (commit.summary.clone(), theme::text_dim(), false),
                (None, _) if fate == Fate::Folded => (commit.summary.clone(), theme::text_dim(), false),
                (None, _) => (commit.summary.clone(), theme::text(), false),
            };
            let mut notes: Vec<String> = Vec::new();
            match fate {
                Fate::Dropped => notes.push("dropped with the merge".to_owned()),
                Fate::Folded if action != Action::Drop => notes.push("folded in with the merge".to_owned()),
                Fate::Folded => {}
                Fate::Kept => {
                    if let Some(g) = info.group
                        && !groups[line][g].members.is_empty()
                    {
                        notes.push(format!("{} commits become this one", plan.folded_in(&groups[line][g])));
                    }
                }
            }
            if has_sides && !gone {
                match (action, info.into) {
                    (Action::Squash | Action::Fixup, Some(into)) => {
                        notes.push(format!("no longer a merge, folded into {}", short(plan.commit(line, into).oid)))
                    }
                    (Action::Drop, _) => {}
                    _ => notes.push(format!("brings in {}", commits(plan.range.brought_in(source)))),
                }
            }
            if line == 0 && pos + 1 == main_len {
                notes.push("newest".to_owned());
            } else if line == 0 && pos == 0 {
                notes.push("oldest".to_owned());
            }
            let note = notes.join("  ·  ");
            let note_color = if controls.selected == Some(source) { theme::text_dim() } else { theme::text_faint() };
            let note_galley = one_line(&painter, &note, theme::font(11.0), fade(note_color), false, 380.0);
            let note_w = if note.is_empty() { 0.0 } else { note_galley.size().x + 14.0 };
            let room = (author_left - 10.0 - x - note_w).max(40.0);
            let galley = one_line(&painter, &text, theme::font(13.0), fade(text_color), strike, room);
            let used = widgets::galley_left(&painter, rect, x, galley);
            if !note.is_empty() {
                widgets::galley_left(&painter, rect, x + used + 14.0, note_galley);
            }

            widgets::avatar(&painter, pos2(author_left + 9.0, mid), 9.0, &commit.author, &commit.email, None);
            let author = one_line(&painter, &commit.author, theme::font(12.0), fade(theme::text_dim()), false, AUTHOR_W - 34.0);
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
            if !frozen {
                let len = plan.lines[line].len();
                if icon_at(ui, slot(1.0), ("up", source), icons::ARROW_UP, pos + 1 < len, "Move up (Alt+↑)") {
                    edit = Some(Edit::Move { line, from: pos, to: pos + 1 });
                }
                if icon_at(ui, slot(2.0), ("down", source), icons::ARROW_DOWN, pos > 0, "Move down (Alt+↓)") {
                    edit = Some(Edit::Move { line, from: pos, to: pos.saturating_sub(1) });
                }
            }

            // ---- message editor ----
            if let Some(kind) = info.editor
                && open[d]
                && let Some(g) = info.group
            {
                let block = Rect::from_min_max(
                    pos2(rect.left() + indent, rect.bottom()),
                    pos2(rect.right() - bar, content.top() + tops[d + 1]),
                );
                guides(&painter, Rect::from_min_max(pos2(rect.left(), block.top()), block.max), depth);
                editor(ui, plan, controls, &groups[line][g], kind, block, locked);
            }
        }

        // ---- the base, which stays as it is ----
        let base = Rect::from_min_size(pos2(content.left(), content.top() + tops[n]), vec2(content.width(), ROW_H));
        if ui.is_rect_visible(base) {
            let mid = base.center().y;
            painter.rect_filled(base, 0.0, theme::bg_panel());
            painter.text(
                pos2(base.left() + HANDLE_W + 8.0, mid),
                Align2::LEFT_CENTER,
                format!("{}  Base", icons::LOCK_SIMPLE),
                theme::font(12.5),
                theme::text_faint(),
            );
            let x = base.left() + HANDLE_W + CHIP_W + 12.0;
            painter.text(pos2(x, mid), Align2::LEFT_CENTER, short(plan.range.base), theme::mono(11.5), theme::text_faint());
            let range = &plan.range;
            let text = match range.lines[0].start {
                // The base came in through a merge.
                Some(start) if range.starts_below_base() => format!(
                    "{}   (not changed; the merged-in commits above build on it, and {} stays as it is under the merge)",
                    range.base_summary,
                    short(start)
                ),
                _ => format!("{}   (not changed; the commits above are replayed onto it)", range.base_summary),
            };
            let galley = one_line(&painter, &text, theme::font(13.0), theme::text_faint(), false, base.right() - x - SHA_W - 20.0);
            widgets::galley_left(&painter, base, x + SHA_W, galley);
        }

        // ---- a row on the move ----
        if let Some(drag) = &controls.drag {
            let down = ui.input(|i| i.pointer.primary_down());
            match (plan.find(drag.source), pointer) {
                // The rows of the dragged commit's line, as listed; it only
                // moves among them.
                (Some((line, from)), Some(pointer))
                    if (0..n).filter(|d| matches!(rows[*d], Row::Item { line: l, .. } if l == line)).count()
                        == plan.lines[line].len() =>
                {
                    let listed: Vec<usize> =
                        (0..n).filter(|d| matches!(rows[*d], Row::Item { line: l, .. } if l == line)).collect();
                    let len = listed.len();
                    let shown = |pos: usize| len - 1 - pos;
                    let at = shown(from);
                    // The gap the pointer is closest to: above listed row `slot`.
                    let y = pointer.y - content.top();
                    let slot = listed.iter().filter(|d| tops[**d] + ROW_H / 2.0 < y).count();
                    let target = if slot > at { slot - 1 } else { slot };
                    if down {
                        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
                        if target != at && len > 0 {
                            let gap = if slot < len { tops[listed[slot]] } else { block_end(listed[len - 1]) };
                            let at_y = content.top() + gap;
                            let depth = rows[listed[0]].depth() as f32 * INDENT;
                            let x = content.left() + depth;
                            painter.hline(x..=content.right(), at_y, Stroke::new(2.0, theme::accent()));
                            painter.circle_filled(pos2(x + 6.0, at_y), 4.0, theme::accent());
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
                            theme::bg_float(),
                            Stroke::new(1.0, theme::accent()),
                            StrokeKind::Inside,
                        );
                        let commit = plan.commit(line, from);
                        let text = format!("{}   {}", short(commit.oid), commit.summary);
                        let galley = one_line(&top, &text, theme::font(13.0), theme::text_bright(), false, ghost.width() - 24.0);
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
                        if target != at {
                            edit = Some(Edit::Move { line, from, to: shown(target) });
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

/// The heading over the commits a merge brings in through one parent.
fn side_heading(painter: &Painter, plan: &Plan, rect: Rect, line: usize, depth: usize, fate: Fate) {
    let range = &plan.range;
    painter.rect_filled(rect, 0.0, theme::bg_panel());
    guides(painter, rect, depth);
    let x = rect.left() + depth as f32 * INDENT + HANDLE_W - 6.0;
    let mid = rect.center().y;
    // Side 2 of 3, for a merge of several branches.
    let merge = range.lines[line].merge.map(|(merge, _)| merge);
    let sides = merge.and_then(|merge| range.sides.get(&merge)).map_or(&[][..], |s| s.as_slice());
    let which = match sides.iter().position(|l| *l == line) {
        Some(k) if sides.len() > 1 => format!("Side {} of {}: ", k + 1, sides.len()),
        _ => String::new(),
    };
    let fate = match fate {
        Fate::Dropped => "  ·  dropped with the merge",
        Fate::Folded => "  ·  folded in with the merge, no longer in the history",
        Fate::Kept => "",
    };
    let text = format!("{which}{} merged in, {}{fate}", commits(range.size(line)), start_text(range, line));
    painter.text(pos2(x, mid), Align2::LEFT_CENTER, icons::GIT_BRANCH, theme::font(12.0), theme::accent());
    let galley = one_line(painter, &text, theme::font(11.5), theme::text_dim(), false, rect.right() - x - 40.0);
    widgets::galley_left(painter, rect, x + 20.0, galley);
}

/// The message editor under a row: the commit's new message, or the one
/// message for a squash group.
fn editor(ui: &mut Ui, plan: &mut Plan, controls: &mut Controls, group: &Group, kind: Editor, block: Rect, locked: bool) {
    let (line, pos) = (group.line, group.leader);
    let source = plan.lines[line][pos].source;
    let (_, color, _) = action_style(if kind == Editor::Combined { Action::Squash } else { Action::Reword });

    let painter = ui.painter().clone();
    painter.rect_filled(block, 0.0, theme::bg_panel());
    painter.rect_filled(Rect::from_min_max(block.min, pos2(block.left() + 3.0, block.bottom())), 0.0, color);
    painter.hline(block.x_range(), block.bottom() - 0.5, Stroke::new(1.0, theme::border()));

    // What the editor shows until the user types something else.
    let (default, title) = match kind {
        Editor::Reword => (
            rebase::normalize(&plan.commit(line, pos).message),
            format!("New message for {}", short(plan.commit(line, pos).oid)),
        ),
        Editor::Combined => (
            plan.default_combined(group),
            format!(
                "One message for the {} commits squashed together (proposed: their messages, as git would)",
                plan.folded_in(group)
            ),
        ),
    };
    let default = default.trim_end().to_owned();
    let typed = match kind {
        Editor::Reword => plan.lines[line][pos].reword.clone(),
        Editor::Combined => plan.lines[line][pos].combined.clone(),
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
            ui.label(RichText::new(title).size(12.0).color(theme::text_dim()));
            if outdated {
                ui.label(
                    RichText::new(format!("{}  The commits in this group changed after you wrote this.", icons::WARNING))
                        .size(12.0)
                        .color(theme::orange()),
                );
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let label = if kind == Editor::Combined { "Use the proposal" } else { "Original message" };
                if edited && !locked && widgets::tint_button(ui, label, theme::text_dim()).clicked() {
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
                let item = &mut plan.lines[line][pos];
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
        let item = &mut plan.lines[line][pos];
        match kind {
            Editor::Reword => item.reword = None,
            Editor::Combined => {
                item.combined = None;
                controls.edited_from.remove(&source);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{Fixture, git};

    /// main: base, one, then a merge of "side" (s1, s2 from one) and
    /// "after"; inside the side, s2 merges "inner" (i1 from s1).
    fn nested(fx: &Fixture) -> Oid {
        fx.write("base.txt", "base\n");
        fx.commit_all("base");
        let base = fx.snapshot().head.oid.unwrap();
        fx.write("one.txt", "one\n");
        fx.commit_all("one");
        git(&fx.dir, &["checkout", "-q", "-b", "side"]);
        fx.write("s1.txt", "s1\n");
        fx.commit_all("s1");
        git(&fx.dir, &["checkout", "-q", "-b", "inner"]);
        fx.write("i1.txt", "i1\n");
        fx.commit_all("i1");
        git(&fx.dir, &["checkout", "-q", "side"]);
        git(&fx.dir, &["merge", "-q", "--no-ff", "-m", "s2", "inner"]);
        git(&fx.dir, &["checkout", "-q", "main"]);
        git(&fx.dir, &["merge", "-q", "--no-ff", "-m", "Merge side", "side"]);
        fx.write("after.txt", "after\n");
        fx.commit_all("after");
        base
    }

    fn summary(plan: &Plan, row: Row) -> String {
        match row {
            Row::Item { line, pos, depth } => format!("{depth} {}", plan.commit(line, pos).summary),
            Row::Side { depth, .. } => format!("{depth} --"),
        }
    }

    #[test]
    fn sides_are_listed_under_their_merge_and_fold_away() {
        let fx = Fixture::new("ui-rebase-rows");
        let base = nested(&fx);
        let range = fx.job(move |loader| rebase::load_range(loader.repo(), base)).unwrap();
        let mut plan = Plan::new(Arc::new(range));
        let rows = display_rows(&plan, &HashSet::new());
        let listed: Vec<String> = rows.iter().map(|row| summary(&plan, *row)).collect();
        assert_eq!(listed, ["0 after", "0 Merge side", "1 --", "1 s2", "2 --", "2 i1", "1 s1", "0 one"]);

        // Folded away, a merge hides everything under it.
        let merge = plan.lines[0][1].source;
        let rows = display_rows(&plan, &HashSet::from([merge]));
        assert_eq!(rows.len(), 3);

        // Moves stay within a line, and a draft fits only the same lines.
        let mut controls = Controls::default();
        move_item(&mut plan, &mut controls, 1, 0, 1);
        assert_eq!(plan.lines[1].iter().map(|i| plan.range.commits[i.source].summary.as_str()).collect::<Vec<_>>(), ["s2", "s1"]);
        assert!(fits(&plan.lines, &plan.range));
        let mut wrong = plan.lines.clone();
        let stray = wrong[1].pop().unwrap();
        wrong[0].push(stray);
        assert!(!fits(&wrong, &plan.range));

        // Rows dropped with their merge cannot be changed.
        set_action(&mut plan, &mut controls, 0, 1, Action::Drop);
        set_action(&mut plan, &mut controls, 1, 0, Action::Reword);
        assert_eq!(plan.lines[1][0].action, Action::Pick);
        assert!(controls.error.as_deref().unwrap().contains("dropped with the merge"));
    }
}
