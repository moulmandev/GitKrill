//! The stash dialog: a message, whether untracked files go in, keeping the
//! staged changes or stashing only those, and, when opened from a file or
//! folder of the changed-file lists, the paths it is limited to.

use eframe::egui::{Checkbox, RichText, ScrollArea, TextEdit, Ui};

use crate::git::stash::{self, StashOptions};
use crate::git::{ChangeKind, DiffSource, FileChange, RepoSnapshot};
use crate::state::{Dialog, RepoTab, Shared};
use crate::theme::{self, icons};
use crate::ui::dialogs;
use crate::ui::palette::Entry;
use crate::ui::widgets::menu_item;

/// What the stash dialog holds.
pub struct StashForm {
    pub options: StashOptions,
    /// Every preset path is an untracked file: git would find nothing to
    /// stash without untracked files, so that box stays ticked.
    needs_untracked: bool,
    /// Said under the paths (what of a rename stays behind).
    note: Option<String>,
    /// Whether git could take the staged changes out of the working tree
    /// (`git::stash::staged_check`), once the reader thread has looked.
    staged_check: Option<Result<(), String>>,
    checking: bool,
}

/// The reader-thread job behind [`StashForm::staged_check`].
const CHECK: &str = "stash:staged-check";

impl StashForm {
    /// Everything, untracked files included (what the toolbar's Stash does).
    pub fn new() -> Self {
        Self::with(StashOptions::default())
    }

    /// Only `paths` (files or folders relative to the root).
    pub fn paths(paths: Vec<String>, needs_untracked: bool) -> Self {
        StashForm { needs_untracked, ..Self::with(StashOptions { paths, ..StashOptions::default() }) }
    }

    fn with(options: StashOptions) -> Self {
        StashForm { options, needs_untracked: false, note: None, staged_check: None, checking: false }
    }
}

impl Default for StashForm {
    fn default() -> Self {
        Self::new()
    }
}

/// Opens the stash dialog for all changes.
pub fn open(shared: &mut Shared) {
    shared.show(Dialog::Stash(Box::default()));
}

/// Opens the stash dialog with "Only staged changes" ticked (the command
/// palette's "Stash only staged changes"): the dialog says when git could
/// not do it.
pub fn open_staged(shared: &mut Shared) {
    let options = StashOptions { staged: true, ..StashOptions::default() };
    shared.show(Dialog::Stash(Box::new(StashForm::with(options))));
}

/// Whether "Only staged changes" can be offered for `snap`: why not, if not.
pub fn staged_blocker(snap: &RepoSnapshot, paths: &[String]) -> Option<String> {
    if let Some(why) = stash::staged_unsupported() {
        return Some(why);
    }
    if !paths.is_empty() {
        // `git stash push --staged -- <paths>` puts the working files of
        // every other staged file back to HEAD as well (git 2.51).
        return Some("Not for some files only: git would also undo the working-tree changes of other staged files.".to_owned());
    }
    snap.status.staged.is_empty().then(|| "Nothing is staged.".to_owned())
}

/// "Stash this file…" in a changed file's context menu (working-tree
/// lists). Not for a file deleted in the index: git cannot stash a path only
/// HEAD has (it refuses, or with untracked files saves the stash and then
/// fails), which is also why a rename goes by its new name only.
pub fn file_item(ui: &mut Ui, shared: &mut Shared, file: &FileChange, source: DiffSource) {
    let deleted_in_index = source == DiffSource::Staged && file.kind == ChangeKind::Deleted;
    if file.kind == ChangeKind::Conflicted
        || deleted_in_index
        || !menu_item(ui, icons::TRAY_ARROW_DOWN, "Stash this file…")
    {
        return;
    }
    let mut form = StashForm::paths(vec![file.path.clone()], file.kind == ChangeKind::Untracked);
    if let Some(old) = &file.old_path {
        form.note = Some(format!("Renamed from {old}: deleting {old} stays staged."));
    }
    shared.show(Dialog::Stash(Box::new(form)));
}

/// "Stash folder…" for a folder row; `snap` says whether anything in it is
/// tracked, and whether anything is left once deletions staged in it are
/// set aside (git cannot stash those on their own, see [`file_item`]).
pub fn folder_item(ui: &mut Ui, shared: &mut Shared, snap: Option<&RepoSnapshot>, dir: &str) {
    let prefix = format!("{dir}/");
    let stashable = snap.is_none_or(|snap| {
        let status = &snap.status;
        status.unstaged.iter().any(|f| f.path.starts_with(&prefix))
            || status.staged.iter().any(|f| f.path.starts_with(&prefix) && f.kind != ChangeKind::Deleted)
    });
    if !stashable || !menu_item(ui, icons::TRAY_ARROW_DOWN, "Stash folder…") {
        return;
    }
    let only_untracked = snap.is_some_and(|snap| {
        let status = &snap.status;
        status.staged.iter().all(|f| !f.path.starts_with(&prefix))
            && status.unstaged.iter().filter(|f| f.path.starts_with(&prefix)).all(|f| f.kind == ChangeKind::Untracked)
    });
    shared.show(Dialog::Stash(Box::new(StashForm::paths(vec![dir.to_owned()], only_untracked))));
}

/// "Stash…" (the dialog with its options) and "Stash only staged changes",
/// under the conditions the toolbar's Stash has.
pub fn palette_entries(list: &mut Vec<Entry>, tab: &RepoTab, snap: &RepoSnapshot) {
    if snap.status.is_clean() || tab.is_running("Stash changes") {
        return;
    }
    list.push(
        Entry::on_tab("stash-options", icons::TRAY_ARROW_DOWN, "Stash…", |_, shared| open(shared))
            .detail("message, untracked files, staged changes")
            .keywords("stash options keep index untracked message save"),
    );
    if staged_blocker(snap, &[]).is_none() {
        let files = snap.status.staged.len();
        let detail = if files == 1 { "1 staged file".to_owned() } else { format!("{files} staged files") };
        list.push(
            Entry::on_tab("stash-staged", icons::TRAY_ARROW_DOWN, "Stash only staged changes", |_, shared| open_staged(shared))
                .detail(detail)
                .keywords("stash staged index save"),
        );
    }
}

/// Whether the dialog stays open.
pub enum Next {
    Keep,
    Close,
}

pub fn dialog(ui: &mut Ui, form: &mut StashForm, mut tab: Option<&mut RepoTab>, fresh: bool) -> Next {
    let options = &mut form.options;
    let title = match options.paths.first() {
        None => "Stash changes".to_owned(),
        Some(path) if path_is_folder(tab.as_deref(), path) => format!("Stash folder {path}/"),
        Some(path) => format!("Stash {}", crate::git::ignore::file_name(path)),
    };
    dialogs::title(ui, &title);
    let intro = if options.paths.is_empty() {
        "Changes are saved in a new stash and removed from the working tree."
    } else {
        "Only the changes below are saved in a new stash and removed from the working tree:"
    };
    ui.label(RichText::new(intro).color(theme::text_dim()));
    if !options.paths.is_empty() {
        ScrollArea::vertical().max_height(110.0).auto_shrink([false, true]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            for path in &options.paths {
                let shown = match path_is_folder(tab.as_deref(), path) {
                    true => format!("{}  {path}/", icons::FOLDER),
                    false => format!("{}  {path}", icons::FILE),
                };
                ui.label(RichText::new(shown).color(theme::text()));
            }
        });
        if let Some(note) = &form.note {
            ui.label(RichText::new(note).size(12.0).color(theme::text_faint()));
        }
    }

    let edit = ui.add(TextEdit::singleline(&mut options.message).hint_text("Message (optional)").desired_width(f32::INFINITY));
    if fresh {
        edit.request_focus();
    }

    let snap = tab.as_deref().and_then(|tab| tab.snap.clone());
    let mut no_staged = snap.as_deref().and_then(|snap| staged_blocker(snap, &options.paths));
    // A dry run of what git does after saving the stash, so that it is not
    // offered when it would leave the stash saved and the changes in place.
    if no_staged.is_none()
        && let Some(tab) = tab.as_deref_mut()
    {
        if let Some(result) = tab.take_result::<Result<(), String>>(CHECK) {
            form.staged_check = Some(result);
            form.checking = false;
        }
        if form.staged_check.is_none() && !form.checking {
            tab.spawn_job(CHECK, |loader| loader.repo().workdir().map_or(Ok(()), stash::staged_check));
            form.checking = true;
        }
        no_staged = match &form.staged_check {
            Some(Err(why)) => Some(why.clone()),
            Some(Ok(())) => None,
            None => Some("Looking at the staged changes…".to_owned()),
        };
    }
    // Unticked only once known: a preset tick survives the check.
    if no_staged.is_some() && !form.checking {
        options.staged = false;
    }

    ui.add_space(2.0);
    // Only staged changes leaves the other two out: shown unticked then.
    let staged = options.staged;
    let mut untracked = (options.untracked || form.needs_untracked) && !staged;
    let response = ui.add_enabled(!staged && !form.needs_untracked, Checkbox::new(&mut untracked, "Include untracked files"));
    let response = if form.needs_untracked {
        response.on_disabled_hover_text("Untracked files are all there is to stash here.")
    } else {
        response.on_disabled_hover_text("Only staged changes leaves untracked files where they are.")
    };
    if response.changed() {
        options.untracked = untracked;
    }

    let mut keep = options.keep_index && !staged;
    let response = ui
        .add_enabled(!staged, Checkbox::new(&mut keep, "Keep staged changes"))
        .on_hover_text("Staged changes are stashed too, but also stay staged and in the working tree.")
        .on_disabled_hover_text("Only staged changes takes them out of the index.");
    if response.changed() {
        options.keep_index = keep;
    }

    let response = ui
        .add_enabled(no_staged.is_none(), Checkbox::new(&mut options.staged, "Only staged changes"))
        .on_hover_text("Unstaged changes and untracked files stay where they are.");
    if let Some(why) = &no_staged {
        response.on_disabled_hover_text(why);
        // A tick the dialog was opened with is taken back: say why.
        if form.staged_check.as_ref().is_some_and(Result::is_err) && options.paths.is_empty() {
            ui.label(RichText::new(why).size(12.0).color(theme::text_faint()));
        }
    }
    if form.needs_untracked {
        options.untracked = true;
    }

    // Not while the check of a preset "Only staged changes" runs.
    let ready = !(options.staged && no_staged.is_some());
    match dialogs::buttons(ui, "Stash", ready, false) {
        Some(true) => {
            if let Some(tab) = tab {
                tab.run(stash::push(options));
            }
            Next::Close
        }
        Some(false) => Next::Close,
        None => Next::Keep,
    }
}

/// Whether `path` is a folder of changed files rather than one of them.
fn path_is_folder(tab: Option<&RepoTab>, path: &str) -> bool {
    let Some(snap) = tab.and_then(|tab| tab.snap.as_deref()) else { return false };
    let prefix = format!("{path}/");
    snap.status.unstaged.iter().chain(&snap.status.staged).any(|f| f.path.starts_with(&prefix))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Settings;
    use crate::tests::{Fixture, git};

    fn labels(fx: &Fixture) -> Vec<String> {
        let mut tab = RepoTab::new(fx.dir.clone());
        tab.snap = Some(fx.snapshot());
        let entries = crate::ui::palette::entries(&[tab], 0, &Settings::default());
        entries.into_iter().map(|entry| entry.label).filter(|label| label.starts_with("Stash")).collect()
    }

    #[test]
    fn palette_offers_the_stash_dialog_and_staged_only_when_they_can_run() {
        let fx = Fixture::new("stash-palette");
        fx.write("a.txt", "one\n");
        fx.commit_all("first");
        assert!(labels(&fx).is_empty(), "nothing to stash");

        fx.write("a.txt", "two\n");
        assert_eq!(labels(&fx), ["Stash", "Stash…"], "nothing staged");

        git(&fx.dir, &["add", "a.txt"]);
        // Only while the git found is known to be too old is it left out.
        let expected: &[&str] = match stash::staged_unsupported() {
            None => &["Stash", "Stash…", "Stash only staged changes"],
            Some(_) => &["Stash", "Stash…"],
        };
        assert_eq!(labels(&fx), expected);
    }
}
