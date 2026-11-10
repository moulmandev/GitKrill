//! Left panel: local branches, remotes, stashes, tags and submodules.

use std::sync::Arc;

use eframe::egui::{self, Align2, Rect, Response, ScrollArea, Sense, TextEdit, Ui, pos2, vec2};

use crate::git::{BranchInfo, RemoteBranch, RepoSnapshot, ops};
use crate::state::{Dialog, RepoTab, Shared};
use crate::theme::{self, icons};
use crate::ui::graph::drag::{self, Branch, Target};
use crate::ui::widgets::{elided, menu_item};

const ROW_H: f32 = 24.0;
const INDENT: f32 = 14.0;

pub fn show(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared) {
    drag::overlay(ui, tab, shared);
    let Some(snap) = tab.snap.clone() else { return };

    egui::Frame::new().inner_margin(egui::Margin::symmetric(8, 8)).show(ui, |ui| {
        ui.add(
            TextEdit::singleline(&mut tab.sidebar_filter)
                .hint_text(format!("{}  Filter branches and tags", icons::MAGNIFYING_GLASS))
                .desired_width(f32::INFINITY),
        );
    });

    let filter = tab.sidebar_filter.trim().to_lowercase();
    ui.spacing_mut().item_spacing.y = 0.0;

    ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        let matches = |name: &str| filter.is_empty() || name.to_lowercase().contains(&filter);
        // A branch being dragged, from here or from the graph: branch rows
        // and remotes are where it can be dropped.
        let session = drag::session(ui.ctx());
        let session = session.as_ref();

        // ---- local branches ----
        let locals: Vec<(String, usize)> = snap
            .branches
            .iter()
            .enumerate()
            .filter(|(_, b)| matches(&b.name))
            .map(|(i, b)| (b.name.clone(), i))
            .collect();
        if section(ui, tab, "LOCAL", icons::LAPTOP, locals.len(), snap.branches.len()) {
            tree(ui, tab, "local", &locals, 0, !filter.is_empty(), &mut |ui, tab, index, name, depth| {
                let branch = &snap.branches[index];
                let response = local_row(ui, tab, shared, &snap, branch, name, depth);
                drag::source(&response, || Some(Branch::local(&branch.name)), theme::accent());
                landing(ui, tab, &snap, session, &response, || Target::Branch(Branch::local(&branch.name)));
            });
        }

        // ---- remotes ----
        let remote_total = snap.remote_branches.len();
        let remote_shown = snap.remote_branches.iter().filter(|b| matches(&b.full)).count();
        if section(ui, tab, "REMOTE", icons::CLOUD, remote_shown, remote_total) {
            for remote in &snap.remotes {
                let key = format!("remote:{}", remote.name);
                let collapsed = tab.collapsed.contains(&key);
                let response = row(ui, 0, false, |painter, rect, x| {
                    let caret = if collapsed { icons::CARET_RIGHT } else { icons::CARET_DOWN };
                    painter.text(pos2(x, rect.center().y), Align2::LEFT_CENTER, caret, theme::font(11.0), theme::text_dim());
                    painter.text(pos2(x + 16.0, rect.center().y), Align2::LEFT_CENTER, icons::GLOBE, theme::font(14.0), theme::text_dim());
                    let galley = elided(painter, &remote.name, theme::bold(13.0), theme::text(), rect.right() - x - 44.0);
                    painter.galley(pos2(x + 36.0, rect.center().y - galley.size().y / 2.0), galley, theme::text());
                });
                if response.clicked() {
                    toggle(tab, &key);
                }
                // Dropping a local branch on a remote pushes it there.
                landing(ui, tab, &snap, session, &response, || Target::Remote(remote.name.clone()));
                let response = response.on_hover_text(&remote.url);
                response.context_menu(|ui| {
                    if menu_item(ui, icons::ARROWS_CLOCKWISE, &format!("Fetch {}", remote.name)) {
                        tab.run(ops::GitCmd::new(
                            format!("Fetch {}", remote.name),
                            &["fetch", "--prune", &remote.name],
                        ));
                    }
                    if menu_item(ui, icons::COPY, "Copy URL") {
                        ui.ctx().copy_text(remote.url.clone());
                    }
                    crate::ui::hosting::remote_menu(ui, shared, remote);
                });
                if collapsed && filter.is_empty() {
                    continue;
                }

                let items: Vec<(String, usize)> = snap
                    .remote_branches
                    .iter()
                    .enumerate()
                    .filter(|(_, b)| b.remote == remote.name && matches(&b.full))
                    .map(|(i, b)| (b.name.clone(), i))
                    .collect();
                tree(ui, tab, &key, &items, 1, !filter.is_empty(), &mut |ui, tab, index, name, depth| {
                    let branch = &snap.remote_branches[index];
                    let response = remote_row(ui, tab, shared, &snap, branch, name, depth);
                    drag::source(&response, || Some(Branch::remote(branch)), theme::accent());
                    landing(ui, tab, &snap, session, &response, || Target::Branch(Branch::remote(branch)));
                });
            }
        }

        // ---- stashes ----
        let stashes: Vec<_> = snap.stashes.iter().filter(|s| matches(&s.message)).collect();
        if section(ui, tab, "STASHES", icons::ARCHIVE, stashes.len(), snap.stashes.len()) {
            for stash in stashes {
                let response = leaf(ui, 0, false, icons::ARCHIVE, &stash.message, theme::text(), "");
                if response.clicked() {
                    tab.jump_to(stash.oid);
                }
                response.context_menu(|ui| stash_menu(ui, tab, shared, stash.index));
            }
        }

        // ---- tags ----
        let tags: Vec<_> = snap.tags.iter().filter(|t| matches(&t.name)).collect();
        if section(ui, tab, "TAGS", icons::TAG, tags.len(), snap.tags.len()) {
            for tag in tags {
                let response = leaf(ui, 0, false, icons::TAG, &tag.name, theme::text(), "");
                if response.clicked() {
                    tab.jump_to(tag.oid);
                }
                response.context_menu(|ui| {
                    ui.set_min_width(190.0);
                    if menu_item(ui, icons::GIT_COMMIT, "Checkout this tag") {
                        tab.run(ops::checkout(&tag.name));
                    }
                    if let Some(remote) = snap.default_remote()
                        && menu_item(ui, icons::ARROW_LINE_UP, &format!("Push to {remote}")) {
                            tab.run(ops::push_tag(remote, &tag.name));
                        }
                    ui.separator();
                    if menu_item(ui, icons::TRASH, &format!("Delete {}", tag.name)) {
                        shared.show(Dialog::confirm(
                            "Delete tag",
                            format!("Delete the tag \"{}\" from this repository?", tag.name),
                            "Delete",
                            ops::delete_tag(&tag.name),
                        ));
                    }
                    ui.separator();
                    if menu_item(ui, icons::COPY, "Copy tag name") {
                        ui.ctx().copy_text(tag.name.clone());
                    }
                });
            }
        }

        // ---- submodules ----
        let total = snap.submodules.len();
        if section(ui, tab, "SUBMODULES", icons::STACK, total, total) {
            for sub in &snap.submodules {
                leaf(ui, 0, false, icons::PACKAGE, &sub.name, theme::text(), "").on_hover_text(&sub.path);
            }
        }

        // Pull requests, issues and CI runs, when the remote is a known host.
        crate::ui::hosting::sections(ui, tab, shared, &snap);

        ui.add_space(12.0);
    });
}

/// Lets the branch being dragged (if any) be dropped on a row: the row is
/// outlined while the pointer is on it and the pair has something to offer,
/// and releasing there opens the menu of actions.
fn landing(
    ui: &Ui,
    tab: &RepoTab,
    snap: &RepoSnapshot,
    session: Option<&drag::Session>,
    response: &Response,
    target: impl FnOnce() -> Target,
) {
    let Some(session) = session else { return };
    if !response.contains_pointer() {
        return;
    }
    let target = target();
    if session.accepts(snap, &target) {
        drag::highlight_row(ui.painter(), response.rect);
        session.over(ui.ctx(), tab, snap, target);
    }
}

pub fn toggle(tab: &mut RepoTab, key: &str) {
    if !tab.collapsed.remove(key) {
        tab.collapsed.insert(key.to_owned());
    }
}

/// Collapsible section header. Returns whether the section is open.
pub fn section(ui: &mut Ui, tab: &mut RepoTab, title: &str, icon: &str, shown: usize, total: usize) -> bool {
    section_header(ui, tab, title, icon, shown, total).0
}

/// [`section`], also returning the header's response (for a context menu).
pub fn section_header(
    ui: &mut Ui,
    tab: &mut RepoTab,
    title: &str,
    icon: &str,
    shown: usize,
    total: usize,
) -> (bool, Response) {
    let key = format!("section:{title}");
    let open = !tab.collapsed.contains(&key);
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.rect_filled(rect, 0.0, if response.hovered() { theme::bg_hover() } else { theme::bg_panel() });
        painter.hline(rect.x_range(), rect.top(), egui::Stroke::new(1.0, theme::border()));
        let y = rect.center().y;
        let caret = if open { icons::CARET_DOWN } else { icons::CARET_RIGHT };
        painter.text(pos2(rect.left() + 9.0, y), Align2::LEFT_CENTER, caret, theme::font(11.0), theme::text_dim());
        painter.text(pos2(rect.left() + 26.0, y), Align2::LEFT_CENTER, icon, theme::font(15.0), theme::text_dim());
        let count = if shown == total { total.to_string() } else { format!("{shown}/{total}") };
        let count_rect =
            painter.text(pos2(rect.right() - 12.0, y), Align2::RIGHT_CENTER, count, theme::bold(11.5), theme::accent());
        // The title gives way to the count when the sidebar is narrow.
        let title_x = rect.left() + 48.0;
        let galley = elided(painter, title, theme::bold(11.5), theme::text(), count_rect.left() - 8.0 - title_x);
        painter.galley(pos2(title_x, (y - galley.size().y / 2.0).round()), galley, theme::text());
    }
    if response.clicked() {
        toggle(tab, &key);
    }
    (open, response)
}

/// Allocates a full-width row and lets `paint` draw inside it. `x` is the
/// left edge after indentation.
pub fn row(
    ui: &mut Ui,
    depth: usize,
    highlighted: bool,
    paint: impl FnOnce(&egui::Painter, Rect, f32),
) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        crate::ui::widgets::row_background(painter, rect, highlighted, response.hovered());
        paint(painter, rect, rect.left() + 12.0 + depth as f32 * INDENT);
    }
    response
}

/// A plain icon + name row with optional right-aligned text.
pub fn leaf(
    ui: &mut Ui,
    depth: usize,
    highlighted: bool,
    icon: &str,
    name: &str,
    color: egui::Color32,
    trailing: &str,
) -> Response {
    row(ui, depth, highlighted, |painter, rect, x| {
        let y = rect.center().y;
        let icon_color = if highlighted { theme::text_bright() } else { theme::text_dim() };
        painter.text(pos2(x, y), Align2::LEFT_CENTER, icon, theme::font(14.0), icon_color);
        let mut right = rect.right() - 10.0;
        if !trailing.is_empty() {
            let used = painter.text(pos2(right, y), Align2::RIGHT_CENTER, trailing, theme::font(11.0), theme::text_dim());
            right = used.left() - 6.0;
        }
        let font = if highlighted { theme::bold(13.0) } else { theme::font(13.0) };
        let galley = elided(painter, name, font, color, right - (x + 20.0));
        painter.galley(pos2(x + 20.0, (y - galley.size().y / 2.0).round()), galley, color);
    })
}

/// Renders `items` (slash-separated path, payload index) as a folder tree.
fn tree(
    ui: &mut Ui,
    tab: &mut RepoTab,
    key_prefix: &str,
    items: &[(String, usize)],
    base_depth: usize,
    force_open: bool,
    leaf_ui: &mut dyn FnMut(&mut Ui, &mut RepoTab, usize, &str, usize),
) {
    let mut sorted: Vec<&(String, usize)> = items.iter().collect();
    sorted.sort_by_key(|a| a.0.to_lowercase());

    let mut stack: Vec<&str> = Vec::new();
    // Depth of the outermost collapsed folder we are currently inside.
    let mut hidden_from: Option<usize> = None;

    for (path, index) in sorted {
        let mut parts: Vec<&str> = path.split('/').collect();
        let name = parts.pop().unwrap_or(path);

        let common = stack.iter().zip(&parts).take_while(|(a, b)| a == b).count();
        stack.truncate(common);
        if hidden_from.is_some_and(|d| d >= common) {
            hidden_from = None;
        }

        for (depth, part) in parts.iter().enumerate().skip(common) {
            stack.push(part);
            if hidden_from.is_some() {
                continue;
            }
            let key = format!("{key_prefix}/{}", stack.join("/"));
            let collapsed = !force_open && tab.collapsed.contains(&key);
            let response = row(ui, base_depth + depth, false, |painter, rect, x| {
                let y = rect.center().y;
                let icon = if collapsed { icons::FOLDER } else { icons::FOLDER_OPEN };
                painter.text(pos2(x, y), Align2::LEFT_CENTER, icon, theme::font(14.0), theme::text_dim());
                let galley = elided(painter, part, theme::font(13.0), theme::text_dim(), rect.right() - x - 30.0);
                painter.galley(pos2(x + 20.0, (y - galley.size().y / 2.0).round()), galley, theme::text_dim());
            });
            if response.clicked() {
                toggle(tab, &key);
            }
            if collapsed {
                hidden_from = Some(depth);
            }
        }

        if hidden_from.is_none() {
            leaf_ui(ui, tab, *index, name, base_depth + parts.len());
        }
    }
}

fn local_row(
    ui: &mut Ui,
    tab: &mut RepoTab,
    shared: &mut Shared,
    snap: &Arc<RepoSnapshot>,
    branch: &BranchInfo,
    name: &str,
    depth: usize,
) -> Response {
    let mut trailing = String::new();
    if branch.ahead > 0 {
        trailing.push_str(&format!("{}{}", branch.ahead, icons::ARROW_UP));
    }
    if branch.behind > 0 {
        if !trailing.is_empty() {
            trailing.push(' ');
        }
        trailing.push_str(&format!("{}{}", branch.behind, icons::ARROW_DOWN));
    }
    let (icon, color) = if branch.is_head {
        (icons::CHECK, theme::text_bright())
    } else {
        (icons::GIT_BRANCH, theme::text())
    };

    // Branch rows can also be dragged; a click is still a click.
    let response = leaf(ui, depth, branch.is_head, icon, name, color, &trailing).interact(Sense::drag());
    if response.double_clicked() && !branch.is_head {
        tab.run(ops::checkout(&branch.name));
    } else if response.clicked()
        && let Some(oid) = branch.oid {
            tab.jump_to(oid);
        }
    let response = match &branch.upstream {
        Some(upstream) => response.on_hover_text(format!("{}  {}  {upstream}", branch.name, icons::ARROW_RIGHT)),
        None => response.on_hover_text(format!("{} (no upstream)", branch.name)),
    };
    response.context_menu(|ui| local_branch_menu(ui, tab, shared, snap, branch));
    response
}

/// Shared by the sidebar and the graph's ref labels.
pub fn local_branch_menu(
    ui: &mut Ui,
    tab: &mut RepoTab,
    shared: &mut Shared,
    snap: &RepoSnapshot,
    branch: &BranchInfo,
) {
    ui.set_min_width(220.0);
    let current = snap.head.branch.clone();

    if !branch.is_head {
        if menu_item(ui, icons::CHECK, &format!("Checkout {}", branch.name)) {
            tab.run(ops::checkout(&branch.name));
        }
        if let Some(current) = &current {
            ui.separator();
            if menu_item(ui, icons::GIT_MERGE, &format!("Merge {} into {current}", branch.name)) {
                tab.run(ops::merge(&branch.name));
            }
            if menu_item(ui, icons::GIT_FORK, &format!("Rebase {current} onto {}", branch.name)) {
                tab.run(ops::rebase(&branch.name));
            }
        }
        ui.separator();
    }

    if let Some(remote) = snap.default_remote()
        && menu_item(ui, icons::ARROW_LINE_UP, &format!("Push {} to {remote}", branch.name)) {
            tab.run(ops::push_branch(remote, &branch.name));
        }
    if let Some(word) = tab.hosting.connected().map(|shown| shown.repo.service.pull_word())
        && menu_item(ui, icons::GIT_PULL_REQUEST, &format!("Create {word}…"))
    {
        crate::ui::new_pull::open(tab, shared, Some(Branch::local(&branch.name)));
    }
    if menu_item(ui, icons::GIT_BRANCH, "Create branch here") {
        shared.show(Dialog::CreateBranch {
            at: Some(branch.name.clone()),
            at_label: branch.name.clone(),
            name: String::new(),
            checkout: true,
        });
    }
    if menu_item(ui, icons::PENCIL_SIMPLE, &format!("Rename {}", branch.name)) {
        shared.show(Dialog::RenameBranch { old: branch.name.clone(), name: branch.name.clone() });
    }
    if !branch.is_head && menu_item(ui, icons::TRASH, &format!("Delete {}", branch.name)) {
        shared.show(Dialog::confirm(
            "Delete branch",
            format!(
                "Delete the local branch \"{}\"? Commits only reachable from it will no longer \
                 appear in the graph.",
                branch.name
            ),
            "Delete",
            ops::delete_branch(&branch.name, true),
        ));
    }
    crate::ui::gitflow::branch_menu(ui, tab, shared, snap, branch);
    ui.separator();
    if menu_item(ui, icons::COPY, "Copy branch name") {
        ui.ctx().copy_text(branch.name.clone());
    }
}

fn remote_row(
    ui: &mut Ui,
    tab: &mut RepoTab,
    shared: &mut Shared,
    snap: &Arc<RepoSnapshot>,
    branch: &RemoteBranch,
    name: &str,
    depth: usize,
) -> Response {
    let response = leaf(ui, depth, false, icons::GIT_BRANCH, name, theme::text(), "").interact(Sense::drag());
    if response.double_clicked() {
        tab.run(checkout_remote(snap, branch));
    } else if response.clicked() {
        tab.jump_to(branch.oid);
    }
    let response = response.on_hover_text(&branch.full);
    response.context_menu(|ui| remote_branch_menu(ui, tab, shared, snap, branch));
    response
}

pub fn checkout_remote(snap: &RepoSnapshot, branch: &RemoteBranch) -> ops::GitCmd {
    let exists = snap.branches.iter().any(|b| b.name == branch.name);
    ops::checkout_remote(&branch.full, &branch.name, exists)
}

pub fn remote_branch_menu(
    ui: &mut Ui,
    tab: &mut RepoTab,
    shared: &mut Shared,
    snap: &RepoSnapshot,
    branch: &RemoteBranch,
) {
    ui.set_min_width(220.0);
    if menu_item(ui, icons::CHECK, &format!("Checkout {}", branch.full)) {
        tab.run(checkout_remote(snap, branch));
    }
    if let Some(current) = &snap.head.branch {
        ui.separator();
        if menu_item(ui, icons::GIT_MERGE, &format!("Merge {} into {current}", branch.full)) {
            tab.run(ops::merge(&branch.full));
        }
        if menu_item(ui, icons::GIT_FORK, &format!("Rebase {current} onto {}", branch.full)) {
            tab.run(ops::rebase(&branch.full));
        }
        if snap.current_branch().is_some_and(|b| b.upstream.as_deref() != Some(&branch.full))
            && menu_item(ui, icons::CLOUD, &format!("Set as upstream of {current}")) {
                tab.run(ops::set_upstream(current, &branch.full));
            }
    }
    // Only for the hosted remote: a fork's branch would need its owner too.
    if let Some(shown) = tab.hosting.connected().filter(|shown| shown.remote == branch.remote) {
        ui.separator();
        if menu_item(ui, icons::GIT_PULL_REQUEST, &format!("Create {}…", shown.repo.service.pull_word())) {
            crate::ui::new_pull::open(tab, shared, Some(Branch::remote(branch)));
        }
    }
    ui.separator();
    if menu_item(ui, icons::TRASH, &format!("Delete {}", branch.full)) {
        shared.show(Dialog::confirm(
            "Delete remote branch",
            format!(
                "Delete \"{}\" on the remote \"{}\"? This affects everyone using that remote.",
                branch.name, branch.remote
            ),
            "Delete",
            ops::delete_remote_branch(&branch.remote, &branch.name),
        ));
    }
    ui.separator();
    if menu_item(ui, icons::COPY, "Copy branch name") {
        ui.ctx().copy_text(branch.full.clone());
    }
}

pub fn stash_menu(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, index: usize) {
    ui.set_min_width(170.0);
    if menu_item(ui, icons::TRAY_ARROW_UP, "Apply stash") {
        tab.run(ops::stash_apply(index));
    }
    if menu_item(ui, icons::TRAY_ARROW_UP, "Pop stash") {
        tab.run(ops::stash_pop(index));
    }
    ui.separator();
    if menu_item(ui, icons::TRASH, "Delete stash") {
        shared.show(Dialog::confirm(
            "Delete stash",
            "Delete this stash? Its changes cannot be recovered from GitKrill afterwards.",
            "Delete",
            ops::stash_drop(index),
        ));
    }
}
