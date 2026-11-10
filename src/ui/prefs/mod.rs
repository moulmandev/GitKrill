//! The preferences page (File > Preferences, Ctrl+, or the gear at the end
//! of the tab strip): sections listed on the left, the chosen one on the
//! right. While it is open it takes the place of the repository view
//! (toolbar, sidebar, graph, panel); the tabs, menus and status bar stay.
//!
//! Each section is a module here with a `State` (whatever it remembers
//! while preferences are open: text being typed, a list being loaded) and
//! `show(ui, page, state)`. Its persistent part lives in `crate::prefs`.
//! Sections below "Repository-specific" only show with a repository open,
//! and act on the one in front.
//!
//! Nothing here costs anything while preferences are closed: the view and
//! every section's state are created when the page opens and dropped when
//! it closes, and configuration files are read only then (see
//! [`crate::prefs::gitconfig`]).

pub mod appearance;
pub mod commit;
pub mod editor;
pub mod encoding;
pub mod form;
pub mod general;
pub mod gitflow;
pub mod gpg;
pub mod graph;
pub mod hooks;
pub mod integrations;
pub mod lfs;
pub mod notifications;
pub mod profiles;
pub mod ssh;
pub mod tools;

use eframe::egui::{self, Align2, Frame, Margin, Rect, ScrollArea, Sense, Stroke, Ui, pos2, vec2};

use crate::prefs::gitconfig::{ConfigFile, Scope};
use crate::state::{RepoTab, Shared, ToastKind};
use crate::theme::{self, icons};
use crate::ui::widgets;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Section {
    General,
    Profiles,
    Ssh,
    Integrations,
    Notifications,
    Appearance,
    Graph,
    Gpg,
    Editor,
    Tools,
    // Repository-specific.
    Encoding,
    Gitflow,
    Hooks,
    Commit,
    Lfs,
}

impl Section {
    /// For every repository, in sidebar order.
    pub const APP: [Section; 10] = [
        Section::General,
        Section::Profiles,
        Section::Ssh,
        Section::Integrations,
        Section::Notifications,
        Section::Appearance,
        Section::Graph,
        Section::Gpg,
        Section::Editor,
        Section::Tools,
    ];
    /// For the repository in front, in sidebar order.
    pub const REPO: [Section; 5] = [Section::Encoding, Section::Gitflow, Section::Hooks, Section::Commit, Section::Lfs];

    pub fn title(self) -> &'static str {
        match self {
            Section::General => "General",
            Section::Profiles => "Profiles",
            Section::Ssh => "SSH",
            Section::Integrations => "Integrations",
            Section::Notifications => "Notifications",
            Section::Appearance => "UI Customization",
            Section::Graph => "Commit Graph",
            Section::Gpg => "GPG",
            Section::Editor => "Editor",
            Section::Tools => "External Tools",
            Section::Encoding => "Encoding",
            Section::Gitflow => "Gitflow",
            Section::Hooks => "Git Hooks",
            Section::Commit => "Commit",
            Section::Lfs => "LFS",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Section::General => icons::GEAR,
            Section::Profiles => icons::USERS,
            Section::Ssh => icons::SHIELD_CHECK,
            Section::Integrations => icons::PLUGS,
            Section::Notifications => icons::BELL,
            Section::Appearance => icons::PAINT_BRUSH,
            Section::Graph => icons::GRAPH,
            Section::Gpg => icons::SIGNATURE,
            Section::Editor => icons::CODE,
            Section::Tools => icons::WRENCH,
            Section::Encoding => icons::TEXT_T,
            Section::Gitflow => icons::ARROWS_LEFT_RIGHT,
            Section::Hooks => icons::BRACKETS_CURLY,
            Section::Commit => icons::GIT_COMMIT,
            Section::Lfs => icons::FILE_ARCHIVE,
        }
    }

    /// More words that find the section in the command palette.
    pub fn keywords(self) -> &'static str {
        match self {
            Section::General => "settings options fetch prune submodules default branch line endings autocrlf longpaths tabs",
            Section::Profiles => "identity author name email user account work personal",
            Section::Ssh => "keys agent private public generate",
            Section::Integrations => "github gitlab enterprise accounts sign in disconnect token ssh key",
            Section::Notifications => "desktop alerts toasts",
            Section::Appearance => "theme dark light colours colors toolbar date format avatars initials",
            Section::Graph => "columns author date sha message description refs branches tags commits history lazy load",
            Section::Gpg => "signing sign commits tags key openpgp x509 ssh signature",
            Section::Editor => "font size tab width line numbers word wrap syntax highlighting diff",
            Section::Tools => "merge tool diff tool editor terminal vscode open external",
            Section::Encoding => "charset utf-8 latin windows-1252 utf-16 text",
            Section::Gitflow => "git flow feature release hotfix develop master prefixes",
            Section::Hooks => "hooks path hookspath scripts",
            Section::Commit => "template message default action summary description comments",
            Section::Lfs => "large files git-lfs track",
        }
    }

    pub fn is_repo(self) -> bool {
        Section::REPO.contains(&self)
    }
}

/// Everything the page remembers while it is open; dropped when it closes.
pub struct PrefsView {
    pub section: Section,
    /// The user's global git configuration.
    pub global: ConfigFile,
    /// The front repository's own configuration, re-created when another
    /// repository comes to the front.
    pub repo: Option<ConfigFile>,
    scroll_to_top: bool,
    /// Each section's own state.
    pub states: States,
}

/// What each section remembers while preferences are open.
#[derive(Default)]
pub struct States {
    pub general: general::State,
    pub profiles: profiles::State,
    pub ssh: ssh::State,
    pub integrations: integrations::State,
    pub notifications: notifications::State,
    pub appearance: appearance::State,
    pub graph: graph::State,
    pub gpg: gpg::State,
    pub editor: editor::State,
    pub tools: tools::State,
    pub encoding: encoding::State,
    pub gitflow: gitflow::State,
    pub hooks: hooks::State,
    pub commit: commit::State,
    pub lfs: lfs::State,
}

impl PrefsView {
    pub fn new(section: Section) -> Self {
        PrefsView {
            section,
            global: ConfigFile::new(Scope::Global),
            repo: None,
            scroll_to_top: true,
            states: States::default(),
        }
    }

    pub fn open(&mut self, section: Section) {
        if self.section != section {
            self.section = section;
            self.scroll_to_top = true;
        }
    }
}

/// What a section's `show` works with.
pub struct Page<'a> {
    pub ctx: &'a egui::Context,
    /// Settings (`shared.settings`), toasts, dialogs.
    pub shared: &'a mut Shared,
    /// The repository tab in front. Always there for repository sections.
    pub tab: Option<&'a mut RepoTab>,
    /// The user's global git configuration (`~/.gitconfig`). Call
    /// `page.global.load(page.ctx)` before reading it.
    pub global: &'a mut ConfigFile,
    /// The front repository's `.git/config`; `Some` whenever `tab` is.
    pub repo: Option<&'a mut ConfigFile>,
}

/// Shows the page in place of the repository view. Called from the root
/// `ui`: it adds its own side panel and central panel.
pub fn show(ui: &mut Ui, shared: &mut Shared, tab: Option<&mut RepoTab>) {
    let Some(mut view) = shared.prefs.take() else { return };
    let ctx = ui.ctx().clone();

    // The repository sections follow the tab in front.
    match tab.as_deref() {
        Some(tab) => {
            if view.repo.as_ref().is_none_or(|file| file.scope != Scope::Repo(tab.path.clone())) {
                view.repo = Some(ConfigFile::new(Scope::Repo(tab.path.clone())));
            }
        }
        None => {
            view.repo = None;
            if view.section.is_repo() {
                view.open(Section::General);
            }
        }
    }
    view.global.poll();
    if let Some(repo) = &mut view.repo {
        repo.poll();
    }
    for error in [view.global.take_error(), view.repo.as_mut().and_then(ConfigFile::take_error)].into_iter().flatten() {
        shared.toast(ToastKind::Error, "Preference not saved", error);
    }

    let mut close = false;
    egui::Panel::left("prefs-nav")
        .resizable(false)
        .exact_size(250.0)
        .frame(Frame::new().fill(theme::bg_panel()))
        .show(ui, |ui| {
            close = nav(ui, shared, &mut view, tab.as_deref().map(|t| t.name.as_str()));
        });

    egui::CentralPanel::default().frame(Frame::new().fill(theme::bg_app())).show(ui, |ui| {
        let mut scroll = ScrollArea::vertical().auto_shrink([false, false]).id_salt(("prefs", view.section.title()));
        if std::mem::take(&mut view.scroll_to_top) {
            scroll = scroll.vertical_scroll_offset(0.0);
        }
        scroll.show(ui, |ui| {
            Frame::new().inner_margin(Margin { left: 40, right: 32, top: 26, bottom: 40 }).show(ui, |ui| {
                ui.set_max_width(1100.0);
                form::title(ui, view.section.title());
                let PrefsView { section, global, repo, states, .. } = &mut *view;
                let mut page = Page { ctx: &ctx, shared: &mut *shared, tab, global, repo: repo.as_mut() };
                section_ui(ui, *section, &mut page, states);
            });
        });
    });

    if !close && shared.prefs.is_none() {
        shared.prefs = Some(view);
    }
}

/// Hands the page to the section's module.
fn section_ui(ui: &mut Ui, section: Section, page: &mut Page<'_>, states: &mut States) {
    match section {
        Section::General => general::show(ui, page, &mut states.general),
        Section::Profiles => profiles::show(ui, page, &mut states.profiles),
        Section::Ssh => ssh::show(ui, page, &mut states.ssh),
        Section::Integrations => integrations::show(ui, page, &mut states.integrations),
        Section::Notifications => notifications::show(ui, page, &mut states.notifications),
        Section::Appearance => appearance::show(ui, page, &mut states.appearance),
        Section::Graph => graph::show(ui, page, &mut states.graph),
        Section::Gpg => gpg::show(ui, page, &mut states.gpg),
        Section::Editor => editor::show(ui, page, &mut states.editor),
        Section::Tools => tools::show(ui, page, &mut states.tools),
        Section::Encoding => encoding::show(ui, page, &mut states.encoding),
        Section::Gitflow => gitflow::show(ui, page, &mut states.gitflow),
        Section::Hooks => hooks::show(ui, page, &mut states.hooks),
        Section::Commit => commit::show(ui, page, &mut states.commit),
        Section::Lfs => lfs::show(ui, page, &mut states.lfs),
    }
}

const ROW: f32 = 40.0;

/// The section list. Returns true when "Exit Preferences" was clicked.
fn nav(ui: &mut Ui, shared: &Shared, view: &mut PrefsView, repo: Option<&str>) -> bool {
    let mut close = false;
    ScrollArea::vertical().auto_shrink([false, false]).id_salt("prefs-nav-scroll").show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        let width = ui.available_width();

        // Exit Preferences
        let (rect, response) = ui.allocate_exact_size(vec2(width, 46.0), Sense::click());
        let hovered = response.hovered();
        if hovered {
            ui.painter().rect_filled(rect, 0.0, theme::bg_hover());
        }
        let color = if hovered { theme::text_bright() } else { theme::text() };
        let x = widgets::text_left(ui.painter(), rect, rect.left() + 14.0, icons::SIGN_OUT, theme::font(17.0), color);
        widgets::text_left(ui.painter(), rect, rect.left() + 22.0 + x, "Exit Preferences", theme::font(16.0), color);
        if response.on_hover_text("Back to the repository (Esc)").clicked() {
            close = true;
        }

        if let Some(profile) = crate::prefs::identity::current_profile_name(&shared.settings.identity) {
            caption(ui, "Current profile");
            let (rect, response) = ui.allocate_exact_size(vec2(width, 38.0), Sense::click());
            let chip = rect.shrink2(vec2(8.0, 2.0));
            ui.painter().rect_filled(chip, 4.0, theme::bg_input());
            let x = widgets::text_left(ui.painter(), chip, chip.left() + 12.0, icons::USER_CIRCLE, theme::font(17.0), theme::accent());
            let galley = widgets::elided(ui.painter(), profile, theme::font(13.5), theme::text_bright(), chip.width() - x - 30.0);
            widgets::galley_left(ui.painter(), chip, chip.left() + x + 20.0, galley);
            if response.on_hover_text("Profiles").clicked() {
                view.open(Section::Profiles);
            }
            ui.add_space(8.0);
        }

        separator(ui);
        for section in Section::APP {
            nav_row(ui, view, section);
        }

        if let Some(repo) = repo {
            ui.add_space(14.0);
            separator(ui);
            ui.add_space(10.0);
            let (rect, _) = ui.allocate_exact_size(vec2(width, 22.0), Sense::hover());
            widgets::text_left(ui.painter(), rect, rect.left() + 16.0, "Repository-specific", theme::font(14.5), theme::text());
            let (rect, _) = ui.allocate_exact_size(vec2(width, 20.0), Sense::hover());
            let x = widgets::text_left(ui.painter(), rect, rect.left() + 16.0, icons::FOLDER, theme::font(12.5), theme::text_dim());
            let galley = widgets::elided(ui.painter(), &format!("Repo: {repo}"), theme::font(12.5), theme::text_dim(), width - x - 30.0);
            widgets::galley_left(ui.painter(), rect, rect.left() + x + 22.0, galley);
            ui.add_space(8.0);
            separator(ui);
            for section in Section::REPO {
                nav_row(ui, view, section);
            }
        }
        ui.add_space(12.0);
    });
    close
}

fn caption(ui: &mut Ui, text: &str) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
    widgets::text_left(ui.painter(), rect, rect.left() + 10.0, text, theme::font(13.0), theme::text_dim());
}

fn separator(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().hline(rect.x_range(), rect.center().y, Stroke::new(1.0, theme::border()));
}

fn nav_row(ui: &mut Ui, view: &mut PrefsView, section: Section) {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), ROW), Sense::click());
    let selected = view.section == section;
    widgets::row_background(ui.painter(), rect, selected, response.hovered());
    let color = if selected || response.hovered() { theme::text_bright() } else { theme::text() };
    let icon_color = if selected { theme::accent() } else { theme::text_dim() };
    let icon = Rect::from_center_size(pos2(rect.left() + 26.0, rect.center().y), vec2(20.0, 20.0));
    ui.painter().text(icon.center(), Align2::CENTER_CENTER, section.icon(), theme::font(17.0), icon_color);
    widgets::text_left(ui.painter(), rect, rect.left() + 50.0, section.title(), theme::font(15.5), color);
    if response.clicked() {
        view.open(section);
    }
}
