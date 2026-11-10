//! The toolbar under the tab strip: repository and branch pickers on the
//! left, the main actions in the middle.

use eframe::egui::{
    self, Align, Align2, Layout, Popup, Rect, Response, ScrollArea, Sense, Ui, pos2, vec2,
};

use crate::git::ops::{self, PullMode};
use crate::platform;
use crate::state::{Dialog, RepoTab, Shared};
use crate::theme::{self, icons};
use crate::ui::widgets::{elided, icon_button, menu_item};

const BUTTON: egui::Vec2 = vec2(54.0, 46.0);

pub fn show(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared) {
    let rect = ui.max_rect();
    let snap = tab.snap.clone();

    // ---- left: repository > branch ----
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(vec2(10.0, 0.0)))
            .layout(Layout::left_to_right(Align::Center)),
        |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let response = picker(ui, "repository", &tab.name, 190.0);
            crate::ui::workspace::repository_menu(&response, shared, &tab.path);

            ui.label(egui::RichText::new(icons::CARET_RIGHT).size(12.0).color(theme::text_faint()));

            let branch = snap.as_ref().map(|s| s.head_label()).unwrap_or_default();
            let response = picker(ui, "branch", &branch, 220.0);
            if let Some(snap) = &snap {
                Popup::menu(&response).show(|ui| {
                    ui.set_min_width(240.0);
                    ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                        for b in &snap.branches {
                            let text = if b.is_head {
                                format!("{}  {}", icons::CHECK, b.name)
                            } else {
                                format!("{}  {}", icons::GIT_BRANCH, b.name)
                            };
                            if ui.button(text).clicked() && !b.is_head {
                                tab.run(ops::checkout(&b.name));
                            }
                        }
                    });
                    ui.separator();
                    if menu_item(ui, icons::PLUS, "Create branch…") {
                        shared.show(new_branch_dialog());
                    }
                });
            }
        },
    );

    // ---- middle: actions ----
    let dirty = snap.as_ref().is_some_and(|s| !s.status.is_clean());
    let has_stash = snap.as_ref().is_some_and(|s| !s.stashes.is_empty());
    let has_remote = snap.as_ref().is_some_and(|s| !s.remotes.is_empty());
    let on_branch = snap.as_ref().is_some_and(|s| s.current_branch().is_some());
    let ready = snap.is_some();

    let group_w = BUTTON.x * 8.0 + 16.0 + 14.0 + 13.0 + 8.0 * 2.0;
    let center = Rect::from_center_size(rect.center(), vec2(group_w, BUTTON.y));
    // Keep clear of the pickers on narrow windows.
    let center = center.translate(vec2((rect.left() + 440.0 - center.left()).max(0.0), 0.0));

    ui.scope_builder(
        egui::UiBuilder::new().max_rect(center).layout(Layout::left_to_right(Align::Center)),
        |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;

            let undo_tip = tab.undo.last().map(|e| format!("Undo: {}", e.label)).unwrap_or("Nothing to undo".into());
            if tool_button(ui, "Undo", icons::ARROW_COUNTER_CLOCKWISE, !tab.undo.is_empty(), tab.is_running("Undo "), &undo_tip).clicked() {
                tab.undo();
            }
            let redo_tip = tab.redo.last().map(|e| format!("Redo: {}", e.label)).unwrap_or("Nothing to redo".into());
            if tool_button(ui, "Redo", icons::ARROW_CLOCKWISE, !tab.redo.is_empty(), tab.is_running("Redo "), &redo_tip).clicked() {
                tab.redo();
            }
            ui.add_space(8.0);

            let pull_tip = if !has_remote {
                "This repository has no remote"
            } else if !on_branch {
                "Check out a branch to pull"
            } else {
                "Pull from the upstream branch"
            };
            if tool_button(ui, "Pull", icons::ARROW_LINE_DOWN, has_remote && on_branch, tab.is_running("Pull") || tab.is_running("Fetch"), pull_tip).clicked() {
                pull(tab, PullMode::Default);
            }
            let (caret, response) = ui.allocate_exact_size(vec2(14.0, BUTTON.y), Sense::click());
            if response.hovered() {
                ui.painter().rect_filled(caret, 4.0, theme::bg_raised());
            }
            ui.painter().text(
                pos2(caret.center().x, caret.top() + 31.0),
                Align2::CENTER_CENTER,
                icons::CARET_DOWN,
                theme::font(10.0),
                theme::text_dim(),
            );
            Popup::menu(&response).show(|ui| {
                ui.set_min_width(230.0);
                if menu_item(ui, icons::ARROWS_CLOCKWISE, "Fetch all") {
                    fetch(tab);
                }
                ui.separator();
                if menu_item(ui, icons::ARROW_LINE_DOWN, "Pull (fast-forward if possible)") {
                    pull(tab, PullMode::Default);
                }
                if menu_item(ui, icons::ARROW_LINE_DOWN, "Pull (fast-forward only)") {
                    pull(tab, PullMode::FastForwardOnly);
                }
                if menu_item(ui, icons::ARROW_LINE_DOWN, "Pull (rebase)") {
                    pull(tab, PullMode::Rebase);
                }
            });

            let push_tip = if !has_remote {
                "This repository has no remote"
            } else if !on_branch {
                "Check out a branch to push"
            } else {
                "Push to the upstream branch (right-click to force)"
            };
            let push_button = tool_button(ui, "Push", icons::ARROW_LINE_UP, has_remote && on_branch, tab.is_running("Push") || tab.is_running("Force push"), push_tip);
            if push_button.clicked() {
                push(tab);
            }
            push_button.context_menu(|ui| {
                if menu_item(ui, icons::WARNING, "Force push (with lease)") {
                    force_push(tab, shared);
                }
            });
            ui.add_space(8.0);

            if tool_button(ui, "Branch", icons::GIT_BRANCH, ready, tab.is_running("Create branch"), "Create a branch at HEAD").clicked() {
                shared.show(new_branch_dialog());
            }
            if tool_button(ui, "Stash", icons::TRAY_ARROW_DOWN, dirty, tab.is_running("Stash changes"), "Stash all changes").clicked() {
                stash(tab);
            }
            if tool_button(ui, "Pop", icons::TRAY_ARROW_UP, has_stash, tab.is_running("Pop stash"), "Apply and remove the latest stash").clicked() {
                pop_stash(tab);
            }

            let (sep, _) = ui.allocate_exact_size(vec2(13.0, BUTTON.y), Sense::hover());
            ui.painter().vline(sep.center().x, sep.shrink2(vec2(0.0, 6.0)).y_range(), egui::Stroke::new(1.0, theme::border()));

            if tool_button(ui, "Terminal", icons::TERMINAL_WINDOW, true, false, "Open a terminal in this repository").clicked() {
                platform::terminal(&tab.path);
            }
        },
    );

    // ---- right: search / refresh ----
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(vec2(10.0, 0.0)))
            .layout(Layout::right_to_left(Align::Center)),
        |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if icon_button(ui, icons::MAGNIFYING_GLASS, 18.0, "Search commits (Ctrl+F)").clicked() {
                tab.search_open = !tab.search_open;
                tab.search_focus = tab.search_open;
                tab.diff = None;
            }
            if icon_button(ui, icons::ARROWS_CLOCKWISE, 18.0, "Refresh (F5)").clicked() {
                tab.refresh();
            }
            if icon_button(ui, icons::SIDEBAR_SIMPLE, 18.0, "Toggle the left panel").clicked() {
                shared.settings.show_sidebar = !shared.settings.show_sidebar;
            }
        },
    );
}

pub fn new_branch_dialog() -> Dialog {
    Dialog::CreateBranch { at: None, at_label: "HEAD".to_owned(), name: String::new(), checkout: true }
}

pub fn clone_dialog() -> Dialog {
    Dialog::Clone { url: String::new(), dest: default_clone_dir() }
}

// What the buttons do, as functions so the command palette runs the same code.

pub fn fetch(tab: &mut RepoTab) {
    tab.run(ops::fetch_all());
}

pub fn pull(tab: &mut RepoTab, mode: PullMode) {
    tab.run(ops::pull(mode));
}

pub fn push(tab: &mut RepoTab) {
    if let Some(cmd) = push_cmd(tab, false) {
        tab.run(cmd);
    }
}

/// Asks first: commits that exist only on the remote are lost.
pub fn force_push(tab: &mut RepoTab, shared: &mut Shared) {
    if let Some(cmd) = push_cmd(tab, true) {
        shared.show(Dialog::confirm(
            "Force push",
            "Overwrite the remote branch with your local history? Commits that \
             exist only on the remote will be lost.",
            "Force push",
            cmd,
        ));
    }
}

pub fn stash(tab: &mut RepoTab) {
    tab.run(ops::stash_push(""));
}

pub fn pop_stash(tab: &mut RepoTab) {
    tab.run(ops::stash_pop(0));
}

pub fn default_clone_dir() -> String {
    dirs::document_dir()
        .or_else(dirs::home_dir)
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

/// A push for the current branch, setting the upstream on first push.
pub fn push_cmd(tab: &RepoTab, force: bool) -> Option<ops::GitCmd> {
    let snap = tab.snap.as_ref()?;
    let branch = snap.current_branch()?;
    if let Some(upstream) = &branch.upstream {
        Some(ops::push(None, force).done(format!("Pushed {} to {upstream}", branch.name)))
    } else {
        let remote = snap.default_remote()?;
        Some(ops::push(Some((remote, &branch.name)), force))
    }
}

/// A caption-over-value dropdown trigger ("repository / MelonLoader7").
fn picker(ui: &mut Ui, caption: &str, value: &str, max_width: f32) -> Response {
    let font = theme::bold(15.0);
    let galley = elided(ui.painter(), value, font, theme::text_bright(), max_width - 34.0);
    let width = (galley.size().x + 34.0).max(70.0);
    let (rect, response) = ui.allocate_exact_size(vec2(width, BUTTON.y), Sense::click());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        if response.hovered() {
            painter.rect_filled(rect, 4.0, theme::bg_raised());
        }
        painter.text(pos2(rect.left() + 8.0, rect.top() + 12.0), Align2::LEFT_CENTER, caption, theme::font(10.5), theme::text_dim());
        let y = rect.top() + 30.0;
        let text_w = galley.size().x;
        painter.galley(pos2(rect.left() + 8.0, (y - galley.size().y / 2.0).round()), galley, theme::text_bright());
        painter.text(pos2(rect.left() + 14.0 + text_w, y + 1.0), Align2::LEFT_CENTER, icons::CARET_DOWN, theme::font(11.0), theme::text_dim());
    }
    response
}

/// Label above icon, like the reference toolbar. While `busy`, the icon is
/// replaced by a spinner and the button ignores clicks, so a slow push or pull
/// visibly "took".
fn tool_button(ui: &mut Ui, label: &str, icon: &str, enabled: bool, busy: bool, tooltip: &str) -> Response {
    let clickable = enabled && !busy;
    let (rect, response) = ui.allocate_exact_size(BUTTON, if clickable { Sense::click() } else { Sense::hover() });
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let pressed = clickable && response.is_pointer_button_down_on();
        let (label_color, icon_color) = if busy {
            (theme::accent(), theme::accent())
        } else if !enabled {
            (theme::text_faint(), theme::text_faint())
        } else if response.hovered() {
            (theme::text_bright(), theme::text_bright())
        } else {
            (theme::text_dim(), theme::text())
        };
        if pressed {
            painter.rect_filled(rect, 4.0, theme::bg_pressed());
        } else if busy {
            painter.rect_filled(rect, 4.0, theme::with_alpha(theme::accent(), 28));
        } else if clickable && response.hovered() {
            painter.rect_filled(rect, 4.0, theme::bg_raised());
        }
        // The content sinks a pixel while held, like a physical key.
        let nudge = if pressed { 1.0 } else { 0.0 };
        painter.text(
            pos2(rect.center().x, rect.top() + 11.0 + nudge),
            Align2::CENTER_CENTER,
            label,
            theme::font(11.0),
            label_color,
        );
        let icon_at = pos2(rect.center().x, rect.top() + 30.0 + nudge);
        if busy {
            crate::ui::widgets::spinner(painter, icon_at, 8.0, theme::accent());
        } else {
            painter.text(icon_at, Align2::CENTER_CENTER, icon, theme::font(20.0), icon_color);
        }
    }
    response.on_hover_text(tooltip)
}

/// A thin sweeping bar along the bottom of the toolbar while work is running.
pub fn progress_strip(ui: &Ui, rect: Rect) {
    let time = ui.input(|i| i.time) as f32;
    let width = rect.width() * 0.28;
    let travel = rect.width() + width;
    let x = rect.left() - width + (time * 0.9).fract() * travel;
    let bar = Rect::from_min_max(pos2(x, rect.bottom() - 2.0), pos2(x + width, rect.bottom()));
    ui.painter().with_clip_rect(rect).rect_filled(bar, 1.0, theme::accent());
}
