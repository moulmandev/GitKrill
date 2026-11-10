//! Preferences > External Tools: the merge tool, diff tool, editor and
//! terminal GitKrill hands files and repositories to. Which tools are
//! installed is looked up once, in the background, when the page opens
//! ([`crate::external::detect`]); starting them is `crate::external`'s.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use eframe::egui::{self, Id, RichText, TextEdit, Ui, vec2};

use super::{Page, form};
use crate::external::{self, Detected, Found};
use crate::prefs::tools::{DiffTool, Editor, MergeTool, Program, Terminal, Tools};
use crate::theme;

/// What this section remembers while preferences are open.
#[derive(Default)]
pub struct State {
    detecting: Option<Receiver<Detected>>,
    detected: Option<Detected>,
}

impl State {
    /// Starts looking for tools the first time the page shows, and takes
    /// the answer when it is in.
    fn poll(&mut self, page: &mut Page<'_>) {
        if self.detected.is_none() && self.detecting.is_none() {
            let (tx, rx) = channel();
            let repo = page.tab.as_deref().map(|tab| tab.path.clone());
            let check = saved_paths(&page.shared.settings.tools);
            let ctx = page.ctx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(external::detect(repo.as_deref(), &check));
                ctx.request_repaint();
            });
            self.detecting = Some(rx);
        }
        if let Some(rx) = &self.detecting {
            match rx.try_recv() {
                Ok(detected) => {
                    heal(&mut page.shared.settings.tools, &detected);
                    self.detected = Some(detected);
                    self.detecting = None;
                }
                Err(TryRecvError::Disconnected) => self.detecting = None,
                Err(TryRecvError::Empty) => {}
            }
        }
    }
}

/// The programs the settings point at, to check they are still there.
fn saved_paths(tools: &Tools) -> Vec<PathBuf> {
    programs(tools).map(|program| program.path.clone()).collect()
}

fn programs(tools: &Tools) -> impl Iterator<Item = &Program> {
    let merge = match &tools.merge {
        MergeTool::Program(program) => Some(program),
        MergeTool::GitConfig => None,
    };
    let diff = match &tools.diff {
        DiffTool::Program(program) => Some(program),
        _ => None,
    };
    let editor = match &tools.editor {
        Editor::Program(program) => Some(program),
        _ => None,
    };
    let terminal = match &tools.terminal {
        Terminal::Program(program) => Some(program),
        Terminal::System => None,
    };
    [merge, diff, editor, terminal].into_iter().flatten()
}

/// A chosen tool that moved (an update installed a new version in a new
/// folder) follows it, rather than failing the next time it is started.
fn heal(tools: &mut Tools, detected: &Detected) {
    let fix = |program: &mut Program| {
        if detected.missing.contains(&program.path)
            && let Some(found) = detected.find(&program.id)
        {
            program.path = found.path.clone();
        }
    };
    if let MergeTool::Program(program) = &mut tools.merge {
        fix(program);
    }
    if let DiffTool::Program(program) = &mut tools.diff {
        fix(program);
    }
    if let Editor::Program(program) = &mut tools.editor {
        fix(program);
    }
    if let Terminal::Program(program) = &mut tools.terminal {
        fix(program);
    }
}

pub fn show(ui: &mut Ui, page: &mut Page<'_>, state: &mut State) {
    state.poll(page);
    page.global.load(page.ctx);
    let Page { ctx, shared, global, .. } = page;
    let tools = &mut shared.settings.tools;
    let detected = state.detected.as_ref();

    form::intro(
        ui,
        "Programs GitKrill hands files and repositories to. The lists offer the tools installed on this computer.",
    );
    if detected.is_none() {
        form::indented(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(13.0).color(theme::accent()));
                ui.label(RichText::new("Looking for installed tools…").size(12.5).color(theme::text_dim()));
            });
        });
    }

    form::row(ui, "External Merge Tool", |ui| {
        let git = git_label(detected.map(|d| d.git_merge_tool.as_deref()));
        let mut options = vec![(MergeTool::GitConfig, git)];
        let current = match &tools.merge {
            MergeTool::Program(program) => Some(program),
            MergeTool::GitConfig => None,
        };
        options.extend(offered(detected.map(|d| d.compare.as_slice()), current).map(|p| (MergeTool::Program(p.clone()), p.name)));
        choice(ui, "prefs-tools-merge", &mut tools.merge, &options);
        form::help(
            ui,
            "Resolves conflicted files, from the file list or the conflict resolver. GitKrill waits for it to \
             close; the file counts as resolved once the tool has saved it.",
        );
        match &tools.merge {
            MergeTool::Program(program) => location(ui, program, detected),
            MergeTool::GitConfig => {
                if detected.is_some_and(|d| d.git_merge_tool.is_none()) {
                    form::warning(ui, "merge.tool is not set in your Git config. Choose a tool above to use one.");
                }
            }
        }
    });

    form::row(ui, "External Diff Tool", |ui| {
        let git = git_label(detected.map(|d| d.git_diff_tool.as_deref().or(d.git_merge_tool.as_deref())));
        let mut options = vec![(DiffTool::SameAsMerge, "<Use merge tool>".to_owned()), (DiffTool::GitConfig, git)];
        let current = match &tools.diff {
            DiffTool::Program(program) => Some(program),
            _ => None,
        };
        options.extend(offered(detected.map(|d| d.compare.as_slice()), current).map(|p| (DiffTool::Program(p.clone()), p.name)));
        choice(ui, "prefs-tools-diff", &mut tools.diff, &options);
        form::help(ui, "Compares a changed file with its previous version, from the file lists and a commit's files.");
        match &tools.diff {
            DiffTool::Program(program) => location(ui, program, detected),
            DiffTool::GitConfig => {
                if detected.is_some_and(|d| d.git_diff_tool.is_none() && d.git_merge_tool.is_none()) {
                    form::warning(ui, "Neither diff.tool nor merge.tool is set in your Git config.");
                }
            }
            DiffTool::SameAsMerge => {}
        }
    });

    form::row(ui, "External Editor", |ui| {
        let mut options = vec![(Editor::None, "None".to_owned())];
        let current = match &tools.editor {
            Editor::Program(program) => Some(program),
            _ => None,
        };
        options.extend(offered(detected.map(|d| d.editors.as_slice()), current).map(|p| (Editor::Program(p.clone()), p.name)));
        options.push((Editor::Custom, "Custom command".to_owned()));
        choice(ui, "prefs-tools-editor", &mut tools.editor, &options);
        match &tools.editor {
            Editor::Custom => {
                command_text(ui, "prefs-tools-editor-command", &mut tools.editor_command, EDITOR_HINT);
                form::help(
                    ui,
                    "{path} stands for the file or folder to open; without it, the path is added at the end. \
                     Put quotes around a program path that has spaces.",
                );
            }
            Editor::Program(program) => {
                form::help(ui, "Opens files from the file lists, and the repository from the tab menu and the command palette.");
                location(ui, program, detected);
            }
            Editor::None => form::help(ui, "Choose an editor to open files and the repository in it."),
        }
    });

    form::row(ui, "Delete \".orig\" files after merging", |ui| {
        if !global.ready() {
            form::loading(ui);
            return;
        }
        // git keeps a backup unless told not to.
        let mut delete = !global.get_bool("mergetool.keepBackup").unwrap_or(true);
        if form::checkbox(ui, &mut delete, "") {
            global.set_bool(ctx, "mergetool.keepBackup", !delete);
        }
        form::help(
            ui,
            "The merge tool leaves a copy of the conflicted file, with .orig added to its name. \
             Sets mergetool.keepBackup in your global Git config.",
        );
    });

    form::row(ui, "Default External Terminal", |ui| {
        let mut options = vec![(Terminal::System, "System default".to_owned())];
        let current = match &tools.terminal {
            Terminal::Program(program) => Some(program),
            Terminal::System => None,
        };
        options.extend(offered(detected.map(|d| d.terminals.as_slice()), current).map(|p| (Terminal::Program(p.clone()), p.name)));
        ui.add_enabled_ui(!tools.custom_terminal, |ui| choice(ui, "prefs-tools-terminal", &mut tools.terminal, &options));
        if tools.custom_terminal {
            form::help(ui, "The custom command below is used instead.");
        } else {
            match &tools.terminal {
                Terminal::System => form::help(ui, SYSTEM_TERMINAL),
                Terminal::Program(program) => location(ui, program, detected),
            }
        }
    });

    form::row(ui, "Use Custom Terminal Command", |ui| {
        form::checkbox(ui, &mut tools.custom_terminal, "");
        if tools.custom_terminal {
            command_text(ui, "prefs-tools-terminal-command", &mut tools.terminal_command, TERMINAL_HINT);
            form::help(
                ui,
                "{path} stands for the repository folder. The command also starts in that folder, so most \
                 terminals need no argument. Put quotes around a program path that has spaces.",
            );
            if tools.terminal_command.trim().is_empty() {
                form::warning(ui, "Until a command is typed, the terminal chosen above opens.");
            }
        }
    });
}

#[cfg(windows)]
const SYSTEM_TERMINAL: &str = "Windows Terminal when it is installed, Command Prompt otherwise.";
#[cfg(target_os = "macos")]
const SYSTEM_TERMINAL: &str = "Terminal.";
#[cfg(all(unix, not(target_os = "macos")))]
const SYSTEM_TERMINAL: &str = "The first terminal found among x-terminal-emulator, GNOME Terminal, Konsole and XTerm.";

#[cfg(windows)]
const EDITOR_HINT: &str = "\"C:\\Program Files\\Editor\\editor.exe\" {path}";
#[cfg(not(windows))]
const EDITOR_HINT: &str = "editor --new-window {path}";

#[cfg(windows)]
const TERMINAL_HINT: &str = "wt.exe -d {path}";
#[cfg(target_os = "macos")]
const TERMINAL_HINT: &str = "open -a iTerm {path}";
#[cfg(all(unix, not(target_os = "macos")))]
const TERMINAL_HINT: &str = "kitty --directory {path}";

/// "Git config default (kdiff3)", once git's configuration is read.
fn git_label(value: Option<Option<&str>>) -> String {
    match value {
        None => "Git config default".to_owned(),
        Some(Some(tool)) => format!("Git config default ({tool})"),
        Some(None) => "Git config default (not set)".to_owned(),
    }
}

/// The tools to offer: those found, and the chosen one even when it was
/// not (so the drop-down still shows it).
fn offered<'a>(found: Option<&'a [Found]>, current: Option<&'a Program>) -> impl Iterator<Item = Program> + 'a {
    let found = found.unwrap_or_default();
    let extra = current.filter(|program| !found.iter().any(|f| f.id == program.id)).cloned();
    found.iter().map(Found::program).chain(extra)
}

/// [`form::choice`] with labels that were built at run time.
fn choice<T: PartialEq + Clone>(ui: &mut Ui, id: &str, value: &mut T, options: &[(T, String)]) -> bool {
    let options: Vec<(T, &str)> = options.iter().map(|(value, label)| (value.clone(), label.as_str())).collect();
    form::choice(ui, id, value, &options)
}

/// Where the chosen program is, or that it is gone.
fn location(ui: &mut Ui, program: &Program, detected: Option<&Detected>) {
    let path = program.path.display().to_string();
    if detected.is_some_and(|d| d.missing.contains(&program.path)) {
        form::error(ui, &format!("{} is no longer at {path}. Choose another tool, or install it again.", program.name));
    } else {
        form::help(ui, &path);
    }
}

/// A command line field, as wide as the help under it: command lines are
/// longer than the values of other fields. What is typed is kept aside
/// while the field has focus and stored when it loses it, so the settings
/// are not saved after every key.
fn command_text(ui: &mut Ui, id: &str, value: &mut String, hint: &str) {
    let id = Id::new(id);
    let mut draft = ui.data_mut(|d| d.get_temp::<String>(id)).unwrap_or_else(|| value.clone());
    let field = TextEdit::singleline(&mut draft).hint_text(hint).desired_width(form::HELP_WIDTH).margin(vec2(8.0, 5.0));
    let response = ui.add(field);
    if response.has_focus() {
        ui.data_mut(|d| d.insert_temp(id, draft));
    } else {
        ui.data_mut(|d| d.remove::<String>(id));
        if response.lost_focus() && draft.trim() != value.as_str() {
            *value = draft.trim().to_owned();
        }
    }
}
