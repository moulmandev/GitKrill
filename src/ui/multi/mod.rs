//! Several commits selected together ([`pick`]): the right panel that lists
//! them and compares two of them (or shows the combined change of a line of
//! them), the right-click menu of what can be done with them (cherry-pick,
//! revert, squash, copy), "Compare with HEAD" on a commit and "Compare with
//! <current branch>" on a branch or tag, and their command palette entries.
//!
//! The graph calls [`click`] and [`arrow`] instead of selecting a row itself,
//! asks [`Multi::contains`] whether a row looks selected, and lets [`menu`]
//! draw the context menu of a selected commit. The panel calls [`panel`]
//! first. Comparing commits that are not loaded yet loads more history
//! until they are, as "Show in graph" does.

pub mod pick;

use std::sync::{Arc, Weak};

use eframe::egui::{
    self, Align, Button, Color32, Frame, Layout, Margin, Rect, Response, RichText,
    ScrollArea, Sense, Stroke, StrokeKind, Ui, pos2, vec2,
};

use crate::git::multi::{self as ops, Facts, Squash};
use crate::git::{CommitInfo, DiffSource, FileChange, Head, History, Oid, RepoSnapshot, RowKind, short};
use crate::state::{Overlay, RepoTab, Selection, Shared, ToastKind, format_time};
use crate::theme::{self, icons};
use crate::ui::files::file_list;
use crate::ui::palette::{Entry, Env, Kind};
use crate::ui::rebase::RebaseView;
use crate::ui::widgets::{self, elided, icon_button, menu_item, segmented};

use self::pick::{Picked, selectable};

const FILES_JOB: &str = "multi:files";
const SQUASH_JOB: &str = "multi:squash";
const ROW_H: f32 = 40.0;

/// What a [`Facts`] was worked out for.
struct Stamp {
    history: Weak<History>,
    head: Head,
    generation: u64,
}

/// The multi-selection of one tab (`RepoTab::multi`).
#[derive(Default)]
pub struct Multi {
    pub picked: Picked,
    /// The selection generation in which the comparison was swapped round.
    swapped: Option<u64>,
    facts: Option<(Stamp, Arc<Facts>)>,
    /// The sides whose files were asked for, and the answer once it came.
    asked: Option<(Oid, Oid)>,
    files: Option<Result<Arc<Vec<FileChange>>, String>>,
    /// Two commits to compare once both are in the graph.
    pending: Option<(Oid, Oid)>,
    /// The squash is being checked on the reader thread.
    squashing: bool,
}

impl Multi {
    /// Several commits are selected.
    pub fn is_active(&self) -> bool {
        self.picked.is_active()
    }

    /// `oid` is one of several selected commits.
    pub fn contains(&self, oid: Oid) -> bool {
        self.picked.is_active() && self.picked.contains(oid)
    }

    /// Back to one commit (`anchor`) or none; see [`Picked::single`].
    pub fn reset(&mut self, anchor: Option<Oid>) {
        self.picked.single(anchor);
        self.pending = None;
    }

    /// After a refresh; see [`Picked::retain`].
    pub fn retain(&mut self, history: &History, focus: Option<Oid>) -> Option<Oid> {
        self.picked.retain(history, focus)
    }

    fn is_swapped(&self) -> bool {
        self.swapped == Some(self.picked.generation())
    }

    /// The two sides the panel compares: two commits, older first unless
    /// swapped, or for a line of more, the change they make together.
    fn sides(&self, facts: &Facts) -> Option<(Oid, Oid)> {
        match facts.commits.as_slice() {
            [newer, older] if self.is_swapped() => Some((*newer, *older)),
            [newer, older] => Some((*older, *newer)),
            _ => facts.combined(),
        }
    }
}

fn focused(tab: &RepoTab) -> Option<Oid> {
    match tab.selection {
        Selection::Commit(oid) => Some(oid),
        _ => None,
    }
}

/// [`Facts`] about the selection, worked out again only when it, the history
/// or HEAD changed.
fn facts(tab: &mut RepoTab, snap: &RepoSnapshot) -> Arc<Facts> {
    let generation = tab.multi.picked.generation();
    if let Some((stamp, facts)) = &tab.multi.facts
        && Weak::as_ptr(&stamp.history) == Arc::as_ptr(&snap.history)
        && stamp.head == snap.head
        && stamp.generation == generation
    {
        return facts.clone();
    }
    let facts = Arc::new(ops::facts(&snap.history, &snap.head, tab.multi.picked.set()));
    let stamp = Stamp { history: Arc::downgrade(&snap.history), head: snap.head.clone(), generation };
    tab.multi.facts = Some((stamp, facts.clone()));
    facts
}

// ---- selecting ----------------------------------------------------------------

/// A click on graph row `row`: Ctrl toggles the commit, Shift takes the
/// range from the anchor (Ctrl+Shift adds it), nothing else selects it
/// alone. The WIP row and stashes are always selected alone.
pub fn click(ui: &Ui, tab: &mut RepoTab, row: usize) {
    let modifiers = ui.input(|i| i.modifiers);
    let Some(snap) = tab.snap.clone() else { return };
    let Some(commit) = snap.history.commits.get(row) else { return };
    if commit.kind != RowKind::Commit || !(modifiers.command || modifiers.shift) {
        tab.select_row(row);
        return;
    }
    let focus = focused(tab);
    let picked = &mut tab.multi.picked;
    let next = if modifiers.shift {
        picked.extend(&snap.history, focus, commit.oid, modifiers.command)
    } else {
        picked.toggle(&snap.history, focus, commit.oid)
    };
    tab.focus(Selection::Commit(next));
}

/// Up or Down moved the graph to row `next`: with Shift held, the selection
/// grows or shrinks from the anchor instead of moving.
pub fn arrow(ui: &Ui, tab: &mut RepoTab, next: usize) {
    let shift = ui.input(|i| i.modifiers.shift);
    let Some(snap) = tab.snap.clone() else { return };
    if !shift {
        tab.select_row(next);
        tab.scroll_to = Some(next);
        return;
    }
    let commits = &snap.history.commits;
    let down = next >= tab.selected_row().unwrap_or(0);
    // A stash cannot join: step over it in the direction of travel.
    let mut row = next;
    while commits.get(row).is_some_and(|c| c.kind != RowKind::Commit) {
        row = match (down, row.checked_sub(1)) {
            (true, _) => row + 1,
            (false, Some(up)) => up,
            (false, None) => return,
        };
    }
    let Some(commit) = commits.get(row) else { return };
    let next = tab.multi.picked.extend(&snap.history, focused(tab), commit.oid, false);
    tab.focus(Selection::Commit(next));
    tab.scroll_to = Some(row);
}

/// Escape: back to the focused commit alone. False when only one was
/// selected anyway.
pub fn escape(tab: &mut RepoTab) -> bool {
    if !tab.multi.is_active() {
        return false;
    }
    tab.select(tab.selection);
    true
}

/// Brings a selected commit into view in the graph, keeping the selection.
fn reveal(tab: &mut RepoTab, snap: &RepoSnapshot, oid: Oid) {
    let Some(row) = snap.row_of(oid) else { return };
    tab.focus(Selection::Commit(oid));
    tab.scroll_to = Some(row);
    tab.diff = None;
}

/// Selects `a` and `b` as the two commits to compare, focused on `a`. When
/// one of them is not in the graph yet, more history is loaded until it is.
pub fn compare(tab: &mut RepoTab, shared: &mut Shared, a: Oid, b: Oid) {
    tab.multi.pending = Some((a, b));
    if let Some(snap) = tab.snap.clone() {
        finish_compare(tab, shared, &snap);
    }
}

fn finish_compare(tab: &mut RepoTab, shared: &mut Shared, snap: &RepoSnapshot) {
    let Some((a, b)) = tab.multi.pending else { return };
    let history = &snap.history;
    if selectable(history, a) && selectable(history, b) {
        tab.multi.pending = None;
        tab.multi.picked.select(&[a, b]);
        tab.focus(Selection::Commit(a));
        tab.scroll_to = snap.row_of(a);
        tab.diff = None;
    } else if history.has_more {
        // A no-op while a bigger page is already on its way.
        tab.load_more();
    } else {
        tab.multi.pending = None;
        let missing = if selectable(history, a) { b } else { a };
        shared.toast(
            ToastKind::Error,
            "Cannot compare",
            format!(
                "{} is not among the commits the graph shows: whatever points at it is hidden \
                 from the graph. Show it again to compare it.",
                short(missing)
            ),
        );
    }
}

/// What the panel and the palette need checked on every frame: a comparison
/// waiting for history to load, the squash check, the files to compare.
fn poll(tab: &mut RepoTab, shared: &mut Shared, snap: &RepoSnapshot) {
    finish_compare(tab, shared, snap);
    if tab.multi.squashing
        && let Some(result) = tab.take_result::<Result<Squash, String>>(SQUASH_JOB)
    {
        tab.multi.squashing = false;
        match result {
            Ok(squash) => {
                let warning = squash.warning();
                tab.overlay = Some(Overlay::Rebase(RebaseView::squash(squash.base, squash.run, warning)));
            }
            Err(e) => shared.toast(ToastKind::Error, "Cannot squash these commits", e),
        }
    }
    if let Some((from, to, result)) = tab.take_result::<(Oid, Oid, Result<Vec<FileChange>, String>)>(FILES_JOB)
        && tab.multi.asked == Some((from, to))
    {
        tab.multi.files = Some(result.map(Arc::new));
    }
}

/// Asks the reader thread for the files that differ between `sides`, unless
/// they are the ones already shown or asked for.
fn load_files(tab: &mut RepoTab, sides: Option<(Oid, Oid)>) {
    if tab.multi.asked == sides {
        return;
    }
    tab.multi.asked = sides;
    tab.multi.files = None;
    if let Some((from, to)) = sides {
        tab.spawn_job(FILES_JOB, move |loader| (from, to, loader.compare_files(from, to)));
    }
}

// ---- actions ------------------------------------------------------------------

fn commits(n: usize) -> String {
    if n == 1 { "1 commit".to_owned() } else { format!("{n} commits") }
}

/// Why the selected commits cannot be cherry-picked onto HEAD right now.
fn pick_blocker(tab: &RepoTab, snap: &RepoSnapshot, facts: &Facts) -> Option<String> {
    let head = snap.head_label();
    let n = facts.commits.len();
    if let Some(state) = snap.state.label() {
        Some(format!("{state}. Finish or abort it first."))
    } else if snap.head.oid.is_none() {
        Some("This branch has no commits yet.".to_owned())
    } else if tab.is_running("Cherry-pick") {
        Some("A cherry-pick is running.".to_owned())
    } else if facts.on_head == n {
        Some(format!("These commits are on {head} already."))
    } else if facts.on_head > 0 {
        Some(format!(
            "{} of these commits {} on {head} already, and picking {} again would change nothing.",
            facts.on_head,
            if facts.on_head == 1 { "is" } else { "are" },
            if facts.on_head == 1 { "it" } else { "them" },
        ))
    } else {
        None
    }
}

fn revert_blocker(tab: &RepoTab, snap: &RepoSnapshot) -> Option<String> {
    if let Some(state) = snap.state.label() {
        Some(format!("{state}. Finish or abort it first."))
    } else if tab.is_running("Revert") {
        Some("A revert is running.".to_owned())
    } else {
        None
    }
}

fn squash_blocker(tab: &RepoTab, facts: &Facts) -> Option<String> {
    if tab.multi.squashing || tab.is_running(crate::git::rebase::LABEL) {
        return Some("A squash is being prepared.".to_owned());
    }
    facts.squash.as_ref().err().cloned()
}

/// Cherry-picks the selected commits onto HEAD, oldest first.
fn cherry_pick(tab: &mut RepoTab, facts: &Facts) {
    let oldest_first: Vec<Oid> = facts.commits.iter().rev().copied().collect();
    tab.run(ops::cherry_pick(&oldest_first, facts.merges > 0));
}

/// Reverts the selected commits, newest first.
fn revert(tab: &mut RepoTab, facts: &Facts) {
    tab.run(ops::revert(&facts.commits, facts.merges > 0));
}

/// Checks the squash against the repository, then opens the interactive
/// rebase with the commits set to squash (see `poll`).
fn squash(tab: &mut RepoTab, facts: &Facts) {
    let oids = facts.commits.clone();
    tab.spawn_job(SQUASH_JOB, move |loader| ops::check_squash(loader.repo(), &oids));
    tab.multi.squashing = true;
}

/// Full SHAs, one per line, newest first.
fn copy_shas(ctx: &egui::Context, facts: &Facts) {
    let text: Vec<String> = facts.commits.iter().map(Oid::to_string).collect();
    ctx.copy_text(text.join("\n"));
}

/// A menu entry that is disabled, saying why, while `blocker` is set.
fn entry(ui: &mut Ui, icon: &str, text: &str, blocker: Option<&str>) -> bool {
    let response = ui.add_enabled(blocker.is_none(), Button::new(format!("{icon}  {text}")));
    match blocker {
        Some(why) => {
            response.on_disabled_hover_text(why);
            false
        }
        None => response.clicked(),
    }
}

/// The right-click menu of a commit that is one of several selected. False,
/// drawing nothing, for any other commit: it gets its own menu.
pub fn menu(ui: &mut Ui, tab: &mut RepoTab, snap: &RepoSnapshot, oid: Oid) -> bool {
    if !tab.multi.contains(oid) {
        return false;
    }
    ui.set_min_width(250.0);
    let facts = facts(tab, snap);
    let n = facts.commits.len();
    ui.label(RichText::new(format!("{n} commits selected")).size(11.0).color(theme::text_dim()));
    ui.separator();
    let pick = pick_blocker(tab, snap, &facts);
    if entry(ui, icons::GIT_COMMIT, &format!("Cherry-pick {}", commits(n)), pick.as_deref()) {
        cherry_pick(tab, &facts);
    }
    let undo = revert_blocker(tab, snap);
    if entry(ui, icons::ARROW_COUNTER_CLOCKWISE, &format!("Revert {}", commits(n)), undo.as_deref()) {
        revert(tab, &facts);
    }
    let fold = squash_blocker(tab, &facts);
    if entry(ui, icons::ARROWS_MERGE, &format!("Squash {}…", commits(n)), fold.as_deref()) {
        squash(tab, &facts);
    }
    if n == 2 && menu_item(ui, icons::ARROWS_DOWN_UP, "Swap the comparison") {
        swap(tab);
    }
    crate::ui::patch::multi_items(ui, tab, snap, &facts.commits);
    ui.separator();
    if menu_item(ui, icons::COPY, &format!("Copy {n} SHAs")) {
        copy_shas(ui.ctx(), &facts);
    }
    if menu_item(ui, icons::CROSSHAIR, "Select only this commit") {
        tab.select(Selection::Commit(oid));
    }
    true
}

fn swap(tab: &mut RepoTab) {
    let generation = tab.multi.picked.generation();
    tab.multi.swapped = if tab.multi.is_swapped() { None } else { Some(generation) };
    // A file of the comparison that is open turns round with it. A renamed
    // file is found under its other name on the other side.
    if let Some(view) = &tab.diff
        && let DiffSource::Compare { from, to } = view.target.source
        && tab.multi.asked == Some((from, to))
    {
        let mut target = view.target.clone();
        target.source = DiffSource::Compare { from: to, to: from };
        if let Some(old) = target.old_path.take() {
            target.old_path = Some(std::mem::replace(&mut target.path, old));
        }
        tab.open_diff(target);
    }
}

/// "Compare with HEAD", for the menu of a commit selected alone.
pub fn compare_with_head(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, snap: &RepoSnapshot, oid: Oid) {
    let Some(head) = snap.head.oid.filter(|head| *head != oid) else { return };
    if menu_item(ui, icons::GIT_DIFF, "Compare with HEAD") {
        compare(tab, shared, oid, head);
    }
}

/// "Compare with <current branch>", for the menu of a branch or tag whose
/// tip is `tip`. Left out when it is the checked-out commit.
pub fn compare_item(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, snap: &RepoSnapshot, tip: Option<Oid>) {
    let (Some(tip), Some(head)) = (tip, snap.head.oid) else { return };
    if tip == head {
        return;
    }
    let label = match &snap.head.branch {
        Some(branch) if !snap.head.detached => format!("Compare with {branch}"),
        _ => "Compare with HEAD".to_owned(),
    };
    if menu_item(ui, icons::GIT_DIFF, &label) {
        compare(tab, shared, tip, head);
    }
}

// ---- command palette ----------------------------------------------------------

/// Palette entries for the selection: the actions on several commits, or
/// "Compare with HEAD" for one.
pub fn palette_entries(list: &mut Vec<Entry>, tab: &RepoTab, snap: &RepoSnapshot) {
    if !tab.multi.is_active() {
        let Some(oid) = focused(tab).filter(|oid| Some(*oid) != snap.head.oid && selectable(&snap.history, *oid))
        else {
            return;
        };
        let Some(head) = snap.head.oid else { return };
        let summary = snap.row_of(oid).map(|row| snap.history.commits[row].summary.clone()).unwrap_or_default();
        list.push(
            Entry::on_tab("compare-head", icons::GIT_DIFF, "Compare with HEAD", move |tab, shared| {
                compare(tab, shared, oid, head)
            })
            .detail(format!("{} {summary}", short(oid)))
            .keywords("diff difference between commits"),
        );
        return;
    }

    let known = ops::facts(&snap.history, &snap.head, tab.multi.picked.set());
    let n = known.commits.len();
    let on_tab = |run: fn(&mut RepoTab, &Facts)| {
        move |tab: &mut RepoTab, _: &mut Shared| {
            if let Some(snap) = tab.snap.clone() {
                let facts = facts(tab, &snap);
                run(tab, &facts);
            }
        }
    };
    if pick_blocker(tab, snap, &known).is_none() {
        list.push(
            Entry::on_tab("multi-cherry-pick", icons::GIT_COMMIT, format!("Cherry-pick {}", commits(n)), on_tab(cherry_pick))
                .detail(format!("onto {}, oldest first", snap.head_label())),
        );
    }
    if revert_blocker(tab, snap).is_none() {
        list.push(
            Entry::on_tab("multi-revert", icons::ARROW_COUNTER_CLOCKWISE, format!("Revert {}", commits(n)), on_tab(revert))
                .detail("newest first"),
        );
    }
    if squash_blocker(tab, &known).is_none() {
        list.push(
            Entry::on_tab("multi-squash", icons::ARROWS_MERGE, format!("Squash {}", commits(n)), on_tab(squash))
                .detail("into one commit, in the interactive rebase")
                .keywords("combine fold meld"),
        );
    }
    if n == 2 {
        list.push(
            Entry::on_tab("multi-swap", icons::ARROWS_DOWN_UP, "Swap the compared commits", |tab, _| swap(tab))
                .keywords("compare direction reverse"),
        );
    }
    list.push(Entry::new(Kind::Action, "multi-copy", icons::COPY, format!("Copy {n} SHAs"), |env: &mut Env<'_>| {
        if let Some(tab) = env.tab.as_deref_mut()
            && let Some(snap) = tab.snap.clone()
        {
            copy_shas(env.ctx, &facts(tab, &snap));
        }
    }));
    list.push(
        Entry::on_tab("multi-single", icons::CROSSHAIR, "Select one commit only", |tab, _| {
            escape(tab);
        })
        .shortcut("Esc")
        .keywords("deselect clear selection"),
    );
}

// ---- the panel ----------------------------------------------------------------

/// Something clicked in the panel, done once it is drawn.
enum Act {
    Collapse,
    Reveal(Oid),
    Only(Oid),
    Swap,
    CherryPick,
    Revert,
    Squash,
    Copy,
}

/// The right panel while several commits are selected: how many, what they
/// are, what differs, and buttons for what can be done with them. Returns
/// false, drawing nothing, while only one is selected.
pub fn panel(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, snap: &Arc<RepoSnapshot>) -> bool {
    poll(tab, shared, snap);
    if !tab.multi.is_active() {
        return false;
    }
    let facts = facts(tab, snap);
    let n = facts.commits.len();
    if n < 2 {
        return false;
    }
    let sides = tab.multi.sides(&facts);
    load_files(tab, sides);
    let mut act = None;

    Frame::new().inner_margin(Margin::same(12)).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 6.0;
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("{}  {n} commits selected", icons::STACK))
                    .font(theme::bold(15.0))
                    .color(theme::text_bright()),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if icon_button(ui, icons::X, 13.0, "Select one commit only (Esc)").clicked() {
                    act = Some(Act::Collapse);
                }
            });
        });
        let what = match (n, sides) {
            (2, _) => "The files that differ between the two commits.".to_owned(),
            (_, Some((from, _))) => format!("One unbroken line of history: what the {n} change together, from {}.", short(from)),
            _ if facts.line => "One unbroken line, starting at the repository's first commit: there is nothing before it to compare with.".to_owned(),
            _ => "Not one unbroken line of history, so there is no combined change to show.".to_owned(),
        };
        ui.add(egui::Label::new(RichText::new(what).size(12.0).color(theme::text_dim())).wrap());

        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
            let head = snap.head_label();
            let pick = pick_blocker(tab, snap, &facts);
            let tip = format!("Cherry-pick the {n} commits onto {head}, oldest first");
            if button(ui, "Cherry-pick", theme::green(), pick.as_deref(), &tip) {
                act = Some(Act::CherryPick);
            }
            let undo = revert_blocker(tab, snap);
            if button(ui, "Revert", theme::orange(), undo.as_deref(), "One revert commit each, newest first") {
                act = Some(Act::Revert);
            }
            let fold = squash_blocker(tab, &facts);
            let tip = "Squash them into one commit: opens the interactive rebase, where you write its message";
            if button(ui, "Squash…", theme::accent(), fold.as_deref(), tip) {
                act = Some(Act::Squash);
            }
            if button(ui, "Copy SHAs", theme::text_dim(), None, "Copy their full SHAs, one per line") {
                act = Some(Act::Copy);
            }
            if n == 2 && button(ui, &format!("{}  Swap", icons::ARROWS_DOWN_UP), theme::text_dim(), None, "Compare the other way round") {
                act = Some(Act::Swap);
            }
        });
    });

    ui.spacing_mut().item_spacing.y = 0.0;
    let room = ui.available_height();
    // Two commits: both rows. More: up to two fifths of the panel when a
    // file list follows, all of it otherwise.
    let rows: Vec<(Oid, Option<(&str, Color32)>)> = match (n, sides) {
        (2, Some((from, to))) => vec![(from, Some(("from", theme::red()))), (to, Some(("to", theme::green())))],
        _ => facts.commits.iter().map(|oid| (*oid, None)).collect(),
    };
    let list_h = if sides.is_some() { (rows.len() as f32 * ROW_H).min((room * 0.4).max(ROW_H * 2.0)) } else { room };
    let (top, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().hline(top.x_range(), top.top(), Stroke::new(1.0, theme::border()));
    ScrollArea::vertical()
        .id_salt("multi-commits")
        .max_height(list_h)
        .auto_shrink([false, true])
        .show_rows(ui, ROW_H, rows.len(), |ui, range| {
            for (oid, tag) in &rows[range] {
                let Some(row) = snap.row_of(*oid) else { continue };
                let commit = &snap.history.commits[row];
                let focus = tab.selection == Selection::Commit(*oid);
                let response = commit_row(ui, shared, commit, *tag, focus)
                    .on_hover_text(format!("{}\n{}", commit.summary, commit.oid));
                if response.clicked() {
                    act = Some(Act::Reveal(*oid));
                }
                response.context_menu(|ui| {
                    ui.set_min_width(200.0);
                    if menu_item(ui, icons::CROSSHAIR, "Select only this commit") {
                        act = Some(Act::Only(*oid));
                    }
                    if menu_item(ui, icons::COPY, "Copy commit SHA") {
                        ui.ctx().copy_text(oid.to_string());
                    }
                });
            }
        });

    if let Some((from, to)) = sides {
        files(ui, tab, shared, from, to);
    }

    match act {
        Some(Act::Collapse) => {
            escape(tab);
        }
        Some(Act::Reveal(oid)) => reveal(tab, snap, oid),
        Some(Act::Only(oid)) => tab.select(Selection::Commit(oid)),
        Some(Act::Swap) => swap(tab),
        Some(Act::CherryPick) => cherry_pick(tab, &facts),
        Some(Act::Revert) => revert(tab, &facts),
        Some(Act::Squash) => squash(tab, &facts),
        Some(Act::Copy) => copy_shas(ui.ctx(), &facts),
        None => {}
    }
    true
}

/// A tinted button that is greyed out, saying why on hover, while `blocker`
/// is set.
fn button(ui: &mut Ui, text: &str, color: Color32, blocker: Option<&str>, tip: &str) -> bool {
    let color = if blocker.is_some() { theme::text_faint() } else { color };
    // Room taken first, so the row wraps before the button rather than
    // growing the panel.
    let size = widgets::tint_button_size(ui, text);
    let response = ui
        .allocate_ui(size, |ui| ui.add_enabled_ui(blocker.is_none(), |ui| widgets::tint_button(ui, text, color)).inner)
        .inner;
    match blocker {
        Some(why) => {
            response.on_disabled_hover_text(why);
            false
        }
        None => response.on_hover_text(tip).clicked(),
    }
}

/// One selected commit: avatar, summary, then SHA, author and date. `tag`
/// marks the sides of a comparison.
fn commit_row(ui: &mut Ui, shared: &mut Shared, commit: &CommitInfo, tag: Option<(&str, Color32)>, focus: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
    widgets::row_background(&painter, rect, focus, response.hovered());
    let mut x = rect.left() + 12.0;
    if let Some((text, color)) = tag {
        let galley = painter.layout_no_wrap(text.to_uppercase(), theme::bold(10.0), color);
        let chip = Rect::from_min_size(pos2(x, rect.center().y - 9.0), vec2(42.0, 18.0));
        painter.rect(
            chip,
            3.0,
            theme::mix(theme::bg_app(), color, 0.14),
            Stroke::new(1.0, theme::mix(theme::bg_app(), color, 0.6)),
            StrokeKind::Inside,
        );
        let at = chip.center() - galley.size() / 2.0;
        painter.galley(pos2(at.x.round(), at.y.round()), galley, color);
        x = chip.right() + 10.0;
    }
    let picture = shared.avatars.get(&commit.email, shared.settings.fetch_avatars);
    widgets::avatar_picture(&painter, picture, pos2(x + 12.0, rect.center().y), 12.0, &commit.author, &commit.email, None);

    let text_x = x + 32.0;
    let width = (rect.right() - 10.0 - text_x).max(20.0);
    let summary_color = if focus { theme::text_bright() } else { theme::text() };
    let summary = elided(&painter, &commit.summary, theme::font(13.0), summary_color, width);
    painter.galley(pos2(text_x, rect.top() + 4.0), summary, summary_color);
    let sha = painter.layout_no_wrap(short(commit.oid), theme::mono(11.5), theme::accent());
    let sha_w = sha.size().x;
    let line_y = rect.top() + 22.0;
    painter.galley(pos2(text_x, line_y), sha, theme::accent());
    let rest = format!("{}  ·  {}", commit.author, format_time(commit.time));
    let rest = elided(&painter, &rest, theme::font(11.5), theme::text_dim(), width - sha_w - 10.0);
    painter.galley(pos2(text_x + sha_w + 10.0, line_y), rest, theme::text_dim());
    response
}

/// The files that differ from `from` to `to`, with their header.
fn files(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, from: Oid, to: Oid) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::hover());
    ui.painter().hline(rect.x_range(), rect.top(), Stroke::new(1.0, theme::border()));
    let title = match &tab.multi.files {
        None => "Comparing…".to_owned(),
        Some(Ok(files)) if files.is_empty() => "No files differ".to_owned(),
        Some(Ok(files)) => format!("{} file{} differ{}", files.len(), if files.len() == 1 { "" } else { "s" }, if files.len() == 1 { "s" } else { "" }),
        Some(Err(_)) => "Could not compare".to_owned(),
    };
    // The sides are named above: the two rows, or the line's description.
    let galley = elided(ui.painter(), &title, theme::font(13.0), theme::text(), rect.width() - 150.0);
    ui.painter().galley(pos2(rect.left() + 12.0, rect.center().y - galley.size().y / 2.0), galley, theme::text());
    ui.scope_builder(
        egui::UiBuilder::new().max_rect(rect.shrink2(vec2(8.0, 0.0))).layout(Layout::right_to_left(Align::Center)),
        |ui| segmented(ui, &mut shared.settings.tree_view, (icons::LIST, "Path"), (icons::TREE_VIEW, "Tree")),
    );

    match tab.multi.files.clone() {
        Some(Ok(files)) if !files.is_empty() => {
            let height = ui.available_height();
            file_list(ui, tab, shared, &files, DiffSource::Compare { from, to }, height);
        }
        Some(Ok(_)) => widgets::placeholder(ui, "Both commits have the same files."),
        Some(Err(e)) => widgets::placeholder(ui, &e),
        None => {
            let rect = ui.available_rect_before_wrap();
            widgets::spinner(ui.painter(), pos2(rect.center().x, rect.top() + 30.0), 8.0, theme::text_dim());
            ui.ctx().request_repaint();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::Fixture;

    fn labels(tab: &RepoTab, snap: &RepoSnapshot) -> Vec<String> {
        let mut list = Vec::new();
        palette_entries(&mut list, tab, snap);
        list.into_iter().map(|entry| entry.label).collect()
    }

    #[test]
    fn the_palette_offers_what_the_selection_allows() {
        let fx = Fixture::new("multi-palette");
        for name in ["base", "b", "c"] {
            fx.write(&format!("{name}.txt"), "x\n");
            fx.commit_all(name);
        }
        let snap = fx.snapshot();
        let [c, b, base] = [0, 1, 2].map(|row| snap.history.commits[row].oid);
        let mut tab = RepoTab::new(fx.dir.clone());
        tab.snap = Some(snap.clone());

        tab.select(Selection::Commit(base));
        assert_eq!(labels(&tab, &snap), ["Compare with HEAD"]);
        tab.select(Selection::Commit(c));
        assert!(labels(&tab, &snap).is_empty(), "HEAD has nothing to compare with itself");

        // Both on HEAD already: nothing to cherry-pick.
        tab.multi.picked.select(&[c, b]);
        tab.focus(Selection::Commit(c));
        assert_eq!(
            labels(&tab, &snap),
            ["Revert 2 commits", "Squash 2 commits", "Swap the compared commits", "Copy 2 SHAs", "Select one commit only"]
        );
        // The first commit of the repository cannot be squashed into.
        tab.multi.picked.select(&[b, base]);
        assert!(!labels(&tab, &snap).iter().any(|label| label.starts_with("Squash")));
        // A plain selection again.
        tab.select(Selection::Commit(b));
        assert!(!tab.multi.is_active());
        assert_eq!(labels(&tab, &snap), ["Compare with HEAD"]);
    }

    #[test]
    fn two_commits_compare_older_first_until_swapped() {
        let facts = Facts {
            commits: vec![Oid::from_bytes(&[2; 20]).unwrap(), Oid::from_bytes(&[1; 20]).unwrap()],
            line: false,
            below: None,
            merges: 0,
            on_head: 0,
            has_head: false,
            squash: Err(String::new()),
        };
        let [newer, older] = [facts.commits[0], facts.commits[1]];
        let mut multi = Multi::default();
        multi.picked.select(&[newer, older]);
        assert_eq!(multi.sides(&facts), Some((older, newer)));
        multi.swapped = Some(multi.picked.generation());
        assert_eq!(multi.sides(&facts), Some((newer, older)));
        // Another selection compares older first again.
        multi.picked.select(&[older, newer]);
        assert_eq!(multi.sides(&facts), Some((older, newer)));
    }
}
