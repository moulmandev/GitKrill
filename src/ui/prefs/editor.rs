//! Preferences > Editor: how diffs, blame and the conflict editor show code.

use std::sync::mpsc::{Receiver, TryRecvError, channel};

use eframe::egui::{self, ComboBox, DragValue, Id, RichText, ScrollArea, Ui, vec2};

use super::{Page, form};
use crate::git::text::Eol;
use crate::prefs::editor::{self, DiffContext, DiffLayout, MAX_FONT_SIZE, MIN_FONT_SIZE, fonts::FontEntry};
use crate::theme;

/// What this section remembers while preferences are open.
#[derive(Default)]
pub struct State {
    /// The installed fonts, once listed.
    fonts: Option<Vec<FontEntry>>,
    /// The listing on its way from the background.
    listing: Option<Receiver<Vec<FontEntry>>>,
}

impl State {
    /// Lists the installed fonts in the background the first time the page
    /// is shown, and collects the list.
    fn poll_fonts(&mut self, ctx: &egui::Context) {
        if self.fonts.is_none() && self.listing.is_none() {
            let (tx, rx) = channel();
            let ctx = ctx.clone();
            let spawned = std::thread::Builder::new().name("font-list".into()).spawn(move || {
                let _ = tx.send(editor::fonts::scan());
                ctx.request_repaint();
            });
            match spawned {
                Ok(_) => self.listing = Some(rx),
                Err(_) => self.fonts = Some(Vec::new()),
            }
        }
        if let Some(rx) = &self.listing {
            match rx.try_recv() {
                Ok(fonts) => {
                    self.fonts = Some(fonts);
                    self.listing = None;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.fonts = Some(Vec::new());
                    self.listing = None;
                }
            }
        }
    }
}

pub fn show(ui: &mut Ui, page: &mut Page<'_>, state: &mut State) {
    state.poll_fonts(page.ctx);
    let prefs = &mut page.shared.settings.editor;

    form::row(ui, "Font", |ui| {
        font_choice(ui, prefs, state.fonts.as_deref());
        ui.add_space(2.0);
        form::checkbox(ui, &mut prefs.monospace_only, "Show only monospace fonts");
        if state.fonts.is_none() {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(12.0).color(theme::accent()));
                ui.label(RichText::new("Looking for installed fonts…").size(12.0).color(theme::text_dim()));
            });
        }
        if let Some(error) = editor::font_error().filter(|_| !prefs.font.is_empty()) {
            form::error(ui, &format!("{error} The default font is used instead."));
        }
        form::help(ui, "The font of diffs, blame and the conflict editor.");
    });

    form::row(ui, "Font Size", |ui| {
        let field = DragValue::new(&mut prefs.font_size).range(MIN_FONT_SIZE..=MAX_FONT_SIZE).speed(0.1).max_decimals(1);
        ui.add_sized(vec2(form::FIELD_WIDTH, 26.0), field);
    });

    form::row(ui, "Tab Size", |ui| {
        form::number(ui, &mut prefs.tab_size, 1, 16);
        form::help(ui, "How many columns a tab takes.");
    });

    form::row(ui, "End of Line Character", |ui| {
        form::choice(
            ui,
            "prefs-editor-eol",
            &mut prefs.eol,
            &[(Eol::Keep, "Keep the file's own"), (Eol::Lf, "LF"), (Eol::Crlf, "CRLF")],
        );
        form::help(ui, "Line endings of files GitKrill writes, such as a resolved conflict.");
    });

    form::row(ui, "Syntax Highlighting", |ui| {
        form::checkbox(ui, &mut prefs.syntax_highlighting, "");
    });
    form::row(ui, "Show Line Numbers", |ui| {
        form::checkbox(ui, &mut prefs.line_numbers, "");
    });
    form::row(ui, "Word Wrap", |ui| {
        form::checkbox(ui, &mut prefs.word_wrap, "");
        form::help(ui, "Long lines continue on the next row instead of scrolling sideways, in diffs and blame.");
    });

    form::heading(ui, "Diffs");
    form::row(ui, "Layout", |ui| {
        form::choice(
            ui,
            "prefs-editor-diff-layout",
            &mut prefs.diff_layout,
            &[(DiffLayout::Inline, "Inline"), (DiffLayout::Split, "Side by side")],
        );
        form::help(ui, "Side by side puts the old version on the left and the new one on the right.");
    });
    form::row(ui, "Show", |ui| {
        form::choice(
            ui,
            "prefs-editor-diff-context",
            &mut prefs.diff_context,
            &[(DiffContext::Hunks, "The changed hunks"), (DiffContext::File, "The whole file")],
        );
        form::help(ui, "The whole file has every line, with the changes in place; each hunk can still be staged on its own.");
    });
    form::row(ui, "Ignore Whitespace", |ui| {
        form::checkbox(ui, &mut prefs.ignore_whitespace, "Hide changes to whitespace alone (git diff -w)");
        form::help(
            ui,
            "While it is on, hunks and lines cannot be staged or discarded one by one; whole files still can.",
        );
    });
    form::help(ui, "The diff's title bar switches these too, and so does the command palette.");
}

/// The font drop-down: the default font, then the installed ones (only the
/// monospace ones unless the box under it is cleared). Names are shown in
/// the app's own font: drawing each in its face would load every font.
fn font_choice(ui: &mut Ui, prefs: &mut editor::Editor, fonts: Option<&[FontEntry]>) {
    let default = format!("{} (Default)", theme::default_code_font());
    let current = if prefs.font.is_empty() { default.clone() } else { prefs.font.clone() };
    // The chosen font itself shows what it looks like.
    let selected = RichText::new(current).font(theme::code(13.0));
    ComboBox::from_id_salt(Id::new("prefs-editor-font")).selected_text(selected).width(form::FIELD_WIDTH).show_ui(
        ui,
        |ui| {
            ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                if ui.selectable_label(prefs.font.is_empty(), default).clicked() {
                    choose(prefs, None);
                }
                let Some(fonts) = fonts else { return };
                let monospace_only = prefs.monospace_only;
                for font in fonts.iter().filter(|font| font.monospace || !monospace_only) {
                    let chosen = prefs.font.eq_ignore_ascii_case(&font.family);
                    if ui.selectable_label(chosen, &font.family).clicked() && !chosen {
                        choose(prefs, Some(font));
                    }
                }
            });
        },
    );
}

fn choose(prefs: &mut editor::Editor, font: Option<&FontEntry>) {
    match font {
        Some(font) => {
            prefs.font = font.family.clone();
            prefs.font_file = font.file.clone();
            prefs.font_index = font.index;
        }
        None => {
            prefs.font.clear();
            prefs.font_file.clear();
            prefs.font_index = 0;
        }
    }
}
