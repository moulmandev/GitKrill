//! One branch shown alone in the graph: "Show only this branch" on a local
//! or remote branch's right-click menu (in the sidebar or on its label in
//! the graph). Everything else is hidden with the hidden-refs rules
//! (`git::hidden::Hidden::only`), the checked out branch too; showing more
//! branches from the sidebar meanwhile adds them. A bar above the graph says
//! what is shown and goes back with one click to what was hidden before,
//! which the settings kept all along.

use std::sync::{Arc, Weak};

use eframe::egui::{self, Frame, Margin, RichText, Ui};

use crate::git::RepoSnapshot;
use crate::git::hidden::{self, Hidden};
use crate::state::{RepoTab, Settings, Shared, ToastKind};
use crate::theme::{self, icons};
use crate::ui::palette::Entry;
use crate::ui::widgets::{menu_item, tint_button};

/// Kept in `RepoTab::solo` while one branch is shown alone.
pub struct Solo {
    /// The full ref name (`refs/heads/feature/x`).
    pub path: String,
    /// What the UI calls it (`feature/x`, `origin/main`).
    pub name: String,
    /// The snapshot it was last looked for in.
    checked: Weak<RepoSnapshot>,
}

/// Shows only the ref `path` (named `name` in the UI) in the graph.
pub fn enter(tab: &mut RepoTab, path: &str, name: &str) {
    tab.solo = Some(Solo { path: path.to_owned(), name: name.to_owned(), checked: Weak::new() });
    tab.hidden = Hidden::only(path);
    tab.apply_hidden();
}

/// Shows again what the graph showed before.
pub fn leave(tab: &mut RepoTab, settings: &Settings) {
    if tab.solo.take().is_none() {
        return;
    }
    tab.hidden = settings.hidden_refs.get(&tab.path).cloned().unwrap_or_default();
    tab.apply_hidden();
}

/// "Show only this branch" for the ref `path`, or the way back while it is
/// the one shown alone.
pub fn menu_item_for(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, path: &str, name: &str) {
    if tab.solo.as_ref().is_some_and(|solo| solo.path == path) {
        if menu_item(ui, icons::EYE, "Show all branches again") {
            leave(tab, &shared.settings);
        }
    } else if menu_item(ui, icons::CROSSHAIR_SIMPLE, "Show only this branch") {
        enter(tab, path, name);
    }
}

/// The bar above the graph while one branch is shown alone.
pub fn bar(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, snap: &Arc<RepoSnapshot>) {
    let Some(solo) = &mut tab.solo else { return };
    let (path, name) = (solo.path.clone(), solo.name.clone());
    if solo.checked.as_ptr() != Arc::as_ptr(snap) {
        solo.checked = Arc::downgrade(snap);
        let tip = snap
            .branches
            .iter()
            .find(|b| hidden::local(&b.name) == path)
            .map(|b| b.oid)
            .or_else(|| snap.remote_branches.iter().find(|b| hidden::remote(&b.full) == path).map(|b| Some(b.oid)));
        let Some(tip) = tip else {
            // Gone (deleted, renamed, pruned): everything comes back.
            leave(tab, &shared.settings);
            shared.toast(ToastKind::Success, "Showing all branches again", format!("{name} no longer exists."));
            return;
        };
        // The selection left with the rest (HEAD, often): the branch's tip
        // takes its place.
        if tab.selected_row().is_none()
            && let Some(tip) = tip.filter(|tip| snap.row_of(*tip).is_some())
        {
            tab.select(crate::state::Selection::Commit(tip));
        }
    }
    let others = tab.hidden.shown().filter(|shown| *shown != path).count();
    let mut back = false;
    Frame::new()
        .fill(theme::mix(theme::bg_app(), theme::accent(), 0.14))
        .inner_margin(Margin::symmetric(10, 5))
        .show(ui, |ui| {
            egui::Sides::new().shrink_left().truncate().show(
                ui,
                |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.label(RichText::new(icons::CROSSHAIR_SIMPLE).color(theme::accent()).size(15.0));
                    ui.label(RichText::new("Showing only").color(theme::text()));
                    ui.add(egui::Label::new(RichText::new(&name).color(theme::text_bright()).strong()).truncate());
                    if others > 0 {
                        let more = if others == 1 { "and 1 more".to_owned() } else { format!("and {others} more") };
                        ui.label(RichText::new(more).color(theme::text()));
                    }
                },
                |ui| {
                    let tip = "Show again what the graph showed before";
                    if tint_button(ui, "Show all branches", theme::accent()).on_hover_text(tip).clicked() {
                        back = true;
                    }
                },
            );
        });
    if back {
        leave(tab, &shared.settings);
    }
}

/// "Show all branches again" while one is shown alone; otherwise "Show only
/// the checked out branch".
pub fn palette_entries(list: &mut Vec<Entry>, tab: &RepoTab, snap: &RepoSnapshot) {
    if let Some(solo) = &tab.solo {
        list.push(
            Entry::on_tab("solo-leave", icons::EYE, "Show all branches again", |tab, shared| leave(tab, &shared.settings))
                .detail(format!("only {} is shown", solo.name))
                .keywords("solo alone unhide everything graph"),
        );
    } else if let Some(branch) = snap.current_branch() {
        let name = branch.name.clone();
        list.push(
            Entry::on_tab("solo-head", icons::CROSSHAIR_SIMPLE, "Show only the checked out branch", move |tab, _| {
                enter(tab, &hidden::local(&name), &name)
            })
            .detail(branch.name.clone())
            .keywords("solo alone focus hide other branches graph"),
        );
    }
}
