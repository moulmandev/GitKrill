//! "Delete all branches in t3…" from a branch folder's context menu.
//!
//! For local branches the dialog first asks the reader thread which of them
//! are checked out in worktrees and whether those have uncommitted changes
//! (`git::branch_folder::plan`), then lists what goes and what stays.

use eframe::egui::{Align2, RichText, ScrollArea, Sense, Spinner, Ui, pos2, vec2};

use crate::git::branch_folder::{self, FolderPlan, count};
use crate::state::{Dialog, RepoTab, Shared};
use crate::theme::{self, icons};
use crate::ui::dialogs;
use crate::ui::widgets::elided;

/// What the delete dialog holds.
pub struct DeleteForm {
    folder: String,
    remove_worktrees: bool,
    /// Once the reader thread has looked.
    plan: Option<Result<FolderPlan, String>>,
}

fn job_key(folder: &str) -> String {
    format!("branch-folder:{folder}")
}

/// Asks to delete every local branch in `folder` (and folders inside it).
pub fn delete_local(tab: &mut RepoTab, shared: &mut Shared, folder: &str) {
    let Some(snap) = tab.snap.clone() else { return };
    let prefix = format!("{folder}/");
    let names: Vec<String> = snap.branches.iter().filter(|b| b.name.starts_with(&prefix)).map(|b| b.name.clone()).collect();
    let head = snap.head.branch.clone();
    tab.spawn_job(job_key(folder), move |loader| {
        let workdir = loader.repo().workdir().ok_or("The repository has no working tree")?.to_path_buf();
        branch_folder::plan(&workdir, &names, head.as_deref())
    });
    shared.show(Dialog::DeleteFolder(Box::new(DeleteForm {
        folder: folder.to_owned(),
        remove_worktrees: true,
        plan: None,
    })));
}

/// Asks to delete every branch in `folder` on `remote`.
pub fn delete_remote(tab: &RepoTab, shared: &mut Shared, remote: &str, folder: &str) {
    let Some(snap) = &tab.snap else { return };
    let prefix = format!("{folder}/");
    let names: Vec<String> = snap
        .remote_branches
        .iter()
        .filter(|b| b.remote == remote && b.name.starts_with(&prefix))
        .map(|b| b.name.clone())
        .collect();
    if names.is_empty() {
        return;
    }
    const LISTED: usize = 12;
    let mut list: Vec<String> = names.iter().take(LISTED).map(|name| format!("•  {name}")).collect();
    if names.len() > LISTED {
        list.push(format!("and {} more", names.len() - LISTED));
    }
    shared.show(Dialog::confirm(
        "Delete remote branches",
        format!(
            "Delete {} in {folder} on the remote \"{remote}\"? This affects everyone using that \
             remote, and cannot be undone from GitKrill.\n\n{}",
            count(names.len(), "branch", "branches"),
            list.join("\n")
        ),
        "Delete",
        branch_folder::delete_remote(remote, folder, &names),
    ));
}

/// Whether the dialog stays open.
pub enum Next {
    Keep,
    Close,
}

pub fn dialog(ui: &mut Ui, form: &mut DeleteForm, tab: Option<&mut RepoTab>) -> Next {
    dialogs::title(ui, &format!("Delete all branches in {}", form.folder));
    let Some(tab) = tab else { return close_only(ui) };
    if form.plan.is_none() {
        form.plan = tab.take_result(&job_key(&form.folder));
    }
    let plan = match &form.plan {
        None => {
            ui.horizontal(|ui| {
                ui.add(Spinner::new().size(13.0).color(theme::accent()));
                ui.label(RichText::new("Looking at the branches and their worktrees…").color(theme::text_dim()));
            });
            return match dialogs::buttons(ui, "Delete", false, true) {
                Some(false) => Next::Close,
                _ => Next::Keep,
            };
        }
        Some(Err(e)) => {
            ui.colored_label(theme::orange(), e);
            return close_only(ui);
        }
        Some(Ok(plan)) => plan,
    };

    let deleted = plan.deleted(form.remove_worktrees).count();
    let worktrees = plan.worktrees();
    let summary = if deleted == 0 {
        "None of these branches can be deleted now.".to_owned()
    } else {
        format!(
            "Deletes {} in {}. Commits only reachable from them will no longer appear in the graph.",
            count(deleted, "local branch", "local branches"),
            form.folder
        )
    };
    ui.label(RichText::new(summary).color(theme::text()));

    ScrollArea::vertical().max_height(230.0).auto_shrink([false, true]).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        for (name, worktree) in &plan.branches {
            let note = match worktree {
                Some(_) if form.remove_worktrees => "and its worktree",
                Some(_) => "kept: in a worktree",
                None => "",
            };
            let kept = worktree.is_some() && !form.remove_worktrees;
            let response = line(ui, name, note, kept);
            if let Some(path) = worktree {
                response.on_hover_text(path.display().to_string());
            }
        }
        for (name, why) in &plan.kept {
            line(ui, name, &format!("kept: {why}"), true);
        }
    });

    if worktrees > 0 {
        ui.add_space(2.0);
        let label = match plan.branches.iter().find(|(_, worktree)| worktree.is_some()) {
            Some((name, _)) if worktrees == 1 => format!("Also remove the worktree of {name}"),
            _ => format!("Also remove their {worktrees} worktrees"),
        };
        ui.checkbox(&mut form.remove_worktrees, label);
        let text = "A worktree's folder is deleted with everything in it, including files git ignores \
                    such as build output. Worktrees with uncommitted changes are always kept.";
        ui.label(RichText::new(text).size(12.0).color(theme::text_faint()));
    }
    let undo = match worktrees {
        _ if !form.remove_worktrees => "Undo brings the branches back.",
        0 => "Undo brings the branches back.",
        1 => "Undo brings the branches back, but not the worktree.",
        _ => "Undo brings the branches back, but not the worktrees.",
    };
    ui.label(RichText::new(undo).size(12.0).color(theme::text_faint()));

    let button = format!("Delete {}", count(deleted, "branch", "branches"));
    match dialogs::buttons(ui, &button, deleted > 0, true) {
        Some(true) => {
            tab.run(branch_folder::delete_local(&form.folder, plan, form.remove_worktrees));
            Next::Close
        }
        Some(false) => Next::Close,
        None => Next::Keep,
    }
}

/// One branch in the dialog's list, with a note on the right.
fn line(ui: &mut Ui, name: &str, note: &str, kept: bool) -> eframe::egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::hover());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let y = rect.center().y;
        let (icon, color) = if kept { (icons::GIT_BRANCH, theme::text_dim()) } else { (icons::TRASH, theme::red()) };
        painter.text(pos2(rect.left() + 2.0, y), Align2::LEFT_CENTER, icon, theme::font(13.0), color);
        let note_color = if kept { theme::orange() } else { theme::text_dim() };
        let used = painter.text(pos2(rect.right() - 4.0, y), Align2::RIGHT_CENTER, note, theme::font(12.0), note_color);
        let text_color = if kept { theme::text_dim() } else { theme::text() };
        let left = rect.left() + 22.0;
        let galley = elided(painter, name, theme::font(13.0), text_color, used.left() - 10.0 - left);
        painter.galley(pos2(left, y - galley.size().y / 2.0), galley, text_color);
    }
    response
}

fn close_only(ui: &mut Ui) -> Next {
    match dialogs::buttons(ui, "Close", true, false) {
        Some(_) => Next::Close,
        None => Next::Keep,
    }
}
