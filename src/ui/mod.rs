//! User interface, one module per region of the window.

pub mod conflict;
pub mod dialogs;
pub mod diff;
pub mod files;
pub mod graph;
pub mod highlight;
pub mod history;
pub mod image_diff;
pub mod hosting;
pub mod palette;
pub mod new_pull;
pub mod panel;
pub mod rebase;
pub mod sidebar;
pub mod textsel;
pub mod toolbar;
pub mod update;
pub mod widgets;

use eframe::egui::Ui;

use crate::state::{Overlay, RepoTab, Shared};

/// Shows the tab's overlay in place of the graph.
///
/// The overlay is taken out of the tab while it draws, so its `show` gets
/// both its own state and the whole tab mutably. Returning `false` closes it;
/// an overlay may also replace itself by assigning `tab.overlay`.
pub fn overlay(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared) {
    let Some(mut overlay) = tab.overlay.take() else { return };
    let keep = match &mut overlay {
        Overlay::Conflict(view) => conflict::show(ui, tab, shared, view),
        Overlay::History(view) => history::show(ui, tab, shared, view),
        Overlay::Rebase(view) => rebase::show(ui, tab, shared, view),
    };
    if keep && tab.overlay.is_none() {
        tab.overlay = Some(overlay);
    }
}
