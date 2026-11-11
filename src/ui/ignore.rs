//! "Ignore…" in the changed-file lists: the menu items of a file or folder
//! and the dialog that adds the pattern to `.gitignore` or
//! `.git/info/exclude`, saying how many changed files it hides. For files
//! git tracks, ignoring alone changes nothing, so the dialog also offers to
//! stop tracking them (`git rm --cached`), after a confirmation.

use eframe::egui::{Checkbox, RichText, Spinner, TextEdit, Ui};

use crate::git::ignore::{self, Matches, Target};
use crate::git::{ChangeKind, FileChange};
use crate::state::{Dialog, RepoTab, Shared};
use crate::theme::{self, icons};
use crate::ui::dialogs;
use crate::ui::widgets::menu_item;

/// The reader-thread job that checks what a pattern matches.
const JOB: &str = "ignore:check";
/// Names listed before "and N more".
const LISTED: usize = 4;

/// What the ignore dialog holds.
pub struct IgnoreForm {
    title: String,
    pub pattern: String,
    pub target: Target,
    /// Also take the tracked files the pattern matches out of the index.
    pub untrack: bool,
    /// The pattern last checked, and what it matches.
    checked: Option<(String, Result<Matches, String>)>,
    /// The pattern a check is running for.
    pending: Option<String>,
}

/// Opens the dialog for `pattern`; `untrack` ticks "stop tracking" (the
/// file it was opened from is tracked).
pub fn open(shared: &mut Shared, title: impl Into<String>, pattern: String, untrack: bool) {
    shared.show(Dialog::Ignore(Box::new(IgnoreForm {
        title: title.into(),
        pattern,
        target: Target::GitIgnore,
        untrack,
        checked: None,
        pending: None,
    })));
}

/// The ignore items of a changed file's context menu (working-tree lists).
pub fn file_items(ui: &mut Ui, shared: &mut Shared, file: &FileChange) {
    if file.kind == ChangeKind::Conflicted {
        return;
    }
    let name = ignore::file_name(&file.path);
    if file.kind != ChangeKind::Untracked {
        if let Some(pattern) = ignore::file_pattern(&file.path)
            && menu_item(ui, icons::EYE_SLASH, "Stop tracking and ignore…")
        {
            open(shared, format!("Stop tracking and ignore {name}"), pattern, true);
        }
        return;
    }
    if let Some(pattern) = ignore::file_pattern(&file.path)
        && menu_item(ui, icons::EYE_SLASH, &format!("Ignore {name}…"))
    {
        open(shared, format!("Ignore {name}"), pattern, false);
    }
    if let Some(pattern) = ignore::extension_pattern(&file.path)
        && menu_item(ui, icons::EYE_SLASH, &format!("Ignore all {pattern} files…"))
    {
        open(shared, format!("Ignore all {pattern} files"), pattern, false);
    }
    // The nearest folder directly; further ones in a submenu, nearest first.
    match ignore::parents(&file.path).as_slice() {
        [] => {}
        [dir] => folder_item(ui, shared, dir, &format!("Ignore folder {dir}/…"), false),
        dirs => {
            ui.menu_button(format!("{}  Ignore folder", icons::EYE_SLASH), |ui| {
                for dir in dirs {
                    folder_item(ui, shared, dir, &format!("{dir}/…"), false);
                }
            });
        }
    }
}

/// "Ignore folder dir/…" for a folder row; `inside` are the folder's files
/// in that list. Stop tracking starts ticked when they are all tracked.
pub fn folder_items(ui: &mut Ui, shared: &mut Shared, dir: &str, inside: &[&FileChange]) {
    let tracked = !inside.is_empty() && inside.iter().all(|f| f.kind != ChangeKind::Untracked);
    folder_item(ui, shared, dir, &format!("Ignore folder {dir}/…"), tracked);
}

fn folder_item(ui: &mut Ui, shared: &mut Shared, dir: &str, label: &str, untrack: bool) {
    if let Some(pattern) = ignore::folder_pattern(dir)
        && menu_item(ui, icons::EYE_SLASH, label)
    {
        open(shared, format!("Ignore folder {dir}/"), pattern, untrack);
    }
}

/// Whether the dialog stays open.
pub enum Next {
    Keep,
    Close,
}

pub fn dialog(ui: &mut Ui, form: &mut IgnoreForm, shared: &mut Shared, tab: Option<&mut RepoTab>, fresh: bool) -> Next {
    dialogs::title(ui, &form.title);
    let Some(tab) = tab else { return Next::Close };
    let Some(snap) = tab.snap.clone() else { return Next::Close };

    ui.label(RichText::new("Pattern").color(theme::text_dim()));
    let edit = ui.add(TextEdit::singleline(&mut form.pattern).font(theme::code(13.0)).desired_width(f32::INFINITY));
    if fresh {
        edit.request_focus();
    }
    // A pasted line break would make two patterns.
    if form.pattern.contains(['\n', '\r']) {
        form.pattern = form.pattern.replace(['\n', '\r'], "");
    }
    let pattern = form.pattern.clone();
    let valid = ignore::valid(&pattern);

    ui.add_space(2.0);
    ui.label(RichText::new("Add it to").color(theme::text_dim()));
    for (target, help) in [
        (Target::GitIgnore, "committed: ignored for everyone"),
        (Target::Exclude, "only in this clone"),
    ] {
        ui.horizontal(|ui| {
            ui.radio_value(&mut form.target, target, target.label());
            ui.label(RichText::new(help).size(12.0).color(theme::text_faint()));
        });
    }

    // What the pattern matches, asked again whenever it changes. Answers
    // carry their pattern: one from a dialog closed before it came back
    // must not pass for this one's.
    if let Some((asked, result)) = tab.take_result::<(String, Result<Matches, String>)>(JOB)
        && form.pending.as_ref() == Some(&asked)
    {
        form.pending = None;
        form.checked = Some((asked, result));
    }
    let stale = form.checked.as_ref().is_none_or(|(asked, _)| *asked != pattern);
    if valid && stale && form.pending.is_none() {
        let untracked: Vec<String> =
            snap.status.unstaged.iter().filter(|f| f.kind == ChangeKind::Untracked).map(|f| f.path.clone()).collect();
        let asked = pattern.clone();
        tab.spawn_job(JOB, move |loader| {
            let result = ignore::check(loader, &asked, &untracked);
            (asked, result)
        });
        form.pending = Some(pattern.clone());
    }

    ui.add_space(4.0);
    let matches = match &form.checked {
        _ if !valid => {
            let why = if pattern.trim().is_empty() { "Type a pattern." } else { "A line starting with # is a comment." };
            ui.label(RichText::new(why).color(theme::text_dim()));
            None
        }
        Some((asked, Ok(matches))) if *asked == pattern => Some(matches),
        Some((asked, Err(e))) if *asked == pattern => {
            ui.colored_label(theme::orange(), e);
            None
        }
        _ => {
            ui.horizontal(|ui| {
                ui.add(Spinner::new().size(13.0).color(theme::accent()));
                ui.label(RichText::new("Looking for the files it matches…").color(theme::text_dim()));
            });
            None
        }
    };

    let mut ready = false;
    if let Some(matches) = matches {
        let total = snap.status.file_count();
        // "Hides none" says little next to the tracked files it matches.
        let hides = match matches.untracked.len() {
            0 if !matches.tracked.is_empty() => None,
            0 => Some(format!("Hides none of the {total} changed files.")),
            n => Some(format!("Hides {n} of the {total} changed files: {}", names(&matches.untracked))),
        };
        if let Some(hides) = hides {
            ui.label(RichText::new(hides).color(theme::text()));
        }

        let tracked = matches.tracked.len();
        if tracked > 0 {
            let label = match tracked {
                1 => format!("Stop tracking {}, which it matches", matches.tracked[0].path),
                n => format!("Stop tracking the {n} tracked files it matches"),
            };
            ui.add(Checkbox::new(&mut form.untrack, label));
            let paths: Vec<String> = matches.tracked.iter().map(|t| t.path.clone()).collect();
            let note = if form.untrack {
                if tracked > 1 {
                    format!("{}. They stay on disk; the next commit deletes them from the repository.", names(&paths))
                } else {
                    "It stays on disk; the next commit deletes it from the repository.".to_owned()
                }
            } else {
                "Git keeps tracking files it already tracks, ignored or not.".to_owned()
            };
            ui.label(RichText::new(note).size(12.0).color(theme::text_faint()));
        }

        let untrack = form.untrack && tracked > 0;
        let (_, present) = matches.file(form.target);
        if *present {
            ui.colored_label(theme::orange(), format!("{} has this pattern already.", form.target.label()));
        }
        ready = !*present || untrack;
    }

    let untrack = form.untrack && matches.is_some_and(|m| !m.tracked.is_empty());
    let button = if untrack { "Stop tracking…" } else { "Ignore" };
    match dialogs::buttons(ui, button, ready, false) {
        Some(true) => {
            let Some(matches) = matches else { return Next::Keep };
            let (file, _) = matches.file(form.target);
            if untrack {
                confirm(shared, &pattern, file, form.target, matches);
            } else {
                tab.run(ignore::ignore(&pattern, file, form.target, &[]));
            }
            Next::Close
        }
        Some(false) => Next::Close,
        None => Next::Keep,
    }
}

/// Asks before taking files out of the index: the next commit deletes
/// them from the repository.
fn confirm(shared: &mut Shared, pattern: &str, file: &std::path::Path, target: Target, matches: &Matches) {
    let tracked = &matches.tracked;
    let committed = tracked.iter().filter(|t| t.committed).count();
    let paths: Vec<String> = tracked.iter().map(|t| t.path.clone()).collect();
    let (what, them, stay, files) = match tracked.len() {
        1 => (paths[0].clone(), "it", "stays", "an ignored file"),
        n => (format!("These {n} files ({})", names(&paths)), "them", "stay", "ignored files"),
    };
    let consequence = match committed {
        0 => format!("{what} {stay} on disk as {files}, no longer staged."),
        _ => format!("{what} {stay} on disk, but the next commit deletes {them} from the repository."),
    };
    let body = format!(
        "{consequence} {pattern} is added to {} so git does not pick {them} up again. Undo puts {them} back.",
        target.label()
    );
    let title = if tracked.len() == 1 { "Stop tracking 1 file".to_owned() } else { format!("Stop tracking {} files", tracked.len()) };
    shared.show(Dialog::confirm(&title, body, "Stop tracking", ignore::ignore(pattern, file, target, tracked)));
}

/// "a.log, b.log and 3 more".
fn names(paths: &[String]) -> String {
    let shown: Vec<&str> = paths.iter().take(LISTED).map(String::as_str).collect();
    match paths.len() {
        n if n > LISTED => format!("{} and {} more", shown.join(", "), n - LISTED),
        _ => shown.join(", "),
    }
}
