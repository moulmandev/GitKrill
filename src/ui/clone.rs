//! The clone dialog: an address and a folder, and behind "Options" the
//! branch to check out (listed from the remote with `git ls-remote` once
//! the options are open and the address is typed), a shallow history, one
//! branch only and submodules (`git::clone`).

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use eframe::egui::{self, ComboBox, DragValue, RichText, Sense, Spinner, TextEdit, Ui};

use crate::git::clone::{self, CloneOptions, Heads};
use crate::state::{AppAction, Shared};
use crate::theme::{self, icons};
use crate::ui::dialogs;

/// How long typing must pause before the remote is asked for its branches.
const PAUSE: Duration = Duration::from_millis(600);

/// What the clone dialog holds.
pub struct CloneForm {
    pub url: String,
    pub dest: String,
    /// The options are shown.
    open: bool,
    /// The branch picked from the list; `None` is the remote's default.
    branch: Option<String>,
    /// The branch typed, when the remote could not be listed.
    typed_branch: String,
    shallow: bool,
    depth: u32,
    single_branch: bool,
    submodules: bool,
    lookup: Lookup,
}

/// Listing the remote's branches on a thread of its own.
#[derive(Default)]
struct Lookup {
    /// The address as last seen, and since when it reads so.
    seen: Option<(String, Instant)>,
    /// The address being listed, and where the answer arrives.
    pending: Option<(String, Receiver<Result<Heads, String>>)>,
    /// The address listed last, and its answer.
    done: Option<(String, Result<Heads, String>)>,
}

impl CloneForm {
    pub fn new(dest: String) -> Self {
        CloneForm {
            url: String::new(),
            dest,
            open: false,
            branch: None,
            typed_branch: String::new(),
            shallow: false,
            depth: 1,
            single_branch: false,
            submodules: false,
            lookup: Lookup::default(),
        }
    }

    /// The options as chosen (hiding them keeps them). A branch picked
    /// from another address's list is forgotten.
    fn options(&self) -> CloneOptions {
        let listed = matches!(&self.lookup.done, Some((url, Ok(_))) if *url == self.url.trim());
        let branch = if listed {
            self.branch.clone()
        } else {
            Some(self.typed_branch.trim().to_owned()).filter(|b| !b.is_empty())
        };
        CloneOptions {
            branch,
            depth: self.shallow.then_some(self.depth.max(1)),
            single_branch: self.single_branch,
            submodules: self.submodules,
        }
    }
}

/// Whether the dialog stays open.
pub enum Next {
    Keep,
    Close,
}

pub fn dialog(ui: &mut Ui, form: &mut CloneForm, shared: &mut Shared, fresh: bool) -> Next {
    dialogs::title(ui, "Clone repository");
    let url = ui.add(TextEdit::singleline(&mut form.url).hint_text("Repository URL (https or ssh)").desired_width(f32::INFINITY));
    if fresh {
        url.request_focus();
    }
    ui.horizontal(|ui| {
        if ui.button("Browse…").clicked()
            && let Some(dir) = rfd::FileDialog::new().set_directory(form.dest.as_str()).pick_folder()
        {
            form.dest = dir.display().to_string();
        }
        ui.add(TextEdit::singleline(&mut form.dest).hint_text("Clone into this folder").desired_width(f32::INFINITY));
    });

    let address = form.url.trim().to_owned();
    let dest = PathBuf::from(form.dest.trim());
    let problem = (!address.is_empty()).then(|| clone::url_problem(&address)).flatten();
    let target = dest.join(clone::folder_name(&address));
    let exists = !address.is_empty() && target.exists() && std::fs::read_dir(&target).is_ok_and(|mut d| d.next().is_some());
    if let Some(problem) = problem {
        ui.colored_label(theme::orange(), problem);
    } else if exists {
        ui.colored_label(theme::orange(), format!("{} already exists and is not empty.", target.display()));
    } else if !address.is_empty() && dest.is_dir() {
        ui.label(RichText::new(format!("Clones into {}", target.display())).size(12.0).color(theme::text_faint()));
    }

    let chosen = summary(&form.options());
    if disclosure(ui, form.open, &chosen) {
        form.open = !form.open;
    }
    if form.open {
        options(ui, form, &address, &dest);
    }

    let ready = !address.is_empty() && problem.is_none() && !exists && dest.is_dir();
    match dialogs::buttons(ui, "Clone", ready, false) {
        Some(true) => {
            shared.actions.push(AppAction::Clone { url: address, dest, options: form.options() });
            Next::Close
        }
        Some(false) => Next::Close,
        None => Next::Keep,
    }
}

/// What is chosen, in a few words ("dev, shallow, 5 commits"); empty for a
/// plain clone.
fn summary(options: &CloneOptions) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(branch) = &options.branch {
        parts.push(branch.clone());
    }
    if options.single_branch {
        parts.push("that branch only".to_owned());
    }
    if let Some(n) = options.depth {
        parts.push(format!("shallow, {n} {}", if n == 1 { "commit" } else { "commits" }));
    }
    if options.submodules {
        parts.push("submodules".to_owned());
    }
    parts.join(", ")
}

/// The "Options" row that shows and hides them, saying what is chosen
/// while they are hidden; true when clicked.
fn disclosure(ui: &mut Ui, open: bool, chosen: &str) -> bool {
    let caret = if open { icons::CARET_DOWN } else { icons::CARET_RIGHT };
    ui.horizontal(|ui| {
        let text = RichText::new(format!("{caret}  Options")).color(theme::text_dim());
        let response = ui.add(egui::Label::new(text).sense(Sense::click()));
        let response = response.on_hover_text("Branch, shallow clone, single branch, submodules");
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if !open && !chosen.is_empty() {
            ui.label(RichText::new(chosen).size(12.0).color(theme::text_faint()));
        }
        response.clicked()
    })
    .inner
}

fn options(ui: &mut Ui, form: &mut CloneForm, address: &str, dest: &Path) {
    let ctx = ui.ctx().clone();
    look_up(&ctx, &mut form.lookup, address, dest);

    egui::Frame::new().inner_margin(egui::Margin { left: 18, right: 0, top: 0, bottom: 0 }).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 6.0;
        ui.horizontal(|ui| {
            ui.label(RichText::new("Branch").color(theme::text_dim()));
            branch_choice(ui, form, address);
        });
        let default = match &form.lookup.done {
            Some((url, Ok(heads))) if url == address => heads.default.clone(),
            _ => None,
        };
        let single = match form.options().branch.or(default) {
            Some(branch) => format!("Only the history of {branch}"),
            None => "Only the history of that branch".to_owned(),
        };
        ui.checkbox(&mut form.single_branch, single);
        ui.horizontal(|ui| {
            ui.checkbox(&mut form.shallow, "Shallow: only the last");
            ui.add_enabled(form.shallow, DragValue::new(&mut form.depth).range(1..=1_000_000).speed(0.2));
            ui.label(if form.depth == 1 { "commit" } else { "commits" });
        });
        ui.checkbox(&mut form.submodules, "Clone the submodules too");
    });
}

/// The branch drop-down once the remote has answered; a spinner while it
/// is asked; a field to type one into when it could not be listed.
fn branch_choice(ui: &mut Ui, form: &mut CloneForm, address: &str) {
    if address.is_empty() || clone::url_problem(address).is_some() {
        ui.label(RichText::new("Type the address first").size(12.0).color(theme::text_faint()));
        return;
    }
    let answer = form.lookup.done.as_ref().filter(|(url, _)| url == address).map(|(_, result)| result);
    match answer {
        Some(Ok(heads)) => {
            let default = match &heads.default {
                Some(name) => format!("Default ({name})"),
                None => "Default".to_owned(),
            };
            if form.branch.as_ref().is_some_and(|b| !heads.branches.contains(b)) {
                form.branch = None;
            }
            let shown = form.branch.clone().unwrap_or_else(|| default.clone());
            ComboBox::from_id_salt("clone-branch").selected_text(shown).width(ui.available_width() - 4.0).show_ui(ui, |ui| {
                ui.selectable_value(&mut form.branch, None, default);
                for branch in &heads.branches {
                    ui.selectable_value(&mut form.branch, Some(branch.clone()), branch);
                }
            });
        }
        Some(Err(e)) => {
            // Listing never asks for a password; the clone may still get in.
            let first = e.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
            let first = first.strip_prefix("fatal: ").unwrap_or(first).to_owned();
            ui.add(egui::Label::new(RichText::new(icons::INFO).color(theme::text_faint())).sense(Sense::hover()))
                .on_hover_text(format!("Its branches could not be listed: {first}
Type a branch name, or leave it empty for the default."));
            ui.add(TextEdit::singleline(&mut form.typed_branch).hint_text("Default branch").desired_width(ui.available_width()));
        }
        None => {
            ui.add(Spinner::new().size(12.0).color(theme::accent()));
            ui.label(RichText::new("Looking up its branches…").size(12.0).color(theme::text_dim()));
        }
    }
}

/// Asks the remote at `address` for its branches once typing has paused,
/// and collects the answer.
fn look_up(ctx: &egui::Context, lookup: &mut Lookup, address: &str, dest: &Path) {
    if let Some((asked, rx)) = &lookup.pending
        && let Ok(result) = rx.try_recv()
    {
        lookup.done = Some((asked.clone(), result));
        lookup.pending = None;
    }
    if lookup.seen.as_ref().is_none_or(|(seen, _)| seen != address) {
        lookup.seen = Some((address.to_owned(), Instant::now()));
    }
    let known = lookup.done.as_ref().is_some_and(|(url, _)| url == address)
        || lookup.pending.as_ref().is_some_and(|(url, _)| url == address);
    if known || address.is_empty() || clone::url_problem(address).is_some() {
        return;
    }
    let waited = lookup.seen.as_ref().map_or(PAUSE, |(_, since)| since.elapsed());
    if waited < PAUSE {
        ctx.request_repaint_after(PAUSE - waited);
        return;
    }
    // A relative address means something next to the destination folder.
    let dir = if dest.is_dir() { dest.to_path_buf() } else { std::env::temp_dir() };
    let (tx, rx) = channel();
    let url = address.to_owned();
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let _ = tx.send(clone::list_heads(&dir, &url));
        ctx.request_repaint();
    });
    lookup.pending = Some((address.to_owned(), rx));
}
