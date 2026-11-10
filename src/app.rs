//! The application shell: tabs, menus, panels and everything that is not
//! specific to a single repository.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};

use eframe::egui::{
    self, Align, Align2, Frame, Key, Layout, Margin, Modifiers, Rect, Sense, Stroke, Ui,
    ViewportCommand, pos2, vec2,
};

use crate::git::cli;
use crate::platform;
use crate::state::{AppAction, Dialog, RepoTab, Settings, Shared, ToastKind};
use crate::theme::{self, icons};
use crate::ui::prefs::{PrefsView, Section};
use crate::ui::{self, widgets};
use crate::workspace::Opens;

pub struct App {
    tabs: Vec<RepoTab>,
    active: usize,
    shared: Shared,
    /// A clone running in the background: (what, result).
    cloning: Option<(String, Receiver<Result<PathBuf, String>>)>,
    saved_settings: String,
    window_title: String,
    shot: Option<crate::shot::Shot>,
    palette: ui::palette::Palette,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, open: Option<PathBuf>) -> Self {
        theme::install(&cc.egui_ctx);

        let settings = Settings::load();
        crate::prefs::apply(&cc.egui_ctx, &settings);
        let mut app = App {
            tabs: Vec::new(),
            active: 0,
            shared: Shared {
                settings: settings.clone(),
                avatars: crate::avatars::Avatars::new(&cc.egui_ctx),
                dialog: None,
                dialog_fresh: false,
                toasts: Vec::new(),
                actions: Vec::new(),
                updater: crate::update::Updater::new(),
                workspaces: Default::default(),
                prefs: None,
                git: crate::git_setup::GitSetup::new(),
            },
            cloning: None,
            saved_settings: String::new(),
            window_title: String::new(),
            shot: crate::shot::Shot::from_env(),
            palette: Default::default(),
        };

        // Preferences > General > Remember tabs.
        if settings.general.remember_tabs {
            for path in &settings.tabs {
                if path.is_dir() {
                    app.tabs.push(RepoTab::new(path.clone()));
                }
            }
            app.active = settings.active.min(app.tabs.len().saturating_sub(1));
        }
        if let Some(path) = open {
            app.open_repo(&path);
        }
        app
    }

    /// Opens (or focuses) the repository containing `path`.
    fn open_repo(&mut self, path: &Path) {
        let Some(workdir) = self.workdir_of(path) else { return };
        self.shared.settings.remember(&workdir);
        match self.tabs.iter().position(|t| t.path == workdir) {
            Some(i) => self.active = i,
            None => {
                self.tabs.push(RepoTab::new(workdir));
                self.active = self.tabs.len() - 1;
            }
        }
    }

    /// Shows the repository containing `path` in the current tab; see
    /// [`AppAction::Switch`].
    fn switch_repo(&mut self, path: &Path) {
        let Some(workdir) = self.workdir_of(path) else { return };
        if self.tabs.iter().any(|t| t.path == workdir) {
            return self.open_repo(&workdir);
        }
        match self.tabs.get_mut(self.active) {
            Some(tab) if tab.can_switch() => {
                self.shared.settings.remember(&workdir);
                *tab = RepoTab::new(workdir);
            }
            _ => self.open_repo(&workdir),
        }
    }

    /// The working directory of the repository containing `path`, or a
    /// toast saying why there is none.
    fn workdir_of(&mut self, path: &Path) -> Option<PathBuf> {
        match git2::Repository::discover(path) {
            Ok(repo) => match repo.workdir() {
                // Normalise separators and drop the trailing slash libgit2 adds.
                Some(dir) => Some(crate::workspace::clean(dir)),
                None => {
                    self.shared.toast(ToastKind::Error, "Cannot open repository", "Bare repositories are not supported.");
                    None
                }
            },
            Err(_) => {
                self.shared.toast(
                    ToastKind::Error,
                    "Not a Git repository",
                    format!("{} is not inside a Git repository.", path.display()),
                );
                None
            }
        }
    }

    fn close_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        self.tabs.remove(index);
        if self.active > index || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
    }

    fn start_clone(&mut self, ctx: &egui::Context, url: String, dest: PathBuf) {
        let name = url
            .trim_end_matches('/')
            .rsplit(['/', ':'])
            .next()
            .unwrap_or("repository")
            .trim_end_matches(".git")
            .to_owned();
        let target = dest.join(&name);
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        let label = format!("Cloning {name}");
        std::thread::spawn(move || {
            let args = vec!["clone".to_owned(), url, target.display().to_string()];
            let result = cli::run(&dest, &args, cli::Options::default()).map(|_| target);
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        self.cloning = Some((label, rx));
    }

    fn apply_actions(&mut self, ctx: &egui::Context) {
        for action in std::mem::take(&mut self.shared.actions) {
            match action {
                AppAction::OpenDialog => {
                    if let Some(dir) = rfd::FileDialog::new().set_title("Open repository").pick_folder() {
                        self.open_repo(&dir);
                    }
                }
                AppAction::Open(path) => self.open_repo(&path),
                AppAction::Switch(path) => self.switch_repo(&path),
                AppAction::AddWorkspace(from) => {
                    let picked = crate::shot::Shot::picked_folder()
                        .or_else(|| rfd::FileDialog::new().set_title("Add a folder of repositories").pick_folder());
                    if let Some(dir) = picked {
                        crate::workspace::add(ctx, &mut self.shared, &dir, from);
                    }
                }
                AppAction::CloseTab(i) => self.close_tab(i),
                AppAction::Activate(i) => {
                    if i < self.tabs.len() {
                        self.active = i;
                    }
                }
                AppAction::Clone { url, dest } => self.start_clone(ctx, url, dest),
                AppAction::InitDialog => {
                    let picked = crate::shot::Shot::picked_folder()
                        .or_else(|| rfd::FileDialog::new().set_title("Initialise a repository in…").pick_folder());
                    if let Some(dir) = picked {
                        match cli::run(&dir, &["init".to_owned()], cli::Options::default()) {
                            Ok(_) => self.open_repo(&dir),
                            Err(e) => self.shared.toast(ToastKind::Error, "git init failed", e),
                        }
                    }
                }
                AppAction::OpenPreferences(section) => match &mut self.shared.prefs {
                    Some(view) => view.open(section),
                    None => self.shared.prefs = Some(Box::new(PrefsView::new(section))),
                },
                AppAction::ClosePreferences => self.shared.prefs = None,
                AppAction::Quit => ctx.send_viewport_cmd(ViewportCommand::Close),
            }
        }
    }

    fn poll(&mut self, ctx: &egui::Context) {
        let active = self.active;
        for (i, tab) in self.tabs.iter_mut().enumerate() {
            tab.poll(ctx, &mut self.shared, i == active);
        }
        self.shared.workspaces.open_tabs = self
            .tabs
            .iter()
            .map(|t| (t.path.clone(), t.snap.as_deref().map(crate::workspace::Summary::of_snapshot)))
            .collect();
        crate::workspace::poll(ctx, &mut self.shared);
        // Without a tab the welcome page lists what a new workspace holds;
        // no menu needs to open for it.
        if self.tabs.is_empty() {
            self.shared.workspaces.reveal = None;
        }

        if let Some((_, rx)) = &self.cloning
            && let Ok(result) = rx.try_recv() {
                self.cloning = None;
                match result {
                    Ok(path) => {
                        self.shared.toast(ToastKind::Success, "Clone finished", path.display().to_string());
                        self.open_repo(&path);
                    }
                    Err(e) => self.shared.toast(ToastKind::Error, "Clone failed", e),
                }
            }

        // Folders dropped onto the window open as repositories.
        let dropped: Vec<PathBuf> =
            ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        for path in dropped {
            self.open_repo(&path);
        }

        // Catch up with whatever happened while we were in the background.
        if ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::WindowFocused(true))))
            && let Some(tab) = self.tabs.get_mut(self.active) {
                tab.refresh();
            }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let modal = self.shared.dialog.is_some();
        let pressed = |modifiers: Modifiers, key: Key| ctx.input_mut(|i| i.consume_key(modifiers, key));

        if !modal && pressed(Modifiers::COMMAND, Key::P) {
            self.palette.toggle(ctx);
        }
        // While it is open the palette has the keyboard, Escape included.
        if self.palette.is_open() {
            self.palette.take_keys(ctx);
            return;
        }
        if !modal && pressed(Modifiers::COMMAND, Key::O) {
            self.shared.actions.push(AppAction::OpenDialog);
        }
        if !modal && pressed(Modifiers::COMMAND, Key::W) {
            self.shared.actions.push(AppAction::CloseTab(self.active));
        }
        if !modal && pressed(Modifiers::COMMAND, Key::Comma) {
            self.shared.actions.push(match self.shared.prefs {
                Some(_) => AppAction::ClosePreferences,
                None => AppAction::OpenPreferences(Section::General),
            });
        }
        // The preferences page has the window: Escape leaves it, and the
        // repository's own shortcuts wait until it is closed.
        if self.shared.prefs.is_some() {
            if !modal && !egui::Popup::is_any_open(ctx) && !ctx.egui_wants_keyboard_input() && pressed(Modifiers::NONE, Key::Escape) {
                self.shared.actions.push(AppAction::ClosePreferences);
            }
            return;
        }
        if !modal && !self.tabs.is_empty() && pressed(Modifiers::CTRL, Key::Tab) {
            self.active = (self.active + 1) % self.tabs.len();
        }

        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        if modal {
            return;
        }
        if pressed(Modifiers::COMMAND, Key::F) {
            ui::graph::open_search(tab);
        }
        if pressed(Modifiers::NONE, Key::F5) {
            tab.refresh();
        }
        // Escape peels back one layer at a time; open menus get it first.
        if !egui::Popup::is_any_open(ctx) && ctx.input(|i| i.key_pressed(Key::Escape)) {
            if tab.overlay.is_some() {
                // The conflict resolver may have unsaved choices and asks first.
                if ui::conflict::escape(tab) {
                    tab.overlay = None;
                }
            } else if tab.diff.is_some() {
                tab.diff = None;
            } else if tab.search_open {
                tab.search_open = false;
            }
        }
    }

    fn persist(&mut self, ctx: &egui::Context) {
        // Not while dragging a column edge.
        if ctx.input(|i| i.pointer.any_down()) {
            return;
        }
        self.shared.settings.tabs = self.tabs.iter().map(|t| t.path.clone()).collect();
        self.shared.settings.active = self.active;
        crate::prefs::tidy(&mut self.shared.settings);
        if let Ok(json) = serde_json::to_string(&self.shared.settings)
            && json != self.saved_settings {
                self.shared.settings.save();
                // The first comparison is against nothing: the settings were
                // applied as loaded already.
                if !self.saved_settings.is_empty() {
                    crate::prefs::apply(ctx, &self.shared.settings);
                }
                self.saved_settings = json;
            }
    }

    fn menu_bar(&mut self, ui: &mut Ui) {
        let App { tabs, active, shared, palette, .. } = self;
        egui::containers::menu::MenuBar::new().ui(ui, |ui| {
            ui.spacing_mut().button_padding = vec2(8.0, 3.0);
            ui.menu_button("File", |ui| {
                ui.set_min_width(230.0);
                if ui.add(egui::Button::new("Open Repository…").shortcut_text("Ctrl+O")).clicked() {
                    shared.actions.push(AppAction::OpenDialog);
                }
                if ui.button("Clone Repository…").clicked() {
                    shared.show(ui::toolbar::clone_dialog());
                }
                if ui.button("Init Repository…").clicked() {
                    shared.actions.push(AppAction::InitDialog);
                }
                ui.separator();
                if ui.button("Add Folder of Repositories…").clicked() {
                    shared.actions.push(AppAction::AddWorkspace(Opens::Here));
                }
                if !shared.settings.recent.is_empty() {
                    ui.menu_button("Open Recent", |ui| {
                        ui.set_min_width(260.0);
                        for path in shared.settings.recent.clone() {
                            if ui.button(path.display().to_string()).clicked() {
                                shared.actions.push(AppAction::Open(path));
                            }
                        }
                    });
                }
                ui.separator();
                if ui.add_enabled(!tabs.is_empty(), egui::Button::new("Close Tab").shortcut_text("Ctrl+W")).clicked() {
                    shared.actions.push(AppAction::CloseTab(*active));
                }
                ui.separator();
                if ui.add(egui::Button::new("Preferences…").shortcut_text("Ctrl+,")).clicked() {
                    shared.actions.push(AppAction::OpenPreferences(Section::General));
                }
                ui.separator();
                if ui.button("Exit").clicked() {
                    shared.actions.push(AppAction::Quit);
                }
            });
            ui.menu_button("View", |ui| {
                ui.set_min_width(230.0);
                let label = if shared.settings.show_sidebar { "Hide Left Panel" } else { "Show Left Panel" };
                if ui.button(label).clicked() {
                    shared.settings.show_sidebar = !shared.settings.show_sidebar;
                }
                let mut auto_fetch = shared.settings.auto_fetch_minutes > 0;
                let label = crate::prefs::general::auto_fetch_label(&shared.settings);
                if ui.checkbox(&mut auto_fetch, label).changed() {
                    crate::prefs::general::set_auto_fetch(&mut shared.settings, auto_fetch);
                }
                ui.checkbox(&mut shared.settings.fetch_avatars, "Author pictures from Gravatar and GitHub");
                ui.checkbox(&mut shared.settings.hosting_integration, "Pull requests, issues and CI from GitHub and GitLab");
                if let Some(tab) = tabs.get_mut(*active) {
                    if ui.add(egui::Button::new("Search Commits").shortcut_text("Ctrl+F")).clicked() {
                        ui::graph::open_search(tab);
                    }
                    if ui.add(egui::Button::new("Refresh").shortcut_text("F5")).clicked() {
                        tab.refresh();
                    }
                }
                ui.menu_button("Theme", |ui| {
                    let appearance = &mut shared.settings.appearance;
                    for choice in theme::Choice::ALL {
                        let picked = appearance.custom_theme.is_none() && shared.settings.theme == choice;
                        if ui.radio(picked, choice.label()).clicked() {
                            shared.settings.theme = choice;
                            appearance.custom_theme = None;
                        }
                    }
                    // Custom themes are listed in preferences, which read
                    // the themes folder; the menu only shows the one in use.
                    if let Some(name) = &appearance.custom_theme {
                        let _ = ui.radio(true, name.as_str());
                    }
                    ui.separator();
                    if ui.button("Custom Themes…").clicked() {
                        shared.actions.push(AppAction::OpenPreferences(Section::Appearance));
                    }
                });
                ui.separator();
                if ui.add(egui::Button::new("Zoom In").shortcut_text("Ctrl++")).clicked() {
                    zoom(ui.ctx(), Zoom::In);
                }
                if ui.add(egui::Button::new("Zoom Out").shortcut_text("Ctrl+-")).clicked() {
                    zoom(ui.ctx(), Zoom::Out);
                }
                if ui.add(egui::Button::new("Reset Zoom").shortcut_text("Ctrl+0")).clicked() {
                    zoom(ui.ctx(), Zoom::Reset);
                }
            });
            ui.menu_button("Help", |ui| {
                ui.set_min_width(200.0);
                if ui.add(egui::Button::new("Command Palette").shortcut_text("Ctrl+P")).clicked() {
                    palette.open(ui.ctx());
                }
                ui.separator();
                ui::update::menu(ui, shared);
                ui.separator();
                if ui.button("About GitKrill").clicked() {
                    shared.show(Dialog::About);
                }
            });
        });
    }

    fn tab_strip(&mut self, ui: &mut Ui) {
        let rect = ui.max_rect();
        let painter = ui.painter().clone();
        painter.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::border()));

        let count = self.tabs.len().max(1) as f32;
        // Room at the right end for "+" and the preferences gear.
        let max_w = ((rect.width() - 44.0 - GEAR_ROOM) / count).clamp(70.0, 220.0);
        let mut x = rect.left();

        for (i, tab) in self.tabs.iter().enumerate() {
            let active = i == self.active;
            let color = if active { theme::text_bright() } else { theme::text_dim() };
            let font = if active { theme::bold(13.5) } else { theme::font(13.5) };
            let galley = widgets::elided(&painter, &tab.name, font, color, max_w - 58.0);
            let width = (galley.size().x + 58.0).min(max_w);
            let tab_rect = Rect::from_min_size(pos2(x, rect.top()), vec2(width, rect.height()));
            let response = ui.interact(tab_rect, ui.id().with(("tab", i)), Sense::click());

            if active {
                painter.rect_filled(tab_rect, 0.0, theme::bg_toolbar());
                painter.hline(tab_rect.x_range(), tab_rect.top() + 1.0, Stroke::new(2.0, theme::accent()));
            } else if response.hovered() {
                painter.rect_filled(tab_rect, 0.0, theme::bg_hover());
            }
            painter.text(
                pos2(tab_rect.left() + 12.0, tab_rect.center().y),
                Align2::LEFT_CENTER,
                icons::GIT_BRANCH,
                theme::font(14.0),
                color,
            );
            widgets::galley_left(&painter, tab_rect, tab_rect.left() + 32.0, galley);

            let close = Rect::from_center_size(pos2(tab_rect.right() - 14.0, tab_rect.center().y), vec2(18.0, 18.0));
            let close_response = ui.interact(close, ui.id().with(("tab-close", i)), Sense::click());
            if active || response.hovered() || close_response.hovered() {
                if close_response.hovered() {
                    painter.rect_filled(close, 3.0, theme::bg_raised());
                }
                painter.text(close.center(), Align2::CENTER_CENTER, icons::X, theme::font(11.0), theme::text_dim());
            }

            if close_response.clicked() || response.middle_clicked() {
                self.shared.actions.push(AppAction::CloseTab(i));
            } else if response.clicked() {
                self.shared.actions.push(AppAction::Activate(i));
            }
            let path = tab.path.clone();
            response.on_hover_text(path.display().to_string()).context_menu(|ui| {
                if ui.button("Close tab").clicked() {
                    self.shared.actions.push(AppAction::CloseTab(i));
                }
                if ui.button("Show in file manager").clicked() {
                    platform::reveal(&path);
                }
                let tools = &self.shared.settings.tools;
                if ui.button("Open in terminal").clicked() {
                    crate::external::terminal(tools, &path);
                }
                if tools.has_editor() && ui.button("Open in editor").clicked() {
                    crate::external::edit(tools, &path);
                }
                if ui.button("Copy path").clicked() {
                    ui.ctx().copy_text(path.display().to_string());
                }
            });

            painter.vline(tab_rect.right(), tab_rect.shrink2(vec2(0.0, 7.0)).y_range(), Stroke::new(1.0, theme::border()));
            x += width + 1.0;
        }

        let plus = Rect::from_center_size(pos2(x + 18.0, rect.center().y), vec2(24.0, 24.0));
        let response = ui.interact(plus, ui.id().with("tab-new"), Sense::click());
        let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&response));
        if response.hovered() || open {
            painter.rect_filled(plus, 4.0, theme::bg_raised());
        }
        painter.text(plus.center(), Align2::CENTER_CENTER, icons::PLUS, theme::font(14.0), theme::text_dim());
        let response = if open { response } else { response.on_hover_text("Open a repository in a new tab") };
        let current = self.tabs.get(self.active).map(|t| t.path.clone());
        ui::workspace::new_tab_menu(&response, &mut self.shared, current.as_deref());

        let gear = Rect::from_center_size(pos2(rect.right() - GEAR_ROOM / 2.0, rect.center().y), vec2(28.0, 26.0));
        let response = ui.interact(gear, ui.id().with("prefs-gear"), Sense::click());
        let open = self.shared.prefs.is_some();
        if open {
            painter.rect_filled(gear, 4.0, theme::bg_selected());
        } else if response.hovered() {
            painter.rect_filled(gear, 4.0, theme::bg_raised());
        }
        let color = if open || response.hovered() { theme::text_bright() } else { theme::text_dim() };
        painter.text(gear.center(), Align2::CENTER_CENTER, icons::GEAR, theme::font(16.0), color);
        if response.on_hover_text(if open { "Close preferences (Ctrl+,)" } else { "Preferences (Ctrl+,)" }).clicked() {
            self.shared.actions.push(if open { AppAction::ClosePreferences } else { AppAction::OpenPreferences(Section::General) });
        }
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        let tab = self.tabs.get(self.active);
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let dim = |text: String| egui::RichText::new(text).size(11.5).color(theme::text_dim());

            let busy = tab
                .and_then(|t| t.busy_label().map(str::to_owned))
                .or(self.cloning.as_ref().map(|c| c.0.clone()));
            if let Some(label) = busy {
                ui.add(egui::Spinner::new().size(12.0).color(theme::accent()));
                ui.label(dim(format!("{label}…")));
            } else if let Some(tab) = tab {
                if tab.is_loading() {
                    ui.add(egui::Spinner::new().size(12.0).color(theme::accent()));
                    ui.label(dim("Loading…".to_owned()));
                } else if let Some(snap) = &tab.snap {
                    let mut text = format!("{}  {}", icons::GIT_BRANCH, snap.head_label());
                    if let Some(branch) = snap.current_branch() {
                        if branch.ahead > 0 {
                            text.push_str(&format!("   {}{}", branch.ahead, icons::ARROW_UP));
                        }
                        if branch.behind > 0 {
                            text.push_str(&format!("   {}{}", branch.behind, icons::ARROW_DOWN));
                        }
                    }
                    ui.label(dim(text));
                    if let Some(state) = snap.state.label() {
                        ui.label(egui::RichText::new(state).size(11.5).color(theme::orange()));
                    }
                }
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(dim(format!("v{}", env!("CARGO_PKG_VERSION"))));
                ui::update::status_item(ui, &mut self.shared);
                ui::git_setup::status_item(ui, &mut self.shared);
                let zoom = (ui.ctx().zoom_factor() * 100.0).round();
                if zoom != 100.0 {
                    ui.label(dim(format!("{zoom}%")));
                }
                if let Some(snap) = tab.and_then(|t| t.snap.as_ref()) {
                    let n = snap.history.commits.len();
                    let more = if snap.history.has_more { "+" } else { "" };
                    ui.label(dim(format!("{n}{more} commits")));
                }
            });
        });
    }

    fn welcome(&mut self, ui: &mut Ui) {
        ui.vertical_centered(|ui| {
            // Higher up when the repository list below needs the room.
            let top = if ui::workspace::has_rows(&self.shared) { 0.08 } else { 0.28 };
            ui.add_space((ui.available_height() * top).max(20.0));
            ui.label(egui::RichText::new(icons::GIT_BRANCH).size(54.0).color(theme::accent()));
            ui.add_space(6.0);
            ui.label(egui::RichText::new("GitKrill").font(theme::bold(26.0)).color(theme::text_bright()));
            ui.label(egui::RichText::new("Open a repository to get started, or drop a folder here.").color(theme::text_dim()));
            ui.add_space(18.0);

            let big = |text: String| egui::Button::new(egui::RichText::new(text).size(14.0)).min_size(vec2(260.0, 36.0));
            if ui.add(big(format!("{}   Open repository", icons::FOLDER_OPEN))).clicked() {
                self.shared.actions.push(AppAction::OpenDialog);
            }
            if ui
                .add(big(format!("{}   Add a folder of repositories", icons::FOLDERS)))
                .on_hover_text("Pick a folder: every Git repository inside it is listed here")
                .clicked()
            {
                self.shared.actions.push(AppAction::AddWorkspace(Opens::NewTab));
            }
            if ui.add(big(format!("{}   Clone repository", icons::CLOUD_ARROW_DOWN))).clicked() {
                self.shared.show(ui::toolbar::clone_dialog());
            }
            if ui.add(big(format!("{}   Init repository", icons::PLUS))).clicked() {
                self.shared.actions.push(AppAction::InitDialog);
            }

            if ui::workspace::has_rows(&self.shared) {
                ui.add_space(22.0);
                let width = ui.available_width().min(560.0);
                let height = (ui.available_height() - 16.0).max(120.0);
                ui.allocate_ui(vec2(width, height), |ui| {
                    ui.set_width(width);
                    ui::workspace::list(ui, &mut self.shared, Opens::NewTab, None, egui::Id::new("welcome-repositories"), false, height);
                });
            }
        });
    }
}

/// Width kept free at the right of the tab strip for the preferences gear.
const GEAR_ROOM: f32 = 44.0;

pub enum Zoom {
    In,
    Out,
    Reset,
}

/// View > Zoom In / Zoom Out / Reset Zoom.
pub fn zoom(ctx: &egui::Context, how: Zoom) {
    let factor = match how {
        Zoom::In => (ctx.zoom_factor() + 0.1).min(3.0),
        Zoom::Out => (ctx.zoom_factor() - 0.1).max(0.5),
        Zoom::Reset => 1.0,
    };
    ctx.set_zoom_factor(factor);
}

impl eframe::App for App {
    /// Shows through the hairline gaps between panels.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        theme::border().to_normalized_gamma_f32()
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        theme::follow(ui, self.shared.settings.theme);
        let ctx = ui.ctx().clone();
        self.poll(&ctx);
        ui::update::poll(&ctx, &mut self.shared);
        crate::external::poll(&ctx, &mut self.shared);
        ui::git_setup::poll(&ctx, &mut self.shared);
        self.shortcuts(&ctx);

        let title = match self.tabs.get(self.active) {
            Some(tab) => format!("{} - GitKrill", tab.name),
            None => "GitKrill".to_owned(),
        };
        if title != self.window_title {
            ctx.send_viewport_cmd(ViewportCommand::Title(title.clone()));
            self.window_title = title;
        }

        let bar = |id: &'static str, height: f32, fill| {
            egui::Panel::top(id).exact_size(height).show_separator_line(false).frame(Frame::new().fill(fill))
        };

        bar("menu", 24.0, theme::bg_tabbar())
            .frame(Frame::new().fill(theme::bg_tabbar()).inner_margin(Margin::symmetric(4, 0)))
            .show(ui, |ui| self.menu_bar(ui));
        bar("tabs", 34.0, theme::bg_tabbar()).show(ui, |ui| self.tab_strip(ui));

        egui::Panel::bottom("status")
            .exact_size(24.0)
            .show_separator_line(false)
            .frame(Frame::new().fill(theme::bg_tabbar()).inner_margin(Margin::symmetric(10, 0)))
            .show(ui, |ui| self.status_bar(ui));

        if self.shared.prefs.is_some() {
            if let Some(tab) = self.tabs.get_mut(self.active) {
                tab.start(&ctx);
            }
            ui::prefs::show(ui, &mut self.shared, self.tabs.get_mut(self.active));
        } else if self.active < self.tabs.len() {
            let App { tabs, active, shared, .. } = self;
            let tab = &mut tabs[*active];
            tab.start(&ctx);

            bar("toolbar", ui::toolbar::height(&shared.settings), theme::bg_toolbar()).show(ui, |ui| {
                ui::toolbar::show(ui, tab, shared);
                let rect = ui.max_rect();
                ui.painter().hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::border()));
                if tab.busy_label().is_some() {
                    ui::toolbar::progress_strip(ui, rect);
                }
            });

            if shared.settings.show_sidebar {
                egui::Panel::left("sidebar")
                    .resizable(true)
                    .default_size(230.0)
                    .size_range(170.0..=460.0)
                    .frame(Frame::new().fill(theme::bg_panel()))
                    .show(ui, |ui| ui::sidebar::show(ui, tab, shared));
            }

            egui::Panel::right("details")
                .resizable(true)
                .default_size(420.0)
                .size_range(300.0..=900.0)
                .frame(Frame::new().fill(theme::bg_app()))
                .show(ui, |ui| ui::panel::show(ui, tab, shared));

            egui::CentralPanel::default().frame(Frame::new().fill(theme::bg_app())).show(ui, |ui| {
                if tab.overlay.is_some() {
                    ui::overlay(ui, tab, shared);
                } else if tab.diff.is_some() {
                    ui::diff::show(ui, tab, shared);
                } else {
                    ui::graph::show(ui, tab, shared);
                }
            });
        } else {
            egui::CentralPanel::default()
                .frame(Frame::new().fill(theme::bg_app()))
                .show(ui, |ui| self.welcome(ui));
        }

        self.palette.show(&ctx, &mut self.tabs, self.active, &mut self.shared);
        ui::dialogs::show(&ctx, &mut self.shared, self.tabs.get_mut(self.active));
        ui::dialogs::toasts(&ctx, &mut self.shared, self.tabs.get(self.active));

        // Spinners and progress bars need frames while work is in flight.
        if self.cloning.is_some() || self.tabs.get(self.active).is_some_and(|t| t.busy_label().is_some()) {
            ctx.request_repaint();
        }

        self.apply_actions(&ctx);
        self.persist(&ctx);

        if let Some(shot) = &mut self.shot
            && shot.drive(&ctx, self.tabs.get_mut(self.active)) {
                self.shot = None;
            }
    }

    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        if let Some(shot) = &mut self.shot {
            shot.inject(raw_input);
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.shared.settings.tabs = self.tabs.iter().map(|t| t.path.clone()).collect();
        self.shared.settings.active = self.active;
        self.shared.settings.save();
        ui::update::on_exit(&mut self.shared);
    }
}
