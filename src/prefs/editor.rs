//! Preferences > Editor: the font and layout of diffs, blame and the
//! conflict editor, and the line endings of files GitKrill writes. Also
//! Preferences > Encoding, the text encoding of one repository's files.
//!
//! Shown by `crate::ui::prefs::editor` and `crate::ui::prefs::encoding`.
//!
//! The views read what they need every frame from [`look`] (one atomic
//! load); the reader threads get the tab width and encodings through
//! `git::text` (see [`apply`]). The chosen font is read from disk on a
//! thread of its own when it changes, and only that one font is loaded.

pub mod fonts;

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use eframe::egui::{self, FontData, FontId};
use serde::{Deserialize, Serialize};

use crate::git::text::{self, Encoding, Eol};
use crate::state::Settings;
use crate::theme;

/// Font sizes the views accept, in points.
pub const MIN_FONT_SIZE: f32 = 8.0;
pub const MAX_FONT_SIZE: f32 = 32.0;
/// Font size of code before anyone changed it.
pub const DEFAULT_FONT_SIZE: f32 = 12.5;

/// Held by `Settings::editor`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Editor {
    /// Family name of the code font; empty for GitKrill's own.
    pub font: String,
    /// Where that font was found, and its face in a collection, so starting
    /// up needs no search of the font folders. Searched again if the file
    /// is gone or holds another font now.
    pub font_file: PathBuf,
    pub font_index: u32,
    /// The font list shows monospace fonts only.
    pub monospace_only: bool,
    pub font_size: f32,
    /// Columns between tab stops.
    pub tab_size: u32,
    /// Line endings of files GitKrill writes (the conflict resolver's result).
    pub eol: Eol,
    pub syntax_highlighting: bool,
    pub line_numbers: bool,
    pub word_wrap: bool,
    /// Diffs: one column of changes, or the two versions side by side.
    pub diff_layout: DiffLayout,
    /// Diffs: the hunks, or the whole file with the changes in place.
    pub diff_context: DiffContext,
    /// Diffs leave out changes of whitespace alone (`git diff -w`).
    pub ignore_whitespace: bool,
    /// The conflict resolver shows the common ancestor between the sides.
    pub conflict_ancestor: bool,
}

/// How a diff is laid out.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DiffLayout {
    /// Removed and added lines one under the other.
    #[default]
    Inline,
    /// The old version on the left, the new one on the right.
    Split,
}

/// How much of the file a diff shows.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DiffContext {
    /// The changes with three lines around each.
    #[default]
    Hunks,
    /// Every line of the file.
    File,
}

impl Editor {
    /// How diffs are worked out on the reader threads.
    pub fn diff_mode(&self) -> crate::git::diff::Mode {
        crate::git::diff::Mode {
            whole_file: self.diff_context == DiffContext::File,
            ignore_whitespace: self.ignore_whitespace,
        }
    }
}

impl Default for Editor {
    fn default() -> Self {
        Editor {
            font: String::new(),
            font_file: PathBuf::new(),
            font_index: 0,
            monospace_only: true,
            font_size: DEFAULT_FONT_SIZE,
            tab_size: text::DEFAULT_TAB_WIDTH as u32,
            eol: Eol::Keep,
            syntax_highlighting: true,
            line_numbers: true,
            word_wrap: false,
            diff_layout: DiffLayout::Inline,
            diff_context: DiffContext::Hunks,
            ignore_whitespace: false,
            conflict_ancestor: false,
        }
    }
}

/// Preferences > Encoding, for one repository; held by `RepoPrefs::encoding`.
#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
#[serde(default)]
pub struct RepoEncoding {
    /// How the repository's files are read and written, unless a byte order
    /// mark says otherwise.
    pub encoding: Encoding,
}

// ---- what the views read ----------------------------------------------------

/// How code looks in the diff, blame and conflict views.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Look {
    pub size: f32,
    pub highlight: bool,
    pub numbers: bool,
    pub wrap: bool,
    /// Diffs side by side (`DiffLayout::Split`).
    pub split: bool,
}

impl Default for Look {
    fn default() -> Self {
        Look { size: DEFAULT_FONT_SIZE, highlight: true, numbers: true, wrap: false, split: false }
    }
}

impl Look {
    fn of(editor: &Editor) -> Self {
        Look {
            size: clamp_size(editor.font_size),
            highlight: editor.syntax_highlighting,
            numbers: editor.line_numbers,
            wrap: editor.word_wrap,
            split: editor.diff_layout == DiffLayout::Split,
        }
    }

    /// The font of the code.
    pub fn font(&self) -> FontId {
        theme::code(self.size)
    }

    /// The font of line numbers and hunk headers, a little smaller.
    pub fn small(&self) -> FontId {
        theme::code(self.size - 1.0)
    }

    /// Height of a row of code: 19 points at the default size, as always.
    pub fn line_height(&self) -> f32 {
        (self.size * 1.52).round()
    }

    fn pack(self) -> u64 {
        u64::from(self.size.to_bits())
            | u64::from(self.highlight) << 32
            | u64::from(self.numbers) << 33
            | u64::from(self.wrap) << 34
            | u64::from(self.split) << 35
    }

    fn unpack(bits: u64) -> Self {
        Look {
            size: f32::from_bits(bits as u32),
            highlight: bits & 1 << 32 != 0,
            numbers: bits & 1 << 33 != 0,
            wrap: bits & 1 << 34 != 0,
            split: bits & 1 << 35 != 0,
        }
    }
}

fn clamp_size(size: f32) -> f32 {
    if size.is_finite() { size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE) } else { DEFAULT_FONT_SIZE }
}

/// 0 stands for "not set yet": the defaults.
static LOOK: AtomicU64 = AtomicU64::new(0);

/// How code looks, as set in Preferences > Editor. One atomic load.
pub fn look() -> Look {
    match LOOK.load(Ordering::Relaxed) {
        0 => Look::default(),
        bits => Look::unpack(bits),
    }
}

// ---- putting it into effect ---------------------------------------------------

/// The font last asked for: (family, file, face).
type FontChoice = (String, PathBuf, u32);

static APPLIED_FONT: Mutex<Option<FontChoice>> = Mutex::new(None);
/// Numbers font loads, so a slow one finishing after a newer one is dropped.
static FONT_LOADS: AtomicU64 = AtomicU64::new(0);
/// Why the chosen font could not be loaded, for the preferences page.
static FONT_ERROR: Mutex<Option<String>> = Mutex::new(None);

/// Why the code font picked could not be used, if it could not.
pub fn font_error() -> Option<String> {
    FONT_ERROR.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Puts these preferences into effect; see [`crate::prefs::apply`].
pub fn apply(ctx: &egui::Context, settings: &Settings) {
    let editor = &settings.editor;
    LOOK.store(Look::of(editor).pack(), Ordering::Relaxed);
    crate::git::diff::set_mode(editor.diff_mode());

    let encodings: Vec<(PathBuf, Encoding)> =
        settings.repos.iter().map(|(dir, prefs)| (dir.clone(), prefs.encoding.encoding)).collect();
    text::set_format(editor.tab_size as usize, &encodings);

    let choice = (editor.font.clone(), editor.font_file.clone(), editor.font_index);
    {
        let mut applied = APPLIED_FONT.lock().unwrap_or_else(|e| e.into_inner());
        // At start the default font is already in place.
        let unchanged = match &*applied {
            Some(last) => *last == choice,
            None => choice.0.is_empty(),
        };
        if unchanged {
            *applied = Some(choice);
            return;
        }
        *applied = Some(choice.clone());
    }
    let load = FONT_LOADS.fetch_add(1, Ordering::Relaxed) + 1;
    let ctx = ctx.clone();
    let spawned = std::thread::Builder::new().name("code-font".into()).spawn(move || {
        let (family, file, index) = choice;
        let result = if family.is_empty() { Ok(None) } else { fonts::load(&family, &file, index).map(Some) };
        // A newer choice was made meanwhile; it will put itself in place.
        if FONT_LOADS.load(Ordering::Relaxed) != load {
            return;
        }
        let (font, error) = match result {
            Ok(font) => (font.map(|data: FontData| (family, data)), None),
            Err(error) => (None, Some(error)),
        };
        *FONT_ERROR.lock().unwrap_or_else(|e| e.into_inner()) = error;
        theme::set_code_font(&ctx, font);
    });
    if let Err(e) = spawned {
        *FONT_ERROR.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!("Could not start loading the font: {e}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_look_survives_its_trip_through_an_atomic() {
        for look in [
            Look::default(),
            Look { size: 9.5, highlight: false, numbers: true, wrap: true, split: false },
            Look { size: 32.0, highlight: true, numbers: false, wrap: false, split: true },
        ] {
            assert_eq!(Look::unpack(look.pack()), look);
            assert_ne!(look.pack(), 0);
        }
        assert_eq!(Look::default().line_height(), 19.0);
        assert_eq!(clamp_size(f32::NAN), DEFAULT_FONT_SIZE);
        assert_eq!(clamp_size(100.0), MAX_FONT_SIZE);
    }

    #[test]
    fn settings_written_before_this_page_read_as_the_defaults() {
        let editor: Editor = serde_json::from_str("{}").unwrap();
        assert_eq!(editor, Editor::default());
        assert!(editor.monospace_only && editor.syntax_highlighting && editor.line_numbers && !editor.word_wrap);
        assert_eq!((editor.tab_size, editor.eol), (4, Eol::Keep));
        assert_eq!((editor.diff_layout, editor.diff_context, editor.ignore_whitespace), (DiffLayout::Inline, DiffContext::Hunks, false));
        assert_eq!(editor.diff_mode(), crate::git::diff::Mode::default());
        let diffs: Editor =
            serde_json::from_str(r#"{"diff_layout": "split", "diff_context": "file", "ignore_whitespace": true}"#).unwrap();
        assert_eq!(diffs.diff_mode(), crate::git::diff::Mode { whole_file: true, ignore_whitespace: true });
        assert!(Look::of(&diffs).split);
        let encoding: RepoEncoding = serde_json::from_str(r#"{"encoding": "windows-1252"}"#).unwrap();
        assert_eq!(encoding.encoding, Encoding::Windows1252);
    }
}
