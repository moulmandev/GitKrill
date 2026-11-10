//! Preferences > UI Customization and Notifications: themes, the toolbar,
//! dates, where toasts go, desktop notifications.
//!
//! Shown by `crate::ui::prefs::appearance` and `notifications`.
//!
//! - **Custom themes** are JSON files in [`themes_dir`] (`themes` next to
//!   `settings.json`). `"base": "dark"` or `"light"` picks the built-in set
//!   to start from; any colour of [`Palette`], named by its field
//!   (`"accent": "#e0489b"`, `#rrggbbaa` for translucent ones), replaces the
//!   base's, and `"lanes"` lists the branch colours. [`write_templates`]
//!   writes every colour of both sets as a starting point. The chosen file
//!   is read once, in [`apply`]; `crate::theme` keeps its colours in a
//!   static that the colour functions read.
//! - **Dates** follow a short pattern of tokens ([`DateFormat`]), compiled
//!   once in [`apply`]; `state::format_time` and the history view's dates
//!   ask [`custom_date`] first.
//! - **Desktop notifications** ask the window manager for attention (the
//!   taskbar button flashes, the dock icon bounces) when a toast is raised
//!   while the window is in the background; see `ui::dialogs::toasts`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use chrono::{DateTime, Local};
use eframe::egui::{self, Color32};
use serde::{Deserialize, Serialize};

use crate::state::Settings;
use crate::theme::{self, DARK, LIGHT, Palette};

/// Held by `Settings::appearance`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Appearance {
    /// A theme file of [`themes_dir`], by name (the file name without
    /// `.json`), used in place of `Settings::theme`; `None` for the
    /// built-in themes.
    pub custom_theme: Option<String>,
    /// Where toasts appear.
    pub toast_corner: Corner,
    /// A [`DateFormat`] pattern (`M/D/YYYY`); empty for the built-in
    /// `Nov 10, 2026`.
    pub date_format: String,
    /// Labels under the toolbar's icons. Off makes the toolbar shorter.
    pub toolbar_labels: bool,
    /// "workspace › repository › branch" in the toolbar for a repository of
    /// a workspace.
    pub workspace_breadcrumb: bool,
    /// Ask for attention when an operation ends while the window is in the
    /// background.
    pub desktop_notifications: bool,
}

impl Default for Appearance {
    fn default() -> Self {
        Appearance {
            custom_theme: None,
            toast_corner: Corner::default(),
            date_format: String::new(),
            toolbar_labels: true,
            workspace_breadcrumb: true,
            desktop_notifications: false,
        }
    }
}

/// A corner of the window, for toasts.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Corner {
    #[default]
    BottomLeft,
    BottomRight,
    TopLeft,
    TopRight,
}

impl Corner {
    pub const ALL: [Corner; 4] = [Corner::BottomLeft, Corner::BottomRight, Corner::TopLeft, Corner::TopRight];

    pub fn label(self) -> &'static str {
        match self {
            Corner::BottomLeft => "Bottom Left",
            Corner::BottomRight => "Bottom Right",
            Corner::TopLeft => "Top Left",
            Corner::TopRight => "Top Right",
        }
    }
}

/// What [`apply`] put into effect last, so that it does nothing when its
/// part of the settings did not change.
struct Applied {
    theme: Option<String>,
    /// The custom set in use, to tell whether a file read again changed.
    palette: Option<Palette>,
    date_format: String,
}

static APPLIED: Mutex<Option<Applied>> = Mutex::new(None);

/// Puts these preferences into effect; see [`crate::prefs::apply`]. Reads
/// the chosen theme file when the choice changed (once, a few kilobytes),
/// and compiles the date format when it changed.
pub fn apply(ctx: &egui::Context, settings: &Settings) {
    let prefs = &settings.appearance;
    let mut applied = APPLIED.lock().unwrap_or_else(PoisonError::into_inner);
    let first = applied.is_none();
    let last = applied.get_or_insert_with(|| Applied { theme: None, palette: None, date_format: String::new() });

    if first || last.theme != prefs.custom_theme {
        last.theme = prefs.custom_theme.clone();
        // A file that went missing or does not read leaves the built-in
        // theme in use; the page says why.
        last.palette = prefs
            .custom_theme
            .as_deref()
            .and_then(|name| themes_dir().map(|dir| theme_path(&dir, name)))
            .and_then(|path| fs::read_to_string(path).ok())
            .and_then(|text| parse_theme(&text).ok())
            .map(|(palette, _)| palette);
        theme::set_custom(last.palette);
        ctx.request_repaint();
    }

    if first || last.date_format != prefs.date_format {
        last.date_format = prefs.date_format.clone();
        set_date_format(DateFormat::compile(&prefs.date_format));
        ctx.request_repaint();
    }
}

/// The preferences page read `name` again and found `palette`: use it if
/// `name` is the theme in use and its colours changed since it was applied,
/// so an edited file shows without restarting.
pub fn refresh_theme(ctx: &egui::Context, name: &str, palette: &Palette) {
    let mut applied = APPLIED.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(last) = applied.as_mut()
        && last.theme.as_deref() == Some(name)
        && last.palette.as_ref() != Some(palette)
    {
        last.palette = Some(*palette);
        theme::set_custom(last.palette);
        ctx.request_repaint();
    }
}

// ---- custom themes ----------------------------------------------------------

/// The folder custom themes are read from: `themes` next to `settings.json`
/// (the same folder as `state::Settings::file`, so `GITKRILL_CONFIG_DIR`
/// moves it too).
pub fn themes_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("GITKRILL_CONFIG_DIR") {
        return Some(PathBuf::from(dir).join("themes"));
    }
    Some(dirs::config_dir()?.join("GitKrill").join("themes"))
}

const TEMPLATE: &str = ".template.json";

/// The file of the theme called `name`.
pub fn theme_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.json"))
}

/// A theme file, as read.
pub struct ThemeFile {
    /// The file name without `.json`.
    pub name: String,
    /// Its colours, or why it could not be used.
    pub palette: Result<Palette, String>,
    /// Keys and values that were ignored, and colours that read poorly.
    pub warnings: Vec<String>,
}

/// Every theme in `dir`, by name ignoring case. Templates are left out:
/// they are copied, not used.
pub fn scan(dir: &Path) -> Vec<ThemeFile> {
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    let mut themes: Vec<ThemeFile> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| {
            let file_name = entry.file_name().to_string_lossy().into_owned();
            if file_name.to_ascii_lowercase().ends_with(TEMPLATE) {
                return None;
            }
            let name = file_name.strip_suffix(".json")?.to_owned();
            let (palette, warnings) = match fs::read_to_string(entry.path()) {
                Ok(text) => match parse_theme(&text) {
                    Ok((palette, warnings)) => (Ok(palette), warnings),
                    Err(error) => (Err(error), Vec::new()),
                },
                Err(error) => (Err(format!("Could not read the file: {error}")), Vec::new()),
            };
            Some(ThemeFile { name, palette, warnings })
        })
        .collect();
    themes.sort_by_key(|theme| theme.name.to_lowercase());
    themes
}

/// Writes `dark.template.json` and `light.template.json` into `dir` (made
/// if missing), replacing earlier copies.
pub fn write_templates(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    fs::write(dir.join(format!("dark{TEMPLATE}")), template(&DARK))?;
    fs::write(dir.join(format!("light{TEMPLATE}")), template(&LIGHT))
}

/// Every colour of `base` as a theme file.
pub fn template(base: &Palette) -> String {
    let mut copy = *base;
    let mut out = format!("{{\n  \"base\": \"{}\",\n", if base.dark { "dark" } else { "light" });
    for (name, color) in colours(&mut copy) {
        out.push_str(&format!("  \"{name}\": \"{}\",\n", hex(*color)));
    }
    let lanes: Vec<String> = base.lanes.iter().map(|lane| format!("\"{}\"", hex(*lane))).collect();
    out.push_str(&format!("  \"lanes\": [{}]\n}}\n", lanes.join(", ")));
    out
}

/// A theme file's colours. `Err` when it is not a JSON object at all;
/// otherwise what could not be used is left as the base has it, with a
/// warning, and colours that read poorly are warned about too.
pub fn parse_theme(text: &str) -> Result<(Palette, Vec<String>), String> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|error| format!("Not valid JSON: {error}"))?;
    let object = value.as_object().ok_or("Not a JSON object: a theme is { \"base\": \"dark\", ... }.")?;
    let mut warnings = Vec::new();
    let mut palette = match object.get("base").map(|base| base.as_str()) {
        Some(Some("dark")) => DARK,
        Some(Some("light")) => LIGHT,
        None => {
            warnings.push("No \"base\": starting from the dark theme.".to_owned());
            DARK
        }
        Some(_) => {
            warnings.push("\"base\" is \"dark\" or \"light\": starting from the dark theme.".to_owned());
            DARK
        }
    };
    let not_a_colour = |key: &str, value: &serde_json::Value| format!("\"{key}\": {value} is not a colour (#rrggbb or #rrggbbaa); ignored.");
    for (key, value) in object {
        match key.as_str() {
            "base" => {}
            "lanes" => match value.as_array() {
                Some(list) => {
                    let count = palette.lanes.len();
                    for (i, item) in list.iter().enumerate().take(count) {
                        match item.as_str().and_then(parse_color) {
                            Some(color) => palette.lanes[i] = color,
                            None => warnings.push(not_a_colour(&format!("lanes[{i}]"), item)),
                        }
                    }
                    if list.len() > count {
                        warnings.push(format!("\"lanes\" has {} colours; the first {count} are used.", list.len()));
                    }
                }
                None => warnings.push("\"lanes\" is a list of colours; ignored.".to_owned()),
            },
            _ => match colours(&mut palette).into_iter().find(|(name, _)| name == key) {
                Some((_, slot)) => match value.as_str().and_then(parse_color) {
                    Some(color) => *slot = color,
                    None => warnings.push(not_a_colour(key, value)),
                },
                None => warnings.push(format!("Unknown key \"{key}\"; ignored.")),
            },
        }
    }
    warnings.extend(readability(&palette));
    Ok((palette, warnings))
}

/// `#rrggbb` or `#rrggbbaa`.
pub fn parse_color(text: &str) -> Option<Color32> {
    let digits = text.trim().strip_prefix('#')?;
    if !matches!(digits.len(), 6 | 8) || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&digits[i..i + 2], 16).ok();
    let alpha = if digits.len() == 8 { byte(6)? } else { 255 };
    Some(Color32::from_rgba_unmultiplied(byte(0)?, byte(2)?, byte(4)?, alpha))
}

/// `#rrggbb`, or `#rrggbbaa` for a translucent colour.
pub fn hex(color: Color32) -> String {
    let [r, g, b, a] = color.to_srgba_unmultiplied();
    if a == 255 { format!("#{r:02x}{g:02x}{b:02x}") } else { format!("#{r:02x}{g:02x}{b:02x}{a:02x}") }
}

/// Every colour of a palette but the branch lines, by the name a theme file
/// gives it.
fn colours(p: &mut Palette) -> Vec<(&'static str, &mut Color32)> {
    vec![
        ("bg_input", &mut p.bg_input),
        ("bg_app", &mut p.bg_app),
        ("bg_tabbar", &mut p.bg_tabbar),
        ("bg_panel", &mut p.bg_panel),
        ("bg_header", &mut p.bg_header),
        ("bg_toolbar", &mut p.bg_toolbar),
        ("bg_raised", &mut p.bg_raised),
        ("bg_hover", &mut p.bg_hover),
        ("bg_selected", &mut p.bg_selected),
        ("bg_pressed", &mut p.bg_pressed),
        ("bg_float", &mut p.bg_float),
        ("bg_void", &mut p.bg_void),
        ("border", &mut p.border),
        ("border_light", &mut p.border_light),
        ("text", &mut p.text),
        ("text_bright", &mut p.text_bright),
        ("text_dim", &mut p.text_dim),
        ("text_faint", &mut p.text_faint),
        ("on_accent", &mut p.on_accent),
        ("accent", &mut p.accent),
        ("green", &mut p.green),
        ("green_dark", &mut p.green_dark),
        ("red", &mut p.red),
        ("red_dark", &mut p.red_dark),
        ("yellow", &mut p.yellow),
        ("orange", &mut p.orange),
        ("blue", &mut p.blue),
        ("diff_add_bg", &mut p.diff_add_bg),
        ("diff_del_bg", &mut p.diff_del_bg),
        ("diff_hunk_bg", &mut p.diff_hunk_bg),
        ("widget_border", &mut p.widget_border),
        ("widget_hover", &mut p.widget_hover),
        ("widget_hover_border", &mut p.widget_hover_border),
        ("widget_active", &mut p.widget_active),
        ("text_selection", &mut p.text_selection),
        ("focus", &mut p.focus),
        ("shadow", &mut p.shadow),
        ("well", &mut p.well),
        ("picture_bg", &mut p.picture_bg),
    ]
}

/// Pairs of colours that hold text, and how much contrast each needs to be
/// read comfortably (the same bars the built-in sets are tested against).
fn readability(p: &Palette) -> Vec<String> {
    [
        ("text", p.text, "bg_app", p.bg_app, 4.5),
        ("text", p.text, "bg_panel", p.bg_panel, 4.5),
        ("text", p.text, "bg_toolbar", p.bg_toolbar, 4.5),
        ("text", p.text, "bg_float", p.bg_float, 4.5),
        ("text_bright", p.text_bright, "bg_selected", p.bg_selected, 4.5),
        ("text_dim", p.text_dim, "bg_app", p.bg_app, 3.0),
        ("text_dim", p.text_dim, "bg_panel", p.bg_panel, 3.0),
        ("accent", p.accent, "bg_app", p.bg_app, 3.0),
    ]
    .into_iter()
    .filter_map(|(text, fg, surface, bg, wanted)| {
        let ratio = theme::contrast(fg, bg);
        (ratio < wanted).then(|| format!("\"{text}\" on \"{surface}\" is hard to read ({ratio:.1}:1; {wanted}:1 or more reads well)."))
    })
    .collect()
}

// ---- dates ------------------------------------------------------------------

/// What a token prints, whether that is part of the time of day, and its
/// tokens: what is typed and what chrono prints for it. Longer tokens come
/// before the shorter ones they start with, so the first match is the right
/// one; no two units share a letter.
pub type Unit = (&'static str, bool, &'static [(&'static str, &'static str)]);

/// The tokens of a date pattern, by unit.
pub const TOKENS: &[Unit] = &[
    ("Year", false, &[("YYYY", "%Y"), ("YY", "%y")]),
    ("Month", false, &[("MMMM", "%B"), ("MMM", "%b"), ("MM", "%m"), ("M", "%-m")]),
    ("Day", false, &[("DD", "%d"), ("D", "%-d")]),
    ("Weekday", false, &[("dddd", "%A"), ("ddd", "%a")]),
    ("Hour", true, &[("HH", "%H"), ("H", "%-H"), ("hh", "%I"), ("h", "%-I")]),
    ("Minute", true, &[("mm", "%M"), ("m", "%-M")]),
    ("Second", true, &[("ss", "%S"), ("s", "%-S")]),
    ("AM / PM", true, &[("A", "%p"), ("a", "%P")]),
];

/// A date pattern such as `M/D/YYYY` or `DD.MM.YYYY HH:mm`, compiled into
/// chrono's format. Letters that are not a token, and anything in
/// `[brackets]`, are printed as they are.
#[derive(Clone, Debug, PartialEq)]
pub struct DateFormat {
    /// The pattern as chrono writes it.
    date: String,
    /// The same with the time of day added (`HH:mm`), unless the pattern
    /// prints one itself: for the places that show a time.
    date_time: String,
    /// Whether the pattern prints any part of a date or time.
    pub has_tokens: bool,
}

impl DateFormat {
    /// `None` for an empty pattern: the built-in format.
    pub fn compile(pattern: &str) -> Option<DateFormat> {
        if pattern.trim().is_empty() {
            return None;
        }
        let literal = |out: &mut String, text: &str| out.push_str(&text.replace('%', "%%"));
        let (mut date, mut has_time, mut has_tokens) = (String::new(), false, false);
        let mut rest = pattern;
        while let Some(c) = rest.chars().next() {
            if c == '[' {
                let inside = &rest[1..];
                let (text, after) = inside.split_once(']').unwrap_or((inside, ""));
                literal(&mut date, text);
                rest = after;
            } else if let Some((time, token, spec)) = TOKENS
                .iter()
                .flat_map(|(_, time, tokens)| tokens.iter().map(move |(token, spec)| (*time, token, spec)))
                .find(|(_, token, _)| rest.starts_with(**token))
            {
                date.push_str(spec);
                has_tokens = true;
                has_time |= time;
                rest = &rest[token.len()..];
            } else {
                literal(&mut date, &rest[..c.len_utf8()]);
                rest = &rest[c.len_utf8()..];
            }
        }
        let date_time = if has_time { date.clone() } else { format!("{date} %H:%M") };
        Some(DateFormat { date, date_time, has_tokens })
    }

    /// `time` in this format; with the time of day when `with_time`.
    pub fn format(&self, time: &DateTime<Local>, with_time: bool) -> String {
        time.format(if with_time { &self.date_time } else { &self.date }).to_string()
    }
}

// The format in use. Tests get one per thread, like the theme, so a test of
// a custom format does not change the dates another test sees.
#[cfg(not(test))]
static DATE_FORMAT: std::sync::RwLock<Option<DateFormat>> = std::sync::RwLock::new(None);
#[cfg(not(test))]
static CUSTOM_DATE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
#[cfg(test)]
thread_local! {
    static DATE_FORMAT: std::cell::RefCell<Option<DateFormat>> = const { std::cell::RefCell::new(None) };
}

fn set_date_format(format: Option<DateFormat>) {
    #[cfg(not(test))]
    {
        CUSTOM_DATE.store(format.is_some(), std::sync::atomic::Ordering::Relaxed);
        *DATE_FORMAT.write().unwrap_or_else(PoisonError::into_inner) = format;
    }
    #[cfg(test)]
    DATE_FORMAT.with(|current| *current.borrow_mut() = format);
}

/// `time` in the user's date format, with the time of day when `with_time`;
/// `None` while the built-in format is in use (one atomic load).
pub fn custom_date(time: &DateTime<Local>, with_time: bool) -> Option<String> {
    #[cfg(not(test))]
    {
        if !CUSTOM_DATE.load(std::sync::atomic::Ordering::Relaxed) {
            return None;
        }
        let format = DATE_FORMAT.read().unwrap_or_else(PoisonError::into_inner);
        format.as_ref().map(|format| format.format(time, with_time))
    }
    #[cfg(test)]
    DATE_FORMAT.with(|format| format.borrow().as_ref().map(|format| format.format(time, with_time)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, mo, d, h, mi, 7).single().unwrap()
    }

    #[test]
    fn date_patterns_print_their_tokens() {
        let time = at(2026, 3, 5, 14, 9);
        let cases = [
            ("M/D/YYYY", "3/5/2026", "3/5/2026 14:09"),
            ("DD.MM.YYYY", "05.03.2026", "05.03.2026 14:09"),
            ("YYYY-MM-DD HH:mm", "2026-03-05 14:09", "2026-03-05 14:09"),
            ("ddd D MMM YY, h:mm A", "Thu 5 Mar 26, 2:09 PM", "Thu 5 Mar 26, 2:09 PM"),
            ("dddd D MMMM", "Thursday 5 March", "Thursday 5 March 14:09"),
            ("H:m:s a", "14:9:7 pm", "14:9:7 pm"),
            ("hh:ss", "02:07", "02:07"),
            // Brackets keep letters that would be tokens; % is not chrono's.
            ("[Day] D [of] M, 100%", "Day 5 of 3, 100%", "Day 5 of 3, 100% 14:09"),
            ("[unclosed", "unclosed", "unclosed 14:09"),
            ("é YYYY", "é 2026", "é 2026 14:09"),
        ];
        for (pattern, date, date_time) in cases {
            let format = DateFormat::compile(pattern).unwrap();
            assert_eq!(format.format(&time, false), date, "{pattern}");
            assert_eq!(format.format(&time, true), date_time, "{pattern}");
        }
        assert!(DateFormat::compile("  ").is_none());
        assert!(!DateFormat::compile("[YYYY] [at] -").unwrap().has_tokens);
        // Letters of a token are one even inside a word: "at" prints am / pm.
        assert!(DateFormat::compile("[YYYY] at").unwrap().has_tokens);
    }

    #[test]
    fn every_token_prints_without_panicking() {
        let time = at(2026, 11, 10, 9, 5);
        for token in TOKENS.iter().flat_map(|(_, _, tokens)| tokens.iter().map(|(token, _)| token)) {
            let printed = DateFormat::compile(token).unwrap().format(&time, false);
            assert!(!printed.is_empty() && printed != *token, "{token}: {printed}");
        }
    }

    #[test]
    fn format_time_and_short_dates_follow_the_custom_format() {
        let secs = at(2026, 3, 5, 14, 9).timestamp();
        assert_eq!(crate::state::format_time(secs), "Mar 5, 2026 14:09");
        set_date_format(DateFormat::compile("DD/MM/YYYY"));
        assert_eq!(crate::state::format_time(secs), "05/03/2026 14:09");
        assert_eq!(custom_date(&at(2026, 3, 5, 14, 9), false).as_deref(), Some("05/03/2026"));
        set_date_format(None);
        assert_eq!(custom_date(&at(2026, 3, 5, 14, 9), false), None);
    }

    #[test]
    fn colours_parse_and_print_with_and_without_alpha() {
        assert_eq!(parse_color("#18a8c9"), Some(Color32::from_rgb(0x18, 0xa8, 0xc9)));
        assert_eq!(parse_color(" #00000060 "), Some(Color32::from_black_alpha(0x60)));
        assert_eq!(parse_color("#ABCDEF"), Some(Color32::from_rgb(0xab, 0xcd, 0xef)));
        for bad in ["18a8c9", "#18a8c", "#18a8c9f", "#gg0000", "", "#"] {
            assert_eq!(parse_color(bad), None, "{bad}");
        }
        assert_eq!(hex(Color32::from_rgb(1, 2, 255)), "#0102ff");
        assert_eq!(hex(Color32::from_black_alpha(96)), "#00000060");
        assert_eq!(hex(Color32::TRANSPARENT), "#00000000");
    }

    #[test]
    fn templates_hold_every_colour_and_read_back_as_their_set() {
        // Every field of the palette is in the table: compare with the
        // names `Debug` prints, so a colour added later is not forgotten.
        // (`Color32` prints as `#14_16_1A_FF`, one field per line.)
        let debug = format!("{DARK:#?}");
        let fields: Vec<&str> = debug
            .lines()
            .filter_map(|line| line.trim().split_once(": #").map(|(name, _)| name))
            .collect();
        let mut copy = DARK;
        let named: Vec<&str> = colours(&mut copy).into_iter().map(|(name, _)| name).collect();
        assert_eq!(fields, named);

        for base in [DARK, LIGHT] {
            let (palette, warnings) = parse_theme(&template(&base)).unwrap();
            assert_eq!(palette, base);
            assert!(warnings.is_empty(), "{warnings:?}");
        }
    }

    #[test]
    fn a_theme_overrides_its_base_and_warns_about_what_it_ignores() {
        let text = r##"{
            "base": "light",
            "accent": "#7a00cc",
            "lanes": ["#112233", 5],
            "bg_app": "blue",
            "accnet": "#000000"
        }"##;
        let (palette, warnings) = parse_theme(text).unwrap();
        assert!(!palette.dark);
        assert_eq!(palette.accent, Color32::from_rgb(0x7a, 0x00, 0xcc));
        assert_eq!(palette.lanes[0], Color32::from_rgb(0x11, 0x22, 0x33));
        assert_eq!(palette.lanes[1], LIGHT.lanes[1]);
        assert_eq!(palette.bg_app, LIGHT.bg_app);
        assert_eq!(palette.text, LIGHT.text);
        let joined = warnings.join("\n");
        assert!(joined.contains("lanes[1]"), "{joined}");
        assert!(joined.contains("\"bg_app\": \"blue\""), "{joined}");
        assert!(joined.contains("Unknown key \"accnet\""), "{joined}");
        assert_eq!(warnings.len(), 3, "{joined}");

        let (palette, warnings) = parse_theme(r##"{"text": "#22252c"}"##).unwrap();
        assert!(palette.dark);
        assert!(warnings[0].contains("No \"base\""), "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("\"text\" on \"bg_app\" is hard to read")), "{warnings:?}");

        assert!(parse_theme("[1, 2]").is_err());
        assert!(parse_theme("{ nope").is_err());
    }

    #[test]
    fn scanning_lists_themes_and_leaves_templates_out() {
        let dir = std::env::temp_dir().join(format!("gitkrill-themes-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        write_templates(&dir).unwrap();
        fs::write(dir.join("Ocean.json"), r##"{"base": "dark", "accent": "#00ffaa"}"##).unwrap();
        fs::write(dir.join("broken.json"), "{").unwrap();
        fs::write(dir.join("notes.txt"), "not a theme").unwrap();
        let themes = scan(&dir);
        let names: Vec<&str> = themes.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["broken", "Ocean"]);
        assert!(themes[0].palette.is_err());
        assert_eq!(themes[1].palette.as_ref().unwrap().accent, Color32::from_rgb(0, 0xff, 0xaa));
        assert!(fs::read_to_string(dir.join("light.template.json")).unwrap().contains("\"base\": \"light\""));
        fs::remove_dir_all(&dir).unwrap();
        assert!(scan(&dir).is_empty());
    }

    #[test]
    fn older_settings_keep_loading() {
        let settings: Settings = serde_json::from_str(r#"{"theme": "Dark", "fetch_avatars": false}"#).unwrap();
        assert_eq!(settings.theme, theme::Choice::Dark);
        assert_eq!(settings.appearance, Appearance::default());
        assert!(settings.appearance.toolbar_labels && settings.appearance.workspace_breadcrumb);
        let settings: Settings = serde_json::from_str(r#"{"theme": "System", "appearance": {"toast_corner": "TopRight"}}"#).unwrap();
        assert_eq!(settings.theme, theme::Choice::System);
        assert_eq!(settings.appearance.toast_corner, Corner::TopRight);
        assert!(settings.appearance.toolbar_labels);
    }
}
