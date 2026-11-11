//! Several commits selected together ([`pick`]): the right panel that lists
//! them and compares two of them (or shows the combined change of a line of
//! them), the right-click menu of what can be done with them (cherry-pick,
//! revert, squash, copy), "Compare with HEAD" on a commit and "Compare with
//! <current branch>" on a branch or tag, and their command palette entries.
//!
//! Two commits are compared tree to tree, or from where they forked (their
//! merge base: what the newer side adds on its own, git's `A...B`). A commit
//! selected alone can also be compared with the working tree, uncommitted
//! changes included; the panel then shows that comparison instead of the
//! commit's details until another commit is selected.
//!
//! The graph calls [`click`] and [`arrow`] instead of selecting a row itself,
//! asks [`Multi::contains`] whether a row looks selected, and lets [`menu`]
//! draw the context menu of a selected commit. The panel calls [`panel`]
//! first. Comparing commits that are not loaded yet loads more history
//! until they are, as "Show in graph" does.

pub mod pick;

use std::sync::{Arc, Weak};

use crate::git::repo::Loader;

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
    /// The selection generation in which the two commits were set to be
    /// compared from their merge base.
    forked: Option<u64>,
    /// A commit selected alone that the panel compares with the working
    /// tree, while it stays the focus.
    working_tree: Option<Oid>,
    facts: Option<(Stamp, Arc<Facts>)>,
    /// The sides whose files were asked for, the snapshot they were asked
    /// at, and the answer once it came.
    asked: Option<Sides>,
    asked_at: Weak<RepoSnapshot>,
    files: Option<Result<Arc<Compared>, String>>,
    /// A comparison to show once its commits are in the graph.
    pending: Option<Pending>,
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
        self.working_tree = None;
    }

    /// After a refresh; see [`Picked::retain`].
    pub fn retain(&mut self, history: &History, focus: Option<Oid>) -> Option<Oid> {
        self.picked.retain(history, focus)
    }

    fn is_swapped(&self) -> bool {
        self.swapped == Some(self.picked.generation())
    }

    fn is_forked(&self) -> bool {
        self.forked == Some(self.picked.generation())
    }

    /// The two sides the panel compares: two commits, older first unless
    /// swapped (from their merge base when forked), or for a line of more,
    /// the change they make together.
    fn sides(&self, facts: &Facts) -> Option<Sides> {
        match facts.commits.as_slice() {
            [newer, older] => {
                let (from, to) = if self.is_swapped() { (*newer, *older) } else { (*older, *newer) };
                Some(if self.is_forked() { Sides::Forked { other: from, tip: to } } else { Sides::Trees { from, to } })
            }
            _ => facts.combined().map(|(from, to)| Sides::Trees { from, to }),
        }
    }

    /// The commit the panel compares with the working tree, if it shows
    /// that comparison: the focus, selected alone.
    fn working_tree_side(&self, focus: Option<Oid>) -> Option<Oid> {
        self.working_tree.filter(|oid| !self.is_active() && focus == Some(*oid))
    }
}

/// What the panel compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sides {
    /// One commit's tree with another's; `from` is shown as removed lines.
    Trees { from: Oid, to: Oid },
    /// What `tip` adds on its own since it forked from `other`: from their
    /// merge base to `tip` (git's `other...tip`).
    Forked { other: Oid, tip: Oid },
    /// A commit with the working tree, uncommitted changes included.
    WorkingTree { from: Oid },
}

/// The files that differ between two sides, and how their diffs are asked for.
struct Compared {
    files: Arc<Vec<FileChange>>,
    source: DiffSource,
    /// The merge base, for [`Sides::Forked`].
    base: Option<Oid>,
}

/// Works out what differs between `sides`, on the reader thread.
fn compared(loader: &Loader, sides: Sides) -> Result<Compared, String> {
    Ok(match sides {
        Sides::Trees { from, to } => Compared {
            files: Arc::new(loader.compare_files(from, to)?),
            source: DiffSource::Compare { from, to },
            base: None,
        },
        Sides::Forked { other, tip } => {
            let (base, files) = loader.forked_files(other, tip)?;
            Compared { files: Arc::new(files), source: DiffSource::Compare { from: base, to: tip }, base: Some(base) }
        }
        Sides::WorkingTree { from } => Compared {
            files: Arc::new(loader.working_tree_files(from)?),
            source: DiffSource::WorkingTree { from },
            base: None,
        },
    })
}

/// A comparison waiting for its commits to be loaded into the graph.
#[derive(Clone, Copy, Debug)]
enum Pending {
    /// Two commits, the focus on `a`; with `forked`, what `a` adds since it
    /// forked from `b`.
    Two { a: Oid, b: Oid, forked: bool },
    /// A commit with the working tree.
    WorkingTree(Oid),
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
    start(tab, shared, Pending::Two { a, b, forked: false });
}

/// Compares what `tip` adds on its own since it forked from `other`: from
/// their merge base to `tip`. Both are selected, focused on `tip`.
pub fn compare_forked(tab: &mut RepoTab, shared: &mut Shared, tip: Oid, other: Oid) {
    start(tab, shared, Pending::Two { a: tip, b: other, forked: true });
}

/// Selects `oid` alone and compares it with the working tree.
pub fn compare_with_working_tree(tab: &mut RepoTab, shared: &mut Shared, oid: Oid) {
    start(tab, shared, Pending::WorkingTree(oid));
}

fn start(tab: &mut RepoTab, shared: &mut Shared, pending: Pending) {
    tab.multi.pending = Some(pending);
    if let Some(snap) = tab.snap.clone() {
        finish_compare(tab, shared, &snap);
    }
}

fn finish_compare(tab: &mut RepoTab, shared: &mut Shared, snap: &RepoSnapshot) {
    let Some(pending) = tab.multi.pending else { return };
    let history = &snap.history;
    let wanted = match pending {
        Pending::Two { a, b, .. } => vec![a, b],
        Pending::WorkingTree(oid) => vec![oid],
    };
    let missing = wanted.into_iter().find(|oid| !selectable(history, *oid));
    if missing.is_none() {
        tab.multi.pending = None;
        match pending {
            Pending::Two { a, b, forked } => {
                tab.multi.picked.select(&[a, b]);
                tab.focus(Selection::Commit(a));
                if forked {
                    // `a` is the side whose changes are shown: the newer
                    // one unless swapped.
                    let generation = tab.multi.picked.generation();
                    tab.multi.forked = Some(generation);
                    tab.multi.swapped = (snap.row_of(a) > snap.row_of(b)).then_some(generation);
                }
                tab.scroll_to = snap.row_of(a);
            }
            Pending::WorkingTree(oid) => {
                tab.select(Selection::Commit(oid));
                tab.multi.working_tree = Some(oid);
                tab.scroll_to = snap.row_of(oid);
            }
        }
        tab.diff = None;
    } else if history.has_more {
        // A no-op while a bigger page is already on its way.
        tab.load_more();
    } else if let Some(missing) = missing {
        tab.multi.pending = None;
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
    if let Some((sides, result)) = tab.take_result::<(Sides, Result<Compared, String>)>(FILES_JOB)
        && tab.multi.asked == Some(sides)
    {
        tab.multi.files = Some(result.map(Arc::new));
    }
}

/// Asks the reader thread for the files that differ between `sides`, unless
/// they are the ones already shown or asked for. A comparison with the
/// working tree is asked again with every new snapshot, since the files on
/// disk may have changed; the files shown stay until the answer comes.
fn load_files(tab: &mut RepoTab, snap: &Arc<RepoSnapshot>, sides: Option<Sides>) {
    let same = tab.multi.asked == sides;
    let fresh = std::ptr::eq(tab.multi.asked_at.as_ptr(), Arc::as_ptr(snap));
    if same && (fresh || !matches!(sides, Some(Sides::WorkingTree { .. }))) {
        return;
    }
    if !same {
        tab.multi.files = None;
    }
    tab.multi.asked = sides;
    tab.multi.asked_at = Arc::downgrade(snap);
    if let Some(sides) = sides {
        tab.spawn_job(FILES_JOB, move |loader| (sides, compared(loader, sides)));
    }
}

/// The comparison shown, once its files are in.
fn shown(tab: &RepoTab) -> Option<&Compared> {
    tab.multi.files.as_ref()?.as_ref().ok().map(|compared| &**compared)
}

/// Closes an open diff of the comparison shown, when its sides change.
fn close_compared_diff(tab: &mut RepoTab) {
    let source = shown(tab).map(|compared| compared.source);
    if tab.diff.as_ref().is_some_and(|view| Some(view.target.source) == source) {
        tab.diff = None;
    }
}

/// Compares the two selected commits from their merge base, or tree to
/// tree again.
fn set_forked(tab: &mut RepoTab, on: bool) {
    close_compared_diff(tab);
    tab.multi.forked = on.then(|| tab.multi.picked.generation());
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
    if n == 2 {
        let forked = tab.multi.is_forked();
        let (icon, label) = fork_toggle(forked);
        if menu_item(ui, icon, label) {
            set_forked(tab, !forked);
        }
    }
    if menu_item(ui, icons::PENCIL_SIMPLE, "Compare this commit with working tree") {
        // Finished by the panel, which polls every frame (`finish_compare`).
        tab.multi.pending = Some(Pending::WorkingTree(oid));
    }
    ui.separator();
    if menu_item(ui, icons::COPY, &format!("Copy {n} SHAs")) {
        copy_shas(ui.ctx(), &facts);
    }
    if menu_item(ui, icons::CROSSHAIR, "Select only this commit") {
        tab.select(Selection::Commit(oid));
    }
    true
}

/// The menu entry that turns comparing from the merge base on or off.
fn fork_toggle(forked: bool) -> (&'static str, &'static str) {
    if forked {
        (icons::GIT_DIFF, "Compare tree to tree")
    } else {
        (icons::GIT_FORK, "Compare from the merge base")
    }
}

fn swap(tab: &mut RepoTab) {
    let generation = tab.multi.picked.generation();
    if tab.multi.is_forked() {
        // The other side's changes have another merge base, maybe: the
        // file open may not be in them.
        close_compared_diff(tab);
    }
    tab.multi.swapped = if tab.multi.is_swapped() { None } else { Some(generation) };
    // A file of the comparison that is open turns round with it. A renamed
    // file is found under its other name on the other side.
    if let Some(view) = &tab.diff
        && let DiffSource::Compare { from, to } = view.target.source
        && tab.multi.asked == Some(Sides::Trees { from, to })
    {
        let mut target = view.target.clone();
        target.source = DiffSource::Compare { from: to, to: from };
        if let Some(old) = target.old_path.take() {
            target.old_path = Some(std::mem::replace(&mut target.path, old));
        }
        tab.open_diff(target);
    }
}

/// "Compare with HEAD" and "Compare with working tree", for the menu of a
/// commit selected alone.
pub fn compare_with_head(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, snap: &RepoSnapshot, oid: Oid) {
    if let Some(head) = snap.head.oid.filter(|head| *head != oid)
        && menu_item(ui, icons::GIT_DIFF, "Compare with HEAD")
    {
        compare(tab, shared, oid, head);
    }
    working_tree_item(ui, tab, shared, snap, oid);
}

/// "Compare with working tree": left out for HEAD while nothing is changed,
/// where there would be nothing to show.
fn working_tree_item(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, snap: &RepoSnapshot, oid: Oid) {
    if snap.head.oid.is_none() || (snap.head.oid == Some(oid) && snap.status.is_clean()) {
        return;
    }
    let item = ui
        .button(format!("{}  Compare with working tree", icons::PENCIL_SIMPLE))
        .on_hover_text("What differs between this commit and the files on disk, uncommitted changes included");
    if item.clicked() {
        compare_with_working_tree(tab, shared, oid);
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
    let current = snap.head.branch.as_deref().filter(|_| !snap.head.detached).unwrap_or("HEAD");
    let forked = ui
        .button(format!("{}  {label} from the merge base", icons::GIT_FORK))
        .on_hover_text(format!(
            "What this adds on its own: its changes since it forked from {current} (git diff {current}...)"
        ));
    if forked.clicked() {
        compare_forked(tab, shared, tip, head);
    }
    working_tree_item(ui, tab, shared, snap, tip);
}

// ---- command palette ----------------------------------------------------------

/// Palette entries for the selection: the actions on several commits, or
/// "Compare with HEAD" and "Compare with working tree" for one.
pub fn palette_entries(list: &mut Vec<Entry>, tab: &RepoTab, snap: &RepoSnapshot) {
    if !tab.multi.is_active() {
        let Some(oid) = focused(tab).filter(|oid| selectable(&snap.history, *oid)) else { return };
        let Some(head) = snap.head.oid else { return };
        let summary = snap.row_of(oid).map(|row| snap.history.commits[row].summary.clone()).unwrap_or_default();
        let detail = format!("{} {summary}", short(oid));
        if oid != head {
            list.push(
                Entry::on_tab("compare-head", icons::GIT_DIFF, "Compare with HEAD", move |tab, shared| {
                    compare(tab, shared, oid, head)
                })
                .detail(detail.clone())
                .keywords("diff difference between commits"),
            );
        }
        if oid != head || !snap.status.is_clean() {
            list.push(
                Entry::on_tab("compare-working-tree", icons::PENCIL_SIMPLE, "Compare with working tree", move |tab, shared| {
                    compare_with_working_tree(tab, shared, oid)
                })
                .detail(detail)
                .keywords("diff difference uncommitted changes files on disk wip"),
            );
        }
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
        let forked = tab.multi.is_forked();
        let (icon, label) = fork_toggle(forked);
        list.push(
            Entry::on_tab("multi-fork", icon, label, move |tab, _| set_forked(tab, !forked))
                .detail(if forked { "what differs between the two trees" } else { "what the newer one adds on its own" })
                .keywords("compare merge base fork diverged three dot ... branch adds"),
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
    Forked(bool),
    CherryPick,
    Revert,
    Squash,
    Copy,
}

/// What a comparison from the merge base shows, said in words.
fn forked_text(other: Oid, tip: Oid, base: Option<Oid>) -> String {
    let (other_s, tip_s) = (short(other), short(tip));
    match base {
        Some(base) if base == other => format!(
            "From the merge base: what {tip_s} adds on its own. {other_s} is an ancestor of it, so this is the \
             same as comparing the two."
        ),
        Some(base) => format!(
            "From the merge base: what {tip_s} adds on its own, its changes since it forked from {other_s} at \
             {} (git diff {other_s}...{tip_s}).",
            short(base)
        ),
        None => format!("From the merge base: what {tip_s} adds on its own since it forked from {other_s}."),
    }
}

/// The right panel while several commits are selected: how many, what they
/// are, what differs, and buttons for what can be done with them; or while
/// a commit is compared with the working tree, that comparison. Returns
/// false, drawing nothing, otherwise.
pub fn panel(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, snap: &Arc<RepoSnapshot>) -> bool {
    poll(tab, shared, snap);
    if let Some(from) = tab.multi.working_tree_side(focused(tab)) {
        load_files(tab, snap, Some(Sides::WorkingTree { from }));
        working_tree_panel(ui, tab, shared, snap, from);
        return true;
    }
    if !tab.multi.is_active() {
        return false;
    }
    let facts = facts(tab, snap);
    let n = facts.commits.len();
    if n < 2 {
        return false;
    }
    let sides = tab.multi.sides(&facts);
    load_files(tab, snap, sides);
    let base = shown(tab).and_then(|compared| compared.base);
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
            (2, Some(Sides::Forked { other, tip })) => forked_text(other, tip, base),
            (2, _) => "The files that differ between the two commits.".to_owned(),
            (_, Some(Sides::Trees { from, .. })) => format!("One unbroken line of history: what the {n} change together, from {}.", short(from)),
            _ if facts.line => "One unbroken line, starting at the repository's first commit: there is nothing before it to compare with.".to_owned(),
            _ => "Not one unbroken line of history, so there is no combined change to show.".to_owned(),
        };
        ui.add(egui::Label::new(RichText::new(what).size(12.0).color(theme::text_dim())).wrap());
        if n == 2 {
            // Tree to tree, or what one adds since they forked (`A...B`).
            let mut forked = tab.multi.is_forked();
            ui.horizontal(|ui| segmented(ui, &mut forked, (icons::GIT_DIFF, "Tree to tree"), (icons::GIT_FORK, "From merge base")));
            if forked != tab.multi.is_forked() {
                act = Some(Act::Forked(forked));
            }
        }

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
        (2, Some(Sides::Trees { from, to } | Sides::Forked { other: from, tip: to })) => {
            vec![(from, Some(("from", theme::red()))), (to, Some(("to", theme::green())))]
        }
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

    if sides.is_some() {
        let forked = matches!(sides, Some(Sides::Forked { .. }));
        files(ui, tab, shared, if forked { "since the merge base" } else { "" });
    }

    match act {
        Some(Act::Collapse) => {
            escape(tab);
        }
        Some(Act::Reveal(oid)) => reveal(tab, snap, oid),
        Some(Act::Only(oid)) => tab.select(Selection::Commit(oid)),
        Some(Act::Swap) => swap(tab),
        Some(Act::Forked(on)) => set_forked(tab, on),
        Some(Act::CherryPick) => cherry_pick(tab, &facts),
        Some(Act::Revert) => revert(tab, &facts),
        Some(Act::Squash) => squash(tab, &facts),
        Some(Act::Copy) => copy_shas(ui.ctx(), &facts),
        None => {}
    }
    true
}

/// The panel while a commit is compared with the working tree: the commit,
/// the working tree, and the files that differ between them.
fn working_tree_panel(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, snap: &RepoSnapshot, from: Oid) {
    let mut close = false;
    let mut reveal_it = false;
    Frame::new().inner_margin(Margin::same(12)).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 6.0;
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("{}  Compared with the working tree", icons::PENCIL_SIMPLE))
                    .font(theme::bold(15.0))
                    .color(theme::text_bright()),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if icon_button(ui, icons::X, 13.0, "Back to the commit's details").clicked() {
                    close = true;
                }
            });
        });
        let what = format!(
            "What differs between {} and the files on disk: staged, unstaged and untracked changes included \
             (git diff {}).",
            short(from),
            short(from)
        );
        ui.add(egui::Label::new(RichText::new(what).size(12.0).color(theme::text_dim())).wrap());
    });

    ui.spacing_mut().item_spacing.y = 0.0;
    let (top, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().hline(top.x_range(), top.top(), Stroke::new(1.0, theme::border()));
    if let Some(row) = snap.row_of(from) {
        let commit = &snap.history.commits[row];
        let response = commit_row(ui, shared, commit, Some(("from", theme::red())), true)
            .on_hover_text(format!("{}\n{}", commit.summary, commit.oid));
        reveal_it = response.clicked();
    }
    working_tree_row(ui, snap, ("to", theme::green()));
    files(ui, tab, shared, "");

    if close {
        close_compared_diff(tab);
        tab.multi.working_tree = None;
    } else if reveal_it {
        tab.scroll_to = snap.row_of(from);
    }
}

/// A tinted tag naming a side of a comparison, at `x` in `rect`. Returns
/// where what follows it starts.
fn paint_tag(painter: &egui::Painter, rect: Rect, x: f32, (text, color): (&str, Color32)) -> f32 {
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
    chip.right() + 10.0
}

/// The working tree as a side of a comparison: what is changed in it.
fn working_tree_row(ui: &mut Ui, snap: &RepoSnapshot, tag: (&str, Color32)) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
    let x = paint_tag(&painter, rect, rect.left() + 12.0, tag);
    let center = pos2(x + 12.0, rect.center().y);
    painter.circle_filled(center, 12.0, theme::mix(theme::bg_app(), theme::yellow(), 0.18));
    painter.text(center, egui::Align2::CENTER_CENTER, icons::PENCIL_SIMPLE, theme::font(13.0), theme::yellow());

    let text_x = x + 32.0;
    let width = (rect.right() - 10.0 - text_x).max(20.0);
    let summary = elided(&painter, "Working tree", theme::font(13.0), theme::text(), width);
    painter.galley(pos2(text_x, rect.top() + 4.0), summary, theme::text());
    let changes = match snap.status.file_count() {
        0 => "No uncommitted changes".to_owned(),
        1 => "1 file with uncommitted changes".to_owned(),
        n => format!("{n} files with uncommitted changes"),
    };
    let rest = format!("{changes}  ·  on {}", snap.head_label());
    let rest = elided(&painter, &rest, theme::font(11.5), theme::text_dim(), width);
    painter.galley(pos2(text_x, rect.top() + 22.0), rest, theme::text_dim());
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
    if let Some(tag) = tag {
        x = paint_tag(&painter, rect, x, tag);
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

/// The files of the comparison shown, with their header; `since` follows
/// the count ("since the merge base").
fn files(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, since: &str) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::hover());
    ui.painter().hline(rect.x_range(), rect.top(), Stroke::new(1.0, theme::border()));
    let compared = tab.multi.files.clone();
    let title = match &compared {
        None => "Comparing…".to_owned(),
        Some(Ok(compared)) if compared.files.is_empty() => "No files differ".to_owned(),
        Some(Ok(compared)) if !since.is_empty() => {
            let n = compared.files.len();
            format!("{n} file{} {since}", if n == 1 { "" } else { "s" })
        }
        Some(Ok(compared)) => {
            let n = compared.files.len();
            format!("{n} file{} differ{}", if n == 1 { "" } else { "s" }, if n == 1 { "s" } else { "" })
        }
        Some(Err(_)) => "Could not compare".to_owned(),
    };
    // The sides are named above: the two rows, or the line's description.
    let galley = elided(ui.painter(), &title, theme::font(13.0), theme::text(), rect.width() - 150.0);
    ui.painter().galley(pos2(rect.left() + 12.0, rect.center().y - galley.size().y / 2.0), galley, theme::text());
    ui.scope_builder(
        egui::UiBuilder::new().max_rect(rect.shrink2(vec2(8.0, 0.0))).layout(Layout::right_to_left(Align::Center)),
        |ui| segmented(ui, &mut shared.settings.tree_view, (icons::LIST, "Path"), (icons::TREE_VIEW, "Tree")),
    );

    match compared {
        Some(Ok(compared)) if !compared.files.is_empty() => {
            let height = ui.available_height();
            file_list(ui, tab, shared, &compared.files, compared.source, height);
        }
        Some(Ok(compared)) if matches!(compared.source, DiffSource::WorkingTree { .. }) => {
            widgets::placeholder(ui, "The files on disk are as they are in this commit.")
        }
        Some(Ok(_)) if !since.is_empty() => widgets::placeholder(ui, "Nothing changed since they forked."),
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
        assert_eq!(labels(&tab, &snap), ["Compare with HEAD", "Compare with working tree"]);
        tab.select(Selection::Commit(c));
        assert!(labels(&tab, &snap).is_empty(), "HEAD has nothing to compare with itself");

        // Both on HEAD already: nothing to cherry-pick.
        tab.multi.picked.select(&[c, b]);
        tab.focus(Selection::Commit(c));
        assert_eq!(
            labels(&tab, &snap),
            [
                "Revert 2 commits",
                "Squash 2 commits",
                "Swap the compared commits",
                "Compare from the merge base",
                "Copy 2 SHAs",
                "Select one commit only"
            ]
        );
        // The first commit of the repository cannot be squashed into.
        tab.multi.picked.select(&[b, base]);
        assert!(!labels(&tab, &snap).iter().any(|label| label.starts_with("Squash")));
        // A plain selection again.
        tab.select(Selection::Commit(b));
        assert!(!tab.multi.is_active());
        assert_eq!(labels(&tab, &snap), ["Compare with HEAD", "Compare with working tree"]);
        // Something changed: HEAD can be compared with the working tree.
        fx.write("c.txt", "changed\n");
        let dirty = fx.snapshot();
        tab.snap = Some(dirty.clone());
        tab.select(Selection::Commit(c));
        assert_eq!(labels(&tab, &dirty), ["Compare with working tree"]);
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
        assert_eq!(multi.sides(&facts), Some(Sides::Trees { from: older, to: newer }));
        multi.swapped = Some(multi.picked.generation());
        assert_eq!(multi.sides(&facts), Some(Sides::Trees { from: newer, to: older }));
        // From the merge base: what the "to" side adds since it forked.
        multi.forked = Some(multi.picked.generation());
        assert_eq!(multi.sides(&facts), Some(Sides::Forked { other: newer, tip: older }));
        // Another selection compares older first again, tree to tree.
        multi.picked.select(&[older, newer]);
        assert_eq!(multi.sides(&facts), Some(Sides::Trees { from: older, to: newer }));
    }

    fn head(fx: &Fixture) -> Oid {
        Oid::from_str(&crate::tests::git(&fx.dir, &["rev-parse", "HEAD"])).unwrap()
    }

    fn compare_in(fx: &Fixture, sides: Sides) -> Result<Compared, String> {
        fx.job(move |loader| compared(loader, sides))
    }

    fn kinds(compared: &Compared) -> Vec<(&str, crate::git::ChangeKind)> {
        compared.files.iter().map(|file| (file.path.as_str(), file.kind)).collect()
    }

    fn diff_in(fx: &Fixture, source: DiffSource, path: &str, mode: crate::git::diff::Mode) -> crate::git::FileDiff {
        let target = crate::git::DiffTarget { source, path: path.to_owned(), old_path: None };
        fx.job(move |loader| loader.diff_with(&target, mode)).unwrap()
    }

    /// The changed lines of a diff worked out in `mode`, as `+text` and `-text`.
    fn changes(fx: &Fixture, source: DiffSource, path: &str, mode: crate::git::diff::Mode) -> Vec<String> {
        let diff = diff_in(fx, source, path, mode);
        diff.hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter(|l| l.origin != ' ')
            .map(|l| format!("{}{}", l.origin, l.text))
            .collect()
    }

    #[test]
    fn a_commit_is_compared_with_the_working_tree_changes_included() {
        use crate::git::ChangeKind::{Deleted, Modified, Untracked};
        use crate::git::diff::Mode;
        let fx = Fixture::new("multi-working-tree");
        let numbered: String = (1..=20).map(|n| format!("line {n}\n")).collect();
        fx.write("kept.txt", "same\n");
        fx.write("staged ü.txt", "one\n");
        fx.write("edited.txt", "fn main() {\nfoo();\n}\n");
        fx.write("long.txt", &numbered);
        fx.write("gone.txt", "bye\n");
        fx.commit_all("first");
        let first = head(&fx);
        fx.write("staged ü.txt", "two\n");
        fx.commit_all("second");

        // Nothing uncommitted: the commit against what HEAD has.
        let clean = compare_in(&fx, Sides::WorkingTree { from: first }).unwrap();
        assert_eq!(kinds(&clean), [("staged ü.txt", Modified)]);
        assert_eq!(clean.source, DiffSource::WorkingTree { from: first });

        // Staged, unstaged, deleted on disk only, untracked in a new folder.
        fx.write("staged ü.txt", "three\n");
        crate::tests::git(&fx.dir, &["add", "staged ü.txt"]);
        fx.write("edited.txt", "fn main() {\n    foo();\n    bar();\n}\n");
        fx.write("long.txt", &numbered.replace("line 10\n", "line ten\n"));
        std::fs::remove_file(fx.dir.join("gone.txt")).unwrap();
        fx.write("new dir/new file.txt", "fresh\n");
        let dirty = compare_in(&fx, Sides::WorkingTree { from: first }).unwrap();
        assert_eq!(
            kinds(&dirty),
            [
                ("edited.txt", Modified),
                ("gone.txt", Deleted),
                ("long.txt", Modified),
                ("new dir/new file.txt", Untracked),
                ("staged ü.txt", Modified)
            ]
        );

        // Their diffs, in each mode.
        let source = DiffSource::WorkingTree { from: first };
        let plain = Mode::default();
        assert_eq!(changes(&fx, source, "staged ü.txt", plain), ["-one", "+three"]);
        assert_eq!(changes(&fx, source, "new dir/new file.txt", plain), ["+fresh"]);
        assert_eq!(changes(&fx, source, "gone.txt", plain), ["-bye"]);
        assert_eq!(changes(&fx, source, "edited.txt", plain), ["-foo();", "+    foo();", "+    bar();"]);
        let ignoring = Mode { ignore_whitespace: true, ..plain };
        assert_eq!(changes(&fx, source, "edited.txt", ignoring), ["+    bar();"]);
        let whole = diff_in(&fx, source, "long.txt", Mode { whole_file: true, ..plain });
        assert!(whole.hunks.iter().any(|h| h.gap), "the unchanged lines are there too");
        assert_eq!(whole.hunks.iter().flat_map(|h| &h.lines).count(), 21);
        // Read-only, and read from disk.
        assert!(source.is_committed() && source.reads_working_tree());
    }

    #[test]
    fn two_branches_are_compared_from_where_they_forked() {
        use crate::git::ChangeKind::Added;
        let fx = Fixture::new("multi-forked");
        let git = |args: &[&str]| crate::tests::git(&fx.dir, args);
        fx.write("base.txt", "base\n");
        fx.commit_all("base");
        let fork = head(&fx);
        git(&["checkout", "-q", "-b", "feature"]);
        fx.write("feature file.txt", "feature\n");
        fx.commit_all("feature work");
        let feature = head(&fx);
        git(&["checkout", "-q", "main"]);
        fx.write("main.txt", "main\n");
        fx.commit_all("main work");
        let main = head(&fx);

        // Tree to tree, main's own change shows too.
        let trees = compare_in(&fx, Sides::Trees { from: main, to: feature }).unwrap();
        assert_eq!(trees.files.len(), 2);
        // From the merge base: only what the feature adds.
        let forked = compare_in(&fx, Sides::Forked { other: main, tip: feature }).unwrap();
        assert_eq!(forked.base, Some(fork));
        assert_eq!(kinds(&forked), [("feature file.txt", Added)]);
        assert_eq!(forked.source, DiffSource::Compare { from: fork, to: feature });
        // And the other way round, what main adds.
        let back = compare_in(&fx, Sides::Forked { other: feature, tip: main }).unwrap();
        assert_eq!(kinds(&back), [("main.txt", Added)]);
        // An ancestor is its own merge base: the same as comparing the two.
        let ancestor = compare_in(&fx, Sides::Forked { other: fork, tip: feature }).unwrap();
        assert_eq!(ancestor.base, Some(fork));
        assert!(forked_text(fork, feature, ancestor.base).contains("is an ancestor"));

        // Histories that never met have no merge base.
        git(&["checkout", "-q", "--orphan", "alone"]);
        git(&["rm", "-rfq", "."]);
        fx.write("alone.txt", "alone\n");
        fx.commit_all("unrelated");
        let alone = head(&fx);
        let error = compare_in(&fx, Sides::Forked { other: main, tip: alone }).err().expect("no merge base");
        assert!(error.contains("no commit in common"), "{error}");
    }
}
