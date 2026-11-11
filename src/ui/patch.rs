//! Patch files in the UI (`git::patch`): "Save as patch…" on a commit or
//! several, "Save … as a patch…" for the uncommitted changes, and "Apply
//! patch…" (File menu, command palette) with the dialog that says how the
//! patch applies before anything is changed.

use std::path::PathBuf;

use eframe::egui::{Align, Button, Frame, Layout, Margin, RichText, ScrollArea, Spinner, Ui};

use crate::git::patch::{self, Changes, Check, How, PatchInfo};
use crate::git::{ChangeKind, Oid, RepoSnapshot, RepoState, RowKind, ops, short};
use crate::state::{Dialog, RepoTab, Shared};
use crate::theme::{self, icons};
use crate::ui::dialogs;
use crate::ui::palette::Entry;
use crate::ui::widgets::{menu_item, tint_button};

const PATCHES: (&str, &[&str]) = ("Patch files", &["patch", "diff", "mbox", "eml"]);

// ---- file dialogs ---------------------------------------------------------------

/// Where to save a file, asked with the system's dialog. A scripted run
/// (`GITKRILL_SHOT`) opens none: it takes `GITKRILL_SHOT_FILE`.
pub fn ask_save(title: &str, name: &str, filters: &[(&str, &[&str])]) -> Option<PathBuf> {
    if std::env::var_os("GITKRILL_SHOT").is_some() {
        return crate::shot::Shot::picked_file();
    }
    let mut dialog = rfd::FileDialog::new().set_title(title).set_file_name(name);
    for (label, extensions) in filters {
        dialog = dialog.add_filter(*label, extensions);
    }
    dialog.save_file()
}

/// A file to open, as [`ask_save`] asks.
pub fn ask_open(title: &str, filters: &[(&str, &[&str])]) -> Option<PathBuf> {
    if std::env::var_os("GITKRILL_SHOT").is_some() {
        return crate::shot::Shot::picked_file();
    }
    let mut dialog = rfd::FileDialog::new().set_title(title);
    for (label, extensions) in filters {
        dialog = dialog.add_filter(*label, extensions);
    }
    dialog.add_filter("All files", &["*"]).pick_file()
}

/// A folder to save into (`GITKRILL_SHOT_FOLDER` in a scripted run).
fn ask_folder(title: &str) -> Option<PathBuf> {
    if std::env::var_os("GITKRILL_SHOT").is_some() {
        return crate::shot::Shot::picked_folder();
    }
    rfd::FileDialog::new().set_title(title).pick_folder()
}

// ---- saving -------------------------------------------------------------------

/// "Save as patch…" for a commit selected alone. A merge commit has none.
pub fn commit_item(ui: &mut Ui, tab: &mut RepoTab, snap: &RepoSnapshot, oid: Oid) {
    let Some(commit) = snap.row_of(oid).map(|row| &snap.history.commits[row]) else { return };
    let text = format!("{}  Save as patch…", icons::FILE_ARROW_DOWN);
    if commit.parents.len() > 1 {
        ui.add_enabled(false, Button::new(text))
            .on_disabled_hover_text("A merge commit has no patch of its own: git format-patch leaves merges out.");
        return;
    }
    if ui.button(text).on_hover_text("git format-patch: a file git am can commit again").clicked() {
        save_commit(tab, oid, &commit.summary);
    }
}

fn save_commit(tab: &mut RepoTab, oid: Oid, summary: &str) {
    if let Some(file) = ask_save("Save the commit as a patch", &patch::mail_file_name(1, summary), &[PATCHES]) {
        tab.run(patch::save_combined(&[oid], &file));
    }
}

/// The selected commits that have a patch, oldest first (merges have none).
fn patchable(snap: &RepoSnapshot, newest_first: &[Oid]) -> Vec<Oid> {
    newest_first
        .iter()
        .rev()
        .filter(|oid| snap.row_of(**oid).is_some_and(|row| snap.history.commits[row].parents.len() <= 1))
        .copied()
        .collect()
}

/// "Save as patches…" (one file each, in a folder) and "Save as one patch
/// file…" for several selected commits, `commits` newest first.
pub fn multi_items(ui: &mut Ui, tab: &mut RepoTab, snap: &RepoSnapshot, commits: &[Oid]) {
    let picked = patchable(snap, commits);
    let merges = commits.len() - picked.len();
    let note = match merges {
        0 => String::new(),
        1 => " The merge commit among them is left out: it has no patch of its own.".to_owned(),
        n => format!(" The {n} merge commits among them are left out: they have no patch of their own."),
    };
    let enabled = !picked.is_empty();
    let each = Button::new(format!("{}  Save as {}…", icons::FILE_ARROW_DOWN, patch::patches(picked.len())));
    let response = ui.add_enabled(enabled, each);
    if !enabled {
        response.on_disabled_hover_text("Merge commits have no patch of their own.");
        return;
    }
    if response.on_hover_text(format!("One file per commit, numbered oldest first, in a folder you pick.{note}")).clicked() {
        save_each(tab, &picked);
    }
    let one = ui.button(format!("{}  Save as one patch file…", icons::FILE_ARROW_DOWN));
    if one.on_hover_text(format!("Every commit's patch in one file, oldest first, which git am commits in order.{note}")).clicked() {
        save_combined(tab, &picked);
    }
}

fn save_each(tab: &mut RepoTab, oldest_first: &[Oid]) {
    if let Some(dir) = ask_folder("Save the patches in…") {
        tab.run(patch::save_each(oldest_first, &dir));
    }
}

fn save_combined(tab: &mut RepoTab, oldest_first: &[Oid]) {
    let (Some(first), Some(last)) = (oldest_first.first(), oldest_first.last()) else { return };
    let name = format!("{}..{}.patch", short(*first), short(*last));
    if let Some(file) = ask_save("Save the commits as one patch", &name, &[PATCHES]) {
        tab.run(patch::save_combined(oldest_first, &file));
    }
}

/// Which uncommitted changes can be saved: none while a file has conflicts
/// (its diff is not a patch). All of them, then the staged or the unstaged
/// ones when there are both.
fn savable(snap: &RepoSnapshot) -> Vec<Changes> {
    let status = &snap.status;
    if status.is_clean() || status.unstaged.iter().any(|f| f.kind == ChangeKind::Conflicted) {
        return Vec::new();
    }
    let mut list = vec![Changes::All];
    if !status.staged.is_empty() && !status.unstaged.is_empty() {
        list.extend([Changes::Staged, Changes::Unstaged]);
    }
    list
}

fn changes_label(changes: Changes) -> String {
    match changes {
        Changes::All => "Save changes as a patch…".to_owned(),
        other => format!("Save {} as a patch…", other.label()),
    }
}

/// "Save changes as a patch…" and its staged and unstaged variants, for the
/// menu of the WIP row.
pub fn changes_items(ui: &mut Ui, tab: &mut RepoTab, snap: &RepoSnapshot) {
    let list = savable(snap);
    if !list.is_empty() {
        ui.separator();
    }
    for changes in list {
        if menu_item(ui, icons::FILE_ARROW_DOWN, &changes_label(changes)) {
            save_changes(tab, changes);
        }
    }
}

/// "Save staged / unstaged changes as a patch…", for the menus of the
/// changed-file lists' headers.
pub fn list_item(ui: &mut Ui, tab: &mut RepoTab, snap: &RepoSnapshot, changes: Changes) {
    let empty = match changes {
        Changes::Staged => snap.status.staged.is_empty(),
        _ => snap.status.unstaged.is_empty(),
    };
    let conflicted = snap.status.unstaged.iter().any(|f| f.kind == ChangeKind::Conflicted);
    let response = ui.add_enabled(!empty && !conflicted, Button::new(format!("{}  {}", icons::FILE_ARROW_DOWN, changes_label(changes))));
    if empty {
        response.on_disabled_hover_text(format!("There are no {}.", changes.label()));
    } else if conflicted {
        response.on_disabled_hover_text("Resolve the conflicted files first.");
    } else if response.clicked() {
        save_changes(tab, changes);
    }
}

fn save_changes(tab: &mut RepoTab, changes: Changes) {
    if let Some(file) = ask_save("Save the changes as a patch", changes.file_name(), &[PATCHES]) {
        tab.run(patch::save_changes(changes, &file));
    }
}

// ---- applying -----------------------------------------------------------------

/// What the "Apply patch" dialog holds.
pub struct ApplyForm {
    path: PathBuf,
    /// What [`patch::inspect`] found; `None` while it looks.
    info: Option<Result<PatchInfo, String>>,
    how: How,
}

/// The reader-thread job that inspects the picked file.
const INSPECT: &str = "patch:inspect";

/// "Apply patch…": asks for a patch file, then shows how it applies.
pub fn open_apply(tab: &mut RepoTab, shared: &mut Shared) {
    if let Some(path) = ask_open("Apply a patch", &[PATCHES]) {
        open_apply_file(tab, shared, path);
    }
}

/// The dialog for the patch at `path`, checked on the reader thread.
pub fn open_apply_file(tab: &mut RepoTab, shared: &mut Shared, path: PathBuf) {
    let asked = path.clone();
    tab.spawn_job(INSPECT, move |loader| {
        let result = match loader.repo().workdir() {
            Some(workdir) => patch::inspect(workdir, &asked),
            None => Err("This repository has no working tree.".to_owned()),
        };
        (asked, result)
    });
    shared.show(Dialog::ApplyPatch(Box::new(ApplyForm { path, info: None, how: How::Tree })));
}

/// Whether the dialog stays open.
pub enum Next {
    Keep,
    Close,
}

pub fn dialog(ui: &mut Ui, form: &mut ApplyForm, tab: Option<&mut RepoTab>) -> Next {
    dialogs::title(ui, "Apply patch");
    let Some(tab) = tab else { return Next::Close };
    let name = form.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    ui.label(RichText::new(format!("{}  {name}", icons::FILE_TEXT)).color(theme::text())).on_hover_text(form.path.display().to_string());

    // The answer carries the file it is about: one for a dialog closed
    // before it came back must not pass for this one's.
    if let Some((asked, result)) = tab.take_result::<(PathBuf, Result<PatchInfo, String>)>(INSPECT)
        && asked == form.path
    {
        if let Ok(info) = &result {
            form.how = if !info.mails.is_empty() && info.commit_blocker.is_none() && !matches!(info.index, Check::Fails(_)) {
                How::Commit
            } else {
                How::Tree
            };
        }
        form.info = Some(result);
    }
    let info = match &form.info {
        None => {
            ui.horizontal(|ui| {
                ui.add(Spinner::new().size(13.0).color(theme::accent()));
                ui.label(RichText::new("Checking how it applies…").color(theme::text_dim()));
            });
            return match dialogs::buttons(ui, "Apply", false, false) {
                Some(_) => Next::Close,
                None => Next::Keep,
            };
        }
        Some(Err(e)) => {
            ui.colored_label(theme::red(), e);
            return match dialogs::buttons(ui, "Apply", false, false) {
                Some(_) => Next::Close,
                None => Next::Keep,
            };
        }
        Some(Ok(info)) => info.clone(),
    };

    // What it holds.
    let n = info.mails.len();
    if n > 0 {
        let what = if n == 1 { "One commit, from git format-patch:".to_owned() } else { format!("{n} commits, from git format-patch:") };
        ui.label(RichText::new(what).color(theme::text_dim()));
        ScrollArea::vertical().id_salt("patch-mails").max_height(92.0).auto_shrink([false, true]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            for subject in &info.mails {
                ui.label(RichText::new(format!("{}  {subject}", icons::GIT_COMMIT)).color(theme::text()));
            }
        });
    }

    // How to apply it.
    ui.add_space(2.0);
    let state = tab.snap.as_ref().map_or(RepoState::Clean, |snap| snap.state);
    if n > 0 {
        let commit = ui.radio_value(&mut form.how, How::Commit, format!("Commit {}, with their authors and messages", if n == 1 { "it" } else { "them" }));
        if let Some(why) = &info.commit_blocker {
            commit.on_hover_text(why);
        }
    }
    ui.radio_value(&mut form.how, How::Tree, "Apply the changes to the working tree");
    ui.radio_value(&mut form.how, How::Stage, "Apply the changes and stage them");

    // How it applies that way.
    let check = if form.how == How::Tree { &info.tree } else { &info.index };
    ui.add_space(4.0);
    let mut blocker: Option<String> = match state.label() {
        Some(label) if state != RepoState::Clean => Some(format!("{label}: finish or abort it first.")),
        _ => None,
    };
    if form.how == How::Commit && blocker.is_none() {
        blocker = info.commit_blocker.clone();
        if blocker.is_none() && matches!(check, Check::Fails(_)) {
            blocker = Some("git am would stop at the first file that does not apply: apply the changes to the working tree instead.".to_owned());
        }
    }
    summary(ui, check, &info, form.how);
    file_list(ui, check, &info);
    if let Some(why) = &blocker {
        ui.colored_label(theme::orange(), why);
    }
    if tab.is_running("Apply ") {
        blocker = Some(String::new());
    }

    let button = match (form.how, check) {
        (How::Commit, _) => format!("Commit {}", patch::patches(n)),
        (_, Check::Fails(_)) => "Apply the rest".to_owned(),
        _ => "Apply".to_owned(),
    };
    match dialogs::buttons(ui, &button, blocker.is_none(), false) {
        Some(true) => {
            tab.run(patch::apply(&tab.path, &info, form.how));
            Next::Close
        }
        Some(false) => Next::Close,
        None => Next::Keep,
    }
}

/// One line on how the patch applies the way chosen.
fn summary(ui: &mut Ui, check: &Check, info: &PatchInfo, how: How) {
    let files = match info.files.len() {
        1 => "its file".to_owned(),
        n => format!("its {n} files"),
    };
    let (icon, color, text) = match check {
        Check::Clean => (icons::CHECK_CIRCLE, theme::green(), format!("Applies cleanly to {files}.")),
        Check::Merge { conflicts } if conflicts.is_empty() => (
            icons::CHECK_CIRCLE,
            theme::green(),
            "Does not apply as it is, but a three-way merge sorts it out.".to_owned(),
        ),
        Check::Merge { conflicts } => (
            icons::WARNING,
            theme::orange(),
            format!(
                "Does not apply as it is: a three-way merge leaves conflicts in {} to resolve.",
                if conflicts.len() == 1 { "one file".to_owned() } else { format!("{} files", conflicts.len()) }
            ),
        ),
        Check::Fails(failed) if how == How::Commit => {
            (icons::X_CIRCLE, theme::red(), format!("Does not apply: {}.", count_files(failed.len())))
        }
        Check::Fails(failed) => (
            icons::WARNING,
            theme::orange(),
            format!(
                "Does not apply to {}, even with a three-way merge. The rest can be applied; what does not \
                 apply is saved next to each file as <file>.rej.",
                count_files(failed.len())
            ),
        ),
    };
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(icon).color(color));
        ui.label(RichText::new(text).color(theme::text()));
    });
}

fn count_files(n: usize) -> String {
    if n == 1 { "one file".to_owned() } else { format!("{n} files") }
}

/// The files the patch changes, each with how it applies.
fn file_list(ui: &mut Ui, check: &Check, info: &PatchInfo) {
    ScrollArea::vertical().id_salt("patch-files").max_height(150.0).auto_shrink([false, true]).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        if let Check::Fails(failed) = check {
            for failure in failed.iter().filter(|f| f.path.is_none()) {
                ui.label(RichText::new(format!("{}  {}", icons::X_CIRCLE, failure.why)).size(12.5).color(theme::red()));
            }
        }
        for file in &info.files {
            let (icon, color, why) = match check {
                Check::Merge { conflicts } if conflicts.contains(file) => (icons::WARNING, theme::orange(), "conflicts"),
                Check::Fails(failed) => match failed.iter().find(|f| f.path.as_ref() == Some(file)) {
                    Some(failure) => (icons::X_CIRCLE, theme::red(), failure.why.as_str()),
                    None => (icons::CHECK, theme::green(), ""),
                },
                _ => (icons::CHECK, theme::green(), ""),
            };
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(RichText::new(icon).size(12.5).color(color));
                ui.label(RichText::new(file).size(12.5).color(theme::text()));
                if !why.is_empty() {
                    ui.label(RichText::new(why).size(12.0).color(theme::text_dim()));
                }
            });
        }
    });
}

/// The banner over the changed files while `git am` waits on a mail that
/// did not apply: Continue once its conflicts are resolved, Skip it, or
/// Abort them all. The buttons get a row of their own, three do not fit
/// beside the label.
pub fn banner(ui: &mut Ui, tab: &mut RepoTab, label: &str) {
    let state = RepoState::ApplyMailbox;
    Frame::new()
        .fill(theme::mix(theme::bg_app(), theme::orange(), 0.18))
        .inner_margin(Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.label(RichText::new(format!("{}  {label}", icons::WARNING)).color(theme::orange()));
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if let Some(cmd) = ops::abort(state)
                        && tint_button(ui, "Abort", theme::red()).on_hover_text("Stop, and put the branch back as it was").clicked()
                    {
                        tab.run(cmd);
                    }
                    if let Some(cmd) = ops::skip(state)
                        && tint_button(ui, "Skip", theme::orange()).on_hover_text("Leave this patch out and go on with the next").clicked()
                    {
                        tab.run(cmd);
                    }
                    if let Some(cmd) = ops::continue_op(state)
                        && tint_button(ui, "Continue", theme::green()).on_hover_text("Commit this patch once its conflicts are resolved, then go on").clicked()
                    {
                        tab.run(cmd);
                    }
                });
            });
        });
    ui.add_space(4.0);
}

// ---- command palette -------------------------------------------------------------

/// "Apply patch…", saving the changes, and saving the selected commits.
pub fn palette_entries(list: &mut Vec<Entry>, tab: &RepoTab, snap: &RepoSnapshot) {
    if snap.state == RepoState::Clean && !tab.is_running("Apply ") {
        list.push(
            Entry::on_tab("apply-patch", icons::BANDAIDS, "Apply patch…", open_apply)
                .detail("git apply, or git am for commits")
                .keywords("patch diff mbox am import mail"),
        );
    }
    for changes in savable(snap) {
        let label = changes_label(changes);
        list.push(
            Entry::on_tab(format!("save-patch-{}", changes.file_name()), icons::FILE_ARROW_DOWN, label.trim_end_matches('…'), move |tab, _| {
                save_changes(tab, changes)
            })
            .keywords("patch diff export file"),
        );
    }

    if tab.multi.is_active() {
        let picked = patchable(snap, &tab.multi.picked.ordered(&snap.history));
        if picked.is_empty() {
            return;
        }
        let n = picked.len();
        let each = picked.clone();
        list.push(
            Entry::on_tab("multi-save-patches", icons::FILE_ARROW_DOWN, format!("Save {n} commits as patches"), move |tab, _| {
                save_each(tab, &each)
            })
            .detail("one file each, in a folder")
            .keywords("format-patch export patch files"),
        );
        list.push(
            Entry::on_tab("multi-save-mbox", icons::FILE_ARROW_DOWN, format!("Save {n} commits as one patch file"), move |tab, _| {
                save_combined(tab, &picked)
            })
            .keywords("format-patch export patch mbox"),
        );
        return;
    }
    let crate::state::Selection::Commit(oid) = tab.selection else { return };
    let Some(commit) = snap.row_of(oid).map(|row| &snap.history.commits[row]) else { return };
    if commit.kind != RowKind::Commit || commit.parents.len() > 1 {
        return;
    }
    let summary = commit.summary.clone();
    list.push(
        Entry::on_tab("save-commit-patch", icons::FILE_ARROW_DOWN, "Save commit as a patch", move |tab, _| {
            save_commit(tab, oid, &summary)
        })
        .detail(format!("{} {}", short(oid), commit.summary))
        .keywords("format-patch export patch file"),
    );
}
