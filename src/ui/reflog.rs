//! The reflog: a panel under the graph listing where HEAD, or a branch,
//! pointed after each thing git did to it, newest first (`git reflog`).
//!
//! It opens from View > Reflog, the branch picker in the toolbar, the
//! command palette, or "Show reflog" on a branch's right-click menu, and is
//! read again whenever the refs change. Clicking an entry selects its commit
//! in the graph (loading older history when needed); a commit no ref reaches
//! any more, such as one replaced by an amend or a rebase, is shown in the
//! details panel on its own ([`RepoTab::select_outside`]). The right-click
//! menu checks an entry out, creates a branch there, resets the current
//! branch to it or cherry-picks it.

use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use eframe::egui::{
    self, Align2, Color32, CursorIcon, Frame, Key, Modifiers, Popup, Rect, ScrollArea, Sense, Stroke, Ui, pos2,
    vec2,
};

use crate::git::ops::{self, ResetMode};
use crate::git::reflog::{self, Entry, Kind, Reflog};
use crate::git::{Oid, RepoSnapshot, short};
use crate::state::{Dialog, RepoTab, Shared, ToastKind, format_time, relative_time};
use crate::theme::{self, icons};
use crate::ui::palette::Entry as PaletteEntry;
use crate::ui::widgets::{self, menu_item};

const ROW_H: f32 = 26.0;
const BAR_H: f32 = 32.0;

/// Gives every panel its own job keys, so an answer for a reflog that was
/// switched away from is never shown for the next one.
static NEXT: AtomicU64 = AtomicU64::new(0);

/// The open reflog panel of a tab (`RepoTab::reflog`).
pub struct ReflogDock {
    /// `HEAD`, or the full name of a branch.
    refname: String,
    serial: u64,
    /// What the reflog was last asked for: the refs as they were then.
    asked: Option<u64>,
    data: Option<Result<Arc<Reflog>, String>>,
    /// Index of the selected entry.
    selected: Option<usize>,
    /// A clicked commit the graph has not loaded yet; more history is
    /// loaded until it is there.
    jump: Option<Oid>,
    /// Scroll the selected entry into view.
    reveal: bool,
}

impl ReflogDock {
    fn new(refname: &str) -> Self {
        ReflogDock {
            refname: refname.to_owned(),
            serial: NEXT.fetch_add(1, Ordering::Relaxed),
            asked: None,
            data: None,
            selected: None,
            jump: None,
            reveal: false,
        }
    }

    fn job(&self) -> String {
        format!("reflog:{}", self.serial)
    }
}

/// Shows the reflog of `refname` (`HEAD` or `refs/heads/<name>`).
pub fn open(tab: &mut RepoTab, refname: &str) {
    if tab.reflog.as_ref().is_none_or(|dock| dock.refname != refname) {
        tab.reflog = Some(Box::new(ReflogDock::new(refname)));
    }
}

/// Shows the reflog of a local branch.
pub fn open_branch(tab: &mut RepoTab, branch: &str) {
    open(tab, &format!("refs/heads/{branch}"));
}

pub fn close(tab: &mut RepoTab) {
    tab.reflog = None;
}

/// View > Reflog.
pub fn view_menu_item(ui: &mut Ui, tab: &mut RepoTab) {
    let mut shown = tab.reflog.is_some();
    if ui.checkbox(&mut shown, "Reflog").on_hover_text("Where HEAD has been: every commit, checkout, reset and rebase").changed() {
        if shown { open(tab, "HEAD") } else { close(tab) }
    }
}

/// "Show reflog" for HEAD, in the toolbar's branch menu.
pub fn head_item(ui: &mut Ui, tab: &mut RepoTab) {
    if menu_item(ui, icons::CLOCK_COUNTER_CLOCKWISE, "Show reflog") {
        open(tab, "HEAD");
    }
}

/// "Show reflog" on a local branch's right-click menu.
pub fn branch_item(ui: &mut Ui, tab: &mut RepoTab, branch: &str) {
    if menu_item(ui, icons::CLOCK_COUNTER_CLOCKWISE, "Show reflog") {
        open_branch(tab, branch);
    }
}

pub fn palette_entries(list: &mut Vec<PaletteEntry>, tab: &RepoTab, snap: &RepoSnapshot) {
    let shown = tab.reflog.as_ref().map(|dock| dock.refname.clone());
    if shown.as_deref() != Some("HEAD") {
        list.push(
            PaletteEntry::on_tab("reflog", icons::CLOCK_COUNTER_CLOCKWISE, "Show reflog", |tab, _| open(tab, "HEAD"))
                .detail("HEAD")
                .keywords("history lost commits recover undo reset amend log"),
        );
    }
    if let Some(branch) = snap.current_branch() {
        let refname = format!("refs/heads/{}", branch.name);
        if shown.as_deref() != Some(refname.as_str()) {
            let name = branch.name.clone();
            list.push(
                PaletteEntry::on_tab("reflog-branch", icons::CLOCK_COUNTER_CLOCKWISE, format!("Show reflog of {name}"), move |tab, _| {
                    open_branch(tab, &name)
                })
                .keywords("history lost commits recover log branch"),
            );
        }
    }
    if shown.is_some() {
        list.push(PaletteEntry::on_tab("reflog-hide", icons::X, "Hide reflog", |tab, _| close(tab)).keywords("close"));
    }
}

/// A fingerprint of what the reflog depends on: where HEAD and the branches
/// point (and whether an operation is under way).
fn stamp(snap: &RepoSnapshot) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    snap.head.branch.hash(&mut hasher);
    snap.head.oid.hash(&mut hasher);
    snap.head.detached.hash(&mut hasher);
    (snap.state as u8).hash(&mut hasher);
    for branch in &snap.branches {
        branch.name.hash(&mut hasher);
        branch.oid.hash(&mut hasher);
    }
    hasher.finish()
}

/// The panel, at the bottom of the space the graph or the diff is in. Does
/// nothing while it is closed.
pub fn dock(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared) {
    let Some(mut dock) = tab.reflog.take() else { return };
    let Some(snap) = tab.snap.clone() else {
        tab.reflog = Some(dock);
        return;
    };
    load(tab, &snap, &mut dock);
    follow_jump(tab, &snap, &mut dock);

    let mut keep = true;
    egui::Panel::bottom("reflog-dock")
        .resizable(true)
        .default_size(250.0)
        .size_range(110.0..=640.0)
        .show_separator_line(false)
        .frame(Frame::new().fill(theme::bg_app()))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            keep = bar(ui, &snap, &mut dock);
            body(ui, tab, shared, &snap, &mut dock);
        });
    if keep && tab.reflog.is_none() {
        tab.reflog = Some(dock);
    }
}

/// Asks for the reflog when the refs changed since it was read.
fn load(tab: &mut RepoTab, snap: &RepoSnapshot, dock: &mut ReflogDock) {
    if let Some(result) = tab.take_result::<Result<Arc<Reflog>, String>>(&dock.job()) {
        // The same entry stays selected while new ones are added on top.
        if let (Some(selected), Some(Ok(old)), Ok(new)) = (dock.selected, &dock.data, &result) {
            let oid = old.entries.get(selected).map(|e| (e.oid, e.time));
            let added = new.entries.len().saturating_sub(old.entries.len());
            let same = |i: usize| new.entries.get(i).map(|e| (e.oid, e.time));
            dock.selected = if same(selected + added) == oid { Some(selected + added) } else { None };
        }
        dock.data = Some(result);
    }
    let stamp = stamp(snap);
    if dock.asked != Some(stamp) {
        dock.asked = Some(stamp);
        let refname = dock.refname.clone();
        tab.spawn_job(dock.job(), move |loader| reflog::read(loader.repo(), &refname).map(Arc::new));
    }
}

/// Carries out a click on a commit that the graph had not loaded yet.
fn follow_jump(tab: &mut RepoTab, snap: &RepoSnapshot, dock: &mut ReflogDock) {
    let Some(oid) = dock.jump else { return };
    if snap.row_of(oid).is_some() {
        dock.jump = None;
        tab.jump_to(oid);
    } else if !snap.history.has_more {
        // Hidden from the graph, or beyond what it can load.
        dock.jump = None;
        tab.select_outside(oid);
    } else if !tab.loading_more() {
        tab.load_more();
    }
}

/// The title bar: which reflog, how long, and close. Returns false when
/// closed.
fn bar(ui: &mut Ui, snap: &RepoSnapshot, dock: &mut ReflogDock) -> bool {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), BAR_H), Sense::hover());
    let painter = ui.painter().clone();
    painter.rect_filled(rect, 0.0, theme::bg_header());
    painter.hline(rect.x_range(), rect.top() + 0.5, Stroke::new(1.0, theme::border()));
    painter.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::border()));

    let mut x = rect.left() + 12.0;
    x += widgets::text_left(&painter, rect, x, icons::CLOCK_COUNTER_CLOCKWISE, theme::font(15.0), theme::accent()) + 8.0;
    x += widgets::text_left(&painter, rect, x, "Reflog", theme::bold(13.0), theme::text_bright()) + 10.0;

    // Which ref: HEAD or one of the local branches.
    let name = reflog::display_name(&dock.refname).to_owned();
    let label = format!("{name}  {}", icons::CARET_DOWN);
    let galley = widgets::elided(&painter, &label, theme::font(12.5), theme::text(), 260.0);
    let picker = Rect::from_min_size(pos2(x, rect.center().y - 11.0), vec2(galley.size().x + 16.0, 22.0));
    let response = ui.interact(picker, ui.id().with("reflog-ref"), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
    let open_menu = Popup::is_id_open(ui.ctx(), Popup::default_response_id(&response));
    painter.rect_filled(picker, 4.0, if response.hovered() || open_menu { theme::bg_raised() } else { theme::bg_app() });
    widgets::galley_left(&painter, picker, picker.left() + 8.0, galley);
    let mut pick = None;
    Popup::menu(&response).show(|ui| {
        ui.set_min_width(200.0);
        let mark = |on: bool| if on { icons::CHECK } else { "" };
        if ui.button(format!("{:2}  HEAD", mark(dock.refname == "HEAD"))).clicked() {
            pick = Some("HEAD".to_owned());
        }
        ui.separator();
        ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
            for branch in &snap.branches {
                let refname = format!("refs/heads/{}", branch.name);
                let icon = if refname == dock.refname { icons::CHECK } else { icons::GIT_BRANCH };
                if ui.button(format!("{icon}  {}", branch.name)).clicked() {
                    pick = Some(refname);
                }
            }
        });
    });
    if let Some(refname) = pick.filter(|refname| *refname != dock.refname) {
        *dock = ReflogDock::new(&refname);
    }
    x = picker.right() + 12.0;

    let count = match &dock.data {
        Some(Ok(log)) if log.more > 0 => format!("newest {} entries of {}", log.entries.len(), log.entries.len() + log.more),
        Some(Ok(log)) if log.entries.len() == 1 => "1 entry".to_owned(),
        Some(Ok(log)) => format!("{} entries", log.entries.len()),
        _ => String::new(),
    };
    widgets::text_left(&painter, rect, x, &count, theme::font(12.0), theme::text_dim());

    let close = Rect::from_center_size(pos2(rect.right() - 18.0, rect.center().y), vec2(22.0, 22.0));
    let response = ui.interact(close, ui.id().with("reflog-close"), Sense::click()).on_hover_text("Close the reflog");
    if response.hovered() {
        painter.rect_filled(close, 4.0, theme::bg_raised());
    }
    painter.text(close.center(), Align2::CENTER_CENTER, icons::X, theme::font(13.0), theme::text_dim());
    !response.clicked()
}

fn kind_style(kind: Kind) -> (&'static str, Color32) {
    match kind {
        Kind::Commit => (icons::GIT_COMMIT, theme::green()),
        Kind::Amend => (icons::PENCIL_SIMPLE, theme::yellow()),
        Kind::Checkout => (icons::ARROW_BEND_UP_RIGHT, theme::blue()),
        Kind::Reset => (icons::ARROW_COUNTER_CLOCKWISE, theme::red()),
        Kind::Rebase => (icons::GIT_FORK, theme::accent()),
        Kind::Merge => (icons::GIT_MERGE, theme::accent()),
        Kind::Pick => (icons::ARROW_ELBOW_DOWN_RIGHT, theme::orange()),
        Kind::Branch | Kind::Other => (icons::GIT_BRANCH, theme::text_dim()),
    }
}

fn body(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, snap: &RepoSnapshot, dock: &mut ReflogDock) {
    let log = match &dock.data {
        None => return widgets::placeholder(ui, "Reading the reflog…"),
        Some(Err(e)) => return widgets::placeholder(ui, e),
        Some(Ok(log)) => log.clone(),
    };
    if log.entries.is_empty() {
        let text = format!("Nothing in the reflog of {} yet.", reflog::display_name(&log.refname));
        return widgets::placeholder(ui, &text);
    }

    let n = log.entries.len();
    // Arrow keys walk the entries while the pointer is over the panel; the
    // graph, drawn after it, has them otherwise.
    let pointed = ui.rect_contains_pointer(ui.available_rect_before_wrap());
    let keys = if pointed && shared.dialog.is_none() && !ui.ctx().egui_wants_keyboard_input() {
        ui.input_mut(|i| {
            i.consume_key(Modifiers::NONE, Key::ArrowDown) as i64 - i.consume_key(Modifiers::NONE, Key::ArrowUp) as i64
        })
    } else {
        0
    };
    let mut clicked = None;
    if keys != 0 {
        let next = match dock.selected {
            Some(i) => (i as i64 + keys).clamp(0, n as i64 - 1) as usize,
            None => 0,
        };
        clicked = Some(next);
        dock.reveal = true;
    }

    let reveal = std::mem::take(&mut dock.reveal).then_some(dock.selected).flatten();
    let mut scroll = ScrollArea::vertical().id_salt(("reflog-rows", dock.serial)).auto_shrink([false, false]);
    if let Some(i) = reveal.or(clicked) {
        let view = ui.available_height();
        let offset = (i as f32 * ROW_H - view / 2.0).max(0.0);
        if keys != 0 || reveal.is_some() {
            scroll = scroll.vertical_scroll_offset(offset);
        }
    }
    scroll.show_rows(ui, ROW_H, n, |ui, rows| {
        for i in rows {
            let entry = &log.entries[i];
            let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
            paint_row(ui, rect, &log.refname, entry, dock.selected == Some(i), response.hovered());
            let response = response.on_hover_ui(|ui| tooltip(ui, &log.refname, entry));
            if response.clicked() {
                clicked = Some(i);
            }
            response.context_menu(|ui| entry_menu(ui, tab, shared, snap, &log.refname, entry));
        }
    });

    if let Some(i) = clicked {
        dock.selected = Some(i);
        go_to(tab, shared, snap, dock, &log.entries[i]);
    }
}

fn paint_row(ui: &Ui, rect: Rect, refname: &str, entry: &Entry, selected: bool, hovered: bool) {
    let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
    widgets::row_background(&painter, rect, selected, hovered);
    painter.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::bg_panel()));
    let gone = entry.summary.is_none();

    let mut x = rect.left() + 12.0;
    let selector = reflog::selector(refname, entry.index);
    let faint = if selected { theme::text_dim() } else { theme::text_faint() };
    let galley = widgets::elided(&painter, &selector, theme::mono(11.0), faint, 92.0);
    widgets::galley_left(&painter, rect, x, galley);
    x += 96.0;

    let (icon, color) = kind_style(entry.kind());
    painter.text(pos2(x + 7.0, rect.center().y), Align2::CENTER_CENTER, icon, theme::font(13.0), color);
    x += 22.0;
    let galley = widgets::elided(&painter, &entry.action, theme::font(12.5), theme::text_bright(), 170.0);
    let used = widgets::galley_left(&painter, rect, x, galley);
    x += used.max(110.0) + 12.0;

    // From the right: when, the commit, and whether it is lost.
    let right = rect.right() - 12.0;
    let when = {
        let ago = relative_time(entry.time);
        if ago.is_empty() { format_time(entry.time) } else { ago }
    };
    let galley = widgets::elided(&painter, &when, theme::font(12.0), theme::text_dim(), 140.0);
    let when_left = right - 140.0;
    widgets::galley_left(&painter, rect, when_left + 140.0 - galley.size().x, galley);
    let sha_left = when_left - 78.0;
    let sha_color = if gone || entry.unreachable { faint } else { theme::accent() };
    widgets::text_left(&painter, rect, sha_left, &short(entry.oid), theme::mono(11.5), sha_color);
    let mut detail_right = sha_left - 12.0;
    let tag = if gone {
        Some("deleted")
    } else if entry.unreachable {
        Some("unreachable")
    } else {
        None
    };
    if let Some(tag) = tag {
        let galley = painter.layout_no_wrap(tag.to_owned(), theme::font(11.0), theme::orange());
        let chip = Rect::from_min_size(
            pos2(detail_right - galley.size().x - 12.0, rect.center().y - 9.0),
            vec2(galley.size().x + 12.0, 18.0),
        );
        painter.rect_stroke(chip, 9.0, Stroke::new(1.0, theme::mix(theme::bg_app(), theme::orange(), 0.6)), egui::StrokeKind::Inside);
        widgets::galley_left(&painter, chip, chip.left() + 6.0, galley);
        detail_right = chip.left() - 10.0;
    }

    // Git's message, then the commit's summary when the message does not
    // already name it ("moving from main to topic"  Fix the login form).
    let summary = entry.summary.as_deref().unwrap_or_default();
    let (detail, after) = if entry.detail.is_empty() {
        (summary, "")
    } else if entry.detail == summary {
        (entry.detail.as_str(), "")
    } else {
        (entry.detail.as_str(), summary)
    };
    let galley = widgets::elided_pair(
        &painter,
        (detail, theme::font(12.5), theme::text()),
        (after, theme::font(12.5), theme::text_dim()),
        detail_right - x,
    );
    widgets::galley_left(&painter, rect, x, galley);
}

fn tooltip(ui: &mut Ui, refname: &str, entry: &Entry) {
    ui.set_max_width(460.0);
    let message = if entry.detail.is_empty() { entry.action.clone() } else { format!("{}: {}", entry.action, entry.detail) };
    ui.label(egui::RichText::new(message).color(theme::text_bright()));
    ui.label(format!("{}  ·  {} by {}", reflog::selector(refname, entry.index), format_time(entry.time), entry.who));
    if !entry.old.is_zero() && entry.old != entry.oid {
        let name = reflog::display_name(refname);
        ui.label(egui::RichText::new(format!("{name} moved from {} to {}", short(entry.old), short(entry.oid))).color(theme::text_dim()));
    }
    match &entry.summary {
        Some(summary) => {
            ui.label(format!("{}  {summary}", short(entry.oid)));
        }
        None => {
            ui.label(egui::RichText::new(format!("{}: git has thrown this commit away.", short(entry.oid))).color(theme::orange()));
        }
    }
    if entry.unreachable {
        ui.label(
            egui::RichText::new(
                "No branch or tag has this commit any more; git keeps it while the reflog remembers it. \
                 Create a branch here to keep it for good.",
            )
            .color(theme::text_dim()),
        );
    }
}

/// Selects the entry's commit: in the graph when it is there or can be
/// loaded, on its own in the details panel otherwise.
fn go_to(tab: &mut RepoTab, shared: &mut Shared, snap: &RepoSnapshot, dock: &mut ReflogDock, entry: &Entry) {
    dock.jump = None;
    if entry.summary.is_none() {
        shared.toast(
            ToastKind::Error,
            "The commit is gone",
            format!("{} is no longer in the repository: git has thrown it away.", short(entry.oid)),
        );
        return;
    }
    if snap.row_of(entry.oid).is_some() {
        tab.jump_to(entry.oid);
    } else if !entry.unreachable && snap.history.has_more {
        // On a branch, further down than the graph has loaded.
        dock.jump = Some(entry.oid);
        tab.load_more();
    } else {
        tab.select_outside(entry.oid);
        tab.diff = None;
    }
}

fn entry_menu(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, snap: &RepoSnapshot, refname: &str, entry: &Entry) {
    ui.set_min_width(240.0);
    let sha = entry.oid.to_string();
    let abbrev = short(entry.oid);
    if entry.summary.is_some() {
        let is_head = snap.head.oid == Some(entry.oid);
        if !is_head && menu_item(ui, icons::GIT_COMMIT, "Checkout this commit") {
            tab.run(ops::checkout(&sha));
        }
        if menu_item(ui, icons::GIT_BRANCH, "Create branch here…") {
            shared.show(Dialog::CreateBranch { at: Some(sha.clone()), at_label: abbrev.clone(), name: String::new(), checkout: true });
        }
        ui.separator();
        if !is_head && menu_item(ui, icons::GIT_COMMIT, "Cherry pick commit") {
            tab.run(ops::cherry_pick(entry.oid, entry.is_merge));
        }
        if !is_head && snap.head.oid.is_some() {
            let target = snap.head_label();
            ui.menu_button(format!("{}  Reset {target} to this commit", icons::ARROW_LINE_DOWN), |ui| {
                ui.set_min_width(260.0);
                if ui.button("Soft - keep all changes staged").clicked() {
                    tab.run(ops::reset(&sha, ResetMode::Soft));
                }
                if ui.button("Mixed - keep changes in the working tree").clicked() {
                    tab.run(ops::reset(&sha, ResetMode::Mixed));
                }
                if ui.button("Hard - discard all changes").clicked() {
                    shared.show(Dialog::confirm(
                        "Hard reset",
                        format!(
                            "Reset {target} to {abbrev} and discard every uncommitted change? \
                             This cannot be undone."
                        ),
                        "Reset",
                        ops::reset(&sha, ResetMode::Hard),
                    ));
                }
            });
        }
        ui.separator();
    }
    if menu_item(ui, icons::COPY, "Copy commit SHA") {
        ui.ctx().copy_text(sha.clone());
    }
    if menu_item(ui, icons::COPY, &format!("Copy {}", reflog::selector(refname, entry.index))) {
        ui.ctx().copy_text(reflog::selector(refname, entry.index));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::Fixture;

    #[test]
    fn the_palette_opens_switches_and_hides_the_reflog() {
        let fx = Fixture::new("reflog-palette");
        fx.write("a.txt", "a\n");
        fx.commit_all("one");
        let snap = fx.snapshot();
        let mut tab = RepoTab::new(fx.dir.clone());
        let labels = |tab: &RepoTab| {
            let mut list = Vec::new();
            palette_entries(&mut list, tab, &snap);
            list.iter().map(|entry| entry.label.clone()).collect::<Vec<_>>()
        };
        assert_eq!(labels(&tab), ["Show reflog", "Show reflog of main"]);
        open(&mut tab, "HEAD");
        assert_eq!(labels(&tab), ["Show reflog of main", "Hide reflog"]);
        let serial = tab.reflog.as_ref().unwrap().serial;
        // Opening what is open already keeps the panel as it is.
        open(&mut tab, "HEAD");
        assert_eq!(tab.reflog.as_ref().unwrap().serial, serial);
        open_branch(&mut tab, "main");
        assert_eq!(tab.reflog.as_ref().unwrap().refname, "refs/heads/main");
        assert_eq!(labels(&tab), ["Show reflog", "Hide reflog"]);
        close(&mut tab);
        assert!(tab.reflog.is_none());
    }
}
