//! Preferences > UI Customization: the theme (built-in, or a file of the
//! themes folder), where toasts appear, the date format, the toolbar,
//! avatars.
//!
//! The themes folder is read on a background thread when the page opens
//! (and again on Reload, or after writing the templates); the theme in use
//! is put into effect by `crate::prefs::appearance::apply`.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use eframe::egui::{self, Label, RichText, Sense, Ui, vec2};

use super::{Page, form};
use crate::prefs::appearance::{self as prefs, Corner, DateFormat, ThemeFile};
use crate::state::ToastKind;
use crate::theme::{self, Choice};

/// What this section remembers while preferences are open.
#[derive(Default)]
pub struct State {
    /// The themes folder; `None` until the page first shows, or when the
    /// system has no configuration folder.
    dir: Option<PathBuf>,
    started: bool,
    /// What the folder held when last read; `None` while it is first read.
    themes: Option<Vec<ThemeFile>>,
    reading: Option<Receiver<Read>>,
    /// The date pattern being typed, kept aside until the field loses focus.
    date: Option<String>,
}

/// One read of the themes folder.
struct Read {
    themes: Vec<ThemeFile>,
    /// When the templates were written first: how that went.
    wrote: Option<Result<(), String>>,
}

/// A choice in the Theme drop-down.
#[derive(Clone, PartialEq)]
enum Pick {
    BuiltIn(Choice),
    Custom(String),
}

pub fn show(ui: &mut Ui, page: &mut Page<'_>, state: &mut State) {
    if !state.started {
        state.started = true;
        state.dir = prefs::themes_dir();
        read(page.ctx, state, false);
    }
    poll(page, state);

    form::row(ui, "Theme", |ui| theme_row(ui, page, state));

    form::row(ui, "Notification Location", |ui| {
        let options = Corner::ALL.map(|corner| (corner, corner.label()));
        form::choice(ui, "prefs-toast-corner", &mut page.shared.settings.appearance.toast_corner, &options);
        form::help(ui, "Where messages such as \"Pushed\" or \"Pull failed\" appear.");
    });

    form::row(ui, "Date/Time Short Format", |ui| date_row(ui, page, state));

    let settings = &mut page.shared.settings;
    form::row(ui, "Show toolbar icon labels", |ui| {
        form::checkbox(ui, &mut settings.appearance.toolbar_labels, "");
        form::help(ui, "Off: icons only, named in their tooltips, and a shorter toolbar.");
    });

    form::row(ui, "Display author initials and generic remote icons instead of avatars", |ui| {
        let mut initials = !settings.fetch_avatars;
        if form::checkbox(ui, &mut initials, "") {
            settings.fetch_avatars = !initials;
        }
        form::help(ui, "Pictures come from Gravatar and GitHub; with this on, neither is asked for them.");
    });

    form::row(ui, "Show workspace breadcrumb in toolbar", |ui| {
        form::checkbox(ui, &mut settings.appearance.workspace_breadcrumb, "");
        form::help(
            ui,
            "For a repository inside a workspace (File > Add Folder of Repositories), the toolbar reads \
             workspace › repository › branch.",
        );
    });
}

/// Reads the themes folder in the background, writing the templates into
/// it first when `write_templates`.
fn read(ctx: &egui::Context, state: &mut State, write_templates: bool) {
    let Some(dir) = state.dir.clone() else {
        state.themes = Some(Vec::new());
        return;
    };
    let (tx, rx) = channel();
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let wrote = write_templates.then(|| prefs::write_templates(&dir).map_err(|error| error.to_string()));
        let _ = tx.send(Read { themes: prefs::scan(&dir), wrote });
        ctx.request_repaint();
    });
    state.reading = Some(rx);
}

fn poll(page: &mut Page<'_>, state: &mut State) {
    let Some(rx) = &state.reading else { return };
    let read = match rx.try_recv() {
        Ok(read) => read,
        Err(TryRecvError::Empty) => return,
        Err(TryRecvError::Disconnected) => Read { themes: Vec::new(), wrote: None },
    };
    state.reading = None;
    match read.wrote {
        Some(Ok(())) => page.shared.toast(
            ToastKind::Success,
            "Theme templates written",
            "dark.template.json and light.template.json: copy one under a new name and change its colours.",
        ),
        Some(Err(error)) => page.shared.toast(ToastKind::Error, "Theme templates not written", error),
        None => {}
    }
    // The file of the theme in use may have been edited since it was put
    // into effect.
    if let Some(name) = &page.shared.settings.appearance.custom_theme
        && let Some(Ok(palette)) = read.themes.iter().find(|t| &t.name == name).map(|t| &t.palette)
    {
        prefs::refresh_theme(page.ctx, name, palette);
    }
    state.themes = Some(read.themes);
}

fn theme_row(ui: &mut Ui, page: &mut Page<'_>, state: &mut State) {
    let settings = &mut page.shared.settings;
    let current = match &settings.appearance.custom_theme {
        Some(name) => Pick::Custom(name.clone()),
        None => Pick::BuiltIn(settings.theme),
    };
    let mut options: Vec<(Pick, String)> =
        Choice::ALL.iter().map(|choice| (Pick::BuiltIn(*choice), choice.label().to_owned())).collect();
    let themes = state.themes.as_deref();
    options.extend(themes.unwrap_or_default().iter().map(|t| (Pick::Custom(t.name.clone()), t.name.clone())));
    let missing = match &current {
        Pick::Custom(name) => themes.is_some_and(|themes| !themes.iter().any(|t| &t.name == name)),
        Pick::BuiltIn(_) => false,
    };
    if let Pick::Custom(name) = &current
        && !options.iter().any(|(pick, _)| *pick == current)
    {
        options.push((current.clone(), if missing { format!("{name} (missing)") } else { name.clone() }));
    }

    let mut picked = current.clone();
    let labelled: Vec<(Pick, &str)> = options.iter().map(|(pick, label)| (pick.clone(), label.as_str())).collect();
    if form::choice(ui, "prefs-theme", &mut picked, &labelled) {
        match picked {
            Pick::BuiltIn(choice) => {
                settings.theme = choice;
                settings.appearance.custom_theme = None;
            }
            Pick::Custom(name) => settings.appearance.custom_theme = Some(name),
        }
    }

    // What is wrong with the chosen file, if anything.
    if let Pick::Custom(name) = &current {
        let built_in = settings.theme.label();
        if missing {
            form::error(ui, &format!("{name}.json is not in the themes folder: the {built_in} theme is in use."));
        }
        if let Some(file) = themes.unwrap_or_default().iter().find(|t| &t.name == name) {
            match &file.palette {
                Err(error) => form::error(ui, &format!("{name}.json: {error} The {built_in} theme is in use.")),
                Ok(_) => {
                    const SHOWN: usize = 6;
                    for warning in file.warnings.iter().take(SHOWN) {
                        form::warning(ui, warning);
                    }
                    if file.warnings.len() > SHOWN {
                        form::warning(ui, &format!("{} more.", file.warnings.len() - SHOWN));
                    }
                }
            }
        }
    }

    form::help(ui, "Create custom themes by copying and modifying a dark or light theme template in your themes directory:");
    match &state.dir {
        Some(dir) => {
            // In the system's own separators, whatever GITKRILL_CONFIG_DIR used.
            let shown = crate::workspace::clean(dir).display().to_string();
            let text = RichText::new(shown).size(12.0).color(theme::accent());
            let response = ui.add(Label::new(text).sense(Sense::click()).wrap());
            if response.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Open the folder").clicked() {
                let dir = dir.clone();
                // Made first, so there is something to open.
                std::thread::spawn(move || {
                    let _ = std::fs::create_dir_all(&dir);
                    crate::platform::open(&dir);
                });
            }
        }
        None => form::error(ui, "This system has no configuration folder to keep themes in."),
    }
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let ready = state.dir.is_some() && state.reading.is_none();
        if ui.add_enabled_ui(ready, |ui| form::button(ui, "Create templates", theme::green())).inner.clicked() {
            read(page.ctx, state, true);
        }
        let reload = ui.add_enabled_ui(ready, |ui| form::button(ui, "Reload", theme::accent())).inner;
        if reload.on_hover_text("Read the themes folder again").clicked() {
            read(page.ctx, state, false);
        }
        if state.reading.is_some() {
            ui.add(egui::Spinner::new().size(13.0).color(theme::accent()));
        }
    });
    form::help(
        ui,
        "A theme is a .json file: \"base\" is \"dark\" or \"light\", and any colour of the template \
         (#rrggbb, or #rrggbbaa when translucent) replaces the base's. It is used as soon as it is chosen \
         here; Reload picks up changes to it.",
    );
}

fn date_row(ui: &mut Ui, page: &mut Page<'_>, state: &mut State) {
    let stored = page.shared.settings.appearance.date_format.clone();
    let mut text = state.date.take().unwrap_or_else(|| stored.clone());
    let response = form::text(ui, &mut text, "MMM D, YYYY");
    let pattern = text.trim().to_owned();
    // The widget's own focus: `response.has_focus()` is also false while the
    // window is in the background, which would drop what was typed.
    if ui.memory(|m| m.has_focus(response.id)) {
        state.date = Some(text);
    } else if response.lost_focus() && pattern != stored {
        page.shared.settings.appearance.date_format = pattern.clone();
    }

    // Today in the pattern being typed.
    let now = chrono::Local::now();
    let format = DateFormat::compile(&pattern);
    let today = match &format {
        Some(format) => format.format(&now, false),
        None => now.format("%b %-d, %Y").to_string(),
    };
    ui.label(RichText::new(today).size(13.0).color(theme::text()));
    if format.as_ref().is_some_and(|format| !format.has_tokens) {
        form::warning(ui, "No part of a date in this pattern: add tokens such as D, M and YYYY.");
    }
    form::help(
        ui,
        "Used wherever GitKrill shows a date: the graph, commit details, file history. Where a time of day \
         is shown and the pattern has none, HH:mm is added. Text in [brackets] is kept as typed. Empty: \
         Nov 10, 2026.",
    );
    // Every token, with what it prints now.
    egui::Grid::new("prefs-date-tokens").num_columns(2).spacing(vec2(14.0, 2.0)).show(ui, |ui| {
        for (unit, _, tokens) in prefs::TOKENS {
            ui.label(RichText::new(*unit).size(11.5).color(theme::text_dim()));
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                for (token, _) in *tokens {
                    let example = DateFormat::compile(token).map(|f| f.format(&now, false)).unwrap_or_default();
                    ui.label(RichText::new(*token).font(theme::mono(11.5)).color(theme::text()));
                    ui.label(RichText::new(example).size(11.5).color(theme::text_dim()));
                    ui.add_space(10.0);
                }
            });
            ui.end_row();
        }
    });
}
