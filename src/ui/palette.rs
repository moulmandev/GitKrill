//! The command palette (Ctrl+P): a box near the top of the window that finds
//! actions, branches, tags, repositories and commits as you type, and runs
//! the one picked.
//!
//! Every entry calls the same function the toolbar, sidebar, menus or graph
//! call for that action; the palette has no way of doing anything of its own.
//! To offer something new, push an [`Entry`] in [`entries`] (look for "More
//! actions") with a closure that calls the existing function.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use eframe::egui::text::{LayoutJob, TextFormat, TextWrapping};
use eframe::egui::{
    self, Align, Align2, Color32, EventFilter, Frame, Galley, Id, Key, Layout, Margin, Modal, Modifiers,
    Painter, Rect, ScrollArea, Sense, Stroke, StrokeKind, TextEdit, UiBuilder, pos2, vec2,
};

use crate::app::{Zoom, zoom};
use crate::git::ops::{self, PullMode};
use crate::git::{ChangeKind, Oid, RepoSnapshot, RowKind, short};
use crate::platform;
use crate::state::{AppAction, Dialog, RepoTab, Settings, Shared};
use crate::theme::{self, icons};
use crate::ui::prefs::Section;
use crate::ui::widgets::elided;
use crate::ui::{graph, sidebar, toolbar};

const WIDTH: f32 = 620.0;
const ROW_H: f32 = 30.0;
/// Rows shown before the list scrolls.
const MAX_ROWS: usize = 12;
/// How many recently run entries are remembered.
const RECENT: usize = 6;

/// Open / closed state, the query and the selection.
#[derive(Default)]
pub struct Palette {
    open: bool,
    query: String,
    selected: usize,
    /// Rows to move the selection by, from the keys taken this frame.
    step: isize,
    /// Enter was pressed this frame.
    enter: bool,
    /// Bring the selected row into view.
    reveal: bool,
    /// Keys of the entries run lately, most recent first.
    recent: Vec<String>,
    /// What had the keyboard before the palette, to give it back.
    previous_focus: Option<Id>,
    /// Focus to hand back once the pass the palette closed in is over.
    refocus: Option<(Id, u64)>,
}

impl Palette {
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(&mut self, ctx: &egui::Context) {
        self.previous_focus = ctx.memory(|m| m.focused());
        // A context menu left open would sit on top of the palette.
        egui::Popup::close_all(ctx);
        self.open = true;
        self.query.clear();
        self.selected = 0;
        self.step = 0;
        self.enter = false;
        self.reveal = true;
    }

    fn close(&mut self) {
        self.open = false;
        self.query.clear();
        self.previous_focus = None;
    }

    /// Closes without running anything: the keyboard goes back to where it
    /// was, a half-written commit message for instance.
    fn dismiss(&mut self, ctx: &egui::Context) {
        if let Some(id) = self.previous_focus {
            self.refocus = Some((id, ctx.cumulative_pass_nr()));
            ctx.request_repaint();
        }
        self.close();
    }

    pub fn toggle(&mut self, ctx: &egui::Context) {
        if self.open { self.dismiss(ctx) } else { self.open(ctx) }
    }

    /// Takes Escape, the arrow keys and Enter before anything else sees
    /// them. Call it before any other keyboard handling while the palette is
    /// open.
    pub fn take_keys(&mut self, ctx: &egui::Context) {
        let none = Modifiers::NONE;
        let escape = ctx.input_mut(|i| {
            let count = |i: &mut egui::InputState, key| i.count_and_consume_key(none, key) as isize;
            self.step += count(i, Key::ArrowDown) - count(i, Key::ArrowUp);
            self.step += (count(i, Key::PageDown) - count(i, Key::PageUp)) * (MAX_ROWS as isize - 1);
            self.enter |= i.consume_key(none, Key::Enter);
            i.consume_key(none, Key::Escape)
        });
        if escape {
            self.dismiss(ctx);
        }
    }

    /// The entries and how they rank for the current query.
    fn build(&self, tabs: &[RepoTab], active: usize, settings: &Settings) -> (Vec<Entry>, Vec<Ranked>) {
        let mut list = entries(tabs, active, settings);
        if let Some(snap) = tabs.get(active).and_then(|t| t.snap.as_deref()) {
            list.extend(commits(snap, &self.query));
        }
        let mut ranked = rank(&list, &self.query, &self.recent);
        // Always last: look for the words in the commit messages instead.
        let query = self.query.trim();
        if !query.is_empty() && tabs.get(active).is_some() {
            ranked.push(Ranked { index: list.len(), positions: Vec::new() });
            list.push(search_for(query.to_owned()));
        }
        (list, ranked)
    }

    fn remember(&mut self, key: &str) {
        self.recent.retain(|k| k != key);
        self.recent.insert(0, key.to_owned());
        self.recent.truncate(RECENT);
    }

    /// Draws the palette over everything else and runs the entry picked.
    pub fn show(&mut self, ctx: &egui::Context, tabs: &mut [RepoTab], active: usize, shared: &mut Shared) {
        if !self.open {
            // Not in the pass the palette closed in: its layer still blocks
            // everything under it until that pass ends.
            if let Some((id, closed_in)) = self.refocus
                && ctx.cumulative_pass_nr() > closed_in
            {
                ctx.memory_mut(|m| m.request_focus(id));
                self.refocus = None;
            }
            return;
        }
        let (mut list, mut ranked) = self.build(tabs, active, &shared.settings);

        let count = ranked.len();
        if self.step != 0 && count > 0 {
            let last = count as isize - 1;
            let next = self.selected as isize + self.step;
            // Single steps wrap around the ends; page steps stop at them.
            self.selected = match self.step.abs() {
                1 if next < 0 => last,
                1 if next > last => 0,
                _ => next.clamp(0, last),
            } as usize;
            self.reveal = true;
        }
        self.step = 0;
        self.selected = self.selected.min(count.saturating_sub(1));
        let enter = std::mem::take(&mut self.enter);
        let mut chosen = (enter && count > 0).then_some(self.selected);
        let mut typed = false;

        let width = (ctx.content_rect().width() - 32.0).clamp(260.0, WIDTH);
        let area = Modal::default_area(Id::new("command-palette")).anchor(Align2::CENTER_TOP, vec2(0.0, 64.0));
        let frame = Frame::new()
            .fill(theme::bg_panel())
            .stroke(Stroke::new(1.0, theme::border_light()))
            .corner_radius(8.0)
            .shadow(ctx.global_style().visuals.popup_shadow)
            .inner_margin(Margin::same(6));
        let modal = Modal::new(Id::new("command-palette"))
            .area(area)
            .frame(frame)
            .backdrop_color(Color32::TRANSPARENT)
            .show(ctx, |ui| {
                ui.set_width(width);
                ui.spacing_mut().item_spacing = vec2(0.0, 0.0);

                // ---- the query ----
                let (field, _) = ui.allocate_exact_size(vec2(width, 36.0), Sense::hover());
                ui.painter().rect(field, 5.0, theme::bg_input(), Stroke::new(1.0, theme::accent()), StrokeKind::Inside);
                ui.painter().text(
                    pos2(field.left() + 12.0, field.center().y),
                    Align2::LEFT_CENTER,
                    icons::MAGNIFYING_GLASS,
                    theme::font(15.0),
                    theme::text_dim(),
                );
                let text_rect = Rect::from_min_max(pos2(field.left() + 34.0, field.top()), pos2(field.right() - 10.0, field.bottom()));
                let edit = ui
                    .scope_builder(UiBuilder::new().max_rect(text_rect).layout(Layout::left_to_right(Align::Center)), |ui| {
                        ui.add(
                            TextEdit::singleline(&mut self.query)
                                .id_salt("command-palette-query")
                                .hint_text("Search commands, branches, tags and commits")
                                .font(theme::font(14.5))
                                .frame(Frame::NONE)
                                .margin(Margin::ZERO)
                                .desired_width(text_rect.width())
                                // Arrows, Tab and Escape are the palette's;
                                // egui would otherwise move the focus away.
                                .event_filter(EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true }),
                        )
                    })
                    .inner;
                edit.request_focus();
                if edit.changed() {
                    self.selected = 0;
                    self.reveal = true;
                    typed = true;
                }
                ui.add_space(6.0);

                // ---- the list ----
                if count == 0 {
                    let (rect, _) = ui.allocate_exact_size(vec2(width, ROW_H), Sense::hover());
                    ui.painter().text(
                        pos2(rect.left() + 12.0, rect.center().y),
                        Align2::LEFT_CENTER,
                        "Nothing matches",
                        theme::font(13.0),
                        theme::text_faint(),
                    );
                    return;
                }
                // Both bounds: an area lays out in last frame's size, which
                // would otherwise cut the list short as it grows.
                let height = count.min(MAX_ROWS) as f32 * ROW_H;
                ScrollArea::vertical()
                    .id_salt("command-palette-list")
                    .min_scrolled_height(height)
                    .max_height(height)
                    .auto_shrink([false, true])
                    .show_viewport(ui, |ui, viewport| {
                        let (content, _) =
                            ui.allocate_exact_size(vec2(ui.available_width(), count as f32 * ROW_H), Sense::hover());
                        let row_rect = |i: usize| {
                            Rect::from_min_size(
                                pos2(content.left(), content.top() + i as f32 * ROW_H),
                                vec2(content.width(), ROW_H),
                            )
                        };
                        if std::mem::take(&mut self.reveal) {
                            // At once: wrapping from the top to the bottom is
                            // a long way to animate.
                            let instant = egui::style::ScrollAnimation::none();
                            ui.scroll_to_rect_animation(row_rect(self.selected), None, instant);
                        }
                        let first = (viewport.min.y / ROW_H).floor().max(0.0) as usize;
                        let end = ((viewport.max.y / ROW_H).ceil().max(0.0) as usize).min(count);
                        for (i, row) in ranked.iter().enumerate().take(end).skip(first) {
                            let rect = row_rect(i);
                            let response = ui.interact(rect, ui.id().with(("palette-row", i)), Sense::click());
                            paint_row(ui.painter(), rect, &list[row.index], &row.positions, i == self.selected, response.hovered());
                            if response.clicked() {
                                chosen = Some(i);
                            }
                        }
                    });
            });

        let outside = &modal.backdrop_response;
        if outside.clicked() || outside.secondary_clicked() || outside.drag_started() {
            self.dismiss(ctx);
            return;
        }
        if enter && typed {
            // Typed and pressed Enter within one frame: Enter is for the best
            // match of what is in the field now, not of what was there before.
            (list, ranked) = self.build(tabs, active, &shared.settings);
            chosen = (!ranked.is_empty()).then_some(0);
        }
        let Some(row) = chosen else { return };
        let entry = list.swap_remove(ranked[row].index);
        self.remember(&entry.key);
        self.close();
        let mut env = Env { ctx, shared, tab: tabs.get_mut(active), active };
        (entry.run)(&mut env);
    }
}

// ---- entries ---------------------------------------------------------------

/// What an entry can act on when it runs.
pub struct Env<'a> {
    pub ctx: &'a egui::Context,
    pub shared: &'a mut Shared,
    /// The repository tab in front, if any.
    pub tab: Option<&'a mut RepoTab>,
    /// Its index among the tabs.
    pub active: usize,
}

/// Breaks ties between equally good matches, and orders the full list.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Kind {
    Commit,
    Action,
    Branch,
    Tag,
    Repository,
}

type Run = Box<dyn FnOnce(&mut Env<'_>)>;

pub struct Entry {
    /// Stable identity, for "recently run".
    pub key: String,
    pub kind: Kind,
    pub icon: &'static str,
    pub label: String,
    /// Dim text on the right.
    pub detail: String,
    pub shortcut: &'static str,
    /// More words that find the entry ("sidebar" for the left panel).
    pub keywords: &'static str,
    /// Found by something other than its label (a SHA prefix): always shown,
    /// first, with these characters of the label highlighted.
    pinned: Option<Vec<usize>>,
    run: Run,
}

impl Entry {
    pub fn new(
        kind: Kind,
        key: impl Into<String>,
        icon: &'static str,
        label: impl Into<String>,
        run: impl FnOnce(&mut Env<'_>) + 'static,
    ) -> Self {
        Entry {
            key: key.into(),
            kind,
            icon,
            label: label.into(),
            detail: String::new(),
            shortcut: "",
            keywords: "",
            pinned: None,
            run: Box::new(run),
        }
    }

    /// An action on the repository in front.
    pub fn on_tab(
        key: impl Into<String>,
        icon: &'static str,
        label: impl Into<String>,
        run: impl FnOnce(&mut RepoTab, &mut Shared) + 'static,
    ) -> Self {
        Self::new(Kind::Action, key, icon, label, on_tab(run))
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    pub fn shortcut(mut self, shortcut: &'static str) -> Self {
        self.shortcut = shortcut;
        self
    }

    pub fn keywords(mut self, keywords: &'static str) -> Self {
        self.keywords = keywords;
        self
    }
}

/// Runs `run` on the active tab, if there is one when the entry runs.
fn on_tab(run: impl FnOnce(&mut RepoTab, &mut Shared) + 'static) -> impl FnOnce(&mut Env<'_>) + 'static {
    move |env: &mut Env<'_>| {
        if let Some(tab) = env.tab.as_deref_mut() {
            run(tab, env.shared);
        }
    }
}

/// Preferences > Editor switches that are handy while reading a diff.
fn editor_actions(list: &mut Vec<Entry>, settings: &Settings) {
    let editor = &settings.editor;
    let label = if editor.word_wrap { "Turn word wrap off" } else { "Turn word wrap on" };
    list.push(
        Entry::new(Kind::Action, "word-wrap", icons::TEXT_INDENT, label, |env: &mut Env<'_>| {
            let editor = &mut env.shared.settings.editor;
            editor.word_wrap = !editor.word_wrap;
        })
        .keywords("wrap long lines diff blame editor"),
    );
    let label = if editor.line_numbers { "Hide line numbers" } else { "Show line numbers" };
    list.push(
        Entry::new(Kind::Action, "line-numbers", icons::LIST_NUMBERS, label, |env: &mut Env<'_>| {
            let editor = &mut env.shared.settings.editor;
            editor.line_numbers = !editor.line_numbers;
        })
        .keywords("gutter diff blame editor"),
    );
}

/// Everything the palette offers right now, in the order shown for an empty
/// query. Entries that cannot run at the moment are left out.
pub fn entries(tabs: &[RepoTab], active: usize, settings: &Settings) -> Vec<Entry> {
    let mut list = Vec::new();
    let tab = tabs.get(active);
    let snap = tab.and_then(|t| t.snap.as_deref());

    if let Some(tab) = tab {
        repository_actions(&mut list, tab, snap);
    }
    app_actions(&mut list, tab.is_some(), settings);

    // More actions: push further entries here. `Entry::on_tab` is for
    // anything that needs the repository in front; call the function the
    // rest of the UI uses for it, and leave the entry out when it cannot run.
    editor_actions(&mut list, settings);
    crate::ui::diff_mode::palette_entries(&mut list, settings);
    crate::ui::conflict::palette_entries(&mut list, snap, settings);
    if let Some(shown) = tab.and_then(|tab| tab.hosting.connected()) {
        let service = shown.repo.service;
        list.push(
            Entry::on_tab("create-pr", if service.is_gitlab() { icons::GIT_MERGE } else { icons::GIT_PULL_REQUEST }, format!("Create {}", service.pull_word()), |tab, shared| {
                crate::ui::new_pull::open(tab, shared, None)
            })
            .detail(format!("on {}", shown.repo.site()))
            .keywords("pull request merge request mr pr open propose"),
        );
    }

    if let (Some(tab), Some(snap)) = (tab, snap) {
        crate::ui::gitflow::palette_entries(&mut list, tab, snap);
        crate::ui::remotes::palette_entries(&mut list, tab, snap);
        crate::ui::multi::palette_entries(&mut list, tab, snap);
        crate::ui::stash::palette_entries(&mut list, tab, snap);
    }
    if let Some(tab) = tab {
        external_tools(&mut list, tab, snap, settings);
    }
    if tab.is_some_and(|tab| !tab.hidden.is_empty()) {
        list.push(
            Entry::on_tab("show-hidden", icons::EYE, "Show everything hidden from the graph", |tab, shared| {
                tab.set_hidden(&mut shared.settings, "refs/", false)
            })
            .detail("branches, folders, remotes and tags")
            .keywords("hidden unhide reveal show all branches folders tags remotes graph"),
        );
    }

    if let Some(snap) = snap {
        branches_and_tags(&mut list, snap);
    }
    repositories(&mut list, tabs, active, settings);
    list
}

fn repository_actions(list: &mut Vec<Entry>, tab: &RepoTab, snap: Option<&RepoSnapshot>) {
    if let Some(snap) = snap {
        let has_remote = !snap.remotes.is_empty();
        let branch = snap.current_branch();
        let upstream = branch.and_then(|b| b.upstream.clone());
        let on_branch = has_remote && branch.is_some();

        // The toolbar's buttons, under the same conditions it enables them.
        if has_remote && !tab.is_running("Fetch") {
            list.push(Entry::on_tab("fetch", icons::ARROWS_CLOCKWISE, "Fetch", |tab, _| toolbar::fetch(tab)).detail("all remotes"));
        }
        if on_branch && !tab.is_running("Pull") && !tab.is_running("Fetch") {
            let from = upstream.as_ref().map(|u| format!("from {u}")).unwrap_or_default();
            let pulls = [
                ("pull", "Pull", PullMode::Default),
                ("pull-ff", "Pull (fast-forward only)", PullMode::FastForwardOnly),
                ("pull-rebase", "Pull (rebase)", PullMode::Rebase),
            ];
            for (key, label, mode) in pulls {
                list.push(Entry::on_tab(key, icons::ARROW_LINE_DOWN, label, move |tab, _| toolbar::pull(tab, mode)).detail(from.clone()));
            }
        }
        if on_branch && !tab.is_running("Push") && !tab.is_running("Force push") {
            let to = match (&upstream, snap.default_remote()) {
                (Some(upstream), _) => format!("to {upstream}"),
                (None, Some(remote)) => format!("to {remote}, sets the upstream"),
                (None, None) => String::new(),
            };
            list.push(Entry::on_tab("push", icons::ARROW_LINE_UP, "Push", |tab, _| toolbar::push(tab)).detail(to.clone()));
            list.push(Entry::on_tab("force-push", icons::WARNING, "Force push (with lease)", toolbar::force_push).detail(to));
        }
        if !tab.is_running("Create branch") {
            list.push(
                Entry::on_tab("branch", icons::GIT_BRANCH, "Create branch", |_, shared| shared.show(toolbar::new_branch_dialog()))
                    .detail(format!("at {}", snap.head_label())),
            );
        }
        if !snap.status.is_clean() && !tab.is_running("Stash changes") {
            let files = snap.status.file_count();
            let detail = if files == 1 { "1 changed file".to_owned() } else { format!("{files} changed files") };
            list.push(Entry::on_tab("stash", icons::TRAY_ARROW_DOWN, "Stash", |tab, _| toolbar::stash(tab)).detail(detail));
        }
        if let Some(top) = snap.stashes.first()
            && !tab.is_running("Pop stash")
        {
            list.push(Entry::on_tab("pop", icons::TRAY_ARROW_UP, "Pop stash", |tab, _| toolbar::pop_stash(tab)).detail(top.message.clone()));
        }
    }
    if let Some(last) = tab.undo.last()
        && !tab.is_running("Undo ")
    {
        list.push(Entry::on_tab("undo", icons::ARROW_COUNTER_CLOCKWISE, "Undo", |tab, _| tab.undo()).detail(last.label.clone()));
    }
    if let Some(last) = tab.redo.last()
        && !tab.is_running("Redo ")
    {
        list.push(Entry::on_tab("redo", icons::ARROW_CLOCKWISE, "Redo", |tab, _| tab.redo()).detail(last.label.clone()));
    }
    list.push(Entry::on_tab("search", icons::MAGNIFYING_GLASS, "Search commits", |tab, _| graph::open_search(tab)).shortcut("Ctrl+F"));
    list.push(Entry::on_tab("refresh", icons::ARROWS_CLOCKWISE, "Refresh", |tab, _| tab.refresh()).shortcut("F5"));
    list.push(
        Entry::on_tab("terminal", icons::TERMINAL_WINDOW, "Open a terminal here", |tab, shared| {
            crate::external::terminal(&shared.settings.tools, &tab.path)
        })
            .keywords("shell console command prompt"),
    );
    list.push(
        Entry::on_tab("reveal", icons::FOLDER_SIMPLE, "Show in file manager", |tab, _| platform::reveal(&tab.path))
            .keywords("explorer finder folder"),
    );
}

/// The editor and merge tool of Preferences > External Tools.
fn external_tools(list: &mut Vec<Entry>, tab: &RepoTab, snap: Option<&RepoSnapshot>, settings: &Settings) {
    let tools = &settings.tools;
    if tools.has_editor() {
        list.push(
            Entry::on_tab("open-in-editor", icons::CODE, "Open repository in editor", |tab, shared| {
                crate::external::edit(&shared.settings.tools, &tab.path)
            })
            .detail(tools.editor_name().to_owned())
            .keywords("external ide code folder"),
        );
    }
    let conflicted = snap.into_iter().flat_map(|snap| &snap.status.unstaged).filter(|f| f.kind == ChangeKind::Conflicted);
    for file in conflicted.take(20) {
        if tab.is_running(&crate::external::merge_label(tools, &file.path)) {
            continue;
        }
        let path = file.path.clone();
        list.push(
            Entry::on_tab(format!("mergetool:{path}"), icons::GIT_MERGE, "Open in merge tool", move |tab, shared| {
                if let Some(cmd) = crate::external::merge(&shared.settings.tools, &path) {
                    tab.run(cmd);
                }
            })
            .detail(file.path.clone())
            .keywords("conflict resolve external mergetool"),
        );
    }
}

fn app_actions(list: &mut Vec<Entry>, has_tab: bool, settings: &Settings) {
    let action = |key: &str, icon, label: &str, run: fn(&mut Env<'_>)| Entry::new(Kind::Action, key, icon, label, run);

    list.push(action("open", icons::FOLDER_OPEN, "Open repository", |env| env.shared.actions.push(AppAction::OpenDialog)).shortcut("Ctrl+O"));
    list.push(action("clone", icons::CLOUD_ARROW_DOWN, "Clone repository", |env| env.shared.show(toolbar::clone_dialog())));
    list.push(action("init", icons::PLUS, "Init repository", |env| env.shared.actions.push(AppAction::InitDialog)).keywords("new create"));
    list.push(
        action("add-workspace", icons::FOLDERS, "Add a folder of repositories", |env| {
            env.shared.actions.push(AppAction::AddWorkspace(crate::workspace::Opens::Here))
        })
        .keywords("workspace projects parent scan"),
    );
    if has_tab {
        list.push(action("close-tab", icons::X, "Close tab", |env| env.shared.actions.push(AppAction::CloseTab(env.active))).shortcut("Ctrl+W"));
    }

    // The View menu, the graph's column picker and the Path / Tree switch.
    let toggle = |key: &str, icon, on: bool, labels: (&str, &str), keywords, flip: fn(&mut Settings)| {
        let label = if on { labels.0 } else { labels.1 };
        Entry::new(Kind::Action, key, icon, label, move |env: &mut Env<'_>| flip(&mut env.shared.settings)).keywords(keywords)
    };
    list.push(toggle(
        "sidebar",
        icons::SIDEBAR_SIMPLE,
        settings.show_sidebar,
        ("Hide left panel", "Show left panel"),
        "sidebar view",
        |s| s.show_sidebar = !s.show_sidebar,
    ));
    list.push(toggle(
        "column-refs",
        icons::COLUMNS,
        settings.graph.show_refs,
        ("Hide Branch / Tag column", "Show Branch / Tag column"),
        "graph columns view labels branches tags refs",
        |s| s.graph.show_refs = !s.graph.show_refs,
    ));
    list.push(toggle(
        "column-graph",
        icons::COLUMNS,
        settings.graph.show_tree,
        ("Hide Graph column", "Show Graph column"),
        "graph columns view commit tree lanes",
        |s| s.graph.show_tree = !s.graph.show_tree,
    ));
    list.push(toggle(
        "column-message",
        icons::COLUMNS,
        settings.graph.show_message,
        ("Hide Commit message column", "Show Commit message column"),
        "graph columns view summary",
        |s| s.graph.show_message = !s.graph.show_message,
    ));
    list.push(toggle(
        "column-author",
        icons::COLUMNS,
        settings.show_author,
        ("Hide Author column", "Show Author column"),
        "graph columns view",
        |s| s.show_author = !s.show_author,
    ));
    list.push(toggle(
        "column-date",
        icons::COLUMNS,
        settings.show_date,
        ("Hide Date column", "Show Date column"),
        "graph columns view time",
        |s| s.show_date = !s.show_date,
    ));
    list.push(toggle(
        "column-sha",
        icons::COLUMNS,
        settings.show_sha,
        ("Hide SHA column", "Show SHA column"),
        "graph columns view hash",
        |s| s.show_sha = !s.show_sha,
    ));
    if settings.graph.show_message {
        use crate::prefs::graph::Description;
        list.push(toggle(
            "column-description",
            icons::COLUMNS,
            settings.graph.description == Description::Always,
            ("Hide commit descriptions in the graph", "Show commit descriptions in the graph"),
            "graph columns view body message",
            |s| {
                let always = s.graph.description == Description::Always;
                s.graph.description = if always { Description::Never } else { Description::Always };
            },
        ));
    }
    list.push(toggle(
        "tree-view",
        if settings.tree_view { icons::LIST } else { icons::TREE_VIEW },
        settings.tree_view,
        ("Show changed files as paths", "Show changed files as a tree"),
        "tree view list folders",
        |s| s.tree_view = !s.tree_view,
    ));
    let auto_fetch = crate::prefs::general::auto_fetch_label(settings);
    list.push(toggle(
        "auto-fetch",
        icons::CLOCK_COUNTER_CLOCKWISE,
        settings.auto_fetch_minutes > 0,
        ("Stop fetching automatically", &auto_fetch),
        "auto background interval",
        crate::prefs::general::toggle_auto_fetch,
    ));
    list.push(toggle(
        "avatars",
        icons::USER_CIRCLE,
        settings.fetch_avatars,
        ("Hide author pictures", "Show author pictures"),
        "avatars gravatar",
        |s| s.fetch_avatars = !s.fetch_avatars,
    ));
    list.push(toggle(
        "toolbar-labels",
        icons::TEXT_AA,
        settings.appearance.toolbar_labels,
        ("Hide toolbar labels", "Show toolbar labels"),
        "icons compact toolbar",
        |s| s.appearance.toolbar_labels = !s.appearance.toolbar_labels,
    ));
    if crate::update::Kind::detect() != crate::update::Kind::Off {
        list.push(action("updates", icons::ARROW_CIRCLE_UP, "Check for updates", |env| {
            crate::ui::update::check_now(env.ctx, env.shared)
        }));
    }
    list.push(toggle(
        "hosting",
        icons::GIT_PULL_REQUEST,
        settings.hosting_integration,
        ("Hide pull requests, issues and CI", "Show pull requests, issues and CI"),
        "github gitlab hosting actions pipelines merge requests",
        |s| s.hosting_integration = !s.hosting_integration,
    ));

    list.push(action("zoom-in", icons::MAGNIFYING_GLASS_PLUS, "Zoom in", |env| zoom(env.ctx, Zoom::In)).shortcut("Ctrl++"));
    list.push(action("zoom-out", icons::MAGNIFYING_GLASS_MINUS, "Zoom out", |env| zoom(env.ctx, Zoom::Out)).shortcut("Ctrl+-"));
    list.push(action("zoom-reset", icons::MAGNIFYING_GLASS, "Reset zoom", |env| zoom(env.ctx, Zoom::Reset)).shortcut("Ctrl+0"));
    list.push(action("about", icons::INFO, "About GitKrill", |env| env.shared.show(Dialog::About)).keywords("version"));

    list.push(
        action("prefs", icons::GEAR, "Preferences", |env| env.shared.actions.push(AppAction::OpenPreferences(Section::General)))
            .shortcut("Ctrl+,")
            .keywords("settings options configure"),
    );
    for section in Section::APP.into_iter().chain(Section::REPO.into_iter().filter(|_| has_tab)) {
        let label = format!("Preferences: {}", section.title());
        let entry = Entry::new(Kind::Action, format!("prefs-{}", section.title()), section.icon(), label, move |env: &mut Env<'_>| {
            env.shared.actions.push(AppAction::OpenPreferences(section))
        });
        list.push(entry.detail(if section.is_repo() { "this repository" } else { "" }).keywords(section.keywords()));
    }
    for (index, name) in crate::ui::prefs::profiles::switchable(settings) {
        let label = format!("Switch to profile {name}");
        let entry = Entry::new(Kind::Action, format!("profile-{name}"), icons::USER_SWITCH, label, move |env: &mut Env<'_>| {
            crate::ui::prefs::profiles::switch(env.shared, index)
        });
        list.push(entry.keywords("identity author email account work personal"));
    }
}

fn branches_and_tags(list: &mut Vec<Entry>, snap: &RepoSnapshot) {
    for branch in snap.branches.iter().filter(|b| !b.is_head) {
        let mut detail = String::from("local");
        if branch.ahead > 0 {
            detail.push_str(&format!("  {}{}", branch.ahead, icons::ARROW_UP));
        }
        if branch.behind > 0 {
            detail.push_str(&format!("  {}{}", branch.behind, icons::ARROW_DOWN));
        }
        let name = branch.name.clone();
        list.push(
            Entry::new(
                Kind::Branch,
                format!("checkout:{name}"),
                icons::GIT_BRANCH,
                format!("Checkout {name}"),
                on_tab(move |tab, _| tab.run(ops::checkout(&name))),
            )
            .detail(detail),
        );
    }
    // Checking out a remote branch creates its tracking branch, exactly as
    // a double-click in the sidebar does. The one whose local branch is
    // already checked out would do nothing.
    for branch in &snap.remote_branches {
        if snap.head.branch.as_deref() == Some(branch.name.as_str()) {
            continue;
        }
        let remote = branch.clone();
        list.push(
            Entry::new(
                Kind::Branch,
                format!("checkout-remote:{}", branch.full),
                icons::CLOUD,
                format!("Checkout {}", branch.full),
                on_tab(move |tab, _| {
                    if let Some(snap) = tab.snap.clone() {
                        tab.run(sidebar::checkout_remote(&snap, &remote));
                    }
                }),
            )
            .detail("remote"),
        );
    }
    // Tags jump to their commit, as clicking one in the sidebar does; only
    // those whose commit is in the loaded part of the history.
    for tag in snap.tags.iter().filter(|t| snap.row_of(t.oid).is_some()) {
        let oid = tag.oid;
        list.push(
            Entry::new(
                Kind::Tag,
                format!("tag:{}", tag.name),
                icons::TAG,
                format!("Go to tag {}", tag.name),
                on_tab(move |tab, _| tab.jump_to(oid)),
            )
            .detail(short(oid)),
        );
    }
}

fn repositories(list: &mut Vec<Entry>, tabs: &[RepoTab], active: usize, settings: &Settings) {
    for (i, tab) in tabs.iter().enumerate().filter(|(i, _)| *i != active) {
        list.push(
            Entry::new(
                Kind::Repository,
                format!("tab:{}", tab.path.display()),
                icons::BROWSERS,
                format!("Switch to {}", tab.name),
                move |env: &mut Env<'_>| env.shared.actions.push(AppAction::Activate(i)),
            )
            .detail(tab.path.display().to_string()),
        );
    }
    // Repositories in the workspaces, then recent ones, that no tab shows.
    // They open in the current tab, as from the repository picker.
    let open = |path: &PathBuf| tabs.iter().any(|t| &t.path == path);
    let mut listed = Vec::new();
    for ws in &settings.workspaces {
        for path in &ws.repos {
            if open(path) || listed.contains(path) {
                continue;
            }
            let folder = ws.folder_of(path);
            let place = if folder.is_empty() { ws.name() } else { format!("{}/{folder}", ws.name()) };
            list.push(open_entry(path, format!("in {place}")));
            listed.push(path.clone());
        }
    }
    for path in settings.recent.iter().filter(|p| !open(p) && !listed.contains(*p)) {
        list.push(open_entry(path, path.display().to_string()));
    }
}

fn open_entry(path: &Path, detail: String) -> Entry {
    let switch = path.to_path_buf();
    Entry::new(
        Kind::Repository,
        format!("repo:{}", path.display()),
        icons::FOLDER_SIMPLE,
        format!("Open {}", crate::workspace::display_name(path)),
        move |env: &mut Env<'_>| env.shared.actions.push(AppAction::Switch(switch)),
    )
    .detail(detail)
}

/// "Go to commit" for each loaded commit whose SHA starts with `query`,
/// when the query looks like one (four or more hex digits).
pub fn commits(snap: &RepoSnapshot, query: &str) -> Vec<Entry> {
    let query = query.trim().to_lowercase();
    if query.len() < 4 || query.len() > 40 || !query.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Vec::new();
    }
    const PREFIX: &str = "Go to commit ";
    snap.history
        .commits
        .iter()
        .filter(|c| c.kind != RowKind::Wip && sha_starts_with(c.oid, &query))
        .take(8)
        .map(|commit| {
            let sha = short(commit.oid);
            let start = PREFIX.len();
            let highlight = (start..start + query.len().min(sha.len())).collect();
            let oid = commit.oid;
            let mut entry = Entry::new(
                Kind::Commit,
                format!("commit:{oid}"),
                icons::GIT_COMMIT,
                format!("{PREFIX}{sha} {}", commit.summary),
                on_tab(move |tab, _| tab.jump_to(oid)),
            )
            .detail(commit.author.clone());
            entry.pinned = Some(highlight);
            entry
        })
        .collect()
}

/// Whether the SHA of `oid` starts with the lowercase hex digits `hex`,
/// without spelling out every SHA of a long history.
fn sha_starts_with(oid: Oid, hex: &str) -> bool {
    let bytes = oid.as_bytes();
    hex.len() <= bytes.len() * 2
        && hex.bytes().enumerate().all(|(i, digit)| {
            let byte = bytes[i / 2];
            let nibble = if i % 2 == 0 { byte >> 4 } else { byte & 0x0f };
            (digit as char).to_digit(16) == Some(nibble as u32)
        })
}

/// Offered under every query: the graph's own search, for words that are in
/// commit messages rather than in entry names.
fn search_for(query: String) -> Entry {
    let label = format!("Search commits for \u{201c}{query}\u{201d}");
    Entry::on_tab("search-for", icons::MAGNIFYING_GLASS, label, move |tab, _| {
        graph::open_search(tab);
        tab.search = query;
    })
    .detail("messages, authors and SHAs")
}

// ---- matching and ranking ---------------------------------------------------

/// How much lower a match on an entry's keywords scores than one on its label.
const KEYWORD_PENALTY: i32 = 24;
/// Extra points for having been run lately, enough to settle near ties.
const RECENT_BONUS: i32 = 6;

pub struct Ranked {
    /// Into the entry list.
    pub index: usize,
    /// Characters of the label to highlight.
    pub positions: Vec<usize>,
}

/// The entries that match `query`, best first. An empty query lists every
/// entry, the ones run lately first.
pub fn rank(entries: &[Entry], query: &str, recent: &[String]) -> Vec<Ranked> {
    let query: String = query.chars().filter(|c| !c.is_whitespace()).collect();
    if query.is_empty() {
        let recent_first = recent.iter().filter_map(|key| entries.iter().position(|e| &e.key == key));
        let rest = (0..entries.len()).filter(|i| !recent.contains(&entries[*i].key));
        return recent_first
            .chain(rest)
            .map(|index| Ranked { index, positions: Vec::new() })
            .collect();
    }

    let mut scored: Vec<(i32, Ranked)> = entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            if let Some(positions) = &entry.pinned {
                return Some((i32::MAX, Ranked { index, positions: positions.clone() }));
            }
            let by_label = fuzzy(&query, &entry.label);
            let by_keyword = fuzzy(&query, entry.keywords).map(|m| Match { score: m.score - KEYWORD_PENALTY, positions: Vec::new() });
            let best = match (by_label, by_keyword) {
                (Some(a), Some(b)) => if b.score > a.score { b } else { a },
                (a, b) => a.or(b)?,
            };
            let bonus = if recent.contains(&entry.key) { RECENT_BONUS } else { 0 };
            Some((best.score + bonus, Ranked { index, positions: best.positions }))
        })
        .collect();
    scored.sort_by(|(sa, a), (sb, b)| {
        let (ea, eb) = (&entries[a.index], &entries[b.index]);
        sb.cmp(sa)
            .then(ea.kind.cmp(&eb.kind))
            .then(ea.label.chars().count().cmp(&eb.label.chars().count()))
            .then(a.index.cmp(&b.index))
    });
    scored.into_iter().map(|(_, ranked)| ranked).collect()
}

#[derive(Debug, PartialEq, Eq)]
pub struct Match {
    /// Higher is better.
    pub score: i32,
    /// Character indices of `text` that matched, ascending.
    pub positions: Vec<usize>,
}

const MATCH: i32 = 16;
/// A match at the start of a word: "cb" finds "Create branch".
const WORD_START: i32 = 24;
/// A match right after the previous one: "fet" in "Fetch".
const CONSECUTIVE: i32 = 20;
/// Per character skipped between two matches.
const GAP: i32 = 2;
/// Per character before the first match, up to [`LEADING_MAX`].
const LEADING: i32 = 1;
const LEADING_MAX: i32 = 12;

/// Fuzzy match: every character of `query` must appear in `text` in order,
/// ignoring case. Among the ways to match, the best scoring one is taken:
/// runs of consecutive characters and the starts of words score higher,
/// skipped characters lower.
pub fn fuzzy(query: &str, text: &str) -> Option<Match> {
    let query: Vec<char> = query.chars().filter(|c| !c.is_whitespace()).map(lower).collect();
    if query.is_empty() {
        return Some(Match { score: 0, positions: Vec::new() });
    }
    let chars: Vec<char> = text.chars().collect();
    let folded: Vec<char> = chars.iter().copied().map(lower).collect();
    let (n, m) = (chars.len(), query.len());

    // Cheap rejection before the full search.
    let mut it = folded.iter();
    if !query.iter().all(|q| it.any(|c| c == q)) {
        return None;
    }

    let bonus: Vec<i32> = (0..n).map(|i| if word_start(&chars, i) { WORD_START } else { 0 }).collect();
    const NONE: i32 = i32::MIN / 2;
    // best[j * n + i]: the best score with query[..=j] matched and query[j]
    // landing on text[i]; from[...] is where query[j - 1] landed for it.
    let mut best = vec![NONE; m * n];
    let mut from = vec![usize::MAX; m * n];
    for i in 0..n {
        if folded[i] == query[0] {
            best[i] = MATCH + bonus[i] - (i as i32 * LEADING).min(LEADING_MAX);
        }
    }
    for j in 1..m {
        let (prev, cur) = best.split_at_mut(j * n);
        let prev = &prev[(j - 1) * n..];
        let cur = &mut cur[..n];
        // The best earlier landing of query[j - 1], less the gap since.
        let mut carry = NONE;
        let mut carry_from = usize::MAX;
        for i in 0..n {
            if i > 0 && folded[i] == query[j] {
                let mut score = NONE;
                if carry > NONE {
                    score = carry + MATCH + bonus[i];
                    from[j * n + i] = carry_from;
                }
                if prev[i - 1] > NONE {
                    let run = prev[i - 1] + MATCH + bonus[i] + CONSECUTIVE;
                    if run >= score {
                        score = run;
                        from[j * n + i] = i - 1;
                    }
                }
                cur[i] = score;
            }
            if carry > NONE {
                carry -= GAP;
            }
            if prev[i] > carry {
                carry = prev[i];
                carry_from = i;
            }
        }
    }

    let last = &best[(m - 1) * n..];
    let (mut at, &score) = last.iter().enumerate().max_by_key(|(i, s)| (**s, std::cmp::Reverse(*i)))?;
    if score <= NONE {
        return None;
    }
    let mut positions = vec![0; m];
    for j in (0..m).rev() {
        positions[j] = at;
        if j > 0 {
            at = from[j * n + at];
        }
    }
    Some(Match { score, positions })
}

fn lower(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Whether `chars[i]` starts a word: the first character, one after a
/// space or punctuation, a capital after a small letter, a digit after a
/// letter.
fn word_start(chars: &[char], i: usize) -> bool {
    let Some(&c) = chars.get(i) else { return false };
    if !c.is_alphanumeric() {
        return false;
    }
    let Some(&prev) = i.checked_sub(1).and_then(|p| chars.get(p)) else { return true };
    !prev.is_alphanumeric()
        || (prev.is_lowercase() && c.is_uppercase())
        || (prev.is_alphabetic() && c.is_numeric())
}

// ---- drawing ------------------------------------------------------------------

fn paint_row(painter: &Painter, rect: Rect, entry: &Entry, positions: &[usize], selected: bool, hovered: bool) {
    if selected {
        painter.rect_filled(rect, 4.0, theme::bg_raised());
        painter.rect_filled(Rect::from_min_max(rect.min, pos2(rect.left() + 3.0, rect.bottom())), 1.5, theme::accent());
    } else if hovered {
        painter.rect_filled(rect, 4.0, theme::bg_hover());
    }
    let y = rect.center().y;
    let icon_color = if selected { theme::accent() } else { theme::text_dim() };
    painter.text(pos2(rect.left() + 13.0, y), Align2::LEFT_CENTER, entry.icon, theme::font(15.0), icon_color);

    let mut right = rect.right() - 10.0;
    if !entry.shortcut.is_empty() {
        let galley = painter.layout_no_wrap(entry.shortcut.to_owned(), theme::font(11.0), theme::text_dim());
        let key = Rect::from_min_max(
            pos2(right - galley.size().x - 12.0, y - 10.0),
            pos2(right, y + 10.0),
        );
        painter.rect_stroke(key, 3.0, Stroke::new(1.0, theme::border_light()), StrokeKind::Inside);
        let at = key.center() - galley.size() / 2.0;
        painter.galley(pos2(at.x.round(), at.y.round()), galley, theme::text_dim());
        right = key.left() - 10.0;
    }
    let label_x = rect.left() + 38.0;
    if !entry.detail.is_empty() {
        let max = ((right - label_x) * 0.45).max(0.0);
        let galley = elided(painter, &entry.detail, theme::font(12.0), theme::text_dim(), max);
        let x = right - galley.size().x;
        painter.galley(pos2(x, (y - galley.size().y / 2.0).round()), galley, theme::text_dim());
        right = x - 14.0;
    }
    let galley = highlighted(painter, &entry.label, positions, selected, right - label_x);
    painter.galley(pos2(label_x, (y - galley.size().y / 2.0).round()), galley, theme::text());
}

/// `text` on one elided line with the characters at `positions` picked out.
fn highlighted(painter: &Painter, text: &str, positions: &[usize], selected: bool, max_width: f32) -> Arc<Galley> {
    let plain = TextFormat::simple(theme::font(13.5), if selected { theme::text_bright() } else { theme::text() });
    let hit = TextFormat::simple(theme::bold(13.5), theme::accent());
    let mut job = LayoutJob::default();
    let mut run = String::new();
    let mut run_hit = false;
    let mut next = positions.iter().copied().peekable();
    for (i, c) in text.chars().enumerate() {
        let is_hit = next.peek() == Some(&i);
        if is_hit {
            next.next();
        }
        if is_hit != run_hit && !run.is_empty() {
            job.append(&std::mem::take(&mut run), 0.0, if run_hit { hit.clone() } else { plain.clone() });
        }
        run_hit = is_hit;
        run.push(c);
    }
    if !run.is_empty() {
        job.append(&run, 0.0, if run_hit { hit } else { plain });
    }
    job.wrap = TextWrapping { max_width: max_width.max(0.0), max_rows: 1, break_anywhere: true, ..Default::default() };
    painter.layout_job(job)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(query: &str, text: &str) -> i32 {
        fuzzy(query, text).map(|m| m.score).unwrap_or(i32::MIN)
    }

    fn noop(kind: Kind, label: &str) -> Entry {
        Entry::new(kind, label, icons::X, label, |_| {})
    }

    fn labels(entries: &[Entry], ranked: &[Ranked]) -> Vec<String> {
        ranked.iter().map(|r| entries[r.index].label.clone()).collect()
    }

    #[test]
    fn characters_must_appear_in_order_ignoring_case() {
        assert_eq!(fuzzy("ftch", "Fetch").unwrap().positions, [0, 2, 3, 4]);
        assert_eq!(fuzzy("FETCH", "fetch").unwrap().positions, [0, 1, 2, 3, 4]);
        assert!(fuzzy("hctef", "Fetch").is_none());
        assert!(fuzzy("fetchh", "Fetch").is_none());
        assert!(fuzzy("x", "").is_none());
        // Spaces in the query are only for the reader.
        assert_eq!(fuzzy("pop st", "Pop stash").unwrap().positions, [0, 1, 2, 4, 5]);
        assert_eq!(fuzzy("", "anything"), Some(Match { score: 0, positions: vec![] }));
    }

    #[test]
    fn consecutive_runs_beat_scattered_characters() {
        assert!(score("cde", "abcdef") > score("cde", "acxdxe"));
        assert!(score("push", "Push") > score("push", "Pull ush"));
    }

    #[test]
    fn word_starts_beat_the_middle_of_words() {
        assert!(score("b", "foo bar") > score("b", "foobar"));
        assert!(score("cb", "Create branch") > score("cb", "Close tab"));
        // It picks the word starts even when an earlier match exists.
        assert_eq!(fuzzy("cb", "Checkout cb/Create branch").unwrap().positions, [9, 10]);
        assert_eq!(fuzzy("cb", "Check Create branch").unwrap().positions, [6, 13]);
        assert_eq!(fuzzy("lp", "feature/loginPage").unwrap().positions, [8, 13]);
    }

    #[test]
    fn words_start_after_punctuation_capitals_and_letters() {
        let chars: Vec<char> = "feature/loginPage v2".chars().collect();
        let starts: Vec<usize> = (0..chars.len()).filter(|i| word_start(&chars, *i)).collect();
        assert_eq!(starts, [0, 8, 13, 18, 19]);
    }

    #[test]
    fn the_best_match_comes_first() {
        let entries = vec![
            noop(Kind::Branch, "Checkout feature/push-fix"),
            noop(Kind::Action, "Pop stash"),
            noop(Kind::Action, "Push"),
            noop(Kind::Action, "Pull"),
        ];
        let ranked = rank(&entries, "push", &[]);
        assert_eq!(labels(&entries, &ranked), ["Push", "Checkout feature/push-fix"]);
        assert_eq!(ranked[0].positions, [0, 1, 2, 3]);
        assert_eq!(ranked[1].positions, [17, 18, 19, 20]);
    }

    #[test]
    fn ties_go_to_actions_then_shorter_labels() {
        let entries = vec![noop(Kind::Branch, "Checkout main"), noop(Kind::Action, "Checkout main"), noop(Kind::Action, "Checkout")];
        let ranked = rank(&entries, "checkout", &[]);
        assert_eq!(ranked.iter().map(|r| r.index).collect::<Vec<_>>(), [2, 1, 0]);
    }

    #[test]
    fn keywords_find_entries_without_highlighting_anything() {
        let entries = vec![noop(Kind::Action, "Hide left panel").keywords("sidebar"), noop(Kind::Action, "Fetch")];
        let ranked = rank(&entries, "sidebar", &[]);
        assert_eq!(labels(&entries, &ranked), ["Hide left panel"]);
        assert!(ranked[0].positions.is_empty());
        // A good match on the label itself is still highlighted.
        assert_eq!(rank(&entries, "left", &[])[0].positions, [5, 6, 7, 8]);
    }

    #[test]
    fn an_empty_query_lists_recent_entries_first() {
        let entries = vec![noop(Kind::Action, "Fetch"), noop(Kind::Action, "Pull"), noop(Kind::Action, "Push")];
        let recent = vec!["Push".to_owned(), "gone".to_owned(), "Fetch".to_owned()];
        assert_eq!(labels(&entries, &rank(&entries, "  ", &recent)), ["Push", "Fetch", "Pull"]);
        // With a query, having run something lately settles a tie.
        let entries = vec![noop(Kind::Action, "Pull one"), noop(Kind::Action, "Pull two")];
        assert_eq!(labels(&entries, &rank(&entries, "pull", &["Pull two".to_owned()])), ["Pull two", "Pull one"]);
    }

    #[test]
    fn pinned_entries_always_come_first() {
        let mut commit = noop(Kind::Commit, "Go to commit abcd123 Fix");
        commit.pinned = Some(vec![13, 14, 15, 16]);
        let entries = vec![noop(Kind::Action, "abcd"), commit];
        let ranked = rank(&entries, "abcd", &[]);
        assert_eq!(ranked.iter().map(|r| r.index).collect::<Vec<_>>(), [1, 0]);
        assert_eq!(ranked[0].positions, [13, 14, 15, 16]);
    }

    mod with_a_repository {
        use super::*;
        use crate::tests::{Fixture, git};

        fn tab(fx: &Fixture) -> RepoTab {
            let mut tab = RepoTab::new(fx.dir.clone());
            tab.snap = Some(fx.snapshot());
            tab
        }

        fn has(list: &[Entry], label: &str) -> bool {
            list.iter().any(|e| e.label == label)
        }

        #[test]
        fn entries_that_cannot_run_are_left_out() {
            let fx = Fixture::new("palette-entries");
            fx.write("a.txt", "one\n");
            fx.commit_all("first");
            git(&fx.dir, &["branch", "feature/login"]);
            git(&fx.dir, &["tag", "v1.0"]);
            let settings = Settings::default();

            let tabs = [tab(&fx)];
            let list = entries(&tabs, 0, &settings);
            assert!(has(&list, "Checkout feature/login"));
            assert!(!has(&list, "Checkout main"), "the current branch is not offered");
            assert!(has(&list, "Go to tag v1.0"));
            assert!(has(&list, "Create branch"));
            for absent in ["Fetch", "Pull", "Push", "Stash", "Pop stash", "Undo"] {
                assert!(!has(&list, absent), "{absent} cannot run here");
            }

            // A dirty tree can be stashed; a stash can be popped.
            fx.write("a.txt", "two\n");
            let tabs = [tab(&fx)];
            assert!(has(&entries(&tabs, 0, &settings), "Stash"));
            git(&fx.dir, &["stash"]);
            let tabs = [tab(&fx)];
            let list = entries(&tabs, 0, &settings);
            assert!(has(&list, "Pop stash"));
            assert!(!has(&list, "Stash"));

            // With no repository open there is nothing to do to one.
            let list = entries(&[], 0, &settings);
            assert!(has(&list, "Open repository"));
            assert!(!has(&list, "Close tab"));
            assert!(!has(&list, "Refresh"));
        }

        #[test]
        fn remotes_offer_fetch_pull_push_and_their_branches() {
            let origin = Fixture::new("palette-origin");
            origin.write("a.txt", "one\n");
            origin.commit_all("first");
            git(&origin.dir, &["branch", "feature/remote-only"]);
            let fx = Fixture::new("palette-clone");
            git(&fx.dir, &["remote", "add", "origin", origin.dir.to_str().unwrap()]);
            git(&fx.dir, &["fetch", "-q", "origin"]);
            git(&fx.dir, &["checkout", "-q", "-b", "main", "--track", "origin/main"]);

            let tabs = [tab(&fx)];
            let list = entries(&tabs, 0, &Settings::default());
            for present in ["Fetch", "Pull", "Push", "Checkout origin/feature/remote-only"] {
                assert!(has(&list, present), "{present} is offered");
            }
            let push = list.iter().find(|e| e.label == "Push").unwrap();
            assert_eq!(push.detail, "to origin/main");
            // origin/main belongs to the branch already checked out.
            assert!(!has(&list, "Checkout origin/main"));
        }

        #[test]
        fn a_sha_prefix_finds_the_commit() {
            let fx = Fixture::new("palette-commits");
            fx.write("a.txt", "one\n");
            fx.commit_all("The first one");
            let snap = fx.snapshot();
            let sha = snap.head.oid.unwrap().to_string();

            let found = commits(&snap, &sha[..6].to_uppercase());
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].label, format!("Go to commit {} The first one", &sha[..7]));
            assert_eq!(found[0].pinned.as_deref(), Some(&[13, 14, 15, 16, 17, 18][..]));
            assert!(commits(&snap, &sha[..3]).is_empty(), "too short to be a SHA");
            assert_eq!(commits(&snap, &sha).len(), 1, "the whole SHA works too");
            let other = if sha.starts_with('0') { "1111" } else { "0000" };
            assert!(commits(&snap, other).is_empty());
            assert!(commits(&snap, "first").is_empty());
        }
    }
}
