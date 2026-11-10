//! Sidebar sections for pull requests, issues and CI runs.

use eframe::egui::{Align2, Color32, Rect, Response, Sense, Stroke, StrokeKind, Ui, Vec2, pos2};

use crate::git::{BranchInfo, Oid, RepoSnapshot};
use crate::git::ops::GitCmd;
use crate::hosting::{self, Checks, Feed, Issue, PullRequest, Run, RunState, Shown};
use crate::state::{RepoTab, Shared};
use crate::theme::{self, icons};
use crate::ui::graph::drag::{self, Branch};
use crate::ui::sidebar::{leaf, row, section};
use crate::ui::widgets::{elided, elided_pair, menu_item, spinner};

/// What a click or a menu entry asked for; applied once the lists are drawn.
enum Act {
    Refresh,
    Run(GitCmd),
    Jump(Oid),
    /// Open the "Create pull request" dialog, from this branch if given.
    NewPull(Option<Branch>),
}

/// Called at the end of the sidebar, inside its scroll area.
pub fn sections(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, snap: &RepoSnapshot) {
    let enabled = shared.settings.hosting_integration;
    let ctx = ui.ctx().clone();
    let Some(shown) = tab.hosting.sync(&ctx, &snap.workdir, &snap.remotes, enabled) else { return };
    let Shown { repo, view, .. } = &shown;

    let filter = tab.sidebar_filter.trim().to_lowercase();
    // The sidebar's filter box narrows these lists too, by text or `#number`.
    let matches = |number: Option<u64>, texts: &[&str]| {
        filter.is_empty()
            || texts.iter().any(|text| text.to_lowercase().contains(&filter))
            || number.is_some_and(|number| format!("#{number}").contains(&filter))
    };
    let now = hosting::epoch_now() as i64;
    let mut act = None;

    // ---- pull requests ----
    let pulls: Vec<&PullRequest> = view
        .pulls
        .items
        .iter()
        .filter(|pr| matches(Some(pr.number), &[&pr.title, &pr.author, &pr.head_ref]))
        .collect();
    let count = (pulls.len(), view.pulls.items.len());
    let top = ui.cursor().top();
    if header(ui, tab, &shown, "PULL REQUESTS", icons::GIT_PULL_REQUEST, count, true, &mut act) {
        notice(ui, &shown, &view.pulls, &mut act);
        for pr in pulls {
            pull_row(ui, snap, &shown, pr, &mut act);
        }
        more(ui, &shown, &view.pulls, &repo.pulls_url());
    }
    // A branch dropped anywhere on the list opens the dialog for it.
    let list = Rect::from_x_y_ranges(ui.max_rect().x_range(), top..=ui.cursor().top());
    if let Some(branch) = drop_target(ui, snap, &shown, list) {
        act = Some(Act::NewPull(Some(branch)));
    }

    // ---- issues ----
    let issues: Vec<&Issue> = view
        .issues
        .items
        .iter()
        .filter(|issue| matches(Some(issue.number), &[&issue.title, &issue.author]))
        .collect();
    let count = (issues.len(), view.issues.items.len());
    if header(ui, tab, &shown, "ISSUES", icons::RADIO_BUTTON, count, false, &mut act) {
        notice(ui, &shown, &view.issues, &mut act);
        for issue in issues {
            issue_row(ui, &shown, issue, &mut act);
        }
        more(ui, &shown, &view.issues, &repo.issues_url());
    }

    // ---- CI runs ----
    let runs: Vec<&Run> = view
        .runs
        .items
        .iter()
        .filter(|run| matches(None, &[&run.workflow, &run.branch, &run.title]))
        .collect();
    let count = (runs.len(), view.runs.items.len());
    if header(ui, tab, &shown, repo.service.ci_title(), icons::PLAY_CIRCLE, count, false, &mut act) {
        notice(ui, &shown, &view.runs, &mut act);
        for run in runs {
            run_row(ui, &shown, run, now, &mut act);
        }
        more(ui, &shown, &view.runs, &repo.runs_url());
    }

    match act {
        Some(Act::Refresh) => tab.hosting.refresh(),
        Some(Act::Run(cmd)) => tab.run(cmd),
        Some(Act::Jump(oid)) => tab.jump_to(oid),
        Some(Act::NewPull(branch)) => crate::ui::new_pull::open(tab, shared, branch),
        None => {}
    }
}

/// Makes `rect` (the pull request section) a place to drop a branch: it is
/// outlined while a branch that can be proposed is dragged, filled while the
/// pointer is on it, and the branch is returned on the frame it is dropped.
fn drop_target(ui: &Ui, snap: &RepoSnapshot, shown: &Shown, rect: Rect) -> Option<Branch> {
    let session = drag::session(ui.ctx())?;
    if !crate::ui::new_pull::accepts(snap, &shown.remote, session.branch()) {
        return None;
    }
    let clipped = rect.intersect(ui.clip_rect());
    if !ui.is_rect_visible(clipped) {
        return None;
    }
    if ui.rect_contains_pointer(clipped) {
        drag::highlight_row(ui.painter(), clipped);
        if session.over_place(ui.ctx(), "Create pull request") {
            return Some(session.branch().clone());
        }
    } else {
        let stroke = Stroke::new(1.0, theme::with_alpha(theme::accent(), 150));
        ui.painter().rect_stroke(clipped.shrink(0.5), 2.0, stroke, StrokeKind::Inside);
    }
    None
}

/// Opens a page in the default browser.
///
/// Not `ctx.open_url`: eframe is built without its `links` feature here,
/// which turns that call into a silent no-op. Every address passed in is
/// assembled by [`hosting::RepoId`] from validated parts, numbers and
/// percent-escaped branch names, never taken from a server's answer.
///
/// A scripted screenshot run (`GITKRILL_SHOT`) prints the address instead,
/// so trying the links never opens the real site on someone's desktop.
pub fn open_link(url: &str) {
    if std::env::var_os("GITKRILL_SHOT").is_some() {
        eprintln!("would open {url}");
        return;
    }
    crate::platform::open(std::path::Path::new(url));
}

fn browse(url: &str) {
    open_link(url);
}

/// The sidebar's own section header with a refresh button on top of it.
///
/// The header has no room to spare at the sidebar's narrowest, so the button
/// takes the place of the count at the right edge: while the pointer is over
/// the header, and as a spinner while a refresh is in flight. Returns whether
/// the section is open.
///
/// With `add`, a "+" next to it opens the "Create pull request" dialog.
#[allow(clippy::too_many_arguments)]
fn header(
    ui: &mut Ui,
    tab: &mut RepoTab,
    shown: &Shown,
    title: &str,
    icon: &str,
    (visible, total): (usize, usize),
    add: bool,
    act: &mut Option<Act>,
) -> bool {
    let (left, top, width) = (ui.cursor().left(), ui.cursor().top(), ui.available_width());
    let open = section(ui, tab, title, icon, visible, total);
    let bar = Rect::from_min_max(pos2(left, top), pos2(left + width, ui.cursor().top()));

    // Where the header painted its count: right-aligned, 12 points in.
    let count = if visible == total { total.to_string() } else { format!("{visible}/{total}") };
    let count_width = ui.painter().layout_no_wrap(count, theme::bold(11.5), theme::accent()).size().x;
    let slot = Vec2::new(count_width.max(14.0) + 10.0, 22.0);
    let button = Rect::from_min_size(pos2(bar.right() - 7.0 - slot.x, bar.center().y - slot.y / 2.0), slot);
    let response = ui.interact(button, ui.id().with(("hosting-refresh", title)), Sense::click());

    let busy = shown.view.busy;
    let over_header = ui.rect_contains_pointer(bar);
    if (busy || over_header) && ui.is_rect_visible(button) {
        let painter = ui.painter();
        // Cover the count with whatever the header has behind it.
        let fill = if response.hovered() {
            theme::bg_raised()
        } else if over_header {
            theme::bg_hover()
        } else {
            theme::bg_panel()
        };
        painter.rect_filled(button, 4.0, fill);
        if busy {
            spinner(painter, button.center(), 5.0, theme::accent());
            ui.ctx().request_repaint();
        } else {
            let color = if response.hovered() { theme::text_bright() } else { theme::text_dim() };
            painter.text(button.center(), Align2::CENTER_CENTER, icons::ARROWS_CLOCKWISE, theme::font(13.0), color);
        }
    }
    if response.clicked() {
        *act = Some(Act::Refresh);
    }
    // "+" just left of the refresh button, over the end of the title.
    if add && over_header {
        let plus = Rect::from_min_size(pos2(button.left() - 24.0, button.top()), Vec2::new(22.0, button.height()));
        let add_response = ui.interact(plus, ui.id().with(("hosting-add", title)), Sense::click());
        // The header behind is hovered unless one of the buttons is.
        let behind = if add_response.hovered() || response.hovered() { theme::bg_panel() } else { theme::bg_hover() };
        let painter = ui.painter();
        let cover = Rect::from_min_max(pos2(plus.left() - 4.0, bar.top() + 1.0), pos2(button.left(), bar.bottom()));
        painter.rect_filled(cover, 0.0, behind);
        let fill = if add_response.hovered() { theme::bg_raised() } else { behind };
        painter.rect_filled(plus, 4.0, fill);
        let color = if add_response.hovered() { theme::text_bright() } else { theme::text_dim() };
        painter.text(plus.center(), Align2::CENTER_CENTER, icons::PLUS, theme::font(14.0), color);
        if add_response.clicked() {
            *act = Some(Act::NewPull(None));
        }
        add_response.on_hover_text("Create pull request");
    }
    response.on_hover_ui(|ui| {
        let service = shown.repo.service.name();
        let mut text = format!("Refresh from {service}  ({})", shown.repo.slug());
        let auth = shown.view.auth.describe();
        if !auth.is_empty() {
            text.push_str(&format!("\n{auth}"));
        }
        if let Some(updated) = shown.view.updated {
            text.push_str(&format!("\nUpdated at {}", hosting::clock(updated)));
        }
        ui.label(text);
    });
    open
}

/// The one dim line a section shows when its list could not be refreshed
/// (the rows under it are then the last good answer), or while the first
/// answer is on its way. Clicking it tries again.
fn notice<T>(ui: &mut Ui, shown: &Shown, feed: &Feed<T>, act: &mut Option<Act>) {
    let service = shown.repo.service;
    if let Some(problem) = &feed.problem {
        let icon = match problem {
            hosting::Problem::RateLimited { .. } => icons::HOURGLASS,
            _ => icons::WARNING,
        };
        let response = row(ui, 0, false, |painter, rect, x| {
            let y = rect.center().y;
            painter.text(pos2(x, y), Align2::LEFT_CENTER, icon, theme::font(14.0), theme::text_dim());
            // The whole sentence when it fits; otherwise its short form, so
            // the part that matters (the time) is not what gets cut off.
            let room = rect.right() - 10.0 - (x + 20.0);
            let font = theme::font(12.0);
            let full = painter.layout_no_wrap(problem.line(service), font.clone(), theme::text_dim());
            let galley = if full.size().x <= room {
                full
            } else {
                elided(painter, &problem.brief(service), font, theme::text_dim(), room)
            };
            painter.galley(pos2(x + 20.0, (y - galley.size().y / 2.0).round()), galley, theme::text_dim());
        });
        if response.clicked() {
            *act = Some(Act::Refresh);
        }
        response.context_menu(|ui| refresh_item(ui, act));
        response.on_hover_ui(|ui| {
            ui.set_max_width(320.0);
            ui.label(problem.detail(service));
        });
    } else if !feed.loaded && shown.view.busy {
        leaf(ui, 0, false, icons::CIRCLE_NOTCH, "Loading…", theme::text_faint(), "");
    }
}

/// A last row linking to the full list when the service has more than we show.
fn more<T>(ui: &mut Ui, shown: &Shown, feed: &Feed<T>, url: &str) {
    if !feed.more {
        return;
    }
    let text = format!("More on {}…", shown.repo.service.name());
    let response = leaf(ui, 0, false, icons::ARROW_SQUARE_OUT, &text, theme::text_dim(), "");
    if opened(&response) {
        browse(url);
    }
    response.on_hover_text(url);
}

/// A click that should open a page. The second click of a double-click is
/// left out, so the habit of double-clicking sidebar rows opens one tab.
fn opened(response: &Response) -> bool {
    response.clicked() && !response.double_clicked()
}

fn refresh_item(ui: &mut Ui, act: &mut Option<Act>) {
    if menu_item(ui, icons::ARROWS_CLOCKWISE, "Refresh") {
        *act = Some(Act::Refresh);
    }
}

/// "Open in browser" and "Copy link", shared by every kind of row.
fn link_items(ui: &mut Ui, url: &str) {
    if menu_item(ui, icons::ARROW_SQUARE_OUT, "Open in browser") {
        browse(url);
    }
    if menu_item(ui, icons::LINK, "Copy link") {
        ui.ctx().copy_text(url.to_owned());
    }
}

/// An icon in its own colour, a dim prefix and a title, with optional marks
/// at the right edge (rightmost first): pull request and issue rows.
fn item_row(
    ui: &mut Ui,
    highlighted: bool,
    (icon, icon_color): (&str, Color32),
    (prefix, title, title_color): (&str, &str, Color32),
    marks: &[(&str, f32, Color32)],
) -> Response {
    row(ui, 0, highlighted, |painter, rect, x| {
        let y = rect.center().y;
        painter.text(pos2(x, y), Align2::LEFT_CENTER, icon, theme::font(14.0), icon_color);
        let mut right = rect.right() - 10.0;
        for (mark, size, color) in marks {
            let used = painter.text(pos2(right, y), Align2::RIGHT_CENTER, mark, theme::font(*size), *color);
            right = used.left() - 6.0;
        }
        let font = if highlighted { theme::bold(13.0) } else { theme::font(13.0) };
        let galley = elided_pair(
            painter,
            (prefix, theme::font(13.0), theme::text_dim()),
            (title, font, title_color),
            right - (x + 20.0),
        );
        painter.galley(pos2(x + 20.0, (y - galley.size().y / 2.0).round()), galley, title_color);
    })
}

/// The local branch a pull request is available as: the one "Check out"
/// creates, or its own head branch when it comes from this repository.
fn local_branch<'a>(snap: &'a RepoSnapshot, shown: &Shown, pr: &PullRequest) -> Option<&'a BranchInfo> {
    let checked_out = hosting::pull_branch(pr.number);
    snap.branches.iter().find(|branch| branch.name == checked_out).or_else(|| {
        let own = pr.is_from(&shown.repo);
        snap.branches.iter().find(|branch| own && branch.name == pr.head_ref)
    })
}

fn checks_mark(checks: Checks) -> (&'static str, Color32, &'static str) {
    match checks {
        Checks::Passing => (icons::CHECK, theme::green(), "Checks passed"),
        Checks::Failing => (icons::X, theme::red(), "Checks failed"),
        Checks::Pending => (icons::CIRCLE_NOTCH, theme::yellow(), "Checks running"),
    }
}

fn pull_row(ui: &mut Ui, snap: &RepoSnapshot, shown: &Shown, pr: &PullRequest, act: &mut Option<Act>) {
    let local = local_branch(snap, shown, pr);
    let current = local.is_some_and(|branch| branch.is_head);
    let checks = hosting::checks_for(&pr.head_sha, &shown.view.runs.items).map(checks_mark);

    let (icon_color, title_color) = match (pr.draft, current) {
        (true, _) => (theme::text_faint(), theme::text_dim()),
        (false, true) => (theme::green(), theme::text_bright()),
        (false, false) => (theme::green(), theme::text()),
    };
    let mut marks = Vec::new();
    if let Some((mark, color, _)) = checks {
        marks.push((mark, 13.0, color));
    }
    if pr.draft {
        marks.push(("Draft", 11.0, theme::text_dim()));
    }
    let number = format!("#{}", pr.number);
    let response = item_row(
        ui,
        current,
        (icons::GIT_PULL_REQUEST, icon_color),
        (&number, &pr.title, title_color),
        &marks,
    );

    let url = shown.repo.pull_url(pr.number);
    if opened(&response) {
        browse(&url);
    }

    // The exact commit when the graph has it, else the local branch's tip.
    let in_graph = |oid: &Oid| snap.row_of(*oid).is_some();
    let target = Oid::from_str(&pr.head_sha)
        .ok()
        .filter(in_graph)
        .or_else(|| local.and_then(|branch| branch.oid).filter(in_graph));

    response.context_menu(|ui| {
        ui.set_min_width(220.0);
        let branch = hosting::pull_branch(pr.number);
        let on_branch = snap.head.branch.as_deref() == Some(branch.as_str());
        let label = if on_branch {
            format!("Update {branch} from {}", shown.repo.service.name())
        } else {
            format!("Check out as {branch}")
        };
        if menu_item(ui, icons::CHECK, &label) {
            *act = Some(Act::Run(hosting::checkout_pull(
                &shown.remote,
                shown.repo.service,
                pr.number,
                on_branch,
            )));
        }
        if let Some(oid) = target
            && menu_item(ui, icons::GIT_COMMIT, "Show in graph")
        {
            *act = Some(Act::Jump(oid));
        }
        ui.separator();
        link_items(ui, &url);
        if menu_item(ui, icons::COPY, "Copy branch name") {
            ui.ctx().copy_text(pr.head_ref.clone());
        }
        ui.separator();
        refresh_item(ui, act);
    });

    response.on_hover_ui(|ui| {
        ui.set_max_width(360.0);
        let mut text = format!("#{}  {}\n{}  {}  {}", pr.number, pr.title, pr.head_ref, icons::ARROW_RIGHT, pr.base_ref);
        text.push_str(&format!("\nby {}", pr.author));
        if pr.draft {
            text.push_str("  ·  draft");
        }
        if let Some((_, _, meaning)) = checks {
            text.push_str(&format!("\n{meaning}"));
        }
        if let Some(branch) = local {
            text.push_str(&format!("\nLocal branch: {}", branch.name));
        }
        ui.label(text);
    });
}

fn issue_row(ui: &mut Ui, shown: &Shown, issue: &Issue, act: &mut Option<Act>) {
    let number = format!("#{}", issue.number);
    let response = item_row(
        ui,
        false,
        (icons::RADIO_BUTTON, theme::green()),
        (&number, &issue.title, theme::text()),
        &[],
    );

    let url = shown.repo.issue_url(issue.number);
    if opened(&response) {
        browse(&url);
    }
    response.context_menu(|ui| {
        ui.set_min_width(190.0);
        link_items(ui, &url);
        ui.separator();
        refresh_item(ui, act);
    });
    response.on_hover_ui(|ui| {
        ui.set_max_width(360.0);
        let mut text = format!("#{}  {}\nopened by {}", issue.number, issue.title, issue.author);
        match issue.comments {
            0 => {}
            1 => text.push_str("  ·  1 comment"),
            n => text.push_str(&format!("  ·  {n} comments")),
        }
        if !issue.labels.is_empty() {
            text.push_str(&format!("\n{}", issue.labels.join(", ")));
        }
        ui.label(text);
    });
}

fn run_mark(state: RunState) -> (&'static str, Color32) {
    match state {
        RunState::Success => (icons::CHECK_CIRCLE, theme::green()),
        RunState::Failure => (icons::X_CIRCLE, theme::red()),
        RunState::Running => (icons::CIRCLE_NOTCH, theme::yellow()),
        RunState::Queued => (icons::CLOCK, theme::yellow()),
        RunState::NeedsAction => (icons::WARNING_CIRCLE, theme::orange()),
        RunState::Cancelled => (icons::PROHIBIT, theme::text_dim()),
        RunState::Skipped => (icons::MINUS_CIRCLE, theme::text_dim()),
    }
}

fn run_row(ui: &mut Ui, shown: &Shown, run: &Run, now: i64, act: &mut Option<Act>) {
    let when = hosting::ago(run.when, now);
    let response = row(ui, 0, false, |painter, rect, x| {
        let y = rect.center().y;
        let (icon, color) = run_mark(run.state);
        painter.text(pos2(x, y), Align2::LEFT_CENTER, icon, theme::font(14.0), color);
        let mut right = rect.right() - 10.0;
        if !when.is_empty() {
            let used = painter.text(pos2(right, y), Align2::RIGHT_CENTER, &when, theme::font(11.0), theme::text_dim());
            right = used.left() - 6.0;
        }
        let galley = elided_pair(
            painter,
            (&run.workflow, theme::font(13.0), theme::text()),
            (&run.branch, theme::font(12.0), theme::text_dim()),
            right - (x + 20.0),
        );
        painter.galley(pos2(x + 20.0, (y - galley.size().y / 2.0).round()), galley, theme::text());
    });

    let url = shown.repo.run_url(run.id);
    if opened(&response) {
        browse(&url);
    }
    response.context_menu(|ui| {
        ui.set_min_width(190.0);
        link_items(ui, &url);
        ui.separator();
        refresh_item(ui, act);
    });
    response.on_hover_ui(|ui| {
        ui.set_max_width(360.0);
        let mut text = format!("{} #{}  ·  {}", run.workflow, run.number, run.outcome);
        // Scheduled runs are titled after their workflow: say it once.
        if !run.title.is_empty() && run.title != run.workflow {
            text.push_str(&format!("\n{}", run.title));
        }
        let mut origin: Vec<&str> = Vec::new();
        origin.extend([run.branch.as_str(), run.event.as_str()].into_iter().filter(|part| !part.is_empty()));
        if !origin.is_empty() {
            text.push_str(&format!("\n{}", origin.join("  ·  ")));
        }
        if run.when > 0 {
            text.push_str(&format!("\n{}", crate::state::format_time(run.when)));
        }
        ui.label(text);
    });
}
