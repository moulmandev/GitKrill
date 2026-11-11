//! Bisect (`git bisect`): started from a commit's right-click menu, then a
//! banner above the graph with the commit being tested, roughly how many
//! steps are left, and Good / Bad / Skip / Stop. Good, bad and skipped
//! commits, and the one being tested, carry a label in the graph; once git
//! names the first bad commit it is selected. A bisect started in a terminal
//! gets the same banner, read back from git's own files
//! (`git::bisect::read`).

use std::sync::{Arc, Weak};

use eframe::egui::{self, Color32, Frame, Margin, Painter, Rect, RichText, Stroke, StrokeKind, Ui, pos2, vec2};

use crate::git::bisect::{self, Progress, State, Terms};
use crate::git::{Oid, RepoSnapshot, RepoState, short};
use crate::state::{RepoTab, Shared, ToastKind};
use crate::theme::{self, icons};
use crate::ui::palette::Entry;
use crate::ui::widgets::{menu_item, tint_button};

const KEY: &str = "bisect:state";

/// Kept per tab (`RepoTab::bisect`).
#[derive(Default)]
pub struct Bisect {
    /// The snapshot the state was last read for. Held weakly, so a newer
    /// snapshot cannot take its address.
    asked: Weak<RepoSnapshot>,
    /// What was read last, while a bisect is in progress.
    pub state: Option<State>,
    /// The first bad commit already shown.
    announced: Option<Oid>,
    /// The checked out commit when last seen: when git moves on to the next
    /// one to test, the graph follows.
    head: Option<Oid>,
}

/// Reads the bisect again whenever the repository changed, and follows git
/// to the commit it checks out next and, at the end, the first bad commit.
fn poll(tab: &mut RepoTab, shared: &mut Shared, snap: &Arc<RepoSnapshot>) {
    if let Some(result) = tab.take_result::<Result<Option<State>, String>>(KEY) {
        // A failed read keeps the last state; the next change reads again.
        if let Ok(state) = result {
            tab.bisect.state = state;
        }
        if let Some(Progress::Found(oid)) = tab.bisect.state.as_ref().map(|state| &state.progress)
            && tab.bisect.announced != Some(*oid)
        {
            let oid = *oid;
            tab.bisect.announced = Some(oid);
            shared.toast(ToastKind::Success, "Bisect found the first bad commit", describe(snap, oid));
            tab.jump_to(oid);
        }
    }
    if snap.state != RepoState::Bisect {
        if tab.bisect.state.is_some() || tab.bisect.head.is_some() {
            tab.bisect = Bisect::default();
        }
        return;
    }
    if tab.bisect.asked.as_ptr() != Arc::as_ptr(snap) {
        tab.bisect.asked = Arc::downgrade(snap);
        tab.spawn_job(KEY, |loader| bisect::read(loader.repo()));
    }
    if snap.head.oid != tab.bisect.head {
        if tab.bisect.head.is_some()
            && let Some(head) = snap.head.oid
        {
            tab.jump_to(head);
        }
        tab.bisect.head = snap.head.oid;
    }
}

/// `1a2b3c4 Fix the parser`, or the short SHA alone for a commit the graph
/// has not loaded.
fn describe(snap: &RepoSnapshot, oid: Oid) -> String {
    match snap.row_of(oid) {
        Some(row) => format!("{} {}", short(oid), snap.history.commits[row].summary),
        None => short(oid),
    }
}

/// "Bad" for "bad": a term as a button says it.
fn capital(term: &str) -> String {
    let mut chars = term.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

/// The banner above the graph while a bisect is in progress.
pub fn banner(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, snap: &Arc<RepoSnapshot>) {
    poll(tab, shared, snap);
    if snap.state != RepoState::Bisect {
        return;
    }
    let state = tab.bisect.state.clone();
    let terms = state.as_ref().map(|state| state.terms.clone()).unwrap_or_default();
    let head = snap.head.oid;
    let (bad, good) = (capital(&terms.bad), capital(&terms.good));

    // What to say, and which of Good / Bad / Skip apply to the checked out
    // commit.
    let mut marks = (true, true, true);
    let mut found = None;
    let mut tooltip = String::new();
    let (message, hint) = match state.as_ref().map(|state| &state.progress) {
        None => ("Bisect in progress".to_owned(), String::new()),
        Some(Progress::Needs { bad: true, good: true }) => {
            marks.2 = false;
            (
                format!("Bisect started: mark a {} commit and a {} one", terms.bad, terms.good),
                "Right-click commits in the graph to mark them, or mark the checked out one here.".to_owned(),
            )
        }
        Some(Progress::Needs { bad: false, .. }) => {
            let known = state.as_ref().and_then(|state| state.bad);
            // The bad commit itself cannot be good too.
            marks = (known != head, false, false);
            let known = known.map(short).unwrap_or_default();
            (
                format!("{known} is {}. Now mark a {} commit, one without the change", terms.bad, terms.good),
                "Right-click an older commit in the graph, or mark the checked out one here.".to_owned(),
            )
        }
        Some(Progress::Needs { .. }) => {
            marks = (false, true, false);
            let known = state.as_ref().and_then(|state| state.good.first().copied()).map(short).unwrap_or_default();
            (
                format!("{known} is {}. Now mark a {} commit, one with the change", terms.good, terms.bad),
                "Right-click a newer commit in the graph, or mark the checked out one here.".to_owned(),
            )
        }
        Some(Progress::Testing { left, steps }) => {
            let testing = head.map(|oid| describe(snap, oid)).unwrap_or_default();
            let commits = if *left == 1 { "1 commit".to_owned() } else { format!("{left} commits") };
            let steps = if *steps == 1 { "about 1 step".to_owned() } else { format!("about {steps} steps") };
            (
                format!("Testing {testing}"),
                format!("{commits} left to test, {steps}. Is the change in this commit?"),
            )
        }
        Some(Progress::Found(oid)) => {
            found = Some(*oid);
            marks = (false, false, false);
            (
                format!("{} is the first {} commit", describe(snap, *oid), terms.bad),
                match state.as_ref().map(State::start_label) {
                    Some(start) if !start.is_empty() => format!("Stop to check out {start} again."),
                    _ => "Stop to end the bisect.".to_owned(),
                },
            )
        }
        Some(Progress::OnlySkipped(oids)) => {
            marks.2 = false;
            let listed: Vec<String> = oids.iter().map(|oid| describe(snap, *oid)).collect();
            tooltip = listed.join("\n");
            let shas: Vec<String> = oids.iter().map(|oid| short(*oid)).collect();
            (
                format!("Only skipped commits are left: the first {} commit is one of {}", terms.bad, oids.len()),
                format!("{}. Mark one {} or {} to narrow it down.", shas.join(", "), terms.good, terms.bad),
            )
        }
    };
    if tooltip.is_empty() {
        tooltip = hint.clone();
    }

    let tint = if found.is_some() { theme::green() } else { theme::orange() };
    let busy = tab.is_running("Bisect") || tab.is_running("Stop bisect") || tab.is_running("Start bisect");
    let start = state.as_ref().map(State::start_label).unwrap_or_default();
    let mut run = None;
    Frame::new()
        .fill(theme::mix(theme::bg_app(), tint, 0.16))
        .inner_margin(Margin::symmetric(10, 5))
        .show(ui, |ui| {
            egui::Sides::new().shrink_left().truncate().show(
                ui,
                |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.label(RichText::new(icons::BUG).color(tint).size(15.0));
                    let response = ui.add(egui::Label::new(RichText::new(&message).color(theme::text_bright())).truncate());
                    if !hint.is_empty() {
                        response.on_hover_text(&tooltip);
                        ui.add(egui::Label::new(RichText::new(&hint).color(theme::text_dim()).size(12.0)).truncate())
                            .on_hover_text(&tooltip);
                    }
                },
                |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.add_enabled_ui(!busy, |ui| {
                        let stop_tip = if start.is_empty() {
                            "End the bisect (git bisect reset)".to_owned()
                        } else {
                            format!("End the bisect and check out {start} again (git bisect reset)")
                        };
                        if tint_button(ui, "Stop", theme::text_dim()).on_hover_text(stop_tip).clicked() {
                            run = Some(bisect::stop());
                        }
                        if let Some(oid) = found
                            && tint_button(ui, "Show", theme::accent()).on_hover_text("Select it in the graph").clicked()
                        {
                            tab.jump_to(oid);
                        }
                        let (show_good, show_bad, show_skip) = marks;
                        if show_skip
                            && tint_button(ui, "Skip", theme::text_dim())
                                .on_hover_text("Cannot tell for this commit: test another one instead (git bisect skip)")
                                .clicked()
                        {
                            run = Some(bisect::mark("skip", None));
                        }
                        if show_bad
                            && tint_button(ui, &bad, theme::red())
                                .on_hover_text(format!("The checked out commit has the change (git bisect {})", terms.bad))
                                .clicked()
                        {
                            run = Some(bisect::mark(&terms.bad, None));
                        }
                        if show_good
                            && tint_button(ui, &good, theme::green())
                                .on_hover_text(format!("The checked out commit does not have it (git bisect {})", terms.good))
                                .clicked()
                        {
                            run = Some(bisect::mark(&terms.good, None));
                        }
                    });
                },
            );
        });
    if let Some(cmd) = run {
        tab.run(cmd);
    }
}

/// The items of a commit's right-click menu: start a bisect from it, or,
/// while one runs, mark it.
pub fn menu_items(ui: &mut Ui, tab: &mut RepoTab, snap: &RepoSnapshot, oid: Oid) {
    match snap.state {
        RepoState::Clean => {
            if menu_item(ui, icons::BUG, "Start bisect: this commit is bad") {
                tab.run(bisect::start(oid, true));
            }
            if menu_item(ui, icons::BUG, "Start bisect: this commit is good") {
                tab.run(bisect::start(oid, false));
            }
        }
        RepoState::Bisect => {
            let terms = tab.bisect.state.as_ref().map(|state| state.terms.clone()).unwrap_or_default();
            if menu_item(ui, icons::X_CIRCLE, &format!("Bisect: mark this commit {}", terms.bad)) {
                tab.run(bisect::mark(&terms.bad, Some(oid)));
            }
            if menu_item(ui, icons::CHECK_CIRCLE, &format!("Bisect: mark this commit {}", terms.good)) {
                tab.run(bisect::mark(&terms.good, Some(oid)));
            }
            if menu_item(ui, icons::SKIP_FORWARD, "Bisect: skip this commit") {
                tab.run(bisect::mark("skip", Some(oid)));
            }
        }
        _ => {}
    }
}

/// The command palette's bisect entries, for the checked out commit.
pub fn palette_entries(list: &mut Vec<Entry>, tab: &RepoTab, snap: &RepoSnapshot) {
    if tab.is_running("Bisect") || tab.is_running("Start bisect") || tab.is_running("Stop bisect") {
        return;
    }
    let Some(head) = snap.head.oid else { return };
    match snap.state {
        RepoState::Clean => list.push(
            Entry::on_tab("bisect-start", icons::BUG, "Start bisect: the checked out commit is bad", move |tab, _| {
                tab.run(bisect::start(head, true))
            })
            .detail(short(head))
            .keywords("bisect find regression bug binary search"),
        ),
        RepoState::Bisect => {
            let state = tab.bisect.state.as_ref();
            let terms = state.map(|state| state.terms.clone()).unwrap_or_default();
            let found = state.is_some_and(|state| matches!(state.progress, Progress::Found(_)));
            if !found {
                for (key, icon, term) in [
                    ("bisect-bad", icons::X_CIRCLE, terms.bad.clone()),
                    ("bisect-good", icons::CHECK_CIRCLE, terms.good.clone()),
                ] {
                    let label = format!("Bisect: mark the checked out commit {term}");
                    list.push(
                        Entry::on_tab(key, icon, label, move |tab, _| tab.run(bisect::mark(&term, None)))
                            .detail(short(head))
                            .keywords("bisect"),
                    );
                }
                list.push(
                    Entry::on_tab("bisect-skip", icons::SKIP_FORWARD, "Bisect: skip the checked out commit", |tab, _| {
                        tab.run(bisect::mark("skip", None))
                    })
                    .detail(short(head))
                    .keywords("bisect"),
                );
            }
            let start = state.map(State::start_label).unwrap_or_default();
            list.push(
                Entry::on_tab("bisect-stop", icons::STOP_CIRCLE, "Stop bisect", |tab, _| tab.run(bisect::stop()))
                    .detail(if start.is_empty() { String::new() } else { format!("check out {start} again") })
                    .keywords("bisect reset end abort"),
            );
        }
        _ => {}
    }
}

/// What a commit is to the bisect in progress, as a label in its row.
pub struct Badge {
    text: String,
    color: Color32,
    /// The first bad commit: a solid label.
    solid: bool,
}

pub fn badge(tab: &RepoTab, snap: &RepoSnapshot, oid: Oid) -> Option<Badge> {
    if snap.state != RepoState::Bisect {
        return None;
    }
    let state = tab.bisect.state.as_ref()?;
    let Terms { bad, good } = &state.terms;
    let (text, color, solid) = if state.is_found(oid) {
        (format!("first {bad}"), theme::red(), true)
    } else if state.bad == Some(oid) {
        (bad.clone(), theme::red(), false)
    } else if state.good.contains(&oid) {
        (good.clone(), theme::green(), false)
    } else if state.skipped.contains(&oid) {
        ("skipped".to_owned(), theme::text_dim(), false)
    } else if snap.head.oid == Some(oid) && matches!(state.progress, Progress::Testing { .. } | Progress::OnlySkipped(_)) {
        ("testing".to_owned(), theme::orange(), false)
    } else {
        return None;
    };
    Some(Badge { text, color, solid })
}

/// Paints `badge` from `x`, centred on `rect`'s height; returns its width.
pub fn paint_badge(painter: &Painter, rect: Rect, x: f32, badge: &Badge) -> f32 {
    let (fill, text) = if badge.solid {
        (theme::solid(badge.color, 0.7), theme::on_accent())
    } else {
        (theme::mix(theme::bg_app(), badge.color, 0.2), theme::text_bright())
    };
    let font = if badge.solid { theme::bold(11.0) } else { theme::font(11.0) };
    let galley = painter.layout_no_wrap(badge.text.clone(), font, text);
    let size = vec2(galley.size().x + 10.0, 16.0);
    let chip = Rect::from_min_size(pos2(x, rect.center().y - size.y / 2.0), size);
    painter.rect(chip, 3.0, fill, Stroke::new(1.0, theme::mix(theme::bg_app(), badge.color, 0.8)), StrokeKind::Inside);
    painter.galley(chip.center() - galley.size() / 2.0, galley, text);
    size.x
}
