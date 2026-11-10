//! Dragging a branch onto another branch, a remote or a commit.
//!
//! Branch labels in the graph and branch rows in the sidebar are drag
//! sources. Dropping one on another branch, on a remote or on a commit row
//! opens a small menu, at the drop point, with what can be done with that
//! pair: merge, rebase, fast-forward, push, reset.
//!
//! How the pieces fit:
//!
//! * A source calls [`source`]; the dragged branch travels as an egui
//!   drag-and-drop payload, which egui drops on release and on Escape.
//! * A possible target asks [`session`] once per frame whether something is
//!   being dragged, then [`Session::accepts`] for the highlight and
//!   [`Session::over`] when the pointer is on it. Releasing there records a
//!   [`Dropped`] with the actions [`plan`] offers.
//! * [`overlay`], called by both the sidebar and the graph, paints the ghost
//!   that follows the pointer and shows the menu for the last drop.
//!
//! [`offer`] decides which actions exist for a pair. It is a pure function of
//! the pair and a few [`Facts`], so it can be tested without a window.
//!
//! Actions that need another branch checked out (merging into a branch that
//! is not the current one, rebasing one that is not) do the checkout as part
//! of the same command and say so, in the menu and in the toast.

use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui::{
    self, Align, Align2, Color32, Context, DragAndDrop, Id, LayerId, Layout, Order, Painter,
    PointerButton, Popup, PopupAnchor, PopupKind, Pos2, Rect, Response, Stroke, StrokeKind, Ui,
    pos2, vec2,
};

use crate::git::ops::{self, GitCmd, ResetMode, UndoKind};
use crate::git::{History, Oid, RefKind, RefLabel, RemoteBranch, RepoSnapshot, RowKind, short};
use crate::state::{Dialog, RepoTab, Shared};
use crate::theme::{self, icons};

// ---- what is dragged, and onto what -----------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Branch {
    Local(String),
    /// A branch on a remote: `origin` and `main` for `origin/main`.
    Remote { remote: String, name: String },
}

impl Branch {
    pub fn local(name: &str) -> Self {
        Branch::Local(name.to_owned())
    }

    pub fn remote(branch: &RemoteBranch) -> Self {
        Branch::Remote { remote: branch.remote.clone(), name: branch.name.clone() }
    }

    /// The branch a graph label stands for; `None` for tags and `HEAD`.
    pub fn of_label(snap: &RepoSnapshot, label: &RefLabel) -> Option<Self> {
        match label.kind {
            RefKind::Local => Some(Branch::local(&label.name)),
            RefKind::Remote => {
                snap.remote_branches.iter().find(|b| b.full == label.name).map(Branch::remote)
            }
            RefKind::Tag | RefKind::Head => None,
        }
    }

    /// Whether a graph label stands for this very branch.
    pub fn is_label(&self, label: &RefLabel) -> bool {
        match (self, label.kind) {
            (Branch::Local(name), RefKind::Local) => *name == label.name,
            (Branch::Remote { remote, name }, RefKind::Remote) => label
                .name
                .strip_prefix(remote.as_str())
                .and_then(|rest| rest.strip_prefix('/'))
                .is_some_and(|rest| rest == name),
            _ => false,
        }
    }

    /// As git and the rest of the UI call it: `main`, `origin/main`.
    pub fn name(&self) -> String {
        match self {
            Branch::Local(name) => name.clone(),
            Branch::Remote { remote, name } => format!("{remote}/{name}"),
        }
    }

    fn refname(&self) -> String {
        match self {
            Branch::Local(name) => format!("refs/heads/{name}"),
            Branch::Remote { remote, name } => format!("refs/remotes/{remote}/{name}"),
        }
    }

    fn tip(&self, snap: &RepoSnapshot) -> Option<Oid> {
        match self {
            Branch::Local(name) => snap.branches.iter().find(|b| &b.name == name)?.oid,
            Branch::Remote { remote, name } => snap
                .remote_branches
                .iter()
                .find(|b| &b.remote == remote && &b.name == name)
                .map(|b| b.oid),
        }
    }

    fn icon(&self) -> &'static str {
        match self {
            Branch::Local(_) => icons::LAPTOP,
            Branch::Remote { .. } => icons::CLOUD,
        }
    }
}

/// Where a branch can be dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Branch(Branch),
    Commit(Oid),
    /// A remote as a whole: its row in the sidebar.
    Remote(String),
}

impl Target {
    fn name(&self) -> String {
        match self {
            Target::Branch(branch) => branch.name(),
            Target::Commit(oid) => short(*oid),
            Target::Remote(remote) => remote.clone(),
        }
    }

    /// The commit the dragged branch is compared with. For a remote as a
    /// whole that is its branch of the same name, if it has one.
    fn tip(&self, snap: &RepoSnapshot, dragged: &Branch) -> Option<Oid> {
        match (self, dragged) {
            (Target::Branch(branch), _) => branch.tip(snap),
            (Target::Commit(oid), _) => Some(*oid),
            (Target::Remote(remote), Branch::Local(name)) => snap
                .remote_branches
                .iter()
                .find(|b| &b.remote == remote && &b.name == name)
                .map(|b| b.oid),
            (Target::Remote(_), Branch::Remote { .. }) => None,
        }
    }
}

/// How the dragged branch's tip relates to the target's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Relation {
    /// Both point at the same commit.
    Same,
    /// The dragged branch contains everything the target has, and more.
    Ahead,
    /// The target contains everything the dragged branch has, and more.
    Behind,
    /// Each has commits the other lacks, or we cannot tell.
    Diverged,
}

/// Whether `ancestor` can be reached from `descendant`, going by the loaded
/// part of the history. Rows are ordered children first, so one pass from the
/// descendant's row down to the ancestor's is enough.
fn is_ancestor(history: &History, ancestor: Oid, descendant: Oid) -> bool {
    let (Some(&last), Some(&first)) = (history.index_of.get(&ancestor), history.index_of.get(&descendant))
    else {
        return false;
    };
    let (first, last) = (first as usize, last as usize);
    if last < first {
        return false;
    }
    let mut reached = vec![false; last - first + 1];
    reached[0] = true;
    for row in first..=last {
        if !reached[row - first] {
            continue;
        }
        for parent in &history.commits[row].parents {
            if let Some(&index) = history.index_of.get(parent)
                && (first..=last).contains(&(index as usize))
            {
                reached[index as usize - first] = true;
            }
        }
    }
    reached[last - first]
}

/// Works on what is loaded in the graph; commits further back than that
/// make the answer [`Relation::Diverged`], which offers merge and rebase and
/// leaves it to git to notice there is nothing to do.
pub fn relation(history: &History, dragged: Oid, target: Oid) -> Relation {
    if dragged == target {
        Relation::Same
    } else if is_ancestor(history, target, dragged) {
        Relation::Ahead
    } else if is_ancestor(history, dragged, target) {
        Relation::Behind
    } else {
        Relation::Diverged
    }
}

// ---- which actions a pair gets ----------------------------------------------

/// What [`offer`] needs to know besides the pair itself.
#[derive(Clone, Copy, Debug)]
pub struct Facts<'a> {
    /// The checked-out branch, if HEAD is on one.
    pub head: Option<&'a str>,
    /// Of the dragged branch to the target. For a remote as a whole: to the
    /// branch of the same name there, [`Relation::Diverged`] if it has none.
    pub relation: Relation,
    /// Upstream of the dragged branch (`origin/main`), if it has one.
    pub upstream: Option<&'a str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Move `branch` forward to `to`; no merge commit, nothing rewritten.
    FastForward {
        branch: String,
        /// Name of the branch it catches up with.
        to: String,
        /// The same as a full ref name.
        to_ref: String,
        checked_out: bool,
    },
    /// `checkout` when `into` has to be checked out first.
    Merge { source: String, into: String, checkout: bool },
    /// `onto` is what git is given; `onto_name` is how it is shown.
    /// `checkout` when `branch` is not the current one: git checks it out.
    Rebase { branch: String, onto: String, onto_name: String, checkout: bool },
    Push { branch: String, remote: String },
    /// Point `branch` at a commit, wherever that is.
    Reset { branch: String, to: Oid, checked_out: bool },
}

/// The actions that make sense when `dragged` is dropped on `target`, most
/// likely first. Empty when there is nothing to do or the pair makes no sense.
pub fn offer(dragged: &Branch, target: &Target, facts: &Facts<'_>) -> Vec<Action> {
    use Relation::{Ahead, Behind, Diverged, Same};
    let is_head = |branch: &str| facts.head == Some(branch);
    let mut actions = Vec::new();

    match (dragged, target) {
        (Branch::Local(source), Target::Branch(Branch::Local(into))) if source != into => {
            match facts.relation {
                Same => {}
                Ahead => actions.push(Action::FastForward {
                    branch: into.clone(),
                    to: source.clone(),
                    to_ref: dragged.refname(),
                    checked_out: is_head(into),
                }),
                Behind => actions.push(Action::FastForward {
                    branch: source.clone(),
                    to: into.clone(),
                    to_ref: format!("refs/heads/{into}"),
                    checked_out: is_head(source),
                }),
                Diverged => {
                    actions.push(Action::Merge {
                        source: source.clone(),
                        into: into.clone(),
                        checkout: !is_head(into),
                    });
                    actions.push(Action::Rebase {
                        branch: source.clone(),
                        onto: into.clone(),
                        onto_name: into.clone(),
                        checkout: !is_head(source),
                    });
                }
            }
        }

        // A remote branch cannot be merged into, only caught up with, built
        // on, or pushed to.
        (Branch::Local(source), Target::Branch(remote_branch @ Branch::Remote { remote, name })) => {
            let full = remote_branch.name();
            match facts.relation {
                Same | Ahead => {}
                Behind => actions.push(Action::FastForward {
                    branch: source.clone(),
                    to: full.clone(),
                    to_ref: remote_branch.refname(),
                    checked_out: is_head(source),
                }),
                Diverged => actions.push(Action::Rebase {
                    branch: source.clone(),
                    onto: full.clone(),
                    onto_name: full.clone(),
                    checkout: !is_head(source),
                }),
            }
            // Pushing lands on the branch of the same name, so only offer it
            // on the dragged branch's own counterpart, and only when a plain
            // push can succeed.
            let counterpart = name == source || facts.upstream == Some(full.as_str());
            if counterpart && facts.relation == Ahead {
                actions.push(Action::Push { branch: source.clone(), remote: remote.clone() });
            }
        }

        // The relation here is to the branch of the same name on that
        // remote, if there is one: nothing to push when it has it all.
        (Branch::Local(source), Target::Remote(remote)) => {
            if !matches!(facts.relation, Same | Behind) {
                actions.push(Action::Push { branch: source.clone(), remote: remote.clone() });
            }
        }

        // A remote branch is never rewritten or moved from here; it can only
        // be brought into a local one.
        (Branch::Remote { .. }, Target::Branch(Branch::Local(into))) => {
            let full = dragged.name();
            match facts.relation {
                Same | Behind => {}
                Ahead => actions.push(Action::FastForward {
                    branch: into.clone(),
                    to: full,
                    to_ref: dragged.refname(),
                    checked_out: is_head(into),
                }),
                Diverged => actions.push(Action::Merge {
                    source: full,
                    into: into.clone(),
                    checkout: !is_head(into),
                }),
            }
        }

        (Branch::Local(source), Target::Commit(oid)) => {
            if facts.relation == Same {
                return actions;
            }
            if facts.relation == Diverged {
                actions.push(Action::Rebase {
                    branch: source.clone(),
                    onto: oid.to_string(),
                    onto_name: short(*oid),
                    checkout: !is_head(source),
                });
            }
            actions.push(Action::Reset { branch: source.clone(), to: *oid, checked_out: is_head(source) });
        }

        _ => {}
    }
    actions
}

impl Action {
    /// The menu entry, with the real names.
    pub fn label(&self) -> String {
        match self {
            Action::FastForward { branch, to, .. } => format!("Fast-forward {branch} to {to}"),
            Action::Merge { source, into, .. } => format!("Merge {source} into {into}"),
            Action::Rebase { branch, onto_name, .. } => format!("Rebase {branch} onto {onto_name}"),
            Action::Push { branch, remote } => format!("Push {branch} to {remote}"),
            Action::Reset { branch, .. } => format!("Reset {branch} to this commit"),
        }
    }

    /// The side effect worth warning about, if any: which branch ends up
    /// checked out.
    pub fn note(&self) -> Option<String> {
        match self {
            Action::Merge { into, checkout: true, .. } => Some(format!("checks out {into}")),
            Action::Rebase { branch, checkout: true, .. } => Some(format!("checks out {branch}")),
            _ => None,
        }
    }

    fn icon(&self) -> &'static str {
        match self {
            Action::FastForward { .. } => icons::FAST_FORWARD,
            Action::Merge { .. } => icons::GIT_MERGE,
            Action::Rebase { .. } => icons::GIT_FORK,
            Action::Push { .. } => icons::ARROW_LINE_UP,
            Action::Reset { .. } => icons::ARROW_LINE_DOWN,
        }
    }

    /// The git invocations for this action. `None` for a reset, which first
    /// needs a mode: see [`reset_command`].
    pub fn command(&self) -> Option<GitCmd> {
        Some(match self {
            Action::FastForward { branch, to, checked_out: true, .. } => {
                GitCmd::new(format!("Fast-forward {branch}"), &["merge", "--ff-only", to])
                    .undoable(UndoKind::HeadMove)
                    .done(format!("Fast-forwarded {branch} to {to}"))
            }
            // A branch that is not checked out can be moved forward in place:
            // a fetch from the repository itself refuses anything but a
            // fast-forward, and refuses to touch the checked-out branch.
            Action::FastForward { branch, to, to_ref, checked_out: false } => GitCmd::new(
                format!("Fast-forward {branch}"),
                &["fetch", ".", &format!("{to_ref}:refs/heads/{branch}")],
            )
            .done(format!("Fast-forwarded {branch} to {to}")),

            Action::Merge { source, into, checkout: false } => {
                ops::merge(source).done(format!("Merged {source} into {into}"))
            }
            Action::Merge { source, into, checkout: true } => {
                GitCmd::new(format!("Checkout {into} and merge {source}"), &["checkout", into])
                    .step(&["merge", "--no-edit", source])
                    .done(format!("Checked out {into} and merged {source}"))
            }

            Action::Rebase { branch, onto, onto_name, checkout: false } => {
                ops::rebase(onto).done(format!("Rebased {branch} onto {onto_name}"))
            }
            // Given the branch as well, git checks it out before rebasing.
            Action::Rebase { branch, onto, onto_name, checkout: true } => GitCmd::new(
                format!("Checkout {branch} and rebase onto {onto_name}"),
                &["rebase", onto, branch],
            )
            .done(format!("Checked out {branch} and rebased onto {onto_name}")),

            Action::Push { branch, remote } => ops::push_branch(remote, branch),
            Action::Reset { .. } => return None,
        })
    }
}

/// Moves `branch` to `to`. The checked-out branch is reset in the chosen
/// mode; any other branch is only a pointer, so `mode` does not apply.
pub fn reset_command(branch: &str, to: Oid, checked_out: bool, mode: ResetMode) -> GitCmd {
    let sha = to.to_string();
    if checked_out {
        ops::reset(&sha, mode)
    } else {
        GitCmd::new(format!("Reset {branch} to {}", short(to)), &["branch", "-f", branch, &sha])
    }
}

/// The actions for dropping `dragged` on `target` in this repository.
pub fn plan(snap: &RepoSnapshot, dragged: &Branch, target: &Target) -> Vec<Action> {
    offer(dragged, target, &facts(snap, dragged, target))
}

fn facts<'a>(snap: &'a RepoSnapshot, dragged: &Branch, target: &Target) -> Facts<'a> {
    let relation = match (dragged.tip(snap), target.tip(snap, dragged)) {
        (Some(a), Some(b)) => relation(&snap.history, a, b),
        _ => Relation::Diverged,
    };
    let upstream = match dragged {
        Branch::Local(name) => snap.branches.iter().find(|b| &b.name == name).and_then(|b| b.upstream.as_deref()),
        Branch::Remote { .. } => None,
    };
    Facts { head: snap.head.branch.as_deref(), relation, upstream }
}

/// Whether dropping `dragged` on `target` can lead anywhere. Cheap enough to
/// ask for every label on screen while dragging: it looks at the kinds and
/// the tips only, not at how the histories relate.
pub fn accepts(snap: &RepoSnapshot, dragged: &Branch, target: &Target) -> bool {
    let tips = || (dragged.tip(snap), target.tip(snap, dragged));
    let differ = || matches!(tips(), (Some(a), Some(b)) if a != b);
    match (dragged, target) {
        (Branch::Local(a), Target::Branch(Branch::Local(b))) => a != b && differ(),
        (Branch::Local(_), Target::Branch(Branch::Remote { .. })) => differ(),
        // Also when the remote has no such branch yet.
        (Branch::Local(_), Target::Remote(_)) => matches!(tips(), (Some(a), b) if Some(a) != b),
        (Branch::Remote { .. }, Target::Branch(Branch::Local(_))) => differ(),
        (Branch::Local(_), Target::Commit(oid)) => {
            differ() && snap.row_of(*oid).is_some_and(|row| snap.history.commits[row].kind == RowKind::Commit)
        }
        (Branch::Remote { .. }, _) => false,
    }
}

/// Why a drop that was accepted has nothing to offer after all.
fn nothing_to_do(dragged: &Branch, target: &Target, relation: Relation) -> String {
    let (dragged, target) = (dragged.name(), target.name());
    match relation {
        Relation::Ahead => format!("{dragged} already contains {target}"),
        Relation::Behind => format!("{target} already contains {dragged}"),
        Relation::Same => format!("{dragged} and {target} are at the same commit"),
        Relation::Diverged => format!("Nothing to do with {dragged} and {target}"),
    }
}

// ---- dragging ---------------------------------------------------------------

/// The payload that travels with the pointer.
struct Dragged {
    branch: Branch,
    /// Colour of the ghost: the branch's line colour where it is known.
    color: Color32,
}

/// The result of a drop, kept until its menu closes.
struct Dropped {
    /// Repository the drop happened in; a menu never outlives a tab switch.
    tab: PathBuf,
    dragged: Branch,
    target: Target,
    pos: Pos2,
    actions: Vec<Action>,
    /// Shown instead when there are no actions.
    nothing: String,
}

fn menu_id() -> Id {
    Id::new("branch-drop-menu")
}

/// Makes `response` a drag source for `branch`. The widget must sense drags;
/// clicks keep working, since a drag only starts once the pointer has moved.
/// `branch` is only asked when a drag starts; `None` means there is nothing
/// to drag after all. `color` tints the ghost.
pub fn source(response: &Response, branch: impl FnOnce() -> Option<Branch>, color: Color32) {
    if response.drag_started_by(PointerButton::Primary)
        && let Some(branch) = branch()
    {
        DragAndDrop::set_payload(&response.ctx, Dragged { branch, color });
        // A menu from an earlier drop is about another pair.
        Popup::close_id(&response.ctx, menu_id());
    }
}

/// A branch drag in progress, as seen from one frame.
pub struct Session {
    dragged: Arc<Dragged>,
    released: bool,
}

/// `Some` while a branch is being dragged, including the frame it is dropped.
pub fn session(ctx: &Context) -> Option<Session> {
    let dragged = DragAndDrop::payload::<Dragged>(ctx)?;
    let (down, released) = ctx.input(|i| (i.pointer.any_down(), i.pointer.any_released()));
    if !down && !released {
        // The button came up where we could not see it (outside the window):
        // the drag is over, and a later click must not count as its drop.
        DragAndDrop::clear_payload(ctx);
        return None;
    }
    Some(Session { dragged, released })
}

impl Session {
    pub fn branch(&self) -> &Branch {
        &self.dragged.branch
    }

    pub fn accepts(&self, snap: &RepoSnapshot, target: &Target) -> bool {
        accepts(snap, &self.dragged.branch, target)
    }

    /// Call for the accepted target the pointer is on: names it in the ghost
    /// and, when the button was released this frame, opens the menu.
    pub fn over(&self, ctx: &Context, tab: &RepoTab, snap: &RepoSnapshot, target: Target) {
        let frame = ctx.cumulative_pass_nr();
        ctx.data_mut(|data| data.insert_temp(Id::new("branch-drag-over"), (frame, target.name())));
        if !self.released {
            return;
        }
        let Some(pos) = ctx.input(|i| i.pointer.interact_pos()) else { return };
        let dragged = self.dragged.branch.clone();
        let actions = plan(snap, &dragged, &target);
        let nothing = match actions.is_empty() {
            true => nothing_to_do(&dragged, &target, facts(snap, &dragged, &target).relation),
            false => String::new(),
        };
        let dropped = Dropped { tab: tab.path.clone(), dragged, target, pos, actions, nothing };
        DragAndDrop::clear_payload(ctx);
        ctx.data_mut(|data| data.insert_temp(menu_id(), Arc::new(dropped)));
        Popup::open_id(ctx, menu_id());
    }
}

/// Marks a row as the place the dragged branch would land.
pub fn highlight_row(painter: &Painter, rect: Rect) {
    painter.rect(
        rect.shrink(0.5),
        2.0,
        theme::with_alpha(theme::accent(), 46),
        Stroke::new(1.5, theme::accent()),
        StrokeKind::Inside,
    );
}

/// Marks a branch label as the place the dragged branch would land.
pub fn highlight_pill(painter: &Painter, rect: Rect) {
    painter.rect(
        rect.expand(2.0),
        5.0,
        theme::with_alpha(theme::text_bright(), 30),
        Stroke::new(2.0, theme::text_bright()),
        StrokeKind::Outside,
    );
}

/// The ghost of the dragged label and the menu of the last drop. Both the
/// sidebar and the graph call this every frame; the first call does the work.
pub fn overlay(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared) {
    let ctx = ui.ctx().clone();
    let frame = ctx.cumulative_pass_nr();
    let first = ctx.data_mut(|data| {
        let last = data.get_temp_mut_or_insert_with(Id::new("branch-drag-frame"), || u64::MAX);
        std::mem::replace(last, frame) != frame
    });
    if !first {
        return;
    }

    if let Some(session) = session(&ctx) {
        if !session.released {
            ghost(&ctx, &session.dragged, frame);
        }
        // The pointer moves without events of ours; keep the ghost following.
        ctx.request_repaint();
    }

    let Some(dropped) = ctx.data(|data| data.get_temp::<Arc<Dropped>>(menu_id())) else { return };
    let forget = |ctx: &Context| ctx.data_mut(|data| data.remove::<Arc<Dropped>>(menu_id()));
    if dropped.tab != tab.path {
        Popup::close_id(&ctx, menu_id());
        return forget(&ctx);
    }
    let shown = Popup::new(menu_id(), ctx.clone(), PopupAnchor::Position(dropped.pos), ui.layer_id())
        .kind(PopupKind::Menu)
        .layout(Layout::top_down_justified(Align::Min))
        .style(egui::containers::menu::menu_style)
        .open_memory(None)
        .show(|ui| menu(ui, tab, shared, &dropped));
    if shown.is_none() {
        forget(&ctx);
    }
}

fn menu(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, dropped: &Dropped) {
    ui.set_min_width(250.0);
    ui.label(
        egui::RichText::new(format!(
            "{}  {}  {}",
            dropped.dragged.name(),
            icons::ARROW_RIGHT,
            dropped.target.name()
        ))
        .color(theme::text_dim())
        .size(11.5),
    );
    ui.separator();

    let quiet = |ui: &mut Ui, text: String| {
        ui.label(egui::RichText::new(text).color(theme::text_dim()));
    };
    let Some(snap) = tab.snap.clone() else { return };
    if let Some(state) = snap.state.label() {
        return quiet(ui, format!("{}  {state}: finish or abort it first", icons::WARNING));
    }
    if dropped.actions.is_empty() {
        return quiet(ui, dropped.nothing.clone());
    }

    for action in &dropped.actions {
        let text = format!("{}  {}", action.icon(), action.label());
        match action {
            Action::Reset { branch, to, checked_out: true } => {
                let (to, abbrev) = (*to, short(*to));
                ui.menu_button(text, |ui| {
                    ui.set_min_width(260.0);
                    if ui.button("Soft - keep all changes staged").clicked() {
                        tab.run(reset_command(branch, to, true, ResetMode::Soft));
                    }
                    if ui.button("Mixed - keep changes in the working tree").clicked() {
                        tab.run(reset_command(branch, to, true, ResetMode::Mixed));
                    }
                    if ui.button("Hard - discard all changes").clicked() {
                        shared.show(Dialog::confirm(
                            "Hard reset",
                            format!(
                                "Reset {branch} to {abbrev} and discard every uncommitted change? \
                                 This cannot be undone."
                            ),
                            "Reset",
                            reset_command(branch, to, true, ResetMode::Hard),
                        ));
                    }
                });
            }
            Action::Reset { branch, to, checked_out: false } => {
                if ui.button(text).clicked() {
                    shared.show(Dialog::confirm(
                        "Reset branch",
                        format!(
                            "Move {branch} to {}? Commits only reachable from where it points now \
                             will no longer appear in the graph.",
                            short(*to)
                        ),
                        "Reset",
                        reset_command(branch, *to, false, ResetMode::Mixed),
                    ));
                }
            }
            _ => {
                let mut button = egui::Button::new(text);
                if let Some(note) = action.note() {
                    button = button.shortcut_text(note);
                }
                if ui.add(button).clicked()
                    && let Some(cmd) = action.command()
                {
                    tab.run(cmd);
                }
            }
        }
    }
}

/// The label that follows the pointer, and where it would land.
fn ghost(ctx: &Context, dragged: &Dragged, frame: u64) {
    let Some(pointer) = ctx.pointer_latest_pos() else { return };
    let painter = ctx.layer_painter(LayerId::new(Order::Tooltip, Id::new("branch-drag-ghost")));

    let name = painter.layout_no_wrap(dragged.branch.name(), theme::bold(12.5), theme::text_bright());
    let glyph = painter.layout_no_wrap(dragged.branch.icon().to_owned(), theme::font(13.0), theme::text_bright());
    let width = 7.0 + name.size().x + 6.0 + glyph.size().x + 7.0;
    let pill = Rect::from_min_size(pointer + vec2(14.0, 12.0), vec2(width, 22.0));

    painter.rect_filled(pill.translate(vec2(0.0, 2.0)).expand(1.0), 5.0, theme::shadow());
    painter.rect(
        pill,
        4.0,
        theme::mix(theme::bg_app(), dragged.color, 0.55),
        Stroke::new(1.0, theme::mix(dragged.color, theme::text_bright(), 0.35)),
        StrokeKind::Inside,
    );
    let y = |h: f32| (pill.center().y - h / 2.0).round();
    let name_w = name.size().x;
    painter.galley(pos2(pill.left() + 7.0, y(name.size().y)), name, theme::text_bright());
    painter.galley(pos2(pill.left() + 7.0 + name_w + 6.0, y(glyph.size().y)), glyph, theme::text_bright());

    // Targets report themselves while they are laid out, so the name shown is
    // the one from the frame before.
    let over = ctx
        .data(|data| data.get_temp::<(u64, String)>(Id::new("branch-drag-over")))
        .filter(|(seen, _)| seen + 1 >= frame);
    if let Some((_, target)) = over {
        let text = format!("{}  {target}", icons::ARROW_RIGHT);
        let galley = painter.layout_no_wrap(text, theme::font(12.5), theme::text_bright());
        let hint = Rect::from_min_size(
            pos2(pill.right() + 6.0, pill.top()),
            vec2(galley.size().x + 14.0, pill.height()),
        );
        painter.rect(hint, 4.0, theme::bg_float(), Stroke::new(1.0, theme::accent()), StrokeKind::Inside);
        painter.galley(pos2(hint.left() + 7.0, y(galley.size().y)), galley, theme::text_bright());
    } else {
        painter.text(
            pos2(pill.right() + 8.0, pill.center().y),
            Align2::LEFT_CENTER,
            icons::PROHIBIT,
            theme::font(13.0),
            theme::text_dim(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{Fixture, git};

    fn local(name: &str) -> Branch {
        Branch::local(name)
    }

    fn remote(name: &str) -> Branch {
        Branch::Remote { remote: "origin".to_owned(), name: name.to_owned() }
    }

    fn onto(branch: Branch) -> Target {
        Target::Branch(branch)
    }

    fn facts(head: Option<&'static str>, relation: Relation) -> Facts<'static> {
        Facts { head, relation, upstream: None }
    }

    fn labels(dragged: &Branch, target: &Target, facts: &Facts<'_>) -> Vec<String> {
        offer(dragged, target, facts)
            .iter()
            .map(|action| match action.note() {
                Some(note) => format!("{} ({note})", action.label()),
                None => action.label(),
            })
            .collect()
    }

    fn args(cmd: &GitCmd) -> Vec<String> {
        cmd.steps.iter().map(|step| step.args.join(" ")).collect()
    }

    fn oid(n: u8) -> Oid {
        Oid::from_bytes(&[n; 20]).unwrap()
    }

    #[test]
    fn diverged_local_branches_can_be_merged_or_rebased() {
        // On main, dragging feature/x onto main: the classic.
        let offered = labels(&local("feature/x"), &onto(local("main")), &facts(Some("main"), Relation::Diverged));
        assert_eq!(
            offered,
            ["Merge feature/x into main", "Rebase feature/x onto main (checks out feature/x)"]
        );
        // On the feature branch, it is the merge that needs a checkout.
        let offered =
            labels(&local("feature/x"), &onto(local("main")), &facts(Some("feature/x"), Relation::Diverged));
        assert_eq!(
            offered,
            ["Merge feature/x into main (checks out main)", "Rebase feature/x onto main"]
        );
        // Somewhere else entirely (or detached): both say what they switch to.
        let offered = labels(&local("feature/x"), &onto(local("main")), &facts(None, Relation::Diverged));
        assert_eq!(
            offered,
            [
                "Merge feature/x into main (checks out main)",
                "Rebase feature/x onto main (checks out feature/x)"
            ]
        );
    }

    #[test]
    fn a_branch_that_is_simply_behind_is_fast_forwarded() {
        // main has nothing feature/x lacks: no merge commit is needed, and a
        // rebase would do nothing.
        let ahead = facts(Some("feature/x"), Relation::Ahead);
        assert_eq!(
            labels(&local("feature/x"), &onto(local("main")), &ahead),
            ["Fast-forward main to feature/x"]
        );
        // Dragged the other way round, it is still main that moves.
        let behind = facts(Some("feature/x"), Relation::Behind);
        assert_eq!(
            labels(&local("main"), &onto(local("feature/x")), &behind),
            ["Fast-forward main to feature/x"]
        );
    }

    #[test]
    fn nothing_is_offered_for_a_branch_onto_itself_or_its_twin() {
        let same = facts(Some("main"), Relation::Same);
        assert!(offer(&local("main"), &onto(local("main")), &same).is_empty());
        assert!(offer(&local("main"), &onto(local("other")), &same).is_empty());
        assert!(offer(&local("main"), &onto(remote("main")), &same).is_empty());
        assert!(offer(&remote("main"), &onto(local("main")), &same).is_empty());
        assert!(offer(&local("main"), &Target::Commit(oid(1)), &same).is_empty());
    }

    #[test]
    fn a_local_branch_on_a_remote_one_can_catch_up_rebase_or_push() {
        let main = local("main");
        // Behind its remote: catch up.
        assert_eq!(
            labels(&main, &onto(remote("main")), &facts(Some("main"), Relation::Behind)),
            ["Fast-forward main to origin/main"]
        );
        // Diverged: rebase onto it. Merging *into* a remote branch is not a thing.
        assert_eq!(
            labels(&main, &onto(remote("main")), &facts(Some("main"), Relation::Diverged)),
            ["Rebase main onto origin/main"]
        );
        // Ahead of its own counterpart: push.
        assert_eq!(
            labels(&main, &onto(remote("main")), &facts(Some("main"), Relation::Ahead)),
            ["Push main to origin"]
        );
        // Ahead of somebody else's branch: pushing would not land there.
        assert!(offer(&local("feature/x"), &onto(remote("main")), &facts(None, Relation::Ahead)).is_empty());
        // ...unless that branch is its upstream.
        let tracking = Facts { head: None, relation: Relation::Ahead, upstream: Some("origin/main") };
        assert_eq!(
            labels(&local("feature/x"), &onto(remote("main")), &tracking),
            ["Push feature/x to origin"]
        );
    }

    #[test]
    fn a_local_branch_on_a_remote_is_pushed_there() {
        let target = Target::Remote("origin".to_owned());
        let offered = offer(&local("feature/x"), &target, &facts(Some("main"), Relation::Diverged));
        assert_eq!(offered, [Action::Push { branch: "feature/x".to_owned(), remote: "origin".to_owned() }]);
        assert_eq!(offered[0].label(), "Push feature/x to origin");
        assert_eq!(args(&offered[0].command().unwrap()), ["push -u origin feature/x"]);
        // Ahead of its branch there: push. Level with it or behind: nothing to send.
        assert_eq!(offer(&local("feature/x"), &target, &facts(None, Relation::Ahead)).len(), 1);
        assert!(offer(&local("feature/x"), &target, &facts(None, Relation::Same)).is_empty());
        assert!(offer(&local("feature/x"), &target, &facts(None, Relation::Behind)).is_empty());
    }

    #[test]
    fn a_remote_branch_can_only_be_brought_into_a_local_one() {
        let origin_main = remote("main");
        assert_eq!(
            labels(&origin_main, &onto(local("main")), &facts(Some("main"), Relation::Ahead)),
            ["Fast-forward main to origin/main"]
        );
        assert_eq!(
            labels(&origin_main, &onto(local("main")), &facts(Some("dev"), Relation::Diverged)),
            ["Merge origin/main into main (checks out main)"]
        );
        // The local branch already has it all.
        assert!(offer(&origin_main, &onto(local("main")), &facts(None, Relation::Behind)).is_empty());
        // Remote branches are not rebased, reset, pushed or merged into.
        for relation in [Relation::Ahead, Relation::Behind, Relation::Diverged] {
            let facts = facts(Some("main"), relation);
            assert!(offer(&origin_main, &onto(remote("dev")), &facts).is_empty());
            assert!(offer(&origin_main, &Target::Commit(oid(1)), &facts).is_empty());
            assert!(offer(&origin_main, &Target::Remote("origin".to_owned()), &facts).is_empty());
        }
    }

    #[test]
    fn a_local_branch_on_a_commit_can_be_reset_or_rebased() {
        let commit = Target::Commit(oid(0xab));
        // An unrelated commit: rebase onto it, or just point the branch at it.
        assert_eq!(
            labels(&local("main"), &commit, &facts(Some("main"), Relation::Diverged)),
            ["Rebase main onto abababa", "Reset main to this commit"]
        );
        // One of its own ancestors, or a commit ahead of it: only the reset
        // changes anything.
        for relation in [Relation::Ahead, Relation::Behind] {
            assert_eq!(
                labels(&local("main"), &commit, &facts(Some("main"), relation)),
                ["Reset main to this commit"]
            );
        }
    }

    #[test]
    fn commands_do_what_the_labels_say() {
        let command = |action: Action| {
            let cmd = action.command().unwrap();
            (cmd.label.clone(), args(&cmd), cmd.success.clone().unwrap_or_default())
        };
        let s = |text: &str| text.to_owned();

        // Merging into the current branch is the ordinary, undoable merge.
        let (label, steps, done) =
            command(Action::Merge { source: s("feature/x"), into: s("main"), checkout: false });
        assert_eq!((label.as_str(), done.as_str()), ("Merge feature/x", "Merged feature/x into main"));
        assert_eq!(steps, ["merge --no-edit feature/x"]);

        // Into another branch: check it out first, in the same command, and
        // only merge if that worked.
        let (label, steps, done) =
            command(Action::Merge { source: s("feature/x"), into: s("main"), checkout: true });
        assert_eq!(label, "Checkout main and merge feature/x");
        assert_eq!(steps, ["checkout main", "merge --no-edit feature/x"]);
        assert_eq!(done, "Checked out main and merged feature/x");

        let rebase = |checkout| Action::Rebase {
            branch: s("feature/x"),
            onto: s("main"),
            onto_name: s("main"),
            checkout,
        };
        let (label, steps, _) = command(rebase(false));
        assert_eq!((label.as_str(), steps), ("Rebase onto main", vec![s("rebase main")]));
        let (label, steps, _) = command(rebase(true));
        assert_eq!(label, "Checkout feature/x and rebase onto main");
        assert_eq!(steps, ["rebase main feature/x"]);

        let forward = |checked_out| Action::FastForward {
            branch: s("main"),
            to: s("origin/main"),
            to_ref: s("refs/remotes/origin/main"),
            checked_out,
        };
        let (_, steps, done) = command(forward(true));
        assert_eq!(steps, ["merge --ff-only origin/main"]);
        assert_eq!(done, "Fast-forwarded main to origin/main");
        // Not checked out: moved in place, no checkout at all.
        let (_, steps, _) = command(forward(false));
        assert_eq!(steps, ["fetch . refs/remotes/origin/main:refs/heads/main"]);

        // Resets wait for a mode.
        let reset = Action::Reset { branch: s("main"), to: oid(1), checked_out: true };
        assert!(reset.command().is_none());
        let sha = oid(1).to_string();
        assert_eq!(args(&reset_command("main", oid(1), true, ResetMode::Soft)), [format!("reset --soft {sha}")]);
        assert_eq!(
            args(&reset_command("topic", oid(1), false, ResetMode::Hard)),
            [format!("branch -f topic {sha}")]
        );
    }

    /// main and feature diverge after `base`; `old` stays behind at `base`.
    fn diverged(name: &str) -> Fixture {
        let fx = Fixture::new(name);
        fx.write("a.txt", "base\n");
        fx.commit_all("base");
        git(&fx.dir, &["branch", "old"]);
        git(&fx.dir, &["checkout", "-q", "-b", "feature"]);
        fx.write("feature.txt", "feature\n");
        fx.commit_all("on feature");
        git(&fx.dir, &["checkout", "-q", "main"]);
        fx.write("main.txt", "main\n");
        fx.commit_all("on main");
        fx
    }

    fn tip(snap: &RepoSnapshot, branch: &str) -> Oid {
        local(branch).tip(snap).unwrap()
    }

    #[test]
    fn relations_are_read_from_the_loaded_history() {
        let fx = diverged("drag-relations");
        let snap = fx.snapshot();
        let (main, feature, old) = (tip(&snap, "main"), tip(&snap, "feature"), tip(&snap, "old"));
        assert_eq!(relation(&snap.history, feature, main), Relation::Diverged);
        assert_eq!(relation(&snap.history, main, feature), Relation::Diverged);
        assert_eq!(relation(&snap.history, main, old), Relation::Ahead);
        assert_eq!(relation(&snap.history, old, main), Relation::Behind);
        assert_eq!(relation(&snap.history, main, main), Relation::Same);
        // A commit that is not loaded: no claim either way.
        assert_eq!(relation(&snap.history, main, oid(7)), Relation::Diverged);

        // What can be dropped where.
        assert!(accepts(&snap, &local("feature"), &onto(local("main"))));
        assert!(!accepts(&snap, &local("feature"), &onto(local("feature"))));
        assert!(!accepts(&snap, &local("feature"), &onto(local("gone"))));
        assert!(accepts(&snap, &local("feature"), &Target::Commit(main)));
        assert!(!accepts(&snap, &local("feature"), &Target::Commit(feature)));
        assert!(accepts(&snap, &local("feature"), &Target::Remote("origin".to_owned())));

        assert_eq!(
            plan(&snap, &local("old"), &onto(local("main"))),
            [Action::FastForward {
                branch: "old".to_owned(),
                to: "main".to_owned(),
                to_ref: "refs/heads/main".to_owned(),
                checked_out: false,
            }]
        );
    }

    #[test]
    fn merging_into_another_branch_checks_it_out_first() {
        let fx = diverged("drag-merge");
        git(&fx.dir, &["checkout", "-q", "feature"]);
        let snap = fx.snapshot();
        let (main, feature) = (tip(&snap, "main"), tip(&snap, "feature"));

        let actions = plan(&snap, &local("feature"), &onto(local("main")));
        assert_eq!(actions[0].label(), "Merge feature into main");
        assert_eq!(actions[0].note().as_deref(), Some("checks out main"));

        let snap = fx.run(actions[0].command().unwrap()).snap;
        assert_eq!(snap.head.branch.as_deref(), Some("main"));
        let merge = &snap.history.commits[snap.row_of(snap.head.oid.unwrap()).unwrap()];
        assert_eq!(merge.parents, [main, feature]);
        assert_eq!(tip(&snap, "feature"), feature);
    }

    #[test]
    fn a_failed_checkout_stops_the_merge() {
        let fx = diverged("drag-merge-blocked");
        git(&fx.dir, &["checkout", "-q", "feature"]);
        // An uncommitted change that checking out main would overwrite.
        fx.write("main.txt", "in the way\n");
        let snap = fx.snapshot();
        let (main, feature) = (tip(&snap, "main"), tip(&snap, "feature"));

        let merge = plan(&snap, &local("feature"), &onto(local("main"))).remove(0);
        let done = fx.run_as(merge.command().unwrap(), crate::worker::Origin::User);
        assert!(done.result.is_err());
        // Nothing moved, and the change is still there.
        assert_eq!(done.snap.head.branch.as_deref(), Some("feature"));
        assert_eq!((tip(&done.snap, "main"), tip(&done.snap, "feature")), (main, feature));
        assert_eq!(fx.read("main.txt"), "in the way\n");
    }

    #[test]
    fn rebasing_a_branch_that_is_not_checked_out_checks_it_out() {
        let fx = diverged("drag-rebase");
        let snap = fx.snapshot();
        let main = tip(&snap, "main");

        let actions = plan(&snap, &local("feature"), &onto(local("main")));
        assert_eq!(actions[1].label(), "Rebase feature onto main");
        assert_eq!(actions[1].note().as_deref(), Some("checks out feature"));

        let snap = fx.run(actions[1].command().unwrap()).snap;
        assert_eq!(snap.head.branch.as_deref(), Some("feature"));
        let rebased = &snap.history.commits[snap.row_of(tip(&snap, "feature")).unwrap()];
        assert_eq!(rebased.parents, [main]);
        assert_eq!(tip(&snap, "main"), main);
        // Now main is simply behind.
        assert_eq!(
            plan(&snap, &local("feature"), &onto(local("main")))[0].label(),
            "Fast-forward main to feature"
        );
    }

    #[test]
    fn fast_forwarding_moves_a_branch_without_checking_it_out() {
        let fx = diverged("drag-ff");
        let snap = fx.snapshot();
        let main = tip(&snap, "main");

        // `old` is behind main and not checked out.
        let forward = plan(&snap, &local("main"), &onto(local("old"))).remove(0);
        assert_eq!(forward.label(), "Fast-forward old to main");
        let snap = fx.run(forward.command().unwrap()).snap;
        assert_eq!(tip(&snap, "old"), main);
        assert_eq!(snap.head.branch.as_deref(), Some("main"));

        // The same command refuses to move a branch that has diverged.
        let forced = Action::FastForward {
            branch: "feature".to_owned(),
            to: "main".to_owned(),
            to_ref: "refs/heads/main".to_owned(),
            checked_out: false,
        };
        let feature = tip(&snap, "feature");
        let done = fx.run_as(forced.command().unwrap(), crate::worker::Origin::User);
        assert!(done.result.is_err());
        assert_eq!(tip(&done.snap, "feature"), feature);
    }

    #[test]
    fn resetting_moves_the_dragged_branch_to_the_commit() {
        let fx = diverged("drag-reset");
        let snap = fx.snapshot();
        let (main, old) = (tip(&snap, "main"), tip(&snap, "old"));

        // A branch that is not checked out is just moved.
        let actions = plan(&snap, &local("feature"), &Target::Commit(main));
        assert_eq!(
            actions.last(),
            Some(&Action::Reset { branch: "feature".to_owned(), to: main, checked_out: false })
        );
        let snap = fx.run(reset_command("feature", main, false, ResetMode::Mixed)).snap;
        assert_eq!(tip(&snap, "feature"), main);
        assert_eq!(snap.head.branch.as_deref(), Some("main"));

        // The checked-out one is reset for real, and that can be undone.
        let actions = plan(&snap, &local("main"), &Target::Commit(old));
        assert_eq!(actions, [Action::Reset { branch: "main".to_owned(), to: old, checked_out: true }]);
        let done = fx.run(reset_command("main", old, true, ResetMode::Hard));
        assert_eq!(done.snap.head.oid, Some(old));
        assert!(done.undo.is_some());
    }
}
