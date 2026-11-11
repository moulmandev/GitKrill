//! "Export as archive…" on a commit, branch or tag (`git::archive`): the
//! system's save dialog offers zip, tar.gz and tar, and the file name's
//! extension decides.

use eframe::egui::Ui;

use crate::git::RepoSnapshot;
use crate::git::archive::{self, Format};
use crate::state::RepoTab;
use crate::theme::icons;
use crate::ui::palette::Entry;
use crate::ui::patch::ask_save;
use crate::ui::widgets::menu_item;

/// The menu entry for `treeish` (a SHA, branch or tag), which `shown`
/// names to the user ("a1b2c3d", "main", "v1.0").
pub fn item(ui: &mut Ui, tab: &mut RepoTab, treeish: &str, shown: &str) {
    if menu_item(ui, icons::FILE_ZIP, "Export as archive…") {
        export(tab, treeish, shown);
    }
}

/// Asks where, then writes the archive of `treeish`: its files in a folder
/// named after the repository and `shown` ("app-v1.0/").
pub fn export(tab: &mut RepoTab, treeish: &str, shown: &str) {
    let prefix = archive::prefix(&tab.name, shown);
    let name = format!("{}.zip", prefix.trim_end_matches('/'));
    let filters: Vec<(&str, &[&str])> = Format::ALL.iter().map(|f| (f.name(), f.extensions())).collect();
    if let Some(path) = ask_save("Export as archive", &name, &filters) {
        let (file, format) = archive::target(&path, Format::Zip);
        tab.run(archive::export(treeish, shown, &file, format, &prefix));
    }
}

/// "Export <branch> as an archive", for what is checked out.
pub fn palette_entries(list: &mut Vec<Entry>, tab: &RepoTab, snap: &RepoSnapshot) {
    if snap.head.oid.is_none() || tab.is_running("Export ") {
        return;
    }
    let shown = snap.head_label();
    let label = format!("Export {shown} as an archive");
    list.push(
        Entry::on_tab("export-archive", icons::FILE_ZIP, label, move |tab, _| export(tab, "HEAD", &shown))
            .detail("zip or tar.gz, without the history")
            .keywords("archive zip tar tgz download snapshot export"),
    );
}
