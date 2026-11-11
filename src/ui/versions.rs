//! What can be done with a file as it was in a commit: open it in another
//! program (the default one, or the editor set in Preferences > External
//! Tools), save it somewhere, or put it back in the working tree. Offered on
//! the right-click menu of a file in a commit's lists (`ui::files`,
//! `ui::browse`), in the title bar of a file of a commit (`ui::diff`), and in
//! the command palette. The reading and writing is `git::versions`.

use std::path::PathBuf;

use eframe::egui::Ui;

use crate::git::{ChangeKind, DiffSource, FileChange, Oid, RepoSnapshot, short, versions};
use crate::state::{Dialog, RepoTab, Shared, ToastKind};
use crate::theme::{self, icons};
use crate::ui::palette::Entry;
use crate::ui::widgets::{icon_button, menu_item, tint_button};

const OPEN_JOB: &str = "versions:open";
const SAVE_JOB: &str = "versions:save";

/// A file as it is in one commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    pub commit: Oid,
    /// Its path in that commit.
    pub path: String,
}

impl Version {
    pub fn new(commit: Oid, path: impl Into<String>) -> Self {
        Version { commit, path: path.into() }
    }

    fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }
}

/// Something asked for from a menu, done once the menu is drawn.
pub enum Act {
    Open { version: Version, editor: bool },
    Save(Version),
    Restore(Version),
}

/// The version of `file` that a list of `source` stands for, and how a
/// menu names it: the commit's own for a file of a commit, the newer side's
/// in a comparison, and the version from before the change for a deleted
/// file. `None` for the working tree's lists, and where the file is not in
/// the commit compared (added since).
pub fn of_file(tab: &RepoTab, source: DiffSource, file: &FileChange) -> Option<(Version, String)> {
    let deleted = file.kind == ChangeKind::Deleted;
    let named = |commit: Oid| format!("the version in {}", short(commit));
    match source {
        DiffSource::Commit(oid) if !deleted => Some((Version::new(oid, &file.path), "this version".to_owned())),
        DiffSource::Commit(oid) => {
            let details = tab.details.as_ref()?.as_ref().ok().filter(|d| d.oid == oid)?;
            let parent = *details.parents.first()?;
            Some((Version::new(parent, &file.path), "the deleted version".to_owned()))
        }
        DiffSource::Compare { to, .. } if !deleted => Some((Version::new(to, &file.path), named(to))),
        DiffSource::Compare { from, .. } => Some((Version::new(from, &file.path), named(from))),
        DiffSource::WorkingTree { from } => match file.kind {
            ChangeKind::Added | ChangeKind::Untracked => None,
            _ => {
                let path = file.old_path.as_deref().unwrap_or(&file.path);
                Some((Version::new(from, path), named(from)))
            }
        },
        DiffSource::At(oid) => Some((Version::new(oid, &file.path), "this version".to_owned())),
        DiffSource::Unstaged | DiffSource::Staged => None,
    }
}

/// Where the file history of a file in a list of `source` starts: the
/// commit shown (the newer side of a comparison). `None`, HEAD, for the
/// working tree's lists and a comparison with the working tree.
pub fn history_start(source: DiffSource) -> Option<Oid> {
    match source {
        DiffSource::Commit(oid) | DiffSource::At(oid) | DiffSource::Compare { to: oid, .. } => Some(oid),
        DiffSource::Unstaged | DiffSource::Staged | DiffSource::WorkingTree { .. } => None,
    }
}

/// The menu entries for `version`, named `which` ("this version"). Returns
/// what was clicked.
pub fn menu_items(ui: &mut Ui, shared: &Shared, version: &Version, which: &str) -> Option<Act> {
    let mut act = None;
    if menu_item(ui, icons::ARROW_SQUARE_OUT, &format!("Open {which}")) {
        act = Some(Act::Open { version: version.clone(), editor: false });
    }
    if shared.settings.tools.has_editor() && menu_item(ui, icons::CODE, &format!("Open {which} in editor")) {
        act = Some(Act::Open { version: version.clone(), editor: true });
    }
    if menu_item(ui, icons::FLOPPY_DISK, &format!("Save {which} as…")) {
        act = Some(Act::Save(version.clone()));
    }
    if menu_item(ui, icons::ARROW_COUNTER_CLOCKWISE, &format!("Restore {which}")) {
        act = Some(Act::Restore(version.clone()));
    }
    act
}

/// Buttons for the title bar of a file of a commit (right to left).
pub fn bar_buttons(ui: &mut Ui, tab: &mut RepoTab, shared: &mut Shared, commit: Oid, path: &str) {
    let version = Version::new(commit, path);
    let mut act = None;
    let restore = tint_button(ui, &format!("{}  Restore", icons::ARROW_COUNTER_CLOCKWISE), theme::orange())
        .on_hover_text(format!("Put this version in the working tree, in place of the file there (git restore --source={})", short(commit)));
    if restore.clicked() {
        act = Some(Act::Restore(version.clone()));
    }
    if icon_button(ui, icons::FLOPPY_DISK, 15.0, "Save this version as…").clicked() {
        act = Some(Act::Save(version.clone()));
    }
    if shared.settings.tools.has_editor() && icon_button(ui, icons::CODE, 15.0, "Open this version in the editor").clicked() {
        act = Some(Act::Open { version: version.clone(), editor: true });
    }
    if icon_button(ui, icons::ARROW_SQUARE_OUT, 15.0, "Open this version with its default program").clicked() {
        act = Some(Act::Open { version: version.clone(), editor: false });
    }
    if icon_button(ui, icons::CLOCK_COUNTER_CLOCKWISE, 15.0, "File history from this commit").clicked() {
        let view = crate::ui::history::HistoryView::history(path).starting_at(Some(commit));
        tab.overlay = Some(crate::state::Overlay::History(view));
    }
    if let Some(act) = act {
        apply(tab, shared, act);
    }
}

/// Carries out what was asked for: reading happens on the reader thread,
/// [`poll`] finishes it.
pub fn apply(tab: &mut RepoTab, shared: &mut Shared, act: Act) {
    let workdir = tab.path.clone();
    match act {
        Act::Open { version, editor } => {
            let base = versions::temp_base();
            tab.spawn_job(OPEN_JOB, move |_| {
                (editor, versions::write_temp(&workdir, &base, version.commit, &version.path))
            });
        }
        Act::Save(version) => {
            let name = version.name().to_owned();
            // A scripted run cannot click through a file dialog.
            let dest = match crate::shot::Shot::picked_folder() {
                Some(dir) => Some(dir.join(&name)),
                None => rfd::FileDialog::new().set_title("Save this version as").set_file_name(&name).save_file(),
            };
            if let Some(dest) = dest {
                tab.spawn_job(SAVE_JOB, move |_| {
                    let result = versions::save(&workdir, version.commit, &version.path, &dest);
                    (dest, result)
                });
            }
        }
        Act::Restore(version) => {
            let cmd = versions::restore(&workdir, version.commit, &version.path);
            match tab.snap.as_deref().filter(|snap| has_local_changes(snap, &version.path)) {
                Some(_) => shared.show(Dialog::confirm(
                    "Restore this version",
                    format!(
                        "Put \"{}\" back as it was in commit {}? Its uncommitted changes in the working tree are \
                         overwritten; Undo brings them back.",
                        version.path,
                        short(version.commit)
                    ),
                    "Restore",
                    cmd,
                )),
                None => tab.run(cmd),
            }
        }
    }
}

/// Whether the working tree file at `path` holds something the index does
/// not (an unstaged change, or an untracked file): what restoring would
/// overwrite. Staged changes stay in the index.
fn has_local_changes(snap: &RepoSnapshot, path: &str) -> bool {
    snap.status.unstaged.iter().any(|file| file.path == path)
}

/// Finishes what [`apply`] started; called every frame.
pub fn poll(tab: &mut RepoTab, shared: &mut Shared) {
    if let Some((editor, result)) = tab.take_result::<(bool, Result<PathBuf, String>)>(OPEN_JOB) {
        match result {
            Ok(path) if editor => crate::external::edit(&shared.settings.tools, &path),
            // A scripted run never starts other programs.
            Ok(path) if std::env::var_os("GITKRILL_SHOT").is_some() => eprintln!("would open: {}", path.display()),
            Ok(path) => crate::platform::open(&path),
            Err(e) => shared.toast(ToastKind::Error, "Could not open this version", e),
        }
    }
    if let Some((dest, result)) = tab.take_result::<(PathBuf, Result<(), String>)>(SAVE_JOB) {
        match result {
            Ok(()) => {
                let name = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                shared.toast(ToastKind::Success, format!("Saved {name}"), dest.display().to_string());
            }
            Err(e) => shared.toast(ToastKind::Error, "Could not save this version", e),
        }
    }
}

/// Palette entries for the file of a commit that is open in the diff view.
pub fn palette_entries(list: &mut Vec<Entry>, tab: &RepoTab) {
    let Some(view) = &tab.diff else { return };
    let kind = view.data.as_ref().and_then(|data| data.as_ref().ok()).and_then(|diff| diff.kind);
    let file = FileChange {
        path: view.target.path.clone(),
        old_path: view.target.old_path.clone(),
        kind: kind.unwrap_or(ChangeKind::Modified),
    };
    let Some((version, which)) = of_file(tab, view.target.source, &file) else { return };
    let detail = format!("{} as of {}", version.path, short(version.commit));
    let entries = [
        ("version-open", icons::ARROW_SQUARE_OUT, format!("Open {which}"), Act::Open { version: version.clone(), editor: false }),
        ("version-save", icons::FLOPPY_DISK, format!("Save {which} as…"), Act::Save(version.clone())),
        ("version-restore", icons::ARROW_COUNTER_CLOCKWISE, format!("Restore {which}"), Act::Restore(version)),
    ];
    for (key, icon, label, act) in entries {
        list.push(
            Entry::on_tab(key, icon, label, move |tab, shared| apply(tab, shared, act))
                .detail(detail.clone())
                .keywords("old file version revision commit checkout restore save open"),
        );
    }
}
