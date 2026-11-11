//! Left panel: local branches, remotes, stashes, tags and submodules.

use std::sync::Arc;

use eframe::egui::{self, Align2, Rect, Response, ScrollArea, Sense, TextEdit, Ui, pos2, vec2};

use crate::git::hidden::{self, HEADS, REMOTES, TAGS};
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
        let (open, header) = section_header(ui, tab, "LOCAL", icons::LAPTOP, locals.len(), snap.branches.len());
        show_all_menu(&header, tab, shared, HEADS, "Show all local branches in graph");
        if open {
            let change = tree(ui, tab, "local", HEADS, &locals, 0, !filter.is_empty(), &mut |ui, tab, index, name, depth| {
                let branch = &snap.branches[index];
                let response = local_row(ui, tab, shared, &snap, branch, name, depth);
                drag::source(&response, || Some(Branch::local(&branch.name)), theme::accent());
                landing(ui, tab, &snap, session, &response, || Target::Branch(Branch::local(&branch.name)));
            });
            match change {
                Some(FolderAction::Hide(path, hide)) => tab.set_hidden(&mut shared.settings, &path, hide),
                Some(FolderAction::Delete(folder)) => crate::ui::branch_folder::delete_local(tab, shared, &folder),
                None => {}
            }
        }

        // ---- remotes ----
        let remote_total = snap.remote_branches.len();
        let remote_shown = snap.remote_branches.iter().filter(|b| matches(&b.full)).count();
        let (open, header) = section_header(ui, tab, "REMOTE", icons::CLOUD, remote_shown, remote_total);
        show_all_menu(&header, tab, shared, REMOTES, "Show all remote branches in graph");
        crate::ui::remotes::header_button(ui, tab, shared, &header, (remote_shown, remote_total));
        if open {
            if snap.remotes.is_empty() {
                crate::ui::remotes::add_row(ui, tab, shared);
            }
            for remote in &snap.remotes {
                let key = format!("remote:{}", remote.name);
                let collapsed = tab.collapsed.contains(&key);
                let path = hidden::folder(REMOTES, &remote.name);
                let is_hidden = tab.hidden.hides(&path);
                let color = if is_hidden { theme::text_faint() } else { theme::text() };
                let (response, eye) = eye_row(ui, 0, false, Some(is_hidden), |painter, rect, x| {
                    let caret = if collapsed { icons::CARET_RIGHT } else { icons::CARET_DOWN };
                    painter.text(pos2(x, rect.center().y), Align2::LEFT_CENTER, caret, theme::font(11.0), theme::text_dim());
                    painter.text(pos2(x + 16.0, rect.center().y), Align2::LEFT_CENTER, icons::GLOBE, theme::font(14.0), theme::text_dim());
                    let galley = elided(painter, &remote.name, theme::bold(13.0), color, rect.right() - x - 44.0);
                    painter.galley(pos2(x + 36.0, rect.center().y - galley.size().y / 2.0), galley, color);
                });
                if eye {
                    tab.set_hidden(&mut shared.settings, &path, !is_hidden);
                } else if response.clicked() {
                    toggle(tab, &key);
                }
                // Dropping a local branch on a remote pushes it there.
                landing(ui, tab, &snap, session, &response, || Target::Remote(remote.name.clone()));
                let response = response.on_hover_text(&remote.url);
                response.context_menu(|ui| {
                    crate::ui::remotes::menu(ui, tab, shared, remote);
                    crate::ui::hosting::remote_menu(ui, shared, remote);
                    ui.separator();
                    if hide_item(ui, is_hidden, &format!("{} in graph", remote.name)) {
                        tab.set_hidden(&mut shared.settings, &path, !is_hidden);
                    }
                    crate::ui::remotes::remove_item(ui, tab, shared, remote);
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
                let change = tree(ui, tab, &key, &path, &items, 1, !filter.is_empty(), &mut |ui, tab, index, name, depth| {
                    let branch = &snap.remote_branches[index];
                    let response = remote_row(ui, tab, shared, &snap, branch, name, depth);
                    drag::source(&response, || Some(Branch::remote(branch)), theme::accent());
                    landing(ui, tab, &snap, session, &response, || Target::Branch(Branch::remote(branch)));
                });
                match change {
                    Some(FolderAction::Hide(path, hide)) => tab.set_hidden(&mut shared.settings, &path, hide),
                    Some(FolderAction::Delete(folder)) => {
                        crate::ui::branch_folder::delete_remote(tab, shared, &remote.name, &folder);
                    }
                    None => {}
                }
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
        let (open, header) = section_header(ui, tab, "TAGS", icons::TAG, tags.len(), snap.tags.len());
        header.context_menu(|ui| {
            ui.set_min_width(200.0);
            let all_hidden = tab.hidden.hides(TAGS);
            if hide_item(ui, all_hidden, "all tags in graph") {
                tab.set_hidden(&mut shared.settings, TAGS, !all_hidden);
            }
        });
        if open {
            for tag in tags {
                let path = hidden::tag(&tag.name);
                let is_hidden = tab.hidden.hides(&path);
                let color = if is_hidden { theme::text_faint() } else { theme::text() };
                let (response, eye) = eye_row(ui, 0, false, Some(is_hidden), |painter, rect, x| {
                    paint_leaf(painter, rect, x, false, icons::TAG, &tag.name, color, "");
                });
                if eye {
                    tab.set_hidden(&mut shared.settings, &path, !is_hidden);
                } else if response.clicked() {
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
                    crate::ui::multi::compare_item(ui, tab, shared, &snap, Some(tag.oid));
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
                    if hide_item(ui, is_hidden, "in graph") {
                        tab.set_hidden(&mut shared.settings, &path, !is_hidden);
                    }
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
    eye_row(ui, depth, highlighted, None, paint).0
}

/// [`row`] for something that can be hidden from the graph, `hidden` saying
/// whether it is (`None`: it cannot). An eye button at the right end shows
/// while the pointer is on the row, and stays, crossed out, once hidden.
/// `paint` gets the rect left of it. The bool is true when the eye was
/// clicked; the row's own click is not reported then.
fn eye_row(
    ui: &mut Ui,
    depth: usize,
    highlighted: bool,
    hidden: Option<bool>,
    paint: impl FnOnce(&egui::Painter, Rect, f32),
) -> (Response, bool) {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
    // Not while a branch is being dragged over the row.
    let pointer_on_row = ui.rect_contains_pointer(rect) && ui.ctx().dragged_id().is_none();
    let eye = hidden.filter(|hidden| *hidden || pointer_on_row).map(|hidden| {
        let eye_rect = Rect::from_center_size(pos2(rect.right() - 17.0, rect.center().y), vec2(24.0, ROW_H - 4.0));
        let tip = if hidden { "Show in the graph" } else { "Hide from the graph" };
        (eye_rect, hidden, ui.interact(eye_rect, response.id.with("eye"), Sense::click()).on_hover_text(tip))
    });

    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        // The eye covers part of the row; the row still looks hovered.
        let hovered = response.hovered() || eye.as_ref().is_some_and(|(_, _, r)| r.hovered());
        crate::ui::widgets::row_background(painter, rect, highlighted, hovered);
        let mut content = rect;
        if let Some((eye_rect, hidden, eye_response)) = &eye {
            content.max.x = eye_rect.left();
            if eye_response.hovered() {
                painter.rect_filled(eye_rect.shrink(1.0), 4.0, theme::bg_pressed());
            }
            let (icon, color) = match (hidden, eye_response.hovered()) {
                (_, true) => (if *hidden { icons::EYE } else { icons::EYE_SLASH }, theme::text_bright()),
                (true, false) => (icons::EYE_SLASH, theme::text_faint()),
                (false, false) => (icons::EYE, theme::text_dim()),
            };
            painter.text(eye_rect.center(), Align2::CENTER_CENTER, icon, theme::font(14.0), color);
        }
        paint(painter, content, rect.left() + 12.0 + depth as f32 * INDENT);
    }
    let clicked = eye.is_some_and(|(_, _, r)| r.clicked());
    (response, clicked)
}

/// A menu entry that hides `what` from the graph, or shows it again.
pub fn hide_item(ui: &mut Ui, hidden: bool, what: &str) -> bool {
    if hidden {
        menu_item(ui, icons::EYE, &format!("Show {what}"))
    } else {
        menu_item(ui, icons::EYE_SLASH, &format!("Hide {what}"))
    }
}

/// A section header's menu: show everything under `prefix` again, offered
/// while something there is hidden.
fn show_all_menu(header: &Response, tab: &mut RepoTab, shared: &mut Shared, prefix: &str, label: &str) {
    if !tab.hidden.hides_any(prefix) {
        return;
    }
    header.context_menu(|ui| {
        ui.set_min_width(220.0);
        if menu_item(ui, icons::EYE, label) {
            tab.set_hidden(&mut shared.settings, prefix, false);
        }
    });
}

/// "Hide in graph" for a branch, and for the folder it is in, if any.
/// `prefix` and `name` make its full ref name; `itself` is false for the
/// checked out branch, which the graph always shows.
fn hide_items(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, prefix: &str, name: &str, itself: bool) {
    if itself {
        let path = format!("{prefix}{name}");
        let is_hidden = tab.hidden.hides(&path);
        if hide_item(ui, is_hidden, "in graph") {
            tab.set_hidden(&mut shared.settings, &path, !is_hidden);
        }
    }
    if let Some((folder, _)) = name.rsplit_once('/') {
        let path = hidden::folder(prefix, folder);
        let is_hidden = tab.hidden.hides(&path);
        if hide_item(ui, is_hidden, &format!("folder {folder} in graph")) {
            tab.set_hidden(&mut shared.settings, &path, !is_hidden);
        }
    }
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
    row(ui, depth, highlighted, |painter, rect, x| paint_leaf(painter, rect, x, highlighted, icon, name, color, trailing))
}

/// What [`leaf`] draws, for rows made some other way.
#[allow(clippy::too_many_arguments)]
fn paint_leaf(
    painter: &egui::Painter,
    rect: Rect,
    x: f32,
    highlighted: bool,
    icon: &str,
    name: &str,
    color: egui::Color32,
    trailing: &str,
) {
    let y = rect.center().y;
    // Rows hidden from the graph are faint, icon included.
    let icon_color = if highlighted {
        theme::text_bright()
    } else if color == theme::text_faint() {
        color
    } else {
        theme::text_dim()
    };
    painter.text(pos2(x, y), Align2::LEFT_CENTER, icon, theme::font(14.0), icon_color);
    let mut right = rect.right() - 10.0;
    if !trailing.is_empty() {
        let used = painter.text(pos2(right, y), Align2::RIGHT_CENTER, trailing, theme::font(11.0), theme::text_dim());
        right = used.left() - 6.0;
    }
    let font = if highlighted { theme::bold(13.0) } else { theme::font(13.0) };
    let galley = elided(painter, name, font, color, right - (x + 20.0));
    painter.galley(pos2(x + 20.0, (y - galley.size().y / 2.0).round()), galley, color);
}

/// What a folder's eye or context menu asked for, for the caller of [`tree`]
/// to do.
enum FolderAction {
    /// Hide or show this folder (its full path) in the graph.
    Hide(String, bool),
    /// Delete every branch in this folder (its path within the tree).
    Delete(String),
}

/// Renders `items` (slash-separated path, payload index) as a folder tree.
/// `ref_prefix` followed by an item's path is its full ref name. A folder's
/// eye and menu hide it from the graph or delete its branches; that is
/// returned for the caller to do.
#[allow(clippy::too_many_arguments)]
fn tree(
    ui: &mut Ui,
    tab: &mut RepoTab,
    key_prefix: &str,
    ref_prefix: &str,
    items: &[(String, usize)],
    base_depth: usize,
    force_open: bool,
    leaf_ui: &mut dyn FnMut(&mut Ui, &mut RepoTab, usize, &str, usize),
) -> Option<FolderAction> {
    let mut change = None;
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
            let folder = hidden::folder(ref_prefix, &stack.join("/"));
            let is_hidden = tab.hidden.hides(&folder);
            let color = if is_hidden { theme::text_faint() } else { theme::text_dim() };
            let (response, eye) = eye_row(ui, base_depth + depth, false, Some(is_hidden), |painter, rect, x| {
                let y = rect.center().y;
                let icon = if collapsed { icons::FOLDER } else { icons::FOLDER_OPEN };
                painter.text(pos2(x, y), Align2::LEFT_CENTER, icon, theme::font(14.0), color);
                let galley = elided(painter, part, theme::font(13.0), color, rect.right() - x - 30.0);
                painter.galley(pos2(x + 20.0, (y - galley.size().y / 2.0).round()), galley, color);
            });
            if eye {
                change = Some(FolderAction::Hide(folder.clone(), !is_hidden));
            } else if response.clicked() {
                toggle(tab, &key);
            }
            response.context_menu(|ui| {
                ui.set_min_width(200.0);
                if hide_item(ui, is_hidden, "in graph") {
                    change = Some(FolderAction::Hide(folder.clone(), !is_hidden));
                }
                let (icon, label) = if collapsed { (icons::FOLDER_OPEN, "Expand") } else { (icons::FOLDER, "Collapse") };
                if !force_open && menu_item(ui, icon, label) {
                    toggle(tab, &key);
                }
                ui.separator();
                let path = stack.join("/");
                if menu_item(ui, icons::TRASH, &format!("Delete all branches in {path}…")) {
                    change = Some(FolderAction::Delete(path));
                }
            });
            if collapsed {
                hidden_from = Some(depth);
            }
        }

        if hidden_from.is_none() {
            leaf_ui(ui, tab, *index, name, base_depth + parts.len());
        }
    }
    change
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
    // The graph always shows the checked out branch, so it has no eye
    // (unless another branch is shown alone, `ui::solo`).
    let is_hidden = (!branch.is_head || tab.hidden.hides_head()) && tab.hidden.hides_local(&branch.name);
    let (icon, color) = if branch.is_head {
        (icons::CHECK, if is_hidden { theme::text_faint() } else { theme::text_bright() })
    } else if is_hidden {
        (icons::GIT_BRANCH, theme::text_faint())
    } else {
        (icons::GIT_BRANCH, theme::text())
    };

    let eye = (!branch.is_head).then_some(is_hidden);
    let (response, eye) = eye_row(ui, depth, branch.is_head, eye, |painter, rect, x| {
        paint_leaf(painter, rect, x, branch.is_head, icon, name, color, &trailing);
    });
    // Branch rows can also be dragged; a click is still a click.
    let response = response.interact(Sense::drag());
    if response.hovered() && let Some(oid) = branch.oid {
        tab.ref_hover.hover(ui.ctx(), oid);
    }
    if eye {
        tab.set_hidden(&mut shared.settings, &hidden::local(&branch.name), !is_hidden);
    } else if response.double_clicked() && !branch.is_head {
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
        crate::ui::multi::compare_item(ui, tab, shared, snap, branch.oid);
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
    hide_items(ui, tab, shared, HEADS, &branch.name, !branch.is_head);
    crate::ui::solo::menu_item_for(ui, tab, shared, &hidden::local(&branch.name), &branch.name);
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
    let is_hidden = tab.hidden.hides_remote(&branch.full);
    let color = if is_hidden { theme::text_faint() } else { theme::text() };
    let (response, eye) = eye_row(ui, depth, false, Some(is_hidden), |painter, rect, x| {
        paint_leaf(painter, rect, x, false, icons::GIT_BRANCH, name, color, "");
    });
    let response = response.interact(Sense::drag());
    if response.hovered() {
        tab.ref_hover.hover(ui.ctx(), branch.oid);
    }
    if eye {
        tab.set_hidden(&mut shared.settings, &hidden::remote(&branch.full), !is_hidden);
    } else if response.double_clicked() {
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
    crate::ui::multi::compare_item(ui, tab, shared, snap, Some(branch.oid));
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
    hide_items(ui, tab, shared, &hidden::folder(REMOTES, &branch.remote), &branch.name, true);
    crate::ui::solo::menu_item_for(ui, tab, shared, &hidden::remote(&branch.full), &branch.full);
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
