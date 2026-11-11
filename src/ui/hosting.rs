//! Sidebar sections for pull requests (merge requests on GitLab), issues
//! (work items on Azure DevOps, tickets on SourceHut) and CI runs, the
//! offer to connect a self-hosted server, and the dialog that asks what
//! runs on it. SourceHut has no pull requests: its section says how patches
//! are sent instead, and a section the service says the repository does
//! not have (`Feed::absent`) is left out.
//!
//! In a fork whose parent is known (`hosting::Family`), a Fork / Parent
//! switch above the sections picks whose lists they show, remembered per
//! repository (`RepoPrefs::hosting_parent`). The parent's list puts the
//! pull requests from the fork first.

use eframe::egui::{Align2, Button, Color32, Rect, Response, RichText, Sense, Stroke, StrokeKind, Ui, Vec2, pos2, vec2};

use crate::git::{BranchInfo, Oid, RemoteInfo, RepoSnapshot};
use crate::git::ops::GitCmd;
use crate::hosting::{
    self, Checks, Family, Feed, Issue, Offer, PullRequest, Review, Run, RunState, ServerKind, Service, Shown,
};
use crate::state::{Dialog, RepoTab, Shared};
use crate::theme::{self, icons};
use crate::ui::dialogs;
use crate::ui::graph::drag::{self, Branch};
use crate::ui::sidebar::{leaf, plain_section_header, row, section, section_header};
use crate::ui::widgets::{elided, elided_pair, menu_item, segmented_wide, spinner};

/// What a click or a menu entry asked for; applied once the lists are drawn.
enum Act {
    Refresh,
    Run(GitCmd),
    Jump(Oid),
    /// Ask what runs on a server.
    Connect(Offer),
    /// Remember what the user said a server is.
    Choose(String, ServerKind),
    /// Forget it: the server is no longer contacted, and offered again.
    Disconnect(String),
    /// Open the "Create pull request" dialog, from this branch if given.
    NewPull(Option<Branch>),
    /// Show the parent's lists (true) or the fork's own.
    ShowParent(bool),
}

/// Called at the end of the sidebar, inside its scroll area.
pub fn sections(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, snap: &RepoSnapshot) {
    let enabled = shared.settings.hosting_integration;
    let ctx = ui.ctx().clone();
    let servers = &shared.settings.hosting_servers;
    let offers = if enabled { hosting::offers(&snap.remotes, servers) } else { Vec::new() };
    let disconnected = &shared.settings.hosting_disconnected;
    let parent_wanted = crate::prefs::repo(&shared.settings, &tab.path).hosting_parent;
    let shown = tab.hosting.sync(&ctx, &snap.workdir, &snap.remotes, servers, disconnected, enabled, parent_wanted);

    let mut act = None;
    if let Some(shown) = &shown {
        if let Some(family) = &shown.family {
            family_switch(ui, family, &mut act);
        }
        lists(ui, tab, snap, shown, &mut act);
    }
    if !offers.is_empty() {
        offer_section(ui, tab, &offers, &mut act);
    }

    match act {
        Some(Act::Refresh) => tab.hosting.refresh(),
        Some(Act::Run(cmd)) => tab.run(cmd),
        Some(Act::Jump(oid)) => tab.jump_to(oid),
        Some(Act::Connect(offer)) => shared.show(connect_dialog(&offer)),
        Some(Act::Choose(host, kind)) => {
            shared.settings.hosting_servers.insert(host, kind);
        }
        Some(Act::Disconnect(host)) => {
            shared.settings.hosting_servers.remove(&host);
        }
        Some(Act::NewPull(branch)) => crate::ui::new_pull::open(tab, shared, branch),
        Some(Act::ShowParent(parent)) => show_parent(tab, shared, parent),
        None => {}
    }
}

/// In a fork, makes the sidebar show its parent's lists (`parent`) or the
/// fork's own, and remembers that for the repository.
pub fn show_parent(tab: &RepoTab, shared: &mut Shared, parent: bool) {
    crate::prefs::repo_mut(&mut shared.settings, &tab.path).hosting_parent = parent;
    crate::prefs::tidy(&mut shared.settings);
}

/// Fork / Parent: whose pull requests, issues and CI the sections below
/// show, for a fork whose parent is known.
fn family_switch(ui: &mut Ui, family: &Family, act: &mut Option<Act>) {
    let (fork, parent) = (family.fork.slug(), family.parent.slug());
    let tips = [
        format!("Pull requests, issues and CI of {fork} ({})", family.fork_remote),
        format!(
            "Pull requests, issues and CI of {parent}, the repository {fork} is a fork of. Pull \
             requests from {fork} come first."
        ),
    ];
    // The repositories' names too when both halves have room for them.
    let (icon_fork, icon_parent) = (icons::GIT_FORK, icons::ARROW_BEND_LEFT_UP);
    let named = (format!("Fork  {fork}"), format!("Parent  {parent}"));
    let half = (ui.available_width() - 16.0) / 2.0;
    let fits = |icon: &str, label: &str| {
        let width = ui.painter().layout_no_wrap(format!("{icon} {label}"), theme::font(12.0), theme::text()).size().x;
        width + 16.0 <= half
    };
    let (first, second) = match fits(icon_fork, &named.0) && fits(icon_parent, &named.1) {
        true => (named.0.as_str(), named.1.as_str()),
        false => ("Fork", "Parent"),
    };
    let mut parent_shown = family.parent_shown;
    ui.add_space(6.0);
    let changed = ui
        .horizontal(|ui| {
            ui.add_space(8.0);
            ui.set_max_width(ui.available_width() - 8.0);
            let tips = [tips[0].as_str(), tips[1].as_str()];
            segmented_wide(ui, &mut parent_shown, (icon_fork, first), (icon_parent, second), tips)
        })
        .inner;
    ui.add_space(6.0);
    if changed {
        *act = Some(Act::ShowParent(parent_shown));
    }
}

/// The three lists for the repository being shown.
fn lists(ui: &mut Ui, tab: &mut RepoTab, snap: &RepoSnapshot, shown: &Shown, act: &mut Option<Act>) {
    let Shown { repo, view, .. } = shown;
    let service = repo.service;
    let filter = tab.sidebar_filter.trim().to_lowercase();
    // The sidebar's filter box narrows these lists too, by text or by
    // number (`#12`, or `!12` for a merge request).
    let matches = |number: Option<u64>, texts: &[&str]| {
        filter.is_empty()
            || texts.iter().any(|text| text.to_lowercase().contains(&filter))
            || number.is_some_and(|n| format!("#{n}").contains(&filter) || format!("!{n}").contains(&filter))
    };
    let now = hosting::epoch_now() as i64;

    // ---- pull / merge requests, or how patches are sent ----
    if !service.has_pulls() {
        patches_section(ui, tab, shown, act);
    }
    let mut pulls: Vec<&PullRequest> = view
        .pulls
        .items
        .iter()
        .filter(|pr| matches(Some(pr.number), &[&pr.title, &pr.author, &pr.head_ref]))
        .collect();
    // In the parent's list, those from the fork come first.
    if let Some(family) = shown.family.as_ref().filter(|family| family.parent_shown) {
        pulls.sort_by_key(|pr| !family.is_from_fork(pr));
    }
    let count = (pulls.len(), view.pulls.items.len());
    let url = repo.pulls_url();
    let top = ui.cursor().top();
    if service.has_pulls() && header(ui, tab, shown, (service.pulls_title(), pull_icon(service), &url), count, true, act) {
        notice(ui, shown, &view.pulls, act);
        for pr in pulls {
            pull_row(ui, snap, shown, pr, act);
        }
        more(ui, shown, &view.pulls, &url);
    }
    // A branch dropped anywhere on the list opens the dialog for it.
    let list = Rect::from_x_y_ranges(ui.max_rect().x_range(), top..=ui.cursor().top());
    if service.has_pulls()
        && let Some(branch) = drop_target(ui, snap, shown, list)
    {
        *act = Some(Act::NewPull(Some(branch)));
    }

    // ---- issues ----
    let issues: Vec<&Issue> = view
        .issues
        .items
        .iter()
        .filter(|issue| matches(Some(issue.number), &[&issue.title, &issue.author]))
        .collect();
    let count = (issues.len(), view.issues.items.len());
    let url = repo.issues_url();
    let icon = if service == Service::SourceHut { icons::TICKET } else { icons::RADIO_BUTTON };
    let issues_shown = service.has_issues() && !view.issues.absent;
    if issues_shown && header(ui, tab, shown, (service.issues_title(), icon, &url), count, false, act) {
        notice(ui, shown, &view.issues, act);
        for issue in issues {
            issue_row(ui, shown, issue, act);
        }
        more(ui, shown, &view.issues, &url);
    }

    // ---- CI runs ----
    let runs: Vec<&Run> = view
        .runs
        .items
        .iter()
        .filter(|run| matches(None, &[&run.workflow, &run.branch, &run.title]))
        .collect();
    let count = (runs.len(), view.runs.items.len());
    let url = repo.runs_url();
    if !view.runs.absent && header(ui, tab, shown, (service.ci_title(), icons::PLAY_CIRCLE, &url), count, false, act) {
        notice(ui, shown, &view.runs, act);
        for run in runs {
            run_row(ui, shown, run, now, act);
        }
        more(ui, shown, &view.runs, &url);
    }
}

fn pull_icon(service: Service) -> &'static str {
    if service.is_gitlab() { icons::GIT_MERGE } else { icons::GIT_PULL_REQUEST }
}

/// SourceHut's stand-in for the pull request list: patches go by email to
/// the project's mailing list. The row opens SourceHut's guide to setting
/// `git send-email` up.
fn patches_section(ui: &mut Ui, tab: &mut RepoTab, shown: &Shown, act: &mut Option<Act>) {
    let (open, response) = plain_section_header(ui, tab, "PATCHES", icons::ENVELOPE_SIMPLE);
    let home = shown.repo.home_url();
    response.context_menu(|ui| {
        ui.set_min_width(200.0);
        if menu_item(ui, icons::ARROW_SQUARE_OUT, &format!("Open on {}", shown.repo.site())) {
            browse(&home);
        }
    });
    if !open {
        return;
    }
    let row = leaf(ui, 0, false, icons::PAPER_PLANE_TILT, "Patches go by email", theme::text(), "");
    if opened(&row) {
        browse(hosting::sourcehut::SEND_EMAIL_GUIDE);
    }
    row.context_menu(|ui| {
        ui.set_min_width(220.0);
        if menu_item(ui, icons::ARROW_SQUARE_OUT, "How to send patches (git-send-email.io)") {
            browse(hosting::sourcehut::SEND_EMAIL_GUIDE);
        }
        if menu_item(ui, icons::ARROW_SQUARE_OUT, &format!("Open {} on SourceHut", shown.repo.slug())) {
            browse(&home);
        }
        ui.separator();
        refresh_item(ui, act);
    });
    row.on_hover_ui(|ui| {
        ui.set_max_width(320.0);
        ui.label(
            "SourceHut has no pull requests. Patches are mailed to the project's mailing list with \
             git send-email; the repository's page or its README names the list. Click for SourceHut's \
             guide to setting git send-email up.",
        );
    });
}

/// Makes `rect` (the pull request section) a place to drop a branch: it is
/// outlined while a branch that can be proposed is dragged, filled while the
/// pointer is on it, and the branch is returned on the frame it is dropped.
fn drop_target(ui: &Ui, snap: &RepoSnapshot, shown: &Shown, rect: Rect) -> Option<Branch> {
    let session = drag::session(ui.ctx())?;
    // Pull requests are opened from the fork, whichever list is shown.
    let remote = shown.family.as_ref().map_or(&shown.remote, |family| &family.fork_remote);
    if !crate::ui::new_pull::accepts(snap, remote, session.branch()) {
        return None;
    }
    let clipped = rect.intersect(ui.clip_rect());
    if !ui.is_rect_visible(clipped) {
        return None;
    }
    if ui.rect_contains_pointer(clipped) {
        drag::highlight_row(ui.painter(), clipped);
        if session.over_place(ui.ctx(), &format!("Create {}", shown.repo.service.pull_word())) {
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
/// the header, and as a spinner while a refresh is in flight. Right-clicking
/// the header offers the list's page and, for a self-hosted server, a way to
/// disconnect it. Returns whether the section is open.
///
/// With `add`, a "+" next to it opens the "Create pull request" dialog.
#[allow(clippy::too_many_arguments)]
fn header(
    ui: &mut Ui,
    tab: &mut RepoTab,
    shown: &Shown,
    (title, icon, url): (&str, &str, &str),
    (visible, total): (usize, usize),
    add: bool,
    act: &mut Option<Act>,
) -> bool {
    let (left, top, width) = (ui.cursor().left(), ui.cursor().top(), ui.available_width());
    let (open, bar_response) = section_header(ui, tab, title, icon, visible, total);
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
        add_response.on_hover_text(format!("Create {}", shown.repo.service.pull_word()));
    }
    response.on_hover_ui(|ui| {
        let mut text = format!("Refresh from {}  ({})", shown.repo.site(), shown.repo.slug());
        let auth = match shown.disconnected {
            // No sign-in is looked for: say why, rather than "not signed in".
            true => "Disconnected in Preferences > Integrations: no sign-in is used".to_owned(),
            false => shown.view.auth.describe(shown.repo.service),
        };
        if !auth.is_empty() {
            text.push_str(&format!("\n{auth}"));
        }
        if let Some(updated) = shown.view.updated {
            text.push_str(&format!("\nUpdated at {}", hosting::clock(updated)));
        }
        ui.label(text);
    });
    bar_response.context_menu(|ui| {
        ui.set_min_width(200.0);
        refresh_item(ui, act);
        if menu_item(ui, icons::ARROW_SQUARE_OUT, &format!("Open on {}", shown.repo.site())) {
            browse(url);
        }
        disconnect_item(ui, shown, act);
    });
    open
}

/// "Disconnect <host>", for a server the user connected.
fn disconnect_item(ui: &mut Ui, shown: &Shown, act: &mut Option<Act>) {
    if !shown.repo.self_hosted() {
        return;
    }
    ui.separator();
    let host = &shown.repo.host;
    if menu_item(ui, icons::LINK_BREAK, &format!("Disconnect {host}")) {
        *act = Some(Act::Disconnect(host.clone()));
    }
}

/// The one dim line a section shows when its list could not be refreshed
/// (the rows under it are then the last good answer), or while the first
/// answer is on its way. Clicking it tries again.
fn notice<T>(ui: &mut Ui, shown: &Shown, feed: &Feed<T>, act: &mut Option<Act>) {
    let service = shown.repo.service;
    if let Some(problem) = &feed.problem {
        let icon = match problem {
            hosting::Problem::RateLimited { .. } => icons::HOURGLASS,
            hosting::Problem::SignInNeeded { .. } => icons::SIGN_IN,
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
        response.context_menu(|ui| {
            refresh_item(ui, act);
            disconnect_item(ui, shown, act);
        });
        response.on_hover_ui(|ui| {
            ui.set_max_width(320.0);
            let mut detail = problem.detail(service);
            if shown.disconnected {
                detail.push_str(&format!(
                    "\n\nGitKrill does not use your {} sign-in: the account is disconnected in \
                     Preferences > Integrations.",
                    shown.repo.host
                ));
            }
            ui.label(detail);
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
    let text = format!("More on {}…", shown.repo.site());
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
/// creates, or its own head branch when it comes from this repository (or,
/// in a fork's parent, from the fork).
fn local_branch<'a>(snap: &'a RepoSnapshot, shown: &Shown, pr: &PullRequest) -> Option<&'a BranchInfo> {
    let checked_out = hosting::pull_branch(shown.repo.service, pr.number);
    snap.branches.iter().find(|branch| branch.name == checked_out).or_else(|| {
        let own = pr.is_from(&shown.repo) || shown.family.as_ref().is_some_and(|family| family.is_from_fork(pr));
        snap.branches.iter().find(|branch| own && branch.name == pr.head_ref)
    })
}

fn checks_mark(service: Service, checks: Checks) -> (&'static str, Color32, String) {
    // "Pipeline", "Builds" or "Checks".
    let what = if service.has_pipelines() {
        "Pipeline"
    } else if service.has_builds() {
        "Builds"
    } else {
        "Checks"
    };
    match checks {
        Checks::Passing => (icons::CHECK, theme::green(), format!("{what} passed")),
        Checks::Failing => (icons::X, theme::red(), format!("{what} failed")),
        Checks::Pending => (icons::CIRCLE_NOTCH, theme::yellow(), format!("{what} running")),
    }
}

/// The mark for what the reviewers say, with its colour and what it means
/// (for the tooltip). Nothing for a pull request nobody has reviewed yet,
/// or whose reviews are not known.
fn review_mark(pr: &PullRequest) -> Option<(String, Color32, String)> {
    let approvals = pr.approvals.unwrap_or_default();
    let count = |n: u32| if n == 1 { "1 approval".to_owned() } else { format!("{n} approvals") };
    match pr.review {
        Review::Unknown | Review::Unreviewed => None,
        Review::Approved => {
            let meaning = match approvals.given {
                0 => "Approved".to_owned(),
                n => format!("Approved ({})", count(n)),
            };
            Some((icons::SEAL_CHECK.to_owned(), theme::green(), meaning))
        }
        Review::ChangesRequested => Some((icons::NOTE_PENCIL.to_owned(), theme::orange(), "Changes requested".to_owned())),
        Review::Required => {
            let (mark, meaning) = match (approvals.fraction(), approvals.given) {
                (Some(fraction), _) => (
                    format!("{} {fraction}", icons::EYE),
                    format!("Approvals: {fraction}, {} more needed", approvals.left),
                ),
                (None, 0) => (icons::EYE.to_owned(), "Review required".to_owned()),
                (None, n) => (icons::EYE.to_owned(), format!("Review required ({} so far)", count(n))),
            };
            Some((mark, theme::yellow(), meaning))
        }
    }
}

/// Where a pull request's branch is, when that is not the repository shown:
/// for its tooltip.
fn fork_line(service: Service, shown: &Shown, pr: &PullRequest) -> Option<String> {
    if pr.is_from(&shown.repo) {
        return None;
    }
    // The fork this clone's remote points at, in its parent's list.
    let ours = shown.family.as_ref().filter(|family| family.is_from_fork(pr));
    Some(match (&pr.head_repo, service.is_gitlab()) {
        (Some(fork), _) => match ours {
            Some(family) => format!("From the fork {fork} ({})", family.fork_remote),
            None => format!("From the fork {fork}"),
        },
        // GitHub names every fork it still has.
        (None, false) => "From a fork that was deleted".to_owned(),
        (None, true) => "From a fork".to_owned(),
    })
}

fn pull_row(ui: &mut Ui, snap: &RepoSnapshot, shown: &Shown, pr: &PullRequest, act: &mut Option<Act>) {
    let service = shown.repo.service;
    let local = local_branch(snap, shown, pr);
    let current = local.is_some_and(|branch| branch.is_head);
    let checks = hosting::checks_for(&pr.head_sha, &shown.view.runs.items).map(|checks| checks_mark(service, checks));
    let review = review_mark(pr);
    let fork = fork_line(service, shown, pr);

    let (icon_color, title_color) = match (pr.draft, current) {
        (true, _) => (theme::text_faint(), theme::text_dim()),
        (false, true) => (theme::green(), theme::text_bright()),
        (false, false) => (theme::green(), theme::text()),
    };
    let mut marks = Vec::new();
    if let Some((mark, color, _)) = &checks {
        marks.push((*mark, 13.0, *color));
    }
    if let Some((mark, color, _)) = &review {
        // An icon alone, or with a count of approvals (smaller).
        marks.push((mark.as_str(), if mark.chars().count() > 1 { 11.5 } else { 13.0 }, *color));
    }
    if pr.draft {
        marks.push(("Draft", 11.0, theme::text_dim()));
    }
    if fork.is_some() {
        // The fork's own stand out in its parent's list.
        let ours = shown.family.as_ref().is_some_and(|family| family.is_from_fork(pr));
        marks.push((icons::GIT_FORK, 12.5, if ours { theme::accent() } else { theme::text_dim() }));
    }
    let number = service.pull_number(pr.number);
    let response = item_row(
        ui,
        current,
        (pull_icon(service), icon_color),
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
        let branch = hosting::pull_branch(service, pr.number);
        let on_branch = snap.head.branch.as_deref() == Some(branch.as_str());
        let label = if on_branch {
            format!("Update {branch} from {}", shown.repo.site())
        } else {
            format!("Check out as {branch}")
        };
        // Bitbucket and Azure DevOps keep no ref for a pull request: one
        // from a fork cannot be fetched from this remote.
        if let Some(cmd) = hosting::checkout_pull_request(&shown.remote, service, &shown.repo, pr, on_branch)
            && menu_item(ui, icons::CHECK, &label)
        {
            *act = Some(Act::Run(cmd));
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
        // A fork's branch as the services write it: `owner:branch`.
        let head = match (&fork, pr.head_repo.as_deref().and_then(|full| full.rsplit_once('/'))) {
            (Some(_), Some((owner, _))) => format!("{owner}:{}", pr.head_ref),
            _ => pr.head_ref.clone(),
        };
        let mut text = format!("{number}  {}\n{head}  {}  {}", pr.title, icons::ARROW_RIGHT, pr.base_ref);
        text.push_str(&format!("\nby {}", pr.author));
        if pr.draft {
            text.push_str("  ·  draft");
        }
        if let Some(fork) = &fork {
            text.push_str(&format!("\n{fork}"));
        }
        match (&review, pr.review) {
            (Some((_, _, meaning)), _) => text.push_str(&format!("\n{meaning}")),
            (None, Review::Unreviewed) => {
                text.push_str(if service.is_gitlab() { "\nNo approvals yet" } else { "\nNo reviews yet" });
            }
            (None, _) => {}
        }
        if let Some((_, _, meaning)) = &checks {
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

    let url = shown.repo.run_page(run);
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
        // Bitbucket Data Center's builds are not always numbered.
        let mut text = match run.number {
            0 => format!("{}  ·  {}", run.workflow, run.outcome),
            number => format!("{} #{number}  ·  {}", run.workflow, run.outcome),
        };
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

// ---- self-hosted servers --------------------------------------------------

/// Servers the remotes point at that the user has not said anything about:
/// one row each, which asks what runs there. Nothing is sent to a server
/// before that.
fn offer_section(ui: &mut Ui, tab: &mut RepoTab, offers: &[Offer], act: &mut Option<Act>) {
    if !section(ui, tab, "HOSTING", icons::PLUGS, offers.len(), offers.len()) {
        return;
    }
    for offer in offers {
        let text = format!("Connect {}…", offer.host);
        let response = leaf(ui, 0, false, icons::PLUG, &text, theme::text(), "");
        if response.clicked() {
            *act = Some(Act::Connect(offer.clone()));
        }
        response.context_menu(|ui| {
            ui.set_min_width(200.0);
            if menu_item(ui, icons::PLUGS_CONNECTED, &text) {
                *act = Some(Act::Connect(offer.clone()));
            }
            if menu_item(ui, icons::EYE_SLASH, "Don't offer again") {
                *act = Some(Act::Choose(offer.host.clone(), ServerKind::Ignored));
            }
        });
        response.on_hover_ui(|ui| {
            ui.set_max_width(320.0);
            ui.label(format!(
                "{} is on {}. Say what it runs (GitLab, GitHub Enterprise, Gitea, Forgejo or Bitbucket \
                 Data Center) to see its requests, issues and CI here. Nothing is sent to {} until then.",
                offer.path, offer.host, offer.host
            ));
        });
    }
}

/// The dialog that asks what runs on the server of `offer`, with the guess
/// its name suggests already chosen.
fn connect_dialog(offer: &Offer) -> Dialog {
    Dialog::ConnectServer { host: offer.host.clone(), path: offer.path.clone(), choice: offer.guess }
}

/// Entries for a remote's context menu in the REMOTE section: connect the
/// self-hosted server it points at, or disconnect it again.
pub fn remote_menu(ui: &mut Ui, shared: &mut Shared, remote: &RemoteInfo) {
    if !shared.settings.hosting_integration {
        return;
    }
    // A server installed under a path is known by that path, not the host.
    let rooted = hosting::parse_remote(&remote.url, &shared.settings.hosting_servers)
        .filter(|repo| repo.host.contains('/'));
    if let Some(repo) = rooted {
        ui.separator();
        let label = format!("Disconnect {} ({})", repo.host, repo.service.name());
        if menu_item(ui, icons::LINK_BREAK, &label) {
            shared.settings.hosting_servers.remove(&repo.host);
        }
        return;
    }
    let Some(offer) = hosting::offer_for(remote) else { return };
    let connected = shared.settings.hosting_servers.get(&offer.host).and_then(|kind| kind.service());
    ui.separator();
    match connected {
        Some(service) => {
            let label = format!("Disconnect {} ({})", offer.host, service.name());
            if menu_item(ui, icons::LINK_BREAK, &label) {
                shared.settings.hosting_servers.remove(&offer.host);
            }
        }
        None => {
            if menu_item(ui, icons::PLUGS_CONNECTED, &format!("Connect {}…", offer.host)) {
                shared.show(connect_dialog(&offer));
            }
        }
    }
}

/// The body of [`Dialog::ConnectServer`]. Returns true when it should close.
pub fn connect_body(ui: &mut Ui, shared: &mut Shared, host: &str, path: &str, choice: &mut Option<ServerKind>) -> bool {
    dialogs::title(ui, &format!("Connect {host}"));
    // Where the server is kept: with the path it is installed under, for a
    // Bitbucket Data Center whose HTTPS remote shows one.
    let key = |kind: ServerKind| hosting::server_key(kind, host, path);
    // Opened from Preferences > Integrations, it is about no repository in
    // particular.
    let path = if path.is_empty() { "its repositories" } else { path };
    ui.label(
        RichText::new(format!(
            "What runs on {host}? Once you say, the sidebar shows the requests, issues and CI \
             of {path} from there. Nothing is sent to {host} before."
        ))
        .color(theme::text_dim()),
    );
    ui.add_space(2.0);
    let options = [
        (ServerKind::GitLab, icons::GITLAB_LOGO, "GitLab"),
        (ServerKind::GitHubEnterprise, icons::GITHUB_LOGO, "GitHub Enterprise"),
        (ServerKind::Gitea, icons::COFFEE, "Gitea"),
        (ServerKind::Forgejo, icons::HAMMER, "Forgejo"),
        (ServerKind::BitbucketDataCenter, icons::PAINT_BUCKET, "Bitbucket Data Center"),
        (ServerKind::Ignored, icons::PROHIBIT, "None of these: don't offer again"),
    ];
    for (kind, icon, label) in options {
        // GitHub Enterprise is never installed under a path.
        if host.contains('/') && kind == ServerKind::GitHubEnterprise {
            continue;
        }
        let selected = *choice == Some(kind);
        let color = if selected { theme::text_bright() } else { theme::text() };
        let text = RichText::new(format!("{icon}   {label}")).font(theme::font(13.5)).color(color);
        let option = Button::selectable(selected, text)
            .frame_when_inactive(true)
            .min_size(vec2(ui.available_width(), 30.0));
        if ui.add(option).clicked() {
            *choice = Some(kind);
        }
    }

    // A path longer than a project's may start with the one GitLab is
    // installed under, which only the user can tell.
    let rooted = !host.contains('/') && path.split('/').count() > 2;
    let explanation = match *choice {
        Some(ServerKind::GitLab) => format!(
            "Requests go to {} only. A token is used if one is found for this server: \
             GITLAB_TOKEN when GITLAB_HOST names it, glab's sign-in, or Git's credential \
             manager.{}",
            hosting::api_base(Service::GitLab, host),
            match rooted {
                true => format!(
                    " If GitLab is installed under a path ({host}/gitlab), add it with that path in \
                     Preferences > Integrations instead."
                ),
                false => String::new(),
            }
        ),
        Some(ServerKind::GitHubEnterprise) => format!(
            "Requests go to {} only. A token is used if one is found for this server: \
             GH_ENTERPRISE_TOKEN, gh's sign-in, or Git's credential manager. github.com \
             tokens are never sent to it.",
            hosting::api_base(Service::GitHubEnterprise, host)
        ),
        Some(ServerKind::Gitea) => format!(
            "Requests go to {} only. A token is used if one is found for this server: \
             GITEA_TOKEN when GITEA_HOST names it, tea's sign-in, or Git's credential manager.",
            hosting::api_base(Service::Gitea, host)
        ),
        Some(ServerKind::Forgejo) => format!(
            "Requests go to {} only. A token is used if one is found for this server: \
             FORGEJO_TOKEN when FORGEJO_HOST names it, tea's sign-in, or Git's credential manager.",
            hosting::api_base(Service::Forgejo, host)
        ),
        // Kept with the path it is installed under when the remote shows one.
        Some(ServerKind::BitbucketDataCenter) => format!(
            "Requests go to {} only. A token is used if one is found for this server: \
             BITBUCKET_SERVER_TOKEN when BITBUCKET_SERVER_HOST names it, or Git's credential manager.",
            hosting::api_base(Service::BitbucketDataCenter, &key(ServerKind::BitbucketDataCenter))
        ),
        Some(ServerKind::Ignored) => format!(
            "{host} is not contacted, and not offered again. Right-click the remote to \
             connect it later."
        ),
        None => "Choose one to see where requests would go and which sign-in would be used.".to_owned(),
    };
    ui.add_space(2.0);
    // Room for the longest explanation, so the dialog (centred on screen)
    // does not move under the pointer when the choice changes.
    ui.scope(|ui| {
        ui.set_min_height(50.0);
        ui.label(RichText::new(explanation).font(theme::font(12.0)).color(theme::text_dim()));
    });

    let button = if *choice == Some(ServerKind::Ignored) { "Save" } else { "Connect" };
    match dialogs::buttons(ui, button, choice.is_some(), false) {
        Some(true) => {
            if let Some(kind) = *choice {
                shared.settings.hosting_servers.insert(key(kind), kind);
            }
            true
        }
        Some(false) => true,
        None => false,
    }
}
