//! Preferences > Encoding (repository-specific).

use eframe::egui::Ui;

use super::{Page, form};
use crate::git::text::Encoding;

/// What this section remembers while preferences are open.
#[derive(Default)]
pub struct State {}

pub fn show(ui: &mut Ui, page: &mut Page<'_>, _state: &mut State) {
    let Some(tab) = page.tab.as_deref() else { return };
    let workdir = tab.path.clone();
    let mut encoding = crate::prefs::repo(&page.shared.settings, &workdir).encoding.encoding;

    form::row(ui, "Default encoding", |ui| {
        let options: Vec<(Encoding, &str)> = Encoding::ALL.iter().map(|e| (*e, e.label())).collect();
        if form::choice(ui, ("prefs-encoding", &workdir), &mut encoding, &options) {
            // Only now does the repository get an entry of its own.
            crate::prefs::repo_mut(&mut page.shared.settings, &workdir).encoding.encoding = encoding;
        }
        form::help(ui, "Default encoding when reading or writing files in this repo.");
        if encoding != Encoding::Utf8 {
            form::help(ui, "A file that starts with a UTF-16 byte order mark is read as UTF-16 all the same.");
        }
    });
}
