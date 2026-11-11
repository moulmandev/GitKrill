//! The search bar above the graph (`Ctrl+F`): words and filters such as
//! `path:`, `code:` or `after:` (`git::search` reads them), the matching
//! rows lit and the others faded, `Enter` / `Shift+Enter` to step through
//! them. Filters on changed files and code are answered by a `git log` on a
//! thread of its own, started once typing pauses and stopped as soon as the
//! query or the history changes.

use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Key, Popup, PopupCloseBehavior, RichText, TextEdit, Ui};

use crate::git::search::{self, FILTERS, Query, Scan};
use crate::git::{Oid, RepoSnapshot, RowKind};
use crate::state::RepoTab;
use crate::theme::{self, icons};
use crate::ui::palette::Entry;
use crate::ui::widgets::icon_button;

/// How long typing must pause before git is asked.
const SETTLE: Duration = Duration::from_millis(250);

/// Kept per tab (`RepoTab::search_state`).
#[derive(Default)]
pub struct SearchState {
    /// What the query read as.
    parsed: Option<Parsed>,
    /// The `git log` for the query's file and code filters, and what it was
    /// started for.
    scan: Option<(ScanKey, Scan)>,
    /// A run waiting for the typing to pause.
    pending: Option<(ScanKey, Instant)>,
    /// The scan's generation the hits were computed with.
    seen: Option<usize>,
    /// Put the cursor at the end of the query on the next frame.
    to_end: bool,
}

struct Parsed {
    /// The text it was read from.
    text: String,
    query: Result<Query, String>,
    /// The `git log` arguments its file and code filters need
    /// (`git::search::log_args`).
    args: Option<Vec<String>>,
}

#[derive(Clone, PartialEq, Eq)]
struct ScanKey {
    args: Vec<String>,
    /// The history searched (`Arc::as_ptr`).
    history: usize,
}

impl SearchState {
    fn query(&self) -> Option<&Query> {
        self.parsed.as_ref().and_then(|parsed| parsed.query.as_ref().ok())
    }

    fn searching(&self) -> bool {
        self.pending.is_some() || self.scan.as_ref().is_some_and(|(_, scan)| !scan.is_done())
    }
}

/// Whether the search asks for something, so rows that do not match fade.
/// Not when git refused it (a regular expression it cannot read).
pub fn is_active(tab: &RepoTab) -> bool {
    let state = &tab.search_state;
    tab.search_open
        && state.query().is_some_and(|query| !query.is_empty())
        && state.scan.as_ref().is_none_or(|(_, scan)| scan.error().is_none())
}

/// Opens the search with `text` typed in, the cursor after it.
pub fn start(tab: &mut RepoTab, text: &str) {
    super::open_search(tab);
    tab.search = text.to_owned();
    tab.search_state.to_end = true;
}

/// "Search commits by changed file", "by code" and "by date".
pub fn palette_entries(list: &mut Vec<Entry>) {
    let entries = [
        ("search-path", icons::FILE, "Search commits by changed file", "path:", "path: files changed", "file path changed touched"),
        ("search-code", icons::CODE, "Search commits by code", "code:", "code: text added or removed (git log -S)", "pickaxe diff content -S -G regex"),
        ("search-date", icons::CALENDAR_BLANK, "Search commits by date", "after:", "after: / before: 2026-01-31, 2 weeks ago", "since until when day"),
    ];
    for (key, icon, label, prefix, detail, keywords) in entries {
        list.push(
            Entry::on_tab(key, icon, label, move |tab, _| start(tab, prefix))
                .detail(detail)
                .keywords(keywords),
        );
    }
}

pub fn bar(ui: &mut Ui, tab: &mut RepoTab, snap: &RepoSnapshot) {
    let text = tab.search.trim().to_owned();
    if tab.search_state.parsed.as_ref().is_none_or(|parsed| parsed.text != text) {
        let query = Query::parse(&text, chrono::Local::now());
        let args = query.as_ref().ok().and_then(search::log_args);
        tab.search_state.parsed = Some(Parsed { text: text.clone(), query, args });
    }
    let history = Arc::as_ptr(&snap.history) as usize;
    follow_scan(ui.ctx(), tab, snap, history);

    // Recompute matches only when the query, the history or what git found
    // changes.
    let generation = tab.search_state.scan.as_ref().map(|(_, scan)| scan.generation());
    let stamp = (text.clone(), history);
    if tab.search_stamp != stamp || tab.search_state.seen != generation {
        let state = &tab.search_state;
        let hits = &mut tab.search_hits;
        hits.clear();
        if let Some(query) = state.query().filter(|query| !query.is_empty()) {
            let found = state.scan.as_ref().map(|(_, scan)| scan.found());
            // Waiting for git: nothing matches yet.
            if !query.needs_git() || found.is_some() {
                for (i, commit) in snap.history.commits.iter().enumerate() {
                    if query.matches(commit) && found.as_ref().is_none_or(|found| found.contains(&commit.oid)) {
                        hits.push(i as u32);
                    }
                }
            }
        }
        tab.search_stamp = stamp;
        tab.search_state.seen = generation;
    }

    let error = match tab.search_state.parsed.as_ref().map(|parsed| &parsed.query) {
        Some(Err(error)) => Some(error.clone()),
        _ => tab.search_state.scan.as_ref().and_then(|(_, scan)| scan.error()),
    };
    let active = tab.search_state.query().is_some_and(|query| !query.is_empty());
    let mut jump: i32 = 0;
    let mut close = false;
    let mut insert: Option<&str> = None;
    egui::Frame::new()
        .fill(theme::bg_panel())
        .inner_margin(egui::Margin::symmetric(8, 5))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    if icon_button(ui, icons::X, 13.0, "Close search (Esc)").clicked() {
                        close = true;
                    }
                    if icon_button(ui, icons::CARET_DOWN, 13.0, "Next match (Enter)").clicked() {
                        jump = 1;
                    }
                    if icon_button(ui, icons::CARET_UP, 13.0, "Previous match (Shift+Enter)").clicked() {
                        jump = -1;
                    }
                    if let Some(error) = &error {
                        let short: String = error.chars().take(70).collect();
                        let short = if short.len() < error.len() { format!("{short}…") } else { short };
                        ui.label(RichText::new(short).color(theme::red()).size(12.0)).on_hover_text(error);
                    } else if active {
                        let position = tab
                            .selected_row()
                            .and_then(|row| tab.search_hits.binary_search(&(row as u32)).ok())
                            .map(|i| i + 1)
                            .unwrap_or(0);
                        let count = format!("{position} / {}", tab.search_hits.len());
                        let loaded = snap.history.commits.len();
                        let more = if snap.history.has_more { ", older ones are not loaded yet" } else { "" };
                        ui.label(RichText::new(count).color(theme::text_dim()).size(12.0))
                            .on_hover_text(format!("Searched the {loaded} commits in the graph{more}"));
                        if tab.search_state.searching() {
                            ui.add(egui::Spinner::new().size(12.0).color(theme::accent()))
                                .on_hover_text("Asking git which commits changed these files or this code…");
                        }
                        if snap.history.has_more
                            && icon_button(ui, icons::CLOCK_COUNTER_CLOCKWISE, 13.0, "Load older commits to search them too")
                                .clicked()
                        {
                            tab.load_more();
                        }
                    }

                    let filters = icon_button(ui, icons::FUNNEL, 13.0, "");
                    let filters = if Popup::is_id_open(ui.ctx(), Popup::default_response_id(&filters)) {
                        filters
                    } else {
                        filters.on_hover_text("Search by changed file, code, author or date")
                    };
                    Popup::menu(&filters).close_behavior(PopupCloseBehavior::CloseOnClick).show(|ui| {
                        ui.set_min_width(330.0);
                        ui.label(RichText::new("Add a filter").color(theme::text_dim()).size(11.0));
                        for (key, help) in FILTERS {
                            let mut label = egui::text::LayoutJob::default();
                            let format = |font_id, color| egui::TextFormat { font_id, color, ..Default::default() };
                            label.append(key, 0.0, format(theme::mono(12.0), theme::accent()));
                            label.append(help, 8.0, format(theme::font(12.5), theme::text()));
                            if ui.button(label).clicked() {
                                insert = Some(key);
                            }
                        }
                        ui.separator();
                        ui.label(
                            RichText::new("Quote values with spaces: path:\"my file.txt\"\nOnly the commits loaded in the graph are searched.")
                                .color(theme::text_dim())
                                .size(11.0),
                        );
                    });

                    let response = ui.add(
                        TextEdit::singleline(&mut tab.search)
                            .hint_text(format!(
                                "{}  Search commits   \u{b7}   path:   code:   author:   after:   before:",
                                icons::MAGNIFYING_GLASS
                            ))
                            .desired_width(f32::INFINITY),
                    );
                    if std::mem::take(&mut tab.search_focus) {
                        response.request_focus();
                    }
                    if std::mem::take(&mut tab.search_state.to_end) {
                        cursor_to_end(ui.ctx(), response.id, tab.search.chars().count());
                    }
                    if response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        jump = if ui.input(|i| i.modifiers.shift) { -1 } else { 1 };
                        response.request_focus();
                    }
                });
            });
        });

    if let Some(key) = insert {
        if !tab.search.is_empty() && !tab.search.ends_with(' ') {
            tab.search.push(' ');
        }
        tab.search.push_str(key);
        tab.search_focus = true;
        tab.search_state.to_end = true;
    }
    if close {
        tab.search_open = false;
        return;
    }
    if jump != 0 && !tab.search_hits.is_empty() {
        let current = tab.selected_row().map(|r| r as u32);
        let hits = &tab.search_hits;
        let target = match (current, jump > 0) {
            (Some(row), true) => hits.iter().copied().find(|h| *h > row).unwrap_or(hits[0]),
            (Some(row), false) => {
                hits.iter().rev().copied().find(|h| *h < row).unwrap_or(hits[hits.len() - 1])
            }
            (None, _) => hits[0],
        };
        tab.select_row(target as usize);
        tab.scroll_to = Some(target as usize);
    }
}

/// Starts, keeps or stops the `git log` so it answers the query in the
/// history `history` (`Arc::as_ptr`): a run for anything else stops at once,
/// a new one starts once typing has paused.
fn follow_scan(ctx: &egui::Context, tab: &mut RepoTab, snap: &RepoSnapshot, history: usize) {
    let state = &mut tab.search_state;
    let args = state.parsed.as_ref().and_then(|parsed| parsed.args.as_ref());
    let is_wanted = |key: &ScanKey| Some(&key.args) == args && key.history == history;
    if state.scan.as_ref().is_some_and(|(key, _)| is_wanted(key)) {
        state.pending = None;
        return;
    }
    // Dropping a scan stops its git.
    state.scan = None;
    let Some(args) = args else {
        state.pending = None;
        return;
    };
    let since = match &state.pending {
        Some((key, since)) if is_wanted(key) => *since,
        _ => {
            let now = Instant::now();
            state.pending = Some((ScanKey { args: args.clone(), history }, now));
            now
        }
    };
    let waited = since.elapsed();
    if waited < SETTLE {
        ctx.request_repaint_after(SETTLE - waited);
        return;
    }
    let Some((wanted, _)) = state.pending.take() else { return };
    let commits: Vec<Oid> =
        snap.history.commits.iter().filter(|c| c.kind != RowKind::Wip).map(|c| c.oid).collect();
    let repaint = ctx.clone();
    let scan = Scan::start(&snap.workdir, wanted.args.clone(), commits, move || repaint.request_repaint());
    state.scan = Some((wanted, scan));
}

fn cursor_to_end(ctx: &egui::Context, id: egui::Id, chars: usize) {
    let mut state = egui::text_edit::TextEditState::load(ctx, id).unwrap_or_default();
    let end = egui::text::CCursor::new(chars);
    state.cursor.set_char_range(Some(egui::text::CCursorRange::one(end)));
    state.store(ctx, id);
}
