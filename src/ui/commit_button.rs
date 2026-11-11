//! The commit panel's button and what Preferences > Commit changes about
//! the message: the split button (the repository's default action, the
//! others in its ▾ menu), pushing or opening a pull request once a commit
//! succeeded, the commit template put into an empty message, and the
//! summary's character counter.
//!
//! What this keeps between frames lives in egui's memory by working
//! directory, so `RepoTab` needs nothing new: the template read for a
//! repository, the one put in its fields, and an action waiting for a
//! commit to finish.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, CornerRadius, Id, Popup, Rect, Response, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use crate::git::{Oid, RepoSnapshot, ops};
use crate::prefs::commit::{CommitAction, ConfigEnv, RepoCommit, SUMMARY_LIMIT, Template, TemplateFile, read_template};
use crate::state::{AppAction, RepoTab, Shared, ToastKind};
use crate::theme::{self, icons};
use crate::ui::widgets::{self, menu_item};

// ---- the commit template ----------------------------------------------------

const TEMPLATE_JOB: &str = "commit:template";

/// What the panel knows about one repository's template.
#[derive(Clone, Default)]
struct TemplateCache {
    /// `None` until it has been read.
    template: Option<Arc<Template>>,
    reading: bool,
    /// The template last put in the fields, with HEAD at the time. It is not
    /// put back while HEAD stays there, so emptying the fields sticks; the
    /// next commit moves HEAD and brings it back, as git does.
    filled: Option<(Option<Oid>, Arc<Template>)>,
}

fn cache_id(path: &Path) -> Id {
    Id::new(("commit-template", path))
}

fn cache(ctx: &egui::Context, path: &Path) -> TemplateCache {
    ctx.data_mut(|d| d.get_temp(cache_id(path))).unwrap_or_default()
}

fn store(ctx: &egui::Context, path: &Path, cache: TemplateCache) {
    ctx.data_mut(|d| d.insert_temp(cache_id(path), cache));
}

/// Puts the template into an empty message for a new commit. Reads it on
/// the reader thread the first time; costs nothing when the repository
/// does not apply templates.
pub fn prefill(ctx: &egui::Context, tab: &mut RepoTab, prefs: &RepoCommit, head: Option<Oid>) {
    if !prefs.apply_template {
        return;
    }
    let mut cache = cache(ctx, &tab.path);
    let mut changed = false;
    if let Some(file) = tab.take_result::<TemplateFile>(TEMPLATE_JOB) {
        cache.template = Some(Arc::new(file.template));
        cache.reading = false;
        changed = true;
    }
    if cache.template.is_none() && !cache.reading {
        let workdir = tab.path.clone();
        tab.spawn_job(TEMPLATE_JOB, move |_| read_template(&workdir, &ConfigEnv::default()));
        cache.reading = true;
        changed = true;
    }
    if let Some(template) = &cache.template
        && !template.is_empty()
        && !tab.amend
        && tab.summary.is_empty()
        && tab.description.is_empty()
        && !cache.filled.as_ref().is_some_and(|(at, filled)| *at == head && filled == template)
    {
        tab.summary = template.summary.clone();
        tab.description = template.description.clone();
        cache.filled = Some((head, template.clone()));
        changed = true;
    }
    if changed {
        store(ctx, &tab.path, cache);
    }
}

/// Whether the fields hold the template exactly as it was put there.
pub fn untouched_template(ctx: &egui::Context, tab: &RepoTab) -> bool {
    cache(ctx, &tab.path).filled.is_some_and(|(_, template)| {
        !template.is_empty() && tab.summary == template.summary && tab.description == template.description
    })
}

/// The template changed (read or saved by Preferences > Commit): later
/// messages start from it, and one still untouched in the fields is
/// replaced.
pub fn template_changed(ctx: &egui::Context, tab: &mut RepoTab, template: &Template) {
    let untouched = untouched_template(ctx, tab);
    let mut cache = cache(ctx, &tab.path);
    let template = Arc::new(template.clone());
    if untouched && let Some((_, filled)) = &mut cache.filled {
        tab.summary = template.summary.clone();
        tab.description = template.description.clone();
        *filled = template.clone();
    }
    cache.template = Some(template);
    cache.reading = false;
    store(ctx, &tab.path, cache);
}

/// Templates were turned off: an untouched one leaves the fields, and is
/// put back if they are turned on again.
pub fn template_withdrawn(ctx: &egui::Context, tab: &mut RepoTab) {
    if untouched_template(ctx, tab) {
        tab.summary.clear();
        tab.description.clear();
    }
    let mut cache = cache(ctx, &tab.path);
    cache.filled = None;
    store(ctx, &tab.path, cache);
}

/// Paints how many characters the summary has left before
/// [`SUMMARY_LIMIT`] at the right end of its field (which needs about 34
/// points of right margin for it).
pub fn summary_counter(ui: &Ui, field: &Response, summary: &str) {
    let left = SUMMARY_LIMIT as i64 - summary.chars().count() as i64;
    let color = if left < 0 { theme::red() } else { theme::text_dim() };
    ui.painter().text(
        pos2(field.rect.right() - 8.0, field.rect.center().y),
        Align2::RIGHT_CENTER,
        left.to_string(),
        theme::font(11.5),
        color,
    );
}

// ---- after the commit ------------------------------------------------------

/// An action to take once a commit finishes.
#[derive(Clone)]
struct FollowUp {
    /// The commit's label and when it started, for `RepoTab::take_outcome`.
    label: String,
    since: Instant,
    action: CommitAction,
    /// HEAD before the commit. Once it succeeded, the follow-up waits for
    /// a snapshot past it, so the push or the pull request dialog sees the
    /// new commit (and the branch, after a repository's first commit).
    head_before: Option<Oid>,
    /// When the commit was reported done. An amend that changed nothing
    /// within the same second keeps HEAD's id, so the wait is bounded.
    succeeded: Option<Instant>,
}

/// Longest wait for a snapshot past the commit.
const SNAPSHOT_WAIT: Duration = Duration::from_secs(2);

fn follow_up_id(path: &Path) -> Id {
    Id::new(("commit-follow-up", path))
}

/// Once a commit succeeded: puts the commit options back to their defaults
/// and, for those actions, pushes or opens the pull request dialog. Called
/// every frame the repository is shown; one lookup in egui's memory when
/// nothing waits.
pub fn follow_up(ctx: &egui::Context, tab: &mut RepoTab, shared: &mut Shared) {
    let id = follow_up_id(&tab.path);
    let Some(mut pending) = ctx.data(|d| d.get_temp::<FollowUp>(id)) else { return };
    let succeeded = match pending.succeeded {
        Some(at) => at,
        None => match tab.take_outcome(&pending.label, pending.since) {
            // Still running.
            None => return,
            // Its failure is already on screen; nothing follows, and the
            // options stay for the next try.
            Some(false) => {
                ctx.data_mut(|d| d.remove::<FollowUp>(id));
                return;
            }
            Some(true) => {
                // Sign-off, hooks and co-authors were for this commit.
                crate::ui::commit_options::committed(ctx, &tab.path);
                if pending.action == CommitAction::Commit {
                    ctx.data_mut(|d| d.remove::<FollowUp>(id));
                    return;
                }
                *pending.succeeded.insert(Instant::now())
            }
        },
    };
    let waited = succeeded.elapsed() >= SNAPSHOT_WAIT;
    let snap = tab.snap.clone().filter(|snap| waited || snap.head.oid != pending.head_before);
    let Some(snap) = snap else {
        // One more frame when the wait runs out, if nothing comes first.
        ctx.request_repaint_after(SNAPSHOT_WAIT.saturating_sub(succeeded.elapsed()));
        ctx.data_mut(|d| d.insert_temp(id, pending));
        return;
    };
    ctx.data_mut(|d| d.remove::<FollowUp>(id));
    match pending.action {
        CommitAction::Commit => {}
        CommitAction::CommitAndPush => match crate::ui::toolbar::push_cmd(tab, false) {
            Some(cmd) => tab.run(cmd),
            None => shared.toast(ToastKind::Error, "Committed, not pushed", push_blocker(&snap)),
        },
        CommitAction::CommitAndPullRequest => crate::ui::new_pull::open(tab, shared, None),
    }
}

/// Why the current branch cannot be pushed.
fn push_blocker(snap: &RepoSnapshot) -> &'static str {
    if snap.remotes.is_empty() {
        "This repository has no remote to push to."
    } else {
        "Check out a branch to push."
    }
}

// ---- the split button -------------------------------------------------------

/// What the panel's message and staging allow.
struct Plan {
    /// Disabled buttons say why instead.
    blocked: Option<&'static str>,
    /// Nothing is staged: stage everything first.
    everything: bool,
    staged: usize,
}

fn label(action: CommitAction, plan: &Plan, amend: bool, pull: &str) -> String {
    if let Some(why) = plan.blocked {
        return why.to_owned();
    }
    let files = format!("{} file{}", plan.staged, if plan.staged == 1 { "" } else { "s" });
    match (action, amend, plan.everything) {
        (CommitAction::Commit, true, _) => "Amend previous commit".to_owned(),
        (CommitAction::Commit, false, true) => "Stage all changes and commit".to_owned(),
        (CommitAction::Commit, false, false) => format!("Commit changes to {files}"),
        (CommitAction::CommitAndPush, true, _) => "Amend previous commit and push".to_owned(),
        (CommitAction::CommitAndPush, false, true) => "Stage all changes, commit and push".to_owned(),
        (CommitAction::CommitAndPush, false, false) => format!("Commit changes to {files} and push"),
        (CommitAction::CommitAndPullRequest, true, _) => format!("Amend and create {pull}"),
        (CommitAction::CommitAndPullRequest, false, true) => format!("Stage all, commit and create {pull}"),
        (CommitAction::CommitAndPullRequest, false, false) => format!("Commit and create {pull}"),
    }
}

fn action_icon(action: CommitAction) -> &'static str {
    match action {
        CommitAction::Commit => icons::GIT_COMMIT,
        CommitAction::CommitAndPush => icons::ARROW_LINE_UP,
        CommitAction::CommitAndPullRequest => icons::GIT_PULL_REQUEST,
    }
}

/// The commit button: the repository's default action, with the others
/// in a ▾ menu at its right end. `prefs` are the repository's
/// (`RepoPrefs::commit`); `shortcut` is Ctrl+Enter pressed in the message
/// fields, which runs the default action.
pub fn show(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, snap: &RepoSnapshot, prefs: &RepoCommit, shortcut: bool) {
    // Not on SourceHut, which takes patches by email.
    let hosting = tab.hosting.connected().filter(|shown| shown.repo.service.has_pulls());
    let pull = hosting.as_ref().map_or("pull request", |shown| shown.repo.service.pull_word());
    let available = |action: CommitAction| match action {
        CommitAction::Commit => true,
        CommitAction::CommitAndPush => !snap.remotes.is_empty(),
        CommitAction::CommitAndPullRequest => hosting.is_some(),
    };
    let main = if available(prefs.action) { prefs.action } else { CommitAction::Commit };
    let others: Vec<CommitAction> =
        CommitAction::ALL.into_iter().filter(|action| *action != main && available(*action)).collect();

    let staged = snap.status.staged.len();
    let unstaged = snap.status.unstaged.len();
    let blocked = match commit_blocker(ui.ctx(), tab) {
        Some(why) if why == COMMITTING || tab.amend => Some(why),
        _ if !tab.amend && staged == 0 && unstaged == 0 => Some("Nothing to commit"),
        why => why,
    };
    let plan = Plan { blocked, everything: !tab.amend && staged == 0, staged };
    let enabled = plan.blocked.is_none();

    // One rounded button; the menu part is a square at its right end.
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::hover());
    let split = !others.is_empty();
    let arrow_w = if split { 34.0 } else { 0.0 };
    let main_rect = Rect::from_min_max(rect.min, pos2(rect.right() - arrow_w, rect.bottom()));
    let arrow_rect = Rect::from_min_max(pos2(main_rect.right(), rect.top()), rect.max);
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let id = ui.id().with("commit-button");
    let main_response = ui.interact(main_rect, id.with("main"), sense);
    let arrow_response = split.then(|| {
        ui.interact(arrow_rect, id.with("menu"), sense).on_hover_text("More ways to commit")
    });

    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let fill = if enabled { theme::solid(theme::green(), 0.55) } else { theme::green_dark() };
        let hover = theme::solid(theme::green(), 0.68);
        painter.rect(rect, 4.0, fill, Stroke::NONE, StrokeKind::Inside);
        if enabled && main_response.hovered() {
            let corners = if split { CornerRadius { nw: 4, sw: 4, ne: 0, se: 0 } } else { CornerRadius::same(4) };
            painter.rect_filled(main_rect, corners, hover);
        }
        if let Some(arrow) = &arrow_response
            && enabled
            && (arrow.hovered() || Popup::is_id_open(ui.ctx(), Popup::default_response_id(arrow)))
        {
            painter.rect_filled(arrow_rect, CornerRadius { nw: 0, sw: 0, ne: 4, se: 4 }, hover);
        }
        painter.rect_stroke(
            rect,
            4.0,
            Stroke::new(1.0, theme::mix(theme::bg_app(), theme::green(), 0.7)),
            StrokeKind::Inside,
        );
        let text_color = if enabled { theme::on_accent() } else { theme::text_dim() };
        let galley = widgets::elided(
            painter,
            &label(main, &plan, tab.amend, pull),
            theme::font(13.0),
            text_color,
            main_rect.width() - 16.0,
        );
        let at = main_rect.center() - galley.size() / 2.0;
        painter.galley(pos2(at.x.round(), at.y.round()), galley, text_color);
        if split {
            let divider = theme::mix(fill, text_color, 0.35);
            painter.vline(arrow_rect.left(), arrow_rect.y_range().shrink(7.0), Stroke::new(1.0, divider));
            painter.text(arrow_rect.center(), Align2::CENTER_CENTER, icons::CARET_DOWN, theme::font(13.0), text_color);
        }
    }

    let mut chosen = None;
    if enabled && (main_response.clicked() || shortcut) {
        chosen = Some(main);
    }
    if let Some(arrow) = &arrow_response {
        Popup::menu(arrow).show(|ui| {
            ui.set_min_width(250.0);
            for action in &others {
                if menu_item(ui, action_icon(*action), &action.title(pull)) {
                    chosen = Some(*action);
                }
            }
            ui.separator();
            if menu_item(ui, icons::GEAR, "Change the default action…") {
                shared.actions.push(AppAction::OpenPreferences(crate::ui::prefs::Section::Commit));
            }
        });
    }
    if let Some(action) = chosen {
        commit(ui.ctx(), tab, snap, prefs, action, plan.everything);
    }
}

const COMMITTING: &str = "Committing…";

/// Why the message fields do not allow a commit now (a commit running, no
/// message, the template untouched), whatever is staged.
pub fn commit_blocker(ctx: &egui::Context, tab: &RepoTab) -> Option<&'static str> {
    let has_message = !tab.summary.trim().is_empty() && !untouched_template(ctx, tab);
    if tab.is_running("Commit") || tab.is_running("Amend commit") {
        Some(COMMITTING)
    } else if has_message {
        None
    } else if tab.amend {
        Some("Type a message to amend")
    } else if tab.summary.trim().is_empty() {
        Some("Type a message to commit")
    } else {
        Some("Edit the template to commit")
    }
}

/// Commits what is staged with the message in the fields and the commit
/// options, as the plain commit button does; for buttons elsewhere (a squash
/// merge's Continue). Check [`commit_blocker`] first.
pub fn commit_staged(ctx: &egui::Context, tab: &mut RepoTab, snap: &RepoSnapshot, prefs: &RepoCommit) {
    commit(ctx, tab, snap, prefs, CommitAction::Commit, false);
}

/// The message in the fields, as the user wrote it.
pub fn message(tab: &RepoTab) -> String {
    let mut message = tab.summary.trim().to_owned();
    let body = tab.description.trim();
    if !body.is_empty() {
        message.push_str("\n\n");
        message.push_str(body);
    }
    message
}

/// Commits with the commit options (sign-off, hooks, co-authors; see
/// `ui::commit_options`), and leaves what `action` adds, and putting the
/// options back once it succeeded, for [`follow_up`].
fn commit(ctx: &egui::Context, tab: &mut RepoTab, snap: &RepoSnapshot, prefs: &RepoCommit, action: CommitAction, everything: bool) {
    let options = crate::ui::commit_options::options(ctx, tab, prefs);
    let message = options.message(&message(tab));
    let mut cmd = if everything { ops::commit_everything(&message) } else { ops::commit(&message, tab.amend) };
    if prefs.strip_comments {
        cmd = cmd.strip_comments();
    }
    let ident = crate::ui::commit_options::sign_off_ident(ctx, &tab.path);
    let cmd = options.apply(cmd, &message, ident.as_deref());
    let pending = FollowUp {
        label: cmd.label.clone(),
        since: Instant::now(),
        action,
        head_before: snap.head.oid,
        succeeded: None,
    };
    ctx.data_mut(|d| d.insert_temp(follow_up_id(&tab.path), pending));
    tab.run(cmd);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{Fixture, git};

    fn plan(staged: usize, everything: bool) -> Plan {
        Plan { blocked: None, everything, staged }
    }

    #[test]
    fn labels_say_what_the_button_will_do() {
        assert_eq!(label(CommitAction::Commit, &plan(3, false), false, "pull request"), "Commit changes to 3 files");
        assert_eq!(label(CommitAction::CommitAndPush, &plan(1, false), false, "pull request"), "Commit changes to 1 file and push");
        assert_eq!(label(CommitAction::CommitAndPush, &plan(0, true), false, "pull request"), "Stage all changes, commit and push");
        assert_eq!(
            label(CommitAction::CommitAndPullRequest, &plan(2, false), false, "merge request"),
            "Commit and create merge request"
        );
        assert_eq!(label(CommitAction::Commit, &plan(0, false), true, "pull request"), "Amend previous commit");
        let blocked = Plan { blocked: Some("Nothing to commit"), everything: false, staged: 0 };
        assert_eq!(label(CommitAction::CommitAndPush, &blocked, false, "pull request"), "Nothing to commit");
    }

    /// Commit and push as the panel does it: the commit first, with its own
    /// undo entry, then the toolbar's push of the branch, which sets the
    /// upstream on a first push.
    #[test]
    fn commit_and_push_reaches_a_local_bare_remote() {
        let fx = Fixture::new("commit-and-push");
        let bare = fx.dir.with_extension("remote.git");
        let _ = std::fs::remove_dir_all(&bare);
        git(&fx.dir, &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()]);
        git(&fx.dir, &["remote", "add", "origin", bare.to_str().unwrap()]);
        fx.write("a.txt", "a\n");
        fx.commit_all("first");

        fx.write("b.txt", "b\n");
        let done = fx.run(ops::commit_everything("Add b"));
        assert!(done.undo.is_some(), "the commit stays undoable");
        let mut tab = RepoTab::new(fx.dir.clone());
        tab.snap = Some(done.snap.clone());
        let push = crate::ui::toolbar::push_cmd(&tab, false).expect("a branch with a remote can be pushed");
        assert_eq!(push.steps[0].args, ["push", "-u", "origin", "main"]);
        let pushed = fx.run(push);
        assert!(pushed.undo.is_none(), "pushing adds nothing to undo");
        assert_eq!(git(&bare, &["log", "-1", "--format=%s", "main"]), "Add b");

        // Next time it pushes to the upstream it now has.
        fx.write("c.txt", "c\n");
        let done = fx.run(ops::commit_everything("Add c"));
        tab.snap = Some(done.snap);
        let push = crate::ui::toolbar::push_cmd(&tab, false).unwrap();
        assert_eq!(push.steps[0].args, ["push"]);
        fx.run(push);
        assert_eq!(git(&bare, &["log", "-1", "--format=%s", "main"]), "Add c");

        // A commit that fails (nothing to commit) reports failure, so no
        // push follows it.
        let failed = fx.run_as(ops::commit("Nothing here", false), crate::worker::Origin::User);
        assert!(failed.result.is_err());
        let _ = std::fs::remove_dir_all(&bare);
    }
}
