//! Submodules in the UI: the sidebar's SUBMODULES section and each one's
//! right-click menu, the "Add submodule" dialog, the palette's entries, and
//! what the diff view shows for a submodule that moved (`git::submodule`).

use std::sync::Arc;

use eframe::egui::{self, Align2, Frame, Margin, Rect, Response, RichText, ScrollArea, Sense, TextEdit, Ui, pos2, vec2};

use crate::git::submodule::{self, Change, State, SubmoduleInfo};
use crate::git::{DiffSource, DiffTarget, FileChange, RepoSnapshot, short};
use crate::state::{AppAction, Dialog, RepoTab, Shared};
use crate::theme::{self, icons};
use crate::ui::dialogs;
use crate::ui::palette::Entry;
use crate::ui::sidebar;
use crate::ui::widgets::{elided, menu_item, tint_button};

const TITLE: &str = "SUBMODULES";

// ---- sidebar ----------------------------------------------------------------

/// The SUBMODULES section of the sidebar.
pub fn section(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, snap: &Arc<RepoSnapshot>) {
    let total = snap.submodules.len();
    let (open, header) = sidebar::section_header(ui, tab, TITLE, icons::STACK, total, total);
    if header_plus(ui, &header, (total, total), "add-submodule", "Add submodule") {
        open_add(shared);
    }
    header.context_menu(|ui| {
        ui.set_min_width(220.0);
        if menu_item(ui, icons::PLUS, "Add submodule…") {
            open_add(shared);
        }
        if total > 0 {
            ui.separator();
            if menu_item(ui, icons::DOWNLOAD_SIMPLE, "Initialize and update all") {
                tab.run(submodule::init_update(None));
            }
            if menu_item(ui, icons::LINK, "Sync all URLs") {
                tab.run(submodule::sync(None));
            }
        }
    });
    if !open {
        return;
    }
    if total == 0 {
        let response = sidebar::leaf(ui, 0, false, icons::PLUS, "Add a submodule…", theme::text_dim(), "");
        if response.clicked() {
            open_add(shared);
        }
        return;
    }
    for sub in &snap.submodules {
        let response = row(ui, sub);
        if response.double_clicked() {
            open_tab(tab, shared, sub);
        } else if response.clicked() {
            show_change(tab, snap, &sub.path);
        }
        let response = response.on_hover_text(tooltip(sub));
        response.context_menu(|ui| menu(ui, tab, shared, sub));
    }
}

fn row(ui: &mut Ui, sub: &SubmoduleInfo) -> Response {
    let (color, trailing_color) = match (sub.state, sub.modified) {
        (State::NotInitialized, _) => (theme::text_faint(), theme::text_faint()),
        (State::Moved, _) | (_, true) => (theme::text(), theme::orange()),
        (State::Recorded, false) => (theme::text(), theme::text_dim()),
    };
    let trailing = sub.summary();
    sidebar::row(ui, 0, false, |painter, rect, x| {
        let y = rect.center().y;
        let icon_color = if sub.state == State::NotInitialized { theme::text_faint() } else { theme::text_dim() };
        painter.text(pos2(x, y), Align2::LEFT_CENTER, icons::PACKAGE, theme::font(14.0), icon_color);
        // The state gives way to the path when the sidebar is narrow.
        let right = rect.right() - 10.0;
        let name = elided(painter, &sub.path, theme::font(13.0), color, right - (x + 20.0));
        let name_width = name.size().x;
        painter.galley(pos2(x + 20.0, (y - name.size().y / 2.0).round()), name, color);
        let room = right - (x + 20.0 + name_width + 10.0);
        if room > 24.0 {
            let galley = elided(painter, &trailing, theme::font(11.0), trailing_color, room);
            painter.galley(pos2(right - galley.size().x, (y - galley.size().y / 2.0).round()), galley, trailing_color);
        }
    })
}

fn tooltip(sub: &SubmoduleInfo) -> String {
    let mut lines = vec![sub.path.clone()];
    if sub.name != sub.path {
        lines.push(format!("Named {}", sub.name));
    }
    if let Some(url) = &sub.url {
        lines.push(url.clone());
    }
    if let Some(branch) = &sub.branch {
        lines.push(format!("Follows {branch}"));
    }
    lines.push(sub.describe());
    lines.join("\n")
}

/// Opens the diff of the submodule's change, when it has one.
fn show_change(tab: &mut RepoTab, snap: &RepoSnapshot, path: &str) {
    let changed = |files: &[FileChange]| files.iter().find(|f| f.path == path).cloned();
    if let Some(file) = changed(&snap.status.unstaged) {
        tab.open_diff(DiffTarget::new(DiffSource::Unstaged, &file));
    } else if let Some(file) = changed(&snap.status.staged) {
        tab.open_diff(DiffTarget::new(DiffSource::Staged, &file));
    }
}

fn open_tab(tab: &RepoTab, shared: &mut Shared, sub: &SubmoduleInfo) {
    if sub.state != State::NotInitialized {
        shared.actions.push(AppAction::Open(tab.path.join(&sub.path)));
    }
}

fn menu(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, sub: &SubmoduleInfo) {
    ui.set_min_width(230.0);
    let path = sub.path.as_str();
    match sub.state {
        State::NotInitialized => {
            if menu_item(ui, icons::DOWNLOAD_SIMPLE, "Initialize and update") {
                tab.run(submodule::init_update(Some(path)));
            }
        }
        State::Moved => {
            let label = match sub.recorded {
                Some(oid) => format!("Update to the recorded commit ({})", short(oid)),
                None => "Update to the recorded commit".to_owned(),
            };
            if menu_item(ui, icons::ARROW_COUNTER_CLOCKWISE, &label) {
                tab.run(submodule::to_recorded(path));
            }
        }
        State::Recorded => {
            if menu_item(ui, icons::DOWNLOAD_SIMPLE, "Update, with its submodules") {
                tab.run(submodule::init_update(Some(path)));
            }
        }
    }
    if sub.state != State::NotInitialized {
        if menu_item(ui, icons::ARROW_SQUARE_OUT, "Open in a new tab") {
            open_tab(tab, shared, sub);
        }
        if menu_item(ui, icons::FOLDER_OPEN, "Show in folder") {
            crate::platform::reveal(&tab.path.join(path));
        }
    }
    if menu_item(ui, icons::LINK, "Sync URL") {
        tab.run(submodule::sync(Some(path)));
    }
    ui.separator();
    if menu_item(ui, icons::COPY, "Copy path") {
        ui.ctx().copy_text(sub.path.clone());
    }
    if let Some(url) = &sub.url
        && menu_item(ui, icons::COPY, "Copy URL")
    {
        ui.ctx().copy_text(url.clone());
    }
    ui.separator();
    if menu_item(ui, icons::TRASH, &format!("Remove {path}…")) {
        shared.show(Dialog::confirm("Remove submodule", remove_text(sub), "Remove", submodule::remove(path)));
    }
}

fn remove_text(sub: &SubmoduleInfo) -> String {
    let mut text = format!(
        "Remove the submodule \"{}\"? Its folder and its entry in .gitmodules go, and the removal is staged, ready to commit.",
        sub.path
    );
    if sub.modified {
        text.push_str(" Its uncommitted changes are lost.");
    } else {
        text.push_str(" Its clone stays in .git/modules, so commits made in it are not lost.");
    }
    text
}

/// A "+" on a section header, left of its count (shown, total), while the
/// pointer is over the header: true when clicked. Like the REMOTE header's.
pub(crate) fn header_plus(ui: &mut Ui, header: &Response, (shown, total): (usize, usize), salt: &str, tip: &str) -> bool {
    let bar = header.rect;
    if !ui.rect_contains_pointer(bar) || ui.ctx().dragged_id().is_some() {
        return false;
    }
    // Where `sidebar::section_header` painted the count.
    let count = if shown == total { total.to_string() } else { format!("{shown}/{total}") };
    let count_width = ui.painter().layout_no_wrap(count, theme::bold(11.5), theme::accent()).size().x;
    let size = vec2(22.0, 22.0);
    let plus = Rect::from_min_size(pos2(bar.right() - 12.0 - count_width - 6.0 - size.x, bar.center().y - size.y / 2.0), size);
    let response = ui.interact(plus, header.id.with(salt), Sense::click());
    if ui.is_rect_visible(plus) {
        let painter = ui.painter();
        let fill = if response.hovered() { theme::bg_raised() } else { theme::bg_hover() };
        painter.rect_filled(plus, 4.0, fill);
        let color = if response.hovered() { theme::text_bright() } else { theme::text_dim() };
        painter.text(plus.center(), Align2::CENTER_CENTER, icons::PLUS, theme::font(14.0), color);
    }
    response.on_hover_text(tip).clicked()
}

// ---- in file lists ---------------------------------------------------------

/// The icon a changed-file row shows when the file is a submodule, in the
/// colour of its kind of change.
pub fn row_icon(snap: Option<&RepoSnapshot>, file: &FileChange) -> Option<(&'static str, egui::Color32)> {
    let snap = snap?;
    snap.submodules.iter().any(|s| s.path == file.path).then(|| (icons::PACKAGE, crate::ui::widgets::kind_icon(file.kind).1))
}

/// A line for the tooltip of a changed-file row that is a submodule.
pub fn row_note(snap: Option<&RepoSnapshot>, path: &str) -> Option<String> {
    let sub = snap?.submodules.iter().find(|s| s.path == path)?;
    Some(format!("Submodule. {}", sub.describe()))
}

/// Whether `path` is one of the submodules.
pub fn is_submodule(snap: Option<&RepoSnapshot>, path: &str) -> bool {
    snap.is_some_and(|snap| snap.submodules.iter().any(|s| s.path == path))
}

/// The items a submodule's row gets in an unstaged list instead of
/// "Discard changes", which does nothing to one.
pub fn file_items(ui: &mut Ui, tab: &RepoTab, shared: &mut Shared, path: &str) -> Option<crate::git::ops::GitCmd> {
    let sub = tab.snap.as_ref()?.submodules.iter().find(|s| s.path == path)?.clone();
    let mut run = None;
    if sub.state == State::Moved && menu_item(ui, icons::ARROW_COUNTER_CLOCKWISE, "Update to the recorded commit") {
        run = Some(submodule::to_recorded(path));
    }
    if sub.state != State::NotInitialized && menu_item(ui, icons::ARROW_SQUARE_OUT, "Open submodule in a new tab") {
        open_tab(tab, shared, &sub);
    }
    run
}

// ---- in the diff view ---------------------------------------------------------

/// What the diff view shows for a submodule: the commits it moved between,
/// with their summaries and the commits in between when its clone has them.
pub fn diff_view(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, target: &DiffTarget, change: &Change) {
    let snap = tab.snap.clone();
    let sub = snap.as_ref().and_then(|snap| snap.submodules.iter().find(|s| s.path == target.path));
    let mut run = None;
    ScrollArea::vertical().id_salt("submodule-diff").auto_shrink([false, false]).show(ui, |ui| {
        Frame::new().inner_margin(Margin::symmetric(20, 16)).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            ui.horizontal(|ui| {
                ui.label(RichText::new(icons::PACKAGE).size(18.0).color(theme::text_dim()));
                ui.label(RichText::new(format!("Submodule {}", target.path)).font(theme::bold(14.5)).color(theme::text_bright()));
            });
            let mut headline = change.headline();
            if let Some(first) = headline.get(..1) {
                headline = first.to_uppercase() + &headline[1..];
            }
            ui.label(RichText::new(headline).size(13.5).color(theme::text()));
            ui.add_space(4.0);
            end(ui, "From", change.old, change.old_summary.as_deref());
            end(ui, "To", change.new, change.new_summary.as_deref());
            ui.add_space(4.0);

            let commits = |n: usize| if n == 1 { "1 commit".to_owned() } else { format!("{n} commits") };
            let between = match (change.ahead, change.behind) {
                (0, 0) => None,
                (ahead, 0) => Some(format!("{} forward:", commits(ahead))),
                (0, behind) => Some(format!("{} back, leaving out:", commits(behind))),
                (ahead, behind) => Some(format!("To another line of history: {} added, {} left out. Added:", commits(ahead), commits(behind))),
            };
            if let Some(between) = between {
                ui.label(RichText::new(between).size(12.5).color(theme::text_dim()));
                for (oid, summary) in &change.commits {
                    ui.horizontal(|ui| {
                        ui.add_space(10.0);
                        ui.label(RichText::new(short(*oid)).font(theme::mono(12.0)).color(theme::accent()));
                        ui.label(RichText::new(summary).size(12.5).color(theme::text()));
                    });
                }
                let listed = change.commits.len();
                let total = if change.ahead > 0 { change.ahead } else { change.behind };
                if total > listed {
                    ui.label(RichText::new(format!("and {} more", total - listed)).size(12.0).color(theme::text_faint()));
                }
            }
            if !change.checked_out {
                note(ui, "Not checked out here, so its commits cannot be looked up. Initialize it to see them.");
            } else if (change.old.is_some() && change.old_summary.is_none()) || (change.new.is_some() && change.new_summary.is_none()) {
                note(ui, "Its clone does not have every commit named here: fetch in the submodule to see them.");
            }
            if change.dirty {
                ui.add_space(2.0);
                ui.label(
                    RichText::new(format!(
                        "{}  Its own files have changes. Commit them in the submodule: staging here only records which commit it is at.",
                        icons::WARNING
                    ))
                    .size(12.5)
                    .color(theme::orange()),
                );
            }

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                if let Some(sub) = sub {
                    if target.source == DiffSource::Unstaged
                        && sub.state == State::Moved
                        && tint_button(ui, "Update to the recorded commit", theme::accent()).clicked()
                    {
                        run = Some(submodule::to_recorded(&sub.path));
                    }
                    if sub.state != State::NotInitialized && tint_button(ui, "Open in a new tab", theme::accent()).clicked() {
                        open_tab(tab, shared, sub);
                    }
                }
            });
        });
    });
    if let Some(cmd) = run {
        tab.run(cmd);
    }
}

/// One end of a move: "From  abc1234  Fix the thing".
fn end(ui: &mut Ui, label: &str, oid: Option<crate::git::Oid>, summary: Option<&str>) {
    let Some(oid) = oid else { return };
    ui.horizontal(|ui| {
        ui.add_sized(vec2(40.0, 18.0), egui::Label::new(RichText::new(label).size(12.5).color(theme::text_dim())));
        ui.label(RichText::new(short(oid)).font(theme::mono(12.5)).color(theme::accent()));
        let text = summary.unwrap_or("(not in its clone)");
        let color = if summary.is_some() { theme::text_bright() } else { theme::text_faint() };
        ui.label(RichText::new(text).size(13.0).color(color));
    });
}

fn note(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(12.0).color(theme::text_faint()));
}

// ---- adding one -----------------------------------------------------------------

/// What [`Dialog::Submodule`] holds: "Add submodule".
#[derive(Default)]
pub struct AddForm {
    url: String,
    path: String,
    /// The path was typed, not made from the URL.
    path_typed: bool,
    branch: String,
}

/// Whether the dialog stays open.
pub enum Next {
    Keep,
    Close,
}

pub fn open_add(shared: &mut Shared) {
    shared.show(Dialog::Submodule(Box::default()));
}

fn caption(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(12.0).color(theme::text_dim()));
}

pub fn dialog(ui: &mut Ui, form: &mut AddForm, tab: Option<&mut RepoTab>, fresh: bool) -> Next {
    let Some(tab) = tab else { return Next::Close };
    dialogs::title(ui, "Add submodule");
    caption(ui, "Repository URL");
    let url = ui.add(TextEdit::singleline(&mut form.url).hint_text("https://… or git@…").desired_width(f32::INFINITY));
    if fresh {
        url.request_focus();
    }
    if url.changed() && !form.path_typed {
        form.path = submodule::path_from_url(&form.url);
    }
    caption(ui, "Path in this repository");
    let path = ui.add(TextEdit::singleline(&mut form.path).hint_text("libs/name").desired_width(f32::INFINITY));
    if path.changed() {
        form.path_typed = !form.path.is_empty();
    }
    caption(ui, "Branch to follow (optional)");
    ui.add(TextEdit::singleline(&mut form.branch).hint_text("The remote's default branch").desired_width(f32::INFINITY));
    dialogs::sanitize_ref(&mut form.branch);

    let existing = tab.snap.as_ref().map(|snap| snap.submodules.iter().map(|s| s.path.clone()).collect::<Vec<_>>());
    let existing = existing.unwrap_or_default();
    let problem = submodule::check_path(form.path.trim(), existing.iter().map(String::as_str));
    if let Err(problem) = &problem
        && !problem.is_empty()
    {
        ui.label(RichText::new(format!("{}  {problem}", icons::WARNING)).color(theme::orange()));
    }
    let branch_ok = form.branch.trim().is_empty() || dialogs::valid_ref(&form.branch);
    ui.label(
        RichText::new("It is cloned now; it and .gitmodules come staged, ready to commit.")
            .size(12.0)
            .color(theme::text_faint()),
    );
    let ready = !form.url.trim().is_empty() && problem.is_ok() && branch_ok && !tab.is_running("Add submodule");
    match dialogs::buttons(ui, "Add submodule", ready, false) {
        Some(true) => {
            let path = form.path.trim().replace('\\', "/");
            tab.run(submodule::add(form.url.trim(), path.trim_end_matches('/'), form.branch.trim()));
            Next::Close
        }
        Some(false) => Next::Close,
        None => Next::Keep,
    }
}

// ---- command palette ----------------------------------------------------------

pub fn palette_entries(list: &mut Vec<Entry>, tab: &RepoTab, snap: &RepoSnapshot) {
    list.push(
        Entry::on_tab("submodule-add", icons::PLUS, "Add submodule…", |_, shared| open_add(shared))
            .keywords("new submodule nested repository library vendor"),
    );
    if snap.submodules.is_empty() || tab.is_running("Update submodule") {
        return;
    }
    list.push(
        Entry::on_tab("submodule-update-all", icons::DOWNLOAD_SIMPLE, "Initialize and update all submodules", |tab, _| {
            tab.run(submodule::init_update(None))
        })
        .detail("git submodule update --init --recursive")
        .keywords("submodules init clone recursive checkout recorded"),
    );
    list.push(
        Entry::on_tab("submodule-sync", icons::LINK, "Sync submodule URLs", |tab, _| tab.run(submodule::sync(None)))
            .detail("from .gitmodules")
            .keywords("submodules url remote sync"),
    );
    for sub in snap.submodules.iter().filter(|s| s.state != State::NotInitialized) {
        let path = tab.path.join(&sub.path);
        list.push(
            Entry::on_tab(format!("submodule-open:{}", sub.path), icons::ARROW_SQUARE_OUT, format!("Open submodule {}", sub.path), move |_, shared| {
                shared.actions.push(AppAction::Open(path))
            })
            .detail(sub.summary())
            .keywords("submodule tab"),
        );
    }
}
