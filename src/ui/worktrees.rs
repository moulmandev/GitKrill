//! Worktrees in the UI: the sidebar's WORKTREES section and each one's
//! right-click menu, the dialogs to add and remove one, what is said when a
//! branch to check out is in another worktree, and the palette's entries
//! (`git::worktree`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use eframe::egui::{Align2, ComboBox, RichText, Spinner, TextEdit, Ui, pos2};

use crate::git::worktree::{self, WorktreeInfo};
use crate::git::{BranchInfo, RepoSnapshot, short};
use crate::state::{AppAction, Dialog, RepoTab, Shared};
use crate::theme::{self, icons};
use crate::ui::dialogs;
use crate::ui::palette::Entry;
use crate::ui::sidebar;
use crate::ui::widgets::{elided, menu_item};

const TITLE: &str = "WORKTREES";

// ---- sidebar ----------------------------------------------------------------

/// The WORKTREES section of the sidebar.
pub fn section(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, snap: &Arc<RepoSnapshot>) {
    let total = snap.worktrees.len();
    let (open, header) = sidebar::section_header(ui, tab, TITLE, icons::FOLDERS, total, total);
    if crate::ui::submodules::header_plus(ui, &header, (total, total), "add-worktree", "Add worktree") {
        open_add(tab, shared, None);
    }
    let prunable = snap.worktrees.iter().any(|w| w.prunable);
    header.context_menu(|ui| {
        ui.set_min_width(200.0);
        if menu_item(ui, icons::PLUS, "Add worktree…") {
            open_add(tab, shared, None);
        }
        if prunable && menu_item(ui, icons::BROOM, "Prune missing worktrees") {
            tab.run(worktree::prune());
        }
    });
    if !open {
        return;
    }
    for wt in &snap.worktrees {
        let response = row(ui, wt);
        if response.double_clicked() && !wt.current && !wt.prunable {
            shared.actions.push(AppAction::Open(wt.path.clone()));
        } else if response.clicked()
            && let Some(oid) = wt.head
        {
            tab.jump_to(oid);
        }
        let response = response.on_hover_text(tooltip(wt));
        response.context_menu(|ui| menu(ui, tab, shared, wt));
    }
}

fn row(ui: &mut Ui, wt: &WorktreeInfo) -> eframe::egui::Response {
    let (icon, color) = if wt.current {
        (icons::CHECK, theme::text_bright())
    } else if wt.prunable {
        (icons::FOLDER_DASHED, theme::text_faint())
    } else {
        (icons::FOLDER_SIMPLE, theme::text())
    };
    let trailing = match (&wt.branch, wt.head) {
        _ if wt.prunable => "missing".to_owned(),
        (Some(branch), _) => branch.clone(),
        (None, Some(oid)) => format!("detached {}", short(oid)),
        (None, None) => String::new(),
    };
    sidebar::row(ui, 0, wt.current, |painter, rect, x| {
        let y = rect.center().y;
        let icon_color = if wt.current { theme::text_bright() } else { theme::text_dim() };
        painter.text(pos2(x, y), Align2::LEFT_CENTER, icon, theme::font(14.0), icon_color);
        let mut right = rect.right() - 10.0;
        if wt.locked.is_some() {
            let used = painter.text(pos2(right, y), Align2::RIGHT_CENTER, icons::LOCK_SIMPLE, theme::font(12.0), theme::text_dim());
            right = used.left() - 5.0;
        }
        // The branch gives way to the folder's name when the sidebar is narrow.
        let name = elided(painter, &wt.title(), if wt.current { theme::bold(13.0) } else { theme::font(13.0) }, color, right - (x + 20.0));
        let name_width = name.size().x;
        painter.galley(pos2(x + 20.0, (y - name.size().y / 2.0).round()), name, color);
        let room = right - (x + 20.0 + name_width + 10.0);
        if room > 24.0 && !trailing.is_empty() {
            let galley = elided(painter, &trailing, theme::font(11.0), theme::text_dim(), room);
            painter.galley(pos2(right - galley.size().x, (y - galley.size().y / 2.0).round()), galley, theme::text_dim());
        }
    })
}

fn tooltip(wt: &WorktreeInfo) -> String {
    let mut lines = vec![wt.display()];
    match (&wt.branch, wt.head) {
        (Some(branch), _) => lines.push(format!("On {branch}")),
        (None, Some(oid)) => lines.push(format!("Detached at {}", short(oid))),
        (None, None) => {}
    }
    if wt.main {
        lines.push("The main working tree".to_owned());
    }
    if wt.current {
        lines.push("Shown in this tab".to_owned());
    }
    match wt.locked.as_deref() {
        Some("") => lines.push("Locked".to_owned()),
        Some(reason) => lines.push(format!("Locked: {reason}")),
        None => {}
    }
    if wt.prunable {
        lines.push("Its folder is gone: prune it to forget it".to_owned());
    }
    lines.join("\n")
}

fn menu(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, wt: &WorktreeInfo) {
    ui.set_min_width(210.0);
    if wt.prunable {
        if menu_item(ui, icons::BROOM, "Prune missing worktrees") {
            tab.run(worktree::prune());
        }
    } else {
        if !wt.current && menu_item(ui, icons::ARROW_SQUARE_OUT, "Open in a new tab") {
            shared.actions.push(AppAction::Open(wt.path.clone()));
        }
        if menu_item(ui, icons::FOLDER_OPEN, "Open folder") {
            crate::platform::reveal(&wt.path);
        }
    }
    if !wt.main {
        ui.separator();
        // A locked worktree is not pruned, even when its folder is gone.
        if wt.locked.is_some() {
            if menu_item(ui, icons::LOCK_SIMPLE_OPEN, "Unlock") {
                tab.run(worktree::unlock(&wt.path));
            }
        } else if !wt.prunable && menu_item(ui, icons::LOCK_SIMPLE, "Lock") {
            tab.run(worktree::lock(&wt.path));
        }
        if !wt.current && !wt.prunable && menu_item(ui, icons::TRASH, &format!("Remove {}…", wt.title())) {
            open_remove(tab, shared, wt);
        }
    }
    ui.separator();
    if menu_item(ui, icons::COPY, "Copy path") {
        ui.ctx().copy_text(wt.display());
    }
}

// ---- branches in other worktrees ----------------------------------------------

/// The other worktree `branch` is checked out in, for the branch list to
/// mark it and for checking it out to explain instead.
pub fn elsewhere<'a>(snap: &'a RepoSnapshot, branch: &str) -> Option<&'a WorktreeInfo> {
    worktree::elsewhere(&snap.worktrees, branch)
}

/// A local branch's menu items for worktrees: open the one it is checked
/// out in, or make one for it.
pub fn branch_items(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, snap: &RepoSnapshot, branch: &BranchInfo) {
    match elsewhere(snap, &branch.name) {
        Some(wt) => {
            if menu_item(ui, icons::ARROW_SQUARE_OUT, &format!("Open worktree {}", wt.title())) {
                shared.actions.push(AppAction::Open(wt.path.clone()));
            }
        }
        None => {
            if menu_item(ui, icons::FOLDERS, &format!("Add worktree for {}…", branch.name)) {
                open_add(tab, shared, Some(&branch.name));
            }
        }
    }
}

/// "Checkout <branch>", or the explanation when another worktree has it.
pub fn checkout(tab: &mut RepoTab, shared: &mut Shared, snap: &RepoSnapshot, branch: &BranchInfo) {
    match elsewhere(snap, &branch.name) {
        Some(wt) => explain(shared, &branch.name, wt.path.clone()),
        None => tab.run(crate::git::ops::checkout(&branch.name)),
    }
}

/// Says that `branch` is checked out in the worktree at `path`, offering
/// to open it.
pub fn explain(shared: &mut Shared, branch: &str, path: PathBuf) {
    shared.show(Dialog::Worktree(Box::new(Form::Elsewhere { branch: branch.to_owned(), path })));
}

/// For an operation that failed with `message`: when git refused to check
/// out a branch another worktree has, the explanation instead of the error
/// toast. True when it took the error.
pub fn refused(shared: &mut Shared, message: &str) -> bool {
    let Some((branch, path)) = worktree::in_use(message) else { return false };
    explain(shared, &branch, path);
    true
}

// ---- dialogs ------------------------------------------------------------------

/// What [`Dialog::Worktree`] holds.
pub enum Form {
    Add(AddForm),
    Remove(RemoveForm),
    /// A branch to check out is in another worktree.
    Elsewhere { branch: String, path: PathBuf },
}

pub struct AddForm {
    path: String,
    /// The path was typed or picked, not made from the branch.
    path_typed: bool,
    new_branch: bool,
    /// The new branch's name.
    name: String,
    /// The existing branch picked.
    existing: String,
    open_after: bool,
    /// The command once started, until it ends: (label, when).
    running: Option<(String, Instant)>,
}

pub struct RemoveForm {
    worktree: WorktreeInfo,
    /// Whether it has modified or untracked files; `None` while looking.
    changes: Option<bool>,
}

/// Whether the dialog stays open.
pub enum Next {
    Keep,
    Close,
    /// Working: Escape and clicks outside do not close it.
    Hold,
}

fn changes_key(path: &std::path::Path) -> String {
    format!("worktrees:changes:{}", path.display())
}

/// Opens "Add worktree", for an existing branch when `branch` is given.
pub fn open_add(tab: &RepoTab, shared: &mut Shared, branch: Option<&str>) {
    let mut form = AddForm {
        path: String::new(),
        path_typed: false,
        new_branch: branch.is_none(),
        name: String::new(),
        existing: branch.unwrap_or_default().to_owned(),
        open_after: true,
        running: None,
    };
    if let Some(snap) = &tab.snap {
        form.path = default_path(snap, &form);
    }
    shared.show(Dialog::Worktree(Box::new(Form::Add(form))));
}

fn open_remove(tab: &mut RepoTab, shared: &mut Shared, wt: &WorktreeInfo) {
    let path = wt.path.clone();
    tab.spawn_job(changes_key(&path), move |_| worktree::has_changes(&path));
    shared.show(Dialog::Worktree(Box::new(Form::Remove(RemoveForm { worktree: wt.clone(), changes: None }))));
}

fn main_path(snap: &RepoSnapshot) -> PathBuf {
    snap.worktrees.iter().find(|w| w.main).map_or_else(|| snap.workdir.clone(), |w| w.path.clone())
}

fn default_path(snap: &RepoSnapshot, form: &AddForm) -> String {
    let branch = if form.new_branch { &form.name } else { &form.existing };
    worktree::default_path(&main_path(snap), branch).to_string_lossy().into_owned()
}

pub fn dialog(ui: &mut Ui, form: &mut Form, tab: Option<&mut RepoTab>, shared: &mut Shared, fresh: bool) -> Next {
    let Some(tab) = tab else { return Next::Close };
    match form {
        Form::Add(form) => add_dialog(ui, form, tab, shared, fresh),
        Form::Remove(form) => remove_dialog(ui, form, tab),
        Form::Elsewhere { branch, path } => elsewhere_dialog(ui, branch, path, shared),
    }
}

fn caption(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(12.0).color(theme::text_dim()));
}

fn warning(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(format!("{}  {text}", icons::WARNING)).color(theme::orange()));
}

fn note(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(12.0).color(theme::text_faint()));
}

fn add_dialog(ui: &mut Ui, form: &mut AddForm, tab: &mut RepoTab, shared: &mut Shared, fresh: bool) -> Next {
    dialogs::title(ui, "Add worktree");
    let Some(snap) = tab.snap.clone() else { return Next::Close };

    // Waiting for git: open the new worktree once it is there, or let the
    // form be corrected (the error is a toast already).
    if let Some((label, since)) = &form.running {
        match tab.take_outcome(label, *since) {
            Some(true) => {
                if form.open_after {
                    shared.actions.push(AppAction::Open(PathBuf::from(form.path.trim())));
                }
                return Next::Close;
            }
            Some(false) => form.running = None,
            None => {
                ui.horizontal(|ui| {
                    ui.add(Spinner::new().size(13.0).color(theme::accent()));
                    ui.label(RichText::new("Adding the worktree…").color(theme::text_dim()));
                });
                dialogs::buttons(ui, "Add worktree", false, false);
                return Next::Hold;
            }
        }
    }

    let before = (form.new_branch, form.name.clone(), form.existing.clone());
    ui.horizontal(|ui| {
        ui.radio_value(&mut form.new_branch, true, "New branch");
        ui.radio_value(&mut form.new_branch, false, "Existing branch");
    });
    let free: Vec<&BranchInfo> =
        snap.branches.iter().filter(|b| !b.is_head && worktree::elsewhere(&snap.worktrees, &b.name).is_none()).collect();
    let mut problem: Option<String> = None;
    if form.new_branch {
        caption(ui, &format!("Name, starting at {}", snap.head_label()));
        let field = ui.add(TextEdit::singleline(&mut form.name).hint_text("feature/name").desired_width(f32::INFINITY));
        if fresh {
            field.request_focus();
        }
        dialogs::sanitize_ref(&mut form.name);
        if snap.branches.iter().any(|b| b.name == form.name.trim()) {
            problem = Some("A branch with this name exists: pick it under Existing branch.".to_owned());
        }
    } else {
        caption(ui, "Branch");
        if free.is_empty() {
            note(ui, "Every other branch is checked out in a worktree already.");
        } else {
            let shown = if form.existing.is_empty() { "Pick a branch".to_owned() } else { form.existing.clone() };
            ComboBox::from_id_salt("worktree-branch").selected_text(shown).width(ui.available_width()).show_ui(ui, |ui| {
                for branch in &free {
                    ui.selectable_value(&mut form.existing, branch.name.clone(), &branch.name);
                }
            });
        }
    }
    // The folder follows the branch until it is typed.
    if !form.path_typed && before != (form.new_branch, form.name.clone(), form.existing.clone()) {
        form.path = default_path(&snap, form);
    }

    caption(ui, "Folder");
    ui.horizontal(|ui| {
        if ui.button("Browse…").clicked() {
            let picked = crate::shot::Shot::picked_folder().or_else(|| {
                let start = main_path(&snap);
                rfd::FileDialog::new().set_directory(start.parent().unwrap_or(&start)).pick_folder()
            });
            if let Some(dir) = picked {
                form.path = dir.display().to_string();
                form.path_typed = true;
            }
        }
        let field = ui.add(TextEdit::singleline(&mut form.path).hint_text("Where its files go").desired_width(f32::INFINITY));
        if field.changed() {
            form.path_typed = true;
        }
    });
    let path_problem = worktree::check_path(&form.path);
    if let Err(text) = &path_problem
        && !text.is_empty()
    {
        warning(ui, text);
    }
    if let Some(text) = &problem {
        warning(ui, text);
    }
    ui.checkbox(&mut form.open_after, "Open it in a new tab");

    let branch = if form.new_branch { form.name.trim() } else { form.existing.as_str() };
    let ready = dialogs::valid_ref(branch) && problem.is_none() && path_problem.is_ok();
    match dialogs::buttons(ui, "Add worktree", ready, false) {
        Some(true) => {
            let path = PathBuf::from(form.path.trim());
            let cmd = if form.new_branch {
                worktree::add_new(&path, branch, "")
            } else {
                worktree::add_existing(&path, branch)
            };
            form.running = Some((cmd.label.clone(), Instant::now()));
            tab.run(cmd);
            Next::Hold
        }
        Some(false) => Next::Close,
        None => Next::Keep,
    }
}

fn remove_dialog(ui: &mut Ui, form: &mut RemoveForm, tab: &mut RepoTab) -> Next {
    let wt = &form.worktree;
    dialogs::title(ui, &format!("Remove worktree {}", wt.title()));
    if form.changes.is_none() {
        form.changes = tab.take_result::<bool>(&changes_key(&wt.path));
    }
    ui.label(RichText::new(wt.display()).font(theme::mono(12.0)).color(theme::text_dim()));
    let branch = match &wt.branch {
        Some(branch) => format!("The branch {branch} stays."),
        None => "Its commit stays in the repository; HEAD there was detached.".to_owned(),
    };
    if let Some(reason) = &wt.locked {
        let why = if reason.is_empty() { String::new() } else { format!(" ({reason})") };
        warning(ui, &format!("It is locked{why}. Unlock it first if it really is no longer in use."));
        return match dialogs::buttons(ui, "Remove", false, true) {
            Some(_) => Next::Close,
            None => Next::Keep,
        };
    }
    let (button, enabled) = match form.changes {
        None => {
            ui.horizontal(|ui| {
                ui.add(Spinner::new().size(13.0).color(theme::accent()));
                ui.label(RichText::new("Looking for uncommitted changes…").color(theme::text_dim()));
            });
            ("Remove", false)
        }
        Some(false) => {
            ui.label(format!("Deletes its folder, including files git ignores such as build output. {branch}"));
            ("Remove", true)
        }
        Some(true) => {
            warning(ui, "It has modified or untracked files, so git will not remove it.");
            ui.label(format!(
                "Force remove deletes them with the folder, including files git ignores. This cannot be undone. {branch}"
            ));
            ("Force remove", true)
        }
    };
    let force = form.changes == Some(true);
    let ready = enabled && !tab.is_running("Remove worktree");
    match dialogs::buttons(ui, button, ready, true) {
        Some(true) => {
            tab.run(worktree::remove(&wt.path, force));
            Next::Close
        }
        Some(false) => Next::Close,
        None => Next::Keep,
    }
}

fn elsewhere_dialog(ui: &mut Ui, branch: &str, path: &std::path::Path, shared: &mut Shared) -> Next {
    dialogs::title(ui, &format!("{branch} is in another worktree"));
    ui.label(format!(
        "{branch} is checked out in the worktree at {}. A branch can be checked out in one worktree at a time: \
         work on it there, or check out another branch there first.",
        path.to_string_lossy().replace('\\', "/")
    ));
    match dialogs::buttons(ui, "Open that worktree", true, false) {
        Some(true) => {
            shared.actions.push(AppAction::Open(path.to_path_buf()));
            Next::Close
        }
        Some(false) => Next::Close,
        None => Next::Keep,
    }
}

// ---- command palette ----------------------------------------------------------

pub fn palette_entries(list: &mut Vec<Entry>, tab: &RepoTab, snap: &RepoSnapshot) {
    list.push(
        Entry::on_tab("worktree-add", icons::PLUS, "Add worktree…", |tab, shared| open_add(tab, shared, None))
            .keywords("new worktree working tree folder branch parallel checkout"),
    );
    if snap.worktrees.iter().any(|w| w.prunable) && !tab.is_running("Prune worktrees") {
        list.push(
            Entry::on_tab("worktree-prune", icons::BROOM, "Prune missing worktrees", |tab, _| tab.run(worktree::prune()))
                .detail("git worktree prune")
                .keywords("worktrees clean stale forget"),
        );
    }
    for wt in snap.worktrees.iter().filter(|w| !w.current && !w.prunable) {
        let path = wt.path.clone();
        let detail = wt.branch.clone().unwrap_or_else(|| "detached".to_owned());
        list.push(
            Entry::on_tab(format!("worktree-open:{}", wt.display()), icons::ARROW_SQUARE_OUT, format!("Open worktree {}", wt.title()), move |_, shared| {
                shared.actions.push(AppAction::Open(path))
            })
            .detail(detail)
            .keywords("worktree tab switch"),
        );
    }
}
