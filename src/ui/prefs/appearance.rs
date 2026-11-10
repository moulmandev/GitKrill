//! Preferences > UI Customization.

use eframe::egui::Ui;

use super::{Page, form};

/// What this section remembers while preferences are open.
#[derive(Default)]
pub struct State {}

pub fn show(ui: &mut Ui, page: &mut Page<'_>, state: &mut State) {
    let _ = (page, state);
    form::todo(ui);
}
