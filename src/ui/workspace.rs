//! Lists of repositories to open: the workspaces (folders of repositories,
//! `crate::workspace`) and the recently opened ones, with a filter field and
//! each repository's branch and pending changes.
//!
//! The same list appears in three places: the repository picker in the
//! toolbar (opens in the current tab), the new-tab menu behind the `+` at
//! the end of the tab strip, and the welcome page (open in a new tab).

use std::path::{Path, PathBuf};
use std::time::Instant;

use eframe::egui::text::{LayoutJob, TextFormat, TextWrapping};
use eframe::egui::{
    self, Align2, Color32, Id, Key, Modifiers, Popup, PopupCloseBehavior, Rect, Response, ScrollArea,
    Sense, Stroke, TextEdit, Ui, pos2, vec2,
};

use crate::platform;
use crate::state::{AppAction, Shared};
use crate::theme::{self, icons};
use crate::ui::palette::fuzzy;
use crate::ui::widgets::{elided, menu_item, spinner};
pub use crate::workspace::Opens;
use crate::workspace::{self, Summary, Workspace};

const ROW_H: f32 = 30.0;
const HEADER_H: f32 = 28.0;
const MENU_WIDTH: f32 = 500.0;
/// Recently opened repositories listed besides the workspaces.
const RECENT: usize = 8;

/// The filter and keyboard selection of one list, kept between frames.
#[derive(Clone, Default)]
struct ListState {
    query: String,
    selected: usize,
    /// Frames left in which to put the keyboard in the filter field.
    focus: u8,
    /// Bring the selected row into view.
    reveal: bool,
}

struct Row {
    path: PathBuf,
    name: String,
    /// Folders between the workspace root and the repository, or the
    /// parent folder for a recent repository.
    folder: String,
    score: i32,
    /// Characters of `name` that matched the filter.
    positions: Vec<usize>,
}

struct Section {
    /// `None` for the recently opened repositories.
    workspace: Option<Workspace>,
    rows: Vec<Row>,
}

/// Whether [`list`] has anything to show.
pub fn has_rows(shared: &Shared) -> bool {
    !shared.settings.workspaces.is_empty() || !shared.settings.recent.is_empty()
}

/// The repository picker in the toolbar: switches the current tab to the
/// repository chosen.
pub fn repository_menu(response: &Response, shared: &mut Shared, current: &Path) {
    let reveal = shared.workspaces.reveal == Some(Opens::Here);
    let popup_id = Popup::default_response_id(response);
    if reveal {
        shared.workspaces.reveal = None;
        Popup::open_id(&response.ctx, popup_id);
    }
    let reset = reveal || response.clicked();
    menu(response).show(|ui| {
        ui.set_width(MENU_WIDTH);
        let list_id = popup_id.with("list");
        let picked = list(ui, shared, Opens::Here, Some(current), list_id, reset, 420.0);
        ui.separator();
        let mut close = picked;
        if menu_item(ui, icons::FOLDER_OPEN, "Open repository…") {
            shared.actions.push(AppAction::OpenDialog);
            close = true;
        }
        if menu_item(ui, icons::FOLDERS, "Add a folder of repositories…") {
            shared.actions.push(AppAction::AddWorkspace(Opens::Here));
            close = true;
        }
        if menu_item(ui, icons::CLOUD_ARROW_DOWN, "Clone repository…") {
            shared.show(crate::ui::toolbar::clone_dialog());
            close = true;
        }
        if menu_item(ui, icons::FOLDER_SIMPLE, "Show in file manager") {
            platform::reveal(current);
            close = true;
        }
        if close {
            Popup::close_id(ui.ctx(), popup_id);
        }
    });
}

/// The menu behind the `+` at the end of the tab strip: opens the
/// repository chosen in a new tab.
pub fn new_tab_menu(response: &Response, shared: &mut Shared, current: Option<&Path>) {
    let reveal = shared.workspaces.reveal == Some(Opens::NewTab);
    let popup_id = Popup::default_response_id(response);
    if reveal {
        shared.workspaces.reveal = None;
        Popup::open_id(&response.ctx, popup_id);
    }
    let reset = reveal || response.clicked();
    menu(response).show(|ui| {
        ui.set_width(MENU_WIDTH);
        let mut close = false;
        if has_rows(shared) {
            close |= list(ui, shared, Opens::NewTab, current, popup_id.with("list"), reset, 420.0);
            ui.separator();
        }
        if menu_item(ui, icons::FOLDER_OPEN, "Open repository…") {
            shared.actions.push(AppAction::OpenDialog);
            close = true;
        }
        if menu_item(ui, icons::FOLDERS, "Add a folder of repositories…") {
            shared.actions.push(AppAction::AddWorkspace(Opens::NewTab));
            close = true;
        }
        if menu_item(ui, icons::CLOUD_ARROW_DOWN, "Clone repository…") {
            shared.show(crate::ui::toolbar::clone_dialog());
            close = true;
        }
        if menu_item(ui, icons::PLUS, "Init repository…") {
            shared.actions.push(AppAction::InitDialog);
            close = true;
        }
        if close {
            Popup::close_id(ui.ctx(), popup_id);
        }
    });
}

/// A menu that stays open while its filter field and buttons are used.
fn menu(response: &Response) -> Popup<'_> {
    Popup::menu(response).close_behavior(PopupCloseBehavior::CloseOnClickOutside)
}

/// The workspaces and recent repositories, filtered by what is typed in the
/// field above them. Clicking a repository (or Enter on the selected one)
/// opens it as `opens` says; Ctrl+click, a middle click or Ctrl+Enter open it
/// in a new tab. Returns true when one was opened.
///
/// `reset` clears the filter and puts the keyboard in it (the menu holding
/// the list was just opened). `current` is the repository in front, marked
/// with a check.
pub fn list(
    ui: &mut Ui,
    shared: &mut Shared,
    opens: Opens,
    current: Option<&Path>,
    id: Id,
    reset: bool,
    max_height: f32,
) -> bool {
    let ctx = ui.ctx().clone();
    shared.workspaces.shown = Some(Instant::now());
    let mut state: ListState = if reset { ListState::default() } else { ui.data(|d| d.get_temp(id)).unwrap_or_default() };
    if reset {
        state.focus = 2;
    }

    let sections = sections(shared, current, opens, state.query.trim());
    let count: usize = sections.iter().map(|s| s.rows.len()).sum();

    // ---- the filter ----
    let field_id = id.with("filter");
    let mut picked: Option<(PathBuf, bool)> = None;
    // Without the keyboard there is no row for Enter to open.
    let typing = ctx.memory(|m| m.has_focus(field_id));
    if typing {
        let (step, enter, ctrl_enter) = ui.input_mut(|i| {
            let down = i.count_and_consume_key(Modifiers::NONE, Key::ArrowDown) as isize;
            let up = i.count_and_consume_key(Modifiers::NONE, Key::ArrowUp) as isize;
            let ctrl_enter = i.consume_key(Modifiers::COMMAND, Key::Enter);
            (down - up, i.consume_key(Modifiers::NONE, Key::Enter), ctrl_enter)
        });
        if step != 0 && count > 0 {
            state.selected = (state.selected as isize + step).rem_euclid(count as isize) as usize;
            state.reveal = true;
        }
        if (enter || ctrl_enter)
            && let Some(row) = sections.iter().flat_map(|s| &s.rows).nth(state.selected)
        {
            picked = Some((row.path.clone(), ctrl_enter));
        }
    }
    let before = state.query.clone();
    let field = ui.add(
        TextEdit::singleline(&mut state.query)
            .id(field_id)
            .hint_text(format!("{}  Filter repositories", icons::MAGNIFYING_GLASS))
            .desired_width(f32::INFINITY),
    );
    if state.focus > 0 {
        field.request_focus();
        state.focus -= 1;
    }
    if state.query != before {
        state.selected = 0;
        state.reveal = true;
    }
    state.selected = state.selected.min(count.saturating_sub(1));
    ui.add_space(4.0);

    // ---- the rows ----
    let width = ui.available_width();
    let mut index = 0;
    ScrollArea::vertical().id_salt(id.with("scroll")).max_height(max_height).auto_shrink([false, true]).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        if sections.is_empty() {
            let text = if state.query.trim().is_empty() {
                "Add a folder of repositories to have them all listed here."
            } else {
                "No repository matches."
            };
            let (rect, _) = ui.allocate_exact_size(vec2(width, ROW_H), Sense::hover());
            ui.painter().text(pos2(rect.left() + 10.0, rect.center().y), Align2::LEFT_CENTER, text, theme::font(12.5), theme::text_faint());
        }
        for section in &sections {
            header(ui, shared, section, width);
            if let Some(ws) = &section.workspace
                && ws.repos.is_empty()
                && shared.workspaces.is_scanning(&ws.root)
            {
                let (rect, _) = ui.allocate_exact_size(vec2(width, ROW_H), Sense::hover());
                spinner(ui.painter(), pos2(rect.left() + 20.0, rect.center().y), 6.0, theme::accent());
                ui.painter().text(
                    pos2(rect.left() + 36.0, rect.center().y),
                    Align2::LEFT_CENTER,
                    "Looking for repositories…",
                    theme::font(12.5),
                    theme::text_dim(),
                );
                ctx.request_repaint();
            }
            for row in &section.rows {
                let selected = typing && index == state.selected;
                if let Some(choice) = repo_row(ui, shared, row, opens, current, width, selected, selected && state.reveal) {
                    picked = Some((row.path.clone(), choice));
                }
                index += 1;
            }
        }
    });
    state.reveal = false;

    let opened = picked.is_some();
    if let Some((path, new_tab)) = picked {
        let action = if opens == Opens::Here && !new_tab { AppAction::Switch(path) } else { AppAction::Open(path) };
        shared.actions.push(action);
    }
    ui.data_mut(|d| d.insert_temp(id, state));
    opened
}

/// What the list holds for `query`: the workspace of the repository in
/// front first, then the other workspaces, then recent repositories that
/// belong to none.
fn sections(shared: &Shared, current: Option<&Path>, opens: Opens, query: &str) -> Vec<Section> {
    let settings = &shared.settings;
    let mut workspaces: Vec<&Workspace> = settings.workspaces.iter().collect();
    if let Some(current) = current {
        workspaces.sort_by_key(|w| !w.contains(current));
    }

    let mut sections = Vec::new();
    for ws in workspaces {
        let rows = ws.repos.iter().map(|path| (path.clone(), ws.folder_of(path))).collect();
        let rows = filter(rows, query);
        // An empty workspace still shows while it is being scanned.
        if !rows.is_empty() || (query.is_empty() && shared.workspaces.is_scanning(&ws.root)) {
            sections.push(Section { workspace: Some(ws.clone()), rows });
        }
    }

    let in_workspace = |path: &Path| settings.workspaces.iter().any(|w| w.contains(path));
    let recent = settings
        .recent
        .iter()
        // The picker in the toolbar is about other repositories.
        .filter(|path| !(opens == Opens::Here && Some(path.as_path()) == current))
        .filter(|path| !in_workspace(path))
        .take(RECENT)
        .map(|path| {
            let parent = path.parent().map(|p| p.display().to_string()).unwrap_or_default();
            (path.clone(), parent)
        })
        .collect();
    let rows = filter(recent, query);
    if !rows.is_empty() {
        sections.push(Section { workspace: None, rows });
    }
    sections
}

/// The repositories whose name (or, failing that, folder and name) match
/// `query`, best first; all of them in order for an empty query.
fn filter(repos: Vec<(PathBuf, String)>, query: &str) -> Vec<Row> {
    let mut rows: Vec<Row> = repos
        .into_iter()
        .filter_map(|(path, folder)| {
            let name = workspace::display_name(&path);
            let (score, positions) = match fuzzy(query, &name) {
                Some(found) => (found.score, found.positions),
                None => (fuzzy(query, &format!("{folder}/{name}"))?.score - 40, Vec::new()),
            };
            Some(Row { path, name, folder, score, positions })
        })
        .collect();
    if !query.is_empty() {
        // Stable: equal scores keep the folder order.
        rows.sort_by_key(|row| std::cmp::Reverse(row.score));
    }
    rows
}

/// A workspace's name, how many repositories it has, and buttons to scan it
/// again or remove it; "Recent" for the other section.
fn header(ui: &mut Ui, shared: &mut Shared, section: &Section, width: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(width, HEADER_H), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let painter = ui.painter().clone();
    let y = rect.center().y + 2.0;
    let Some(ws) = &section.workspace else {
        painter.text(pos2(rect.left() + 10.0, y), Align2::LEFT_CENTER, "RECENT", theme::bold(11.0), theme::text_dim());
        return;
    };

    let mut x = rect.left() + 8.0;
    x += painter.text(pos2(x, y), Align2::LEFT_CENTER, icons::FOLDERS, theme::font(14.0), theme::text_dim()).width() + 8.0;
    let name = elided(&painter, &ws.name(), theme::bold(12.5), theme::text_bright(), width * 0.45);
    let name_w = name.size().x;
    painter.galley(pos2(x, (y - name.size().y / 2.0).round()), name, theme::text_bright());
    x += name_w + 8.0;
    let count = ws.repos.len().to_string();
    x += painter.text(pos2(x, y), Align2::LEFT_CENTER, &count, theme::font(11.5), theme::text_dim()).width() + 10.0;

    // Buttons on the right: remove, then scan again (a spinner while scanning).
    let remove = Rect::from_center_size(pos2(rect.right() - 16.0, y), vec2(22.0, 22.0));
    let rescan = remove.translate(vec2(-26.0, 0.0));
    let root = ws.root.display().to_string();
    let path = elided(&painter, &root, theme::font(11.0), theme::text_faint(), (rescan.left() - 8.0 - x).max(0.0));
    painter.galley(pos2(x, (y - path.size().y / 2.0).round()), path, theme::text_faint());

    let id = Id::new(("workspace-header", &ws.root));
    if small_button(ui, remove, id.with("remove"), icons::X, "Remove this folder from the list (nothing is deleted)").clicked() {
        workspace::remove(shared, &ws.root);
    }
    if shared.workspaces.is_scanning(&ws.root) {
        spinner(&painter, rescan.center(), 6.0, theme::accent());
        ui.ctx().request_repaint();
    } else if small_button(ui, rescan, id.with("rescan"), icons::ARROWS_CLOCKWISE, "Look for new repositories in this folder").clicked() {
        let ctx = ui.ctx().clone();
        shared.workspaces.rescan(&ctx, &ws.root);
    }
}

/// One repository. Returns `Some(new_tab)` when it was chosen.
#[allow(clippy::too_many_arguments)]
fn repo_row(
    ui: &mut Ui,
    shared: &mut Shared,
    row: &Row,
    opens: Opens,
    current: Option<&Path>,
    width: f32,
    selected: bool,
    reveal: bool,
) -> Option<bool> {
    let (rect, response) = ui.allocate_exact_size(vec2(width, ROW_H), Sense::click());
    if reveal {
        ui.scroll_to_rect_animation(rect, None, egui::style::ScrollAnimation::none());
    }
    if !ui.is_rect_visible(rect) {
        return None;
    }
    let is_current = current == Some(row.path.as_path());
    let is_open = shared.workspaces.is_open(&row.path);
    // A separate tab button only where a plain click would not open one.
    let tab_button = (opens == Opens::Here).then(|| Rect::from_center_size(pos2(rect.right() - 16.0, rect.center().y), vec2(22.0, 22.0)));

    // Over the row or the tab button on it.
    let hovered = ui.rect_contains_pointer(rect);
    let painter = ui.painter().clone();
    // Menus are filled with the usual hover colour: a step lighter, and
    // the command palette's bar for the row Enter would open.
    if selected || hovered {
        painter.rect_filled(rect, 4.0, theme::bg_raised());
    }
    if selected {
        painter.rect_filled(Rect::from_min_max(rect.min, pos2(rect.left() + 3.0, rect.bottom())), 1.5, theme::accent());
    }

    let y = rect.center().y;
    let (icon, icon_color) = if is_current {
        (icons::CHECK, theme::accent())
    } else if is_open {
        (icons::BROWSERS, theme::text_dim())
    } else {
        (icons::GIT_BRANCH, theme::text_faint())
    };
    painter.text(pos2(rect.left() + 12.0, y), Align2::LEFT_CENTER, icon, theme::font(14.0), icon_color);

    // The summary on the right takes what it needs, up to 45% of the row.
    let right_edge = tab_button.map_or(rect.right() - 10.0, |b| b.left() - 6.0);
    let summary = shared.workspaces.summary(ui.ctx(), &row.path);
    let summary = summary_galley(&painter, summary.as_ref(), width * 0.45);
    let summary_w = summary.size().x;
    painter.galley(pos2(right_edge - summary_w, (y - summary.size().y / 2.0).round()), summary, theme::text_dim());

    let x = rect.left() + 34.0;
    let name_color = if is_current { theme::text_bright() } else { theme::text() };
    let text = name_and_folder(row, name_color, (right_edge - summary_w - 12.0 - x).max(0.0));
    let text = painter.layout_job(text);
    painter.galley(pos2(x, (y - text.size().y / 2.0).round()), text, name_color);

    let mut choice = None;
    if let Some(button) = tab_button
        && hovered
        && small_button(ui, button, Id::new(("repo-new-tab", &row.path)), icons::PLUS_SQUARE, "Open in a new tab").clicked()
    {
        choice = Some(true);
    }
    if choice.is_none() {
        if response.middle_clicked() {
            choice = Some(true);
        } else if response.clicked() {
            choice = Some(ui.input(|i| i.modifiers.command));
        }
    }
    choice
}

/// The name, its matched characters in the accent colour, then the folder
/// it is in, dimmer, all cut to `max_width`.
fn name_and_folder(row: &Row, color: Color32, max_width: f32) -> LayoutJob {
    let mut job = LayoutJob::default();
    for (i, ch) in row.name.chars().enumerate() {
        let hit = row.positions.contains(&i);
        let format = TextFormat::simple(theme::bold(13.0), if hit { theme::accent() } else { color });
        job.append(&ch.to_string(), 0.0, format);
    }
    if !row.folder.is_empty() {
        job.append(&row.folder, 10.0, TextFormat::simple(theme::font(11.5), theme::text_dim()));
    }
    job.wrap = TextWrapping { max_width, max_rows: 1, break_anywhere: true, ..Default::default() };
    job
}

/// "branch  ↑1 ↓2  ✎ 3": the branch checked out, how far it is from its
/// upstream, and how many files have changes. Empty while unknown.
fn summary_galley(painter: &egui::Painter, summary: Option<&Result<Summary, String>>, max_width: f32) -> std::sync::Arc<egui::Galley> {
    let mut job = LayoutJob::default();
    let small = |color| TextFormat::simple(theme::font(11.5), color);
    match summary {
        None => {}
        Some(Err(_)) => job.append("cannot be read", 0.0, small(theme::text_faint())),
        Some(Ok(s)) => {
            if s.conflicts {
                job.append(&format!("{} conflicts", icons::WARNING), 0.0, small(theme::orange()));
            } else if s.changes > 0 {
                job.append(&format!("{} {}", icons::PENCIL_SIMPLE, s.changes), 0.0, small(theme::yellow()));
            }
            let mut counts = String::new();
            if s.ahead > 0 {
                counts.push_str(&format!("{}{}", s.ahead, icons::ARROW_UP));
            }
            if s.behind > 0 {
                if !counts.is_empty() {
                    counts.push(' ');
                }
                counts.push_str(&format!("{}{}", s.behind, icons::ARROW_DOWN));
            }
            if !counts.is_empty() {
                job.append(&counts, 10.0, small(theme::text_dim()));
            }
            let icon = if s.detached { icons::GIT_COMMIT } else { icons::GIT_BRANCH };
            job.append(&format!("{icon} {}", s.head), 10.0, small(theme::text_dim()));
        }
    }
    job.wrap = TextWrapping { max_width, max_rows: 1, break_anywhere: true, ..Default::default() };
    painter.layout_job(job)
}

/// A frameless icon button at a given place in a custom-painted row.
fn small_button(ui: &mut Ui, rect: Rect, id: Id, icon: &str, tooltip: &str) -> Response {
    let response = ui.interact(rect, id, Sense::click());
    let painter = ui.painter();
    if response.is_pointer_button_down_on() {
        painter.rect_filled(rect, 4.0, theme::bg_pressed());
    } else if response.hovered() {
        painter.rect_filled(rect, 4.0, theme::bg_raised());
        painter.rect_stroke(rect, 4.0, Stroke::new(1.0, theme::border_light()), egui::StrokeKind::Inside);
    }
    let color = if response.hovered() { theme::text_bright() } else { theme::text_dim() };
    painter.text(rect.center(), Align2::CENTER_CENTER, icon, theme::font(13.0), color);
    response.on_hover_text(tooltip)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(names: &[&str]) -> Vec<(PathBuf, String)> {
        names.iter().map(|n| (PathBuf::from("/w").join(n), String::new())).collect()
    }

    fn found(rows: &[Row]) -> Vec<&str> {
        rows.iter().map(|r| r.name.as_str()).collect()
    }

    #[test]
    fn the_filter_ranks_names_and_falls_back_to_folders() {
        let list = rows(&["cobblemoonmod", "ReinBot-Web", "RustGit", "rb-tools"]);
        assert_eq!(found(&filter(list.clone(), "")), ["cobblemoonmod", "ReinBot-Web", "RustGit", "rb-tools"]);
        assert_eq!(found(&filter(list.clone(), "rg")), ["RustGit"]);
        let hits = filter(list.clone(), "rb");
        assert_eq!(found(&hits), ["rb-tools", "ReinBot-Web"]);
        assert_eq!(hits[0].positions, [0, 1]);
        assert!(filter(list, "zzz").is_empty());

        // A folder name finds the repositories inside it, without highlighting.
        let nested = vec![(PathBuf::from("/w/mods/core"), "mods".to_owned()), (PathBuf::from("/w/app"), String::new())];
        let hits = filter(nested, "mods");
        assert_eq!(found(&hits), ["core"]);
        assert!(hits[0].positions.is_empty());
    }
}
