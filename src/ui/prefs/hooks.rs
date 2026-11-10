//! Preferences > Git Hooks (repository-specific): where git looks for this
//! repository's hooks (`core.hooksPath`), and the hooks it finds there,
//! listed in the background.

use std::path::{Path, PathBuf};

use eframe::egui::{self, Label, RichText, Sense, TextEdit, Ui, vec2};

use super::{Page, form};
use crate::prefs::gitconfig::ConfigFile;
use crate::theme::{self, icons};

const KEY: &str = "core.hooksPath";
const JOB: &str = "prefs-hooks:list";
const DOCS: &str = "https://git-scm.com/docs/githooks";

/// What this section remembers while preferences are open.
#[derive(Default)]
pub struct State {
    /// The path being typed, while the field has focus.
    editing: Option<String>,
    /// The hooks folder as last listed.
    listing: Option<Listing>,
    /// The `core.hooksPath` value a listing is being read for.
    pending: Option<String>,
}

/// The hooks folder git uses, read on the reader thread.
#[derive(Clone, Debug)]
pub struct Listing {
    /// The repository's own `core.hooksPath` it was listed for ("" when
    /// unset).
    for_value: String,
    /// Where git looks for hooks.
    dir: PathBuf,
    /// `.git/hooks`, used when nothing sets `core.hooksPath`; relative to
    /// the working tree when it is inside it.
    default_shown: String,
    /// `core.hooksPath` from the global or system configuration, which
    /// applies while the repository sets none.
    inherited: Option<String>,
    /// `None` when the folder does not exist.
    hooks: Option<Result<Vec<Hook>, String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hook {
    pub name: String,
    pub state: HookState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookState {
    /// Git runs it.
    Active,
    /// An example git ships (`pre-commit.sample`); never run.
    Sample,
    /// Named like a hook, but git will not run it.
    NotExecutable,
    /// A file whose name is no hook's.
    Other,
}

/// Every hook git knows, from its documentation.
const HOOK_NAMES: [&str; 28] = [
    "applypatch-msg",
    "pre-applypatch",
    "post-applypatch",
    "pre-commit",
    "pre-merge-commit",
    "prepare-commit-msg",
    "commit-msg",
    "post-commit",
    "pre-rebase",
    "post-checkout",
    "post-merge",
    "pre-push",
    "pre-receive",
    "update",
    "proc-receive",
    "post-receive",
    "post-update",
    "reference-transaction",
    "push-to-checkout",
    "pre-auto-gc",
    "post-rewrite",
    "sendemail-validate",
    "fsmonitor-watchman",
    "p4-changelist",
    "p4-prepare-changelist",
    "p4-post-changelist",
    "p4-pre-submit",
    "post-index-change",
];

pub fn show(ui: &mut Ui, page: &mut Page<'_>, state: &mut State) {
    let (Some(file), Some(tab)) = (page.repo.as_deref_mut(), page.tab.as_deref_mut()) else { return };
    let ctx = page.ctx;
    file.load(ctx);

    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        let text = "Git hooks are scripts that Git runs before or after events such as a commit or a push. \
                    For more details, see the ";
        ui.label(RichText::new(text).size(13.0).color(theme::text()));
        let link = ui.add(Label::new(RichText::new("Git hooks documentation").size(13.0).color(theme::accent())).sense(Sense::click()));
        if link.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(DOCS).clicked() {
            crate::ui::hosting::open_link(DOCS);
        }
        ui.label(RichText::new(".").size(13.0).color(theme::text()));
    });
    ui.add_space(14.0);

    if !file.ready() {
        form::loading(ui);
        return;
    }
    let value = file.get(KEY).unwrap_or("").to_owned();

    // List the folder again whenever the setting changes.
    if let Some(listing) = tab.take_result::<Listing>(JOB) {
        state.pending = None;
        state.listing = Some(listing);
    }
    let current = state.listing.as_ref().is_some_and(|l| l.for_value == value);
    if !current && state.pending.as_deref() != Some(value.as_str()) {
        let wanted = value.clone();
        tab.spawn_job(JOB, move |loader| list(loader.repo(), &wanted));
        state.pending = Some(value.clone());
    }
    let State { editing, listing, .. } = state;
    let listing = listing.as_ref().filter(|_| current);

    form::row(ui, "Git Hooks directory", |ui| {
        let placeholder = listing.map_or(".git/hooks", |l| l.default_shown.as_str());
        let in_use = listing.map(|l| l.dir.display().to_string());
        path_field(ui, ctx, file, editing, &value, placeholder, in_use.as_deref());
        form::help(ui, "The directory that Git hooks will use for this repository.");
        match listing.and_then(|l| l.inherited.as_deref()) {
            Some(global) if value.is_empty() => {
                form::help(ui, &format!("Empty: {global}, from your global Git configuration."));
            }
            _ => form::help(ui, "Empty: the repository's own hooks folder."),
        }
    });

    form::row(ui, "Hooks", |ui| match listing {
        None => form::loading(ui),
        Some(listing) => hooks_list(ui, listing),
    });
}

/// The hooks found, one per line, and a button to open the folder.
fn hooks_list(ui: &mut Ui, listing: &Listing) {
    match &listing.hooks {
        None => form::warning(ui, &format!("{} does not exist.", listing.dir.display())),
        Some(Err(error)) => form::error(ui, error),
        Some(Ok(hooks)) if hooks.is_empty() => form::help(ui, "There are no hooks in this folder."),
        Some(Ok(hooks)) => {
            ui.add_space(4.0);
            for hook in hooks.iter().filter(|hook| hook.state != HookState::Sample) {
                let (icon, color, note) = match hook.state {
                    HookState::Active => (icons::CHECK_CIRCLE, theme::green(), "Active"),
                    HookState::NotExecutable => (icons::WARNING, theme::orange(), "Not executable, not run"),
                    _ => (icons::FILE, theme::text_faint(), "Not a hook name"),
                };
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    ui.label(RichText::new(icon).size(14.0).color(color));
                    let (rect, _) = ui.allocate_exact_size(vec2(200.0, 18.0), Sense::hover());
                    let name_color = if hook.state == HookState::Active { theme::text_bright() } else { theme::text_dim() };
                    let galley = crate::ui::widgets::elided(ui.painter(), &hook.name, theme::mono(12.5), name_color, rect.width());
                    crate::ui::widgets::galley_left(ui.painter(), rect, rect.left(), galley);
                    ui.label(RichText::new(note).size(12.0).color(theme::text_dim()));
                });
            }
            // The examples git puts in every new repository, on one line.
            let samples: Vec<&str> = hooks
                .iter()
                .filter(|hook| hook.state == HookState::Sample)
                .map(|hook| hook.name.trim_end_matches(".sample"))
                .collect();
            if !samples.is_empty() {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    ui.label(RichText::new(icons::CIRCLE_DASHED).size(14.0).color(theme::text_faint()));
                    ui.vertical(|ui| {
                        ui.set_max_width(form::HELP_WIDTH);
                        let text = format!("Samples, not run: {}.", samples.join(", "));
                        ui.add(Label::new(RichText::new(text).size(12.0).color(theme::text_dim())).wrap());
                    });
                });
            }
            ui.add_space(4.0);
        }
    }
    if listing.hooks.is_some() && form::button(ui, "Open folder", theme::accent()).clicked() {
        crate::platform::open(&listing.dir);
    }
    form::help(
        ui,
        "GitKrill runs hooks through git, so they behave as in a terminal: a hook that fails stops the \
         commit, push or checkout, and its message is shown.",
    );
}

/// The folder field with Browse, bound to `core.hooksPath`: what is typed
/// is written when the field loses focus; an empty field unsets it.
fn path_field(
    ui: &mut Ui,
    ctx: &egui::Context,
    file: &mut ConfigFile,
    editing: &mut Option<String>,
    stored: &str,
    placeholder: &str,
    in_use: Option<&str>,
) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let mut text = editing.clone().unwrap_or_else(|| stored.to_owned());
        let field = TextEdit::singleline(&mut text)
            .hint_text(placeholder)
            .desired_width(form::FIELD_WIDTH - 70.0)
            .margin(vec2(8.0, 5.0));
        let mut response = ui.add(field);
        if let Some(in_use) = in_use {
            response = response.on_hover_text(in_use);
        }
        if response.has_focus() {
            *editing = Some(text.clone());
        } else if editing.take().is_some() || response.lost_focus() {
            write(ctx, file, stored, text.trim());
        }
        if form::button(ui, "Browse", theme::accent()).clicked() {
            let mut dialog = rfd::FileDialog::new().set_title("Git hooks directory");
            let start = Path::new(stored);
            if start.is_absolute() && start.is_dir() {
                dialog = dialog.set_directory(start);
            }
            if let Some(picked) = dialog.pick_folder() {
                write(ctx, file, stored, &picked.display().to_string());
            }
        }
    });
}

fn write(ctx: &egui::Context, file: &mut ConfigFile, stored: &str, value: &str) {
    if value != stored {
        file.set(ctx, KEY, (!value.is_empty()).then_some(value));
    }
}

/// Where git looks for hooks with `core.hooksPath` set to `value` in the
/// repository (or unset, ""), and what it finds there. Blocks: run it on the
/// reader thread.
pub fn list(repo: &git2::Repository, value: &str) -> Listing {
    let default_dir = repo.commondir().join("hooks");
    // The repository's own value is passed in (it may not be on disk yet);
    // otherwise the global or system one applies.
    let inherited = if value.is_empty() {
        repo.config().ok().and_then(|config| {
            [git2::ConfigLevel::Global, git2::ConfigLevel::XDG, git2::ConfigLevel::System]
                .into_iter()
                .find_map(|level| config.open_level(level).ok()?.get_string(KEY).ok())
                .filter(|v| !v.is_empty())
        })
    } else {
        None
    };
    let base = repo.workdir().unwrap_or_else(|| repo.path());
    let dir = match (value, &inherited) {
        ("", None) => default_dir.clone(),
        ("", Some(inherited)) => resolve(inherited, base),
        (value, _) => resolve(value, base),
    };
    let hooks = dir.is_dir().then(|| read_hooks(&dir));
    let default_shown = match repo.workdir().and_then(|workdir| default_dir.strip_prefix(workdir).ok()) {
        Some(inside) => inside.to_string_lossy().replace('\\', "/"),
        None => default_dir.display().to_string(),
    };
    Listing { for_value: value.to_owned(), dir, default_shown, inherited, hooks }
}

/// A `core.hooksPath` value as git reads it: `~/` is the home folder, and a
/// relative path is relative to where hooks run (the working tree).
fn resolve(value: &str, base: &Path) -> PathBuf {
    if let Some(rest) = value.strip_prefix("~/").or_else(|| value.strip_prefix("~\\"))
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    let path = Path::new(value);
    if path.is_absolute() { path.to_path_buf() } else { base.join(path) }
}

fn read_hooks(dir: &Path) -> Result<Vec<Hook>, String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{} cannot be read: {e}", dir.display()))?;
    let mut hooks: Vec<Hook> = entries
        .flatten()
        .filter(|entry| std::fs::metadata(entry.path()).is_ok_and(|m| m.is_file()))
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let state = hook_state(&entry.path(), &name);
            Hook { name, state }
        })
        .collect();
    // Hooks that can run first, samples last.
    let rank = |state: HookState| match state {
        HookState::Active | HookState::NotExecutable => 0,
        HookState::Other => 1,
        HookState::Sample => 2,
    };
    hooks.sort_by(|a, b| (rank(a.state), &a.name).cmp(&(rank(b.state), &b.name)));
    Ok(hooks)
}

fn hook_state(path: &Path, name: &str) -> HookState {
    if name.ends_with(".sample") {
        return HookState::Sample;
    }
    let base = if cfg!(windows) { name.strip_suffix(".exe").unwrap_or(name) } else { name };
    if !HOOK_NAMES.contains(&base) {
        HookState::Other
    } else if executable(path) {
        HookState::Active
    } else {
        HookState::NotExecutable
    }
}

/// Whether git would run the file: the executable bit on Unix; on Windows,
/// where there is none, an `.exe` or a script starting with `#!`, as Git for
/// Windows decides.
#[cfg(unix)]
fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn executable(path: &Path) -> bool {
    use std::io::Read;
    if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe")) {
        return true;
    }
    let mut start = [0u8; 2];
    std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut start)).is_ok() && &start == b"#!"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hooks_are_listed_with_their_state() {
        let fx = crate::tests::Fixture::new("prefs-hooks");
        let hooks = fx.dir.join(".git").join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        std::fs::write(hooks.join("pre-commit"), "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::write(hooks.join("pre-push.sample"), "#!/bin/sh\n").unwrap();
        std::fs::write(hooks.join("notes.txt"), "not a hook").unwrap();
        std::fs::write(hooks.join("post-merge"), "echo no shebang\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(hooks.join("pre-commit"), std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let listing = fx.job(|loader| list(loader.repo(), ""));
        assert!(listing.dir.ends_with(".git/hooks"), "{}", listing.dir.display());
        assert_eq!(listing.default_shown, ".git/hooks");
        let found: Vec<(&str, HookState)> =
            listing.hooks.as_ref().unwrap().as_ref().unwrap().iter().map(|h| (h.name.as_str(), h.state)).collect();
        assert!(found.contains(&("pre-commit", HookState::Active)), "{found:?}");
        assert!(found.contains(&("post-merge", HookState::NotExecutable)), "{found:?}");
        assert!(found.contains(&("notes.txt", HookState::Other)), "{found:?}");
        assert!(found.contains(&("pre-push.sample", HookState::Sample)), "{found:?}");
        // Samples come last.
        assert_eq!(found.last().map(|h| h.1), Some(HookState::Sample));

        // A relative core.hooksPath is relative to the working tree.
        let listing = fx.job(|loader| list(loader.repo(), "tools/hooks"));
        assert!(listing.dir.ends_with("tools/hooks"), "{}", listing.dir.display());
        assert!(listing.hooks.is_none(), "the folder does not exist");
        std::fs::create_dir_all(fx.dir.join("tools/hooks")).unwrap();
        let listing = fx.job(|loader| list(loader.repo(), "tools/hooks"));
        assert_eq!(listing.hooks, Some(Ok(Vec::new())));
    }
}
