//! Merge options in the interface (`git::merge`): the "Merge options"
//! submenu under every "Merge X into Y" of a branch's right-click menu (in
//! the sidebar and on graph labels), the extra entries of the drop menu, the
//! merge entries of the command palette, and a squash merge waiting to be
//! committed: its message put in the commit panel, its banner with Continue
//! (commit it) and Abort.
//!
//! What this keeps between frames lives in egui's memory by working
//! directory: the names of the branches of a squash merge started here (git
//! does not write them down), and which prepared message was last put in
//! the commit panel, so emptying the fields sticks.

use std::path::Path;
use std::time::{Duration, Instant};

use eframe::egui::{self, Button, Frame, Id, Layout, Margin, RichText, Ui};

use crate::git::merge::{self, MergeMode};
use crate::git::{ChangeKind, RepoSnapshot, RepoState};
use crate::state::{RepoTab, Selection, Shared};
use crate::theme::{self, icons};
use crate::ui::graph::drag::Action;
use crate::ui::palette::{Entry, Env, Kind};
use crate::ui::widgets::tint_button;

/// A menu entry that is greyed out, saying why, while `blocker` is set.
pub(crate) fn entry(ui: &mut Ui, icon: &str, text: &str, blocker: Option<&str>, hover: &str) -> bool {
    let response = ui.add_enabled(blocker.is_none(), Button::new(format!("{icon}  {text}")));
    match blocker {
        Some(why) => {
            response.on_disabled_hover_text(why);
            false
        }
        None if hover.is_empty() => response.clicked(),
        None => response.on_hover_text(hover).clicked(),
    }
}

/// Why `mode` cannot start now, if it cannot.
fn blocker(snap: &RepoSnapshot, mode: MergeMode) -> Option<&'static str> {
    (mode == MergeMode::Squash && !snap.status.staged.is_empty()).then_some(merge::STAGED)
}

// ---- starting a merge -------------------------------------------------------

/// What is remembered about a repository's squash merge.
#[derive(Clone, Default)]
struct Memo {
    /// The branch squashed and the one it went into, when started here.
    names: Option<(String, String)>,
    /// The squash merge just started: its label and when, to show the
    /// commit panel once its snapshot is in.
    started: Option<(String, Instant)>,
    /// When it ended, while waiting for the snapshot that shows it.
    ended: Option<Instant>,
    /// The `SQUASH_MSG` last put in the message fields.
    filled: Option<String>,
    /// The summary and description made of it, to take them out again if
    /// the squash merge is aborted or undone before they were edited.
    put: Option<(String, String)>,
}

fn memo_id(path: &Path) -> Id {
    Id::new(("merge-squash", path))
}

fn memo(ctx: &egui::Context, path: &Path) -> Memo {
    ctx.data(|d| d.get_temp(memo_id(path))).unwrap_or_default()
}

fn store(ctx: &egui::Context, path: &Path, memo: Memo) {
    ctx.data_mut(|d| d.insert_temp(memo_id(path), memo));
}

/// Longest wait, once a squash merge ended, for the snapshot that shows it.
const SNAPSHOT_WAIT: Duration = Duration::from_secs(3);

/// Merges `source` into `into` in `mode`; `checkout` when `into` is not the
/// checked-out branch and has to be first.
pub fn run(ctx: &egui::Context, tab: &mut RepoTab, source: &str, into: &str, mode: MergeMode, checkout: bool) {
    let cmd = if checkout { merge::checkout_and_merge(source, into, mode) } else { merge::merge(source, into, mode) };
    if mode == MergeMode::Squash {
        let memo = Memo {
            names: Some((source.to_owned(), into.to_owned())),
            started: Some((cmd.label.clone(), Instant::now())),
            ..memo(ctx, &tab.path)
        };
        store(ctx, &tab.path, memo);
    }
    tab.run(cmd);
}

/// The "Merge options" submenu, for a branch menu's "Merge `source` into
/// `into`" (the checked-out branch).
pub fn options_menu(ui: &mut Ui, tab: &mut RepoTab, snap: &RepoSnapshot, source: &str, into: &str) {
    ui.menu_button(format!("{}  Merge options", icons::GIT_MERGE), |ui| {
        ui.set_min_width(220.0);
        for mode in MergeMode::OPTIONS {
            let icon = match mode {
                MergeMode::FastForwardOnly => icons::FAST_FORWARD,
                MergeMode::Squash => icons::STACK,
                _ => icons::GIT_MERGE,
            };
            if entry(ui, icon, mode.title(), blocker(snap, mode), &mode.explain(source, into)) {
                run(ui.ctx(), tab, source, into, mode, false);
            }
        }
    });
}

/// More entries for the drop menu, after `action`'s own (`dragged` is the
/// dragged branch's name): a squash next to a merge, and a merge commit or
/// a squash next to a fast-forward of the branch dropped on. Not offered
/// next to a fast-forward of the dragged branch itself, which would merge
/// the other way round.
pub fn drop_entries(ui: &mut Ui, tab: &mut RepoTab, snap: &RepoSnapshot, dragged: &str, action: &Action) {
    const SQUASH: &[MergeMode] = &[MergeMode::Squash];
    const MERGE_OR_SQUASH: &[MergeMode] = &[MergeMode::NoFastForward, MergeMode::Squash];
    let (source, into, checkout, modes) = match action {
        Action::Merge { source, into, checkout } => (source.as_str(), into.as_str(), *checkout, SQUASH),
        Action::FastForward { branch, to, checked_out, .. } if to == dragged => {
            (to.as_str(), branch.as_str(), !*checked_out, MERGE_OR_SQUASH)
        }
        _ => return,
    };
    for &mode in modes {
        let icon = if mode == MergeMode::Squash { icons::STACK } else { icons::GIT_MERGE };
        let text = format!("{icon}  {}", mode.action(source, into));
        let mut button = Button::new(text);
        if checkout {
            button = button.shortcut_text(format!("checks out {into}"));
        }
        let blocked = blocker(snap, mode);
        let response = ui.add_enabled(blocked.is_none(), button);
        let response = match blocked {
            Some(why) => response.on_disabled_hover_text(why),
            None => response.on_hover_text(mode.explain(source, into)),
        };
        if response.clicked() {
            run(ui.ctx(), tab, source, into, mode, checkout);
        }
    }
}

/// "Merge X into Y" in every mode, for every other branch, local and
/// remote, while a branch is checked out and nothing is in progress.
pub fn palette_entries(list: &mut Vec<Entry>, tab: &RepoTab, snap: &RepoSnapshot) {
    let Some(into) = snap.head.branch.clone() else { return };
    let busy = ["Merge ", "Squash merge ", "Checkout "].iter().any(|label| tab.is_running(label));
    if busy || snap.state != RepoState::Clean || squash_pending(snap) {
        return;
    }
    let locals = snap.branches.iter().filter(|b| !b.is_head).map(|b| b.name.clone());
    let remotes = snap.remote_branches.iter().map(|b| b.full.clone());
    let modes = [MergeMode::Default, MergeMode::NoFastForward, MergeMode::FastForwardOnly, MergeMode::Squash];
    for source in locals.chain(remotes) {
        for mode in modes {
            if blocker(snap, mode).is_some() {
                continue;
            }
            let detail = match mode {
                MergeMode::Default => "fast-forward when possible",
                MergeMode::NoFastForward => "git merge --no-ff",
                MergeMode::FastForwardOnly => "git merge --ff-only",
                MergeMode::Squash => "staged, not committed",
            };
            let icon = if mode == MergeMode::Squash { icons::STACK } else { icons::GIT_MERGE };
            let (source_run, into_run) = (source.clone(), into.clone());
            let entry = Entry::new(Kind::Branch, format!("merge:{mode:?}:{source}"), icon, mode.action(&source, &into), move |env: &mut Env<'_>| {
                if let Some(tab) = env.tab.as_deref_mut() {
                    run(env.ctx, tab, &source_run, &into_run, mode, false);
                }
            });
            list.push(entry.detail(detail).keywords("merge squash fast-forward no-ff branch"));
        }
    }
}

// ---- a squash merge waiting to be committed -----------------------------------

/// Whether the repository has a squash merge to commit: git's prepared
/// message, and changes staged or conflicted (a message left behind by a
/// squash whose changes were discarded by hand does not count).
pub fn squash_pending(snap: &RepoSnapshot) -> bool {
    snap.squash_message.is_some()
        && snap.state == RepoState::Clean
        && (!snap.status.staged.is_empty() || snap.status.unstaged.iter().any(|f| f.kind == ChangeKind::Conflicted))
}

/// Puts a squash merge's prepared message in the commit panel, once, when
/// its fields are empty (or hold the template untouched), and shows the
/// panel after a squash merge started here. Called every frame the
/// repository is shown; one lookup in egui's memory when there is nothing
/// to do.
pub fn follow_up(ctx: &egui::Context, tab: &mut RepoTab) {
    let Some(snap) = tab.snap.clone() else { return };
    let id = memo_id(&tab.path);
    let pending = squash_pending(&snap);
    let Some(mut memo) = ctx.data(|d| d.get_temp::<Memo>(id)) else {
        if !pending {
            return;
        }
        // A squash merge made elsewhere (or before a restart).
        return store(ctx, &tab.path, Memo::default());
    };
    let before = (memo.started.clone(), memo.ended, memo.filled.clone());

    if let Some((label, since)) = &memo.started
        && tab.take_outcome(label, *since).is_some()
    {
        // Done, or stopped on conflicts: either way the panel is next.
        memo.started = None;
        memo.ended = Some(Instant::now());
    }
    if let Some(ended) = memo.ended {
        if pending {
            tab.select(Selection::Wip);
            memo.ended = None;
        } else if ended.elapsed() >= SNAPSHOT_WAIT {
            memo.ended = None;
        } else {
            ctx.request_repaint_after(SNAPSHOT_WAIT.saturating_sub(ended.elapsed()));
        }
    }

    match &snap.squash_message {
        Some(text) if pending => {
            let empty = tab.summary.trim().is_empty() && tab.description.trim().is_empty();
            let fill = !tab.amend
                && memo.filled.as_deref() != Some(text.as_str())
                && (empty || crate::ui::commit_button::untouched_template(ctx, tab));
            if fill {
                let names = memo.names.as_ref().map(|(source, into)| (source.as_str(), into.as_str()));
                let (summary, description) = merge::prepared_message(text, names);
                if !summary.is_empty() {
                    tab.summary = summary.clone();
                    tab.description = description.clone();
                    memo.put = Some((summary, description));
                }
                memo.filled = Some(text.clone());
            }
        }
        _ if memo.started.is_none() && memo.ended.is_none() => {
            // Committed, aborted or undone: nothing to remember any more,
            // and a prepared message nobody edited goes with it.
            if memo.put.as_ref().is_some_and(|(summary, description)| {
                tab.summary == *summary && tab.description == *description
            }) {
                tab.summary.clear();
                tab.description.clear();
            }
            ctx.data_mut(|d| d.remove::<Memo>(id));
            return;
        }
        _ => {}
    }
    if (memo.started.clone(), memo.ended, memo.filled.clone()) != before {
        store(ctx, &tab.path, memo);
    }
}

/// The banner over the changed files while a squash merge waits to be
/// committed: Continue commits it with the message in the commit panel (and
/// its options), Abort puts everything back as it was.
pub fn banner(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, snap: &RepoSnapshot) {
    if !squash_pending(snap) {
        return;
    }
    let conflicted = snap.status.unstaged.iter().any(|f| f.kind == ChangeKind::Conflicted);
    let prefs = crate::prefs::repo(&shared.settings, &tab.path).commit.clone();
    Frame::new()
        .fill(theme::mix(theme::bg_app(), theme::orange(), 0.18))
        .inner_margin(Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{}  Squash merge pending", icons::WARNING)).color(theme::orange()))
                    .on_hover_text("The squashed changes are staged. Commit them to finish, or abort to put everything back.");
                ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                    if tint_button(ui, "Abort", theme::red()).on_hover_text("git reset --merge").clicked() {
                        tab.run(merge::abort_squash());
                    }
                    let blocked = if conflicted {
                        Some("Resolve the conflicted files first.")
                    } else {
                        crate::ui::commit_button::commit_blocker(ui.ctx(), tab)
                    };
                    let continued = ui
                        .add_enabled_ui(blocked.is_none(), |ui| tint_button(ui, "Continue", theme::green()))
                        .inner;
                    let continued = match blocked {
                        Some(why) => continued.on_disabled_hover_text(why).clicked(),
                        None => continued.on_hover_text("Commit the squashed changes with the message in the commit panel").clicked(),
                    };
                    if continued {
                        crate::ui::commit_button::commit_staged(ui.ctx(), tab, snap, &prefs);
                    }
                });
            });
        });
    ui.add_space(4.0);
}
