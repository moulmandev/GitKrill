//! Colours, fonts and the egui style.

use std::sync::Arc;
#[cfg(not(test))]
use std::sync::atomic::{AtomicBool, Ordering};

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Stroke,
    TextStyle, Vec2, Visuals,
};
use serde::{Deserialize, Serialize};

pub use egui_phosphor::regular as icons;

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// Every colour the UI paints with. [`DARK`] and [`LIGHT`] are the two sets;
/// [`follow`] chooses one and the functions below read the chosen set, so
/// call them each frame rather than keeping their result.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    /// Whether this set is meant for a dark background.
    pub dark: bool,
    // Surfaces, darkest to lightest (in the dark set).
    pub bg_input: Color32,
    pub bg_app: Color32,
    pub bg_tabbar: Color32,
    pub bg_panel: Color32,
    pub bg_header: Color32,
    pub bg_toolbar: Color32,
    pub bg_raised: Color32,
    pub bg_hover: Color32,
    pub bg_selected: Color32,
    pub bg_pressed: Color32,
    /// Cards that float over everything: toasts, drag hints.
    pub bg_float: Color32,
    /// Empty space inside a text pane: the side of a conflict that has no
    /// lines at that point.
    pub bg_void: Color32,
    pub border: Color32,
    pub border_light: Color32,

    pub text: Color32,
    pub text_bright: Color32,
    pub text_dim: Color32,
    pub text_faint: Color32,
    /// Text and icons on solid accent-coloured fills ([`solid`], [`deep`])
    /// and on author initials.
    pub on_accent: Color32,

    pub accent: Color32,
    pub green: Color32,
    /// A faint green surface (the commit button while it is disabled).
    pub green_dark: Color32,
    pub red: Color32,
    /// A faint red surface (a destructive button under the pointer).
    pub red_dark: Color32,
    pub yellow: Color32,
    pub orange: Color32,
    pub blue: Color32,

    pub diff_add_bg: Color32,
    pub diff_del_bg: Color32,
    pub diff_hunk_bg: Color32,

    // egui's own widgets: buttons, checkboxes, text fields, menus.
    /// Outline of a widget at rest; transparent for none.
    pub widget_border: Color32,
    pub widget_hover: Color32,
    pub widget_hover_border: Color32,
    pub widget_active: Color32,
    /// Behind selected text.
    pub text_selection: Color32,
    /// Outline of the focused text field.
    pub focus: Color32,

    /// Translucent: drop shadows of windows, menus and dragged labels.
    pub shadow: Color32,
    /// Translucent: a recessed field drawn over a row (the WIP row's
    /// placeholder message).
    pub well: Color32,
    /// Behind author pictures, which are made for a light page.
    pub picture_bg: Color32,

    /// Branch line colours. Neighbours in this list are deliberately far
    /// apart in hue, because consecutive entries end up side by side in the
    /// graph.
    pub lanes: [Color32; crate::git::graph::COLORS as usize],
}

pub const DARK: Palette = Palette {
    dark: true,
    // Surfaces, darkest to lightest.
    bg_input: rgb(0x14161a),
    bg_app: rgb(0x1b1d23),
    bg_tabbar: rgb(0x1f2127),
    bg_panel: rgb(0x22252c),
    bg_header: rgb(0x262930),
    bg_toolbar: rgb(0x2b2e36),
    bg_raised: rgb(0x333741),
    bg_hover: rgb(0x2a2e37),
    bg_selected: rgb(0x1d5a80),
    bg_pressed: rgb(0x464c5a),
    bg_float: rgb(0x333741),
    bg_void: rgb(0x14161a),
    border: rgb(0x121317),
    border_light: rgb(0x3a3f4a),

    text: rgb(0xd3d6dc),
    text_bright: rgb(0xf4f5f7),
    text_dim: rgb(0x8b909b),
    text_faint: rgb(0x5d626d),
    on_accent: Color32::WHITE,

    accent: rgb(0x15a0bf),
    green: rgb(0x4cb963),
    green_dark: rgb(0x1f3b28),
    red: rgb(0xe5534b),
    red_dark: rgb(0x45201f),
    yellow: rgb(0xe3b341),
    orange: rgb(0xf0883e),
    blue: rgb(0x4d9de0),

    diff_add_bg: rgb(0x1d3324),
    diff_del_bg: rgb(0x3a1f22),
    diff_hunk_bg: rgb(0x242a36),

    widget_border: Color32::TRANSPARENT,
    widget_hover: rgb(0x3e434f),
    widget_hover_border: rgb(0x596070),
    widget_active: rgb(0x474d5b),
    text_selection: rgb(0x1d5c7d),
    focus: rgb(0xf4f5f7),

    shadow: Color32::from_black_alpha(96),
    well: Color32::from_black_alpha(70),
    picture_bg: rgb(0xf4f5f7),

    lanes: [
        rgb(0x18a8c9), // cyan
        rgb(0xe0489b), // pink
        rgb(0xf2a33a), // amber
        rgb(0x6ccb3f), // green
        rgb(0x8a63f5), // violet
        rgb(0xef5b3c), // vermilion
        rgb(0x2fd3a6), // mint
        rgb(0x3d82f7), // blue
        rgb(0xe8d44d), // yellow
        rgb(0xc457e0), // orchid
    ],
};

/// The light set: white work areas, cool grey chrome, and the dark set's
/// hues made deeper so that lines, icons and text keep their contrast on a
/// white page.
pub const LIGHT: Palette = Palette {
    dark: false,
    // Surfaces: the graph, diffs and text fields are white; the chrome
    // around them steps down in light greys.
    bg_input: rgb(0xffffff),
    bg_app: rgb(0xffffff),
    bg_tabbar: rgb(0xe6e8ec),
    bg_panel: rgb(0xf4f5f7),
    bg_header: rgb(0xeceef1),
    bg_toolbar: rgb(0xf8f9fa),
    bg_raised: rgb(0xd9dde4),
    bg_hover: rgb(0xebeef2),
    bg_selected: rgb(0xc6e0f5),
    bg_pressed: rgb(0xd0d5dd),
    bg_float: rgb(0xffffff),
    bg_void: rgb(0xeef0f3),
    border: rgb(0xd3d7de),
    border_light: rgb(0xc8cdd5),

    text: rgb(0x2a2f37),
    text_bright: rgb(0x111418),
    text_dim: rgb(0x5d6470),
    text_faint: rgb(0x828893),
    on_accent: Color32::WHITE,

    accent: rgb(0x077194),
    green: rgb(0x1f7f3a),
    green_dark: rgb(0xe1f0e5),
    red: rgb(0xc8352d),
    red_dark: rgb(0xfbe4e2),
    yellow: rgb(0x9c7200),
    orange: rgb(0xc75f0c),
    blue: rgb(0x2a66c8),

    diff_add_bg: rgb(0xe4f4e8),
    diff_del_bg: rgb(0xfbe9e9),
    diff_hunk_bg: rgb(0xedf2f8),

    widget_border: rgb(0xc4c9d2),
    widget_hover: rgb(0xd8dce3),
    widget_hover_border: rgb(0xa6adb9),
    widget_active: rgb(0xc8cdd6),
    text_selection: rgb(0xb3d7f2),
    focus: rgb(0x077194),

    shadow: Color32::from_black_alpha(34),
    well: Color32::from_black_alpha(14),
    picture_bg: rgb(0xffffff),

    // Same hues and order as the dark set; every one keeps at least 3:1
    // contrast with white.
    lanes: [
        rgb(0x0e93b6), // cyan
        rgb(0xd3337e), // pink
        rgb(0xd7720a), // amber
        rgb(0x3b971f), // green
        rgb(0x6f48e6), // violet
        rgb(0xdd4520), // vermilion
        rgb(0x0e9b79), // mint
        rgb(0x2f6fdf), // blue
        rgb(0xab8400), // yellow
        rgb(0xab3fc4), // orchid
    ],
};

/// The theme the user picked, as saved in the settings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Choice {
    #[default]
    Dark,
    Light,
    /// Whatever the operating system is set to, following it when it changes.
    System,
}

impl Choice {
    pub const ALL: [Choice; 3] = [Choice::Dark, Choice::Light, Choice::System];

    pub fn label(self) -> &'static str {
        match self {
            Choice::Dark => "Dark",
            Choice::Light => "Light",
            Choice::System => "Match system",
        }
    }

    /// Whether this choice means the light set, given what the OS reports.
    pub fn is_light(self, system: Option<egui::Theme>) -> bool {
        match self {
            Choice::Dark => false,
            Choice::Light => true,
            Choice::System => system == Some(egui::Theme::Light),
        }
    }
}

// Which set is in use. Tests get one per thread so that a test looking at the
// light set cannot change the colours another test sees.
#[cfg(not(test))]
static LIGHT_ON: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
thread_local! {
    static LIGHT_ON: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the light set is in use.
pub fn is_light() -> bool {
    #[cfg(not(test))]
    return LIGHT_ON.load(Ordering::Relaxed);
    #[cfg(test)]
    return LIGHT_ON.with(|on| on.get());
}

/// The set in use.
pub fn palette() -> &'static Palette {
    if is_light() { &LIGHT } else { &DARK }
}

/// Switch sets. The egui style still has to be applied again afterwards;
/// [`follow`] does both.
pub fn set_light(light: bool) {
    #[cfg(not(test))]
    LIGHT_ON.store(light, Ordering::Relaxed);
    #[cfg(test)]
    LIGHT_ON.with(|on| on.set(light));
}

/// Puts the theme `choice` stands for into effect if it is not already: the
/// palette, the context's egui style, and the style of `ui`, the frame's root
/// Ui, which took its copy of the style before this was called. Call it at
/// the start of every frame, before anything is laid out; "Match system"
/// then follows the OS as it switches.
pub fn follow(ui: &mut egui::Ui, choice: Choice) {
    let light = choice.is_light(ui.ctx().system_theme());
    if light == is_light() {
        return;
    }
    set_light(light);
    let ctx = ui.ctx().clone();
    apply(&ctx);
    ui.set_style(ctx.global_style());
    ctx.request_repaint();
}

macro_rules! accessors {
    ($($name:ident,)*) => {
        $(
            #[inline]
            pub fn $name() -> Color32 {
                palette().$name
            }
        )*
    };
}

/// Behind text. egui paints its own text fields with it; this is for custom ones.
#[allow(dead_code)]
pub fn bg_input() -> Color32 {
    palette().bg_input
}

accessors! {
    bg_app,
    bg_tabbar,
    bg_panel,
    bg_header,
    bg_toolbar,
    bg_raised,
    bg_hover,
    bg_selected,
    bg_pressed,
    bg_float,
    bg_void,
    border,
    border_light,
    text,
    text_bright,
    text_dim,
    text_faint,
    on_accent,
    accent,
    green,
    green_dark,
    red,
    red_dark,
    yellow,
    orange,
    blue,
    diff_add_bg,
    diff_del_bg,
    diff_hunk_bg,
    shadow,
    well,
    picture_bg,
    text_selection,
}

pub fn lanes() -> [Color32; crate::git::graph::COLORS as usize] {
    palette().lanes
}

/// Colour of the branch line with palette index `index`.
pub fn lane_color(index: u8) -> Color32 {
    let lanes = &palette().lanes;
    lanes[index as usize % lanes.len()]
}

/// Linear blend from `a` to `b`.
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

pub fn with_alpha(c: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), alpha)
}

/// Contrast ratio between two opaque colours, as WCAG defines it: 1 for the
/// same colour, 21 for black on white. Body text wants 4.5 or more.
pub fn contrast(a: Color32, b: Color32) -> f32 {
    let luminance = |c: Color32| {
        let channel = |v: u8| {
            let v = v as f32 / 255.0;
            if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * channel(c.r()) + 0.7152 * channel(c.g()) + 0.0722 * channel(c.b())
    };
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// `color`, darkened just enough for `text` to reach `ratio` contrast on it.
fn deepened(color: Color32, text: Color32, ratio: f32) -> Color32 {
    (0..=20)
        .map(|step| mix(color, Color32::BLACK, step as f32 / 20.0))
        .find(|fill| contrast(*fill, text) >= ratio)
        .unwrap_or(Color32::BLACK)
}

/// A solid fill in `color` for [`on_accent`] text: primary buttons ("Save",
/// "Force push"). The dark theme blends `amount` of `color` into the app
/// background; the light theme uses `color` itself, darkened as far as the
/// text needs.
pub fn solid(color: Color32, amount: f32) -> Color32 {
    let p = palette();
    if p.dark { mix(p.bg_app, color, amount) } else { deepened(color, p.on_accent, 4.5) }
}

/// The checked-out branch's label: a lane colour, deepened so [`on_accent`]
/// text reads on it.
pub fn deep(color: Color32) -> Color32 {
    let p = palette();
    if p.dark { mix(Color32::BLACK, color, 0.82) } else { deepened(color, p.on_accent, 4.0) }
}

pub fn font(size: f32) -> FontId {
    FontId::proportional(size)
}

pub fn bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("bold".into()))
}

pub fn mono(size: f32) -> FontId {
    FontId::monospace(size)
}

fn system_font(names: &[&str]) -> Option<Vec<u8>> {
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    if let Some(windir) = std::env::var_os("WINDIR") {
        dirs.push(std::path::Path::new(&windir).join("Fonts"));
    }
    dirs.push("/System/Library/Fonts".into());
    dirs.push("/usr/share/fonts/truetype/dejavu".into());
    names
        .iter()
        .flat_map(|name| dirs.iter().map(move |dir| dir.join(name)))
        .find_map(|path| std::fs::read(path).ok())
}

/// Fonts and the style of the palette in use. Once, at startup; theme
/// changes go through [`follow`].
pub fn install(ctx: &egui::Context) {
    install_fonts(ctx);
    apply(ctx);
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();

    // Prefer the platform UI font so the app looks native; egui's bundled
    // fonts stay in the list as fallbacks.
    let add = |fonts: &mut FontDefinitions, key: &str, family: FontFamily, files: &[&str]| {
        if let Some(bytes) = system_font(files) {
            fonts.font_data.insert(key.to_owned(), Arc::new(FontData::from_owned(bytes)));
            fonts.families.entry(family).or_default().insert(0, key.to_owned());
            true
        } else {
            false
        }
    };
    add(&mut fonts, "ui", FontFamily::Proportional, &["segoeui.ttf", "SFNS.ttf", "DejaVuSans.ttf"]);
    add(&mut fonts, "code", FontFamily::Monospace, &["CascadiaMono.ttf", "consola.ttf", "Menlo.ttc"]);

    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);

    // "bold" = the semibold face first, then everything proportional has, so
    // icons and fallback glyphs keep working inside bold text.
    let mut bold_family = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    if let Some(bytes) = system_font(&["seguisb.ttf", "segoeuib.ttf", "DejaVuSans-Bold.ttf"]) {
        fonts.font_data.insert("ui-bold".to_owned(), Arc::new(FontData::from_owned(bytes)));
        bold_family.insert(0, "ui-bold".to_owned());
    }
    fonts.families.insert(FontFamily::Name("bold".into()), bold_family);

    // Icons inside monospace text too.
    if let Some(mono) = fonts.families.get_mut(&FontFamily::Monospace) {
        mono.push("phosphor".to_owned());
    }

    ctx.set_fonts(fonts);
}

/// The egui style for the palette in use. Both of egui's styles (dark and
/// light) get it, so whatever egui thinks the theme is, it looks the same.
pub fn apply(ctx: &egui::Context) {
    let p = palette();
    let visuals = visuals(p);
    ctx.set_theme(if p.dark { egui::Theme::Dark } else { egui::Theme::Light });
    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Small, font(11.0)),
            (TextStyle::Body, font(13.0)),
            (TextStyle::Button, font(13.0)),
            (TextStyle::Heading, bold(17.0)),
            (TextStyle::Monospace, mono(12.5)),
        ]
        .into();

        let spacing = &mut style.spacing;
        spacing.item_spacing = Vec2::new(6.0, 4.0);
        spacing.button_padding = Vec2::new(10.0, 4.0);
        spacing.menu_margin = Margin::same(6);
        spacing.window_margin = Margin::same(14);
        spacing.interact_size.y = 22.0;
        spacing.scroll.floating = false;
        spacing.scroll.bar_width = 9.0;
        spacing.scroll.bar_inner_margin = 0.0;
        spacing.scroll.bar_outer_margin = 0.0;
        // The dark set's scroll handles are drawn in the text colour; on a
        // white page that is a black bar, so the light set uses the grey of
        // a resting widget instead.
        spacing.scroll.foreground_color = p.dark;

        style.visuals = visuals.clone();
    });
}

/// A 1-point stroke, or none for a transparent colour.
fn line(color: Color32) -> Stroke {
    if color == Color32::TRANSPARENT { Stroke::NONE } else { Stroke::new(1.0, color) }
}

fn visuals(p: &Palette) -> Visuals {
    // egui's own dark or light visuals underneath, for what is not set here
    // (text rendering, the text cursor).
    let mut v = if p.dark { Visuals::dark() } else { Visuals::light() };
    v.dark_mode = p.dark;
    v.override_text_color = None;
    v.panel_fill = p.bg_app;
    v.window_fill = p.bg_toolbar;
    v.window_stroke = Stroke::new(1.0, p.border_light);
    v.window_corner_radius = CornerRadius::same(6);
    v.menu_corner_radius = CornerRadius::same(6);
    v.window_shadow = egui::Shadow { color: p.shadow, ..Visuals::dark().window_shadow };
    v.popup_shadow = egui::Shadow { color: p.shadow, ..Visuals::dark().popup_shadow };
    v.extreme_bg_color = p.bg_input;
    v.text_edit_bg_color = Some(p.bg_input);
    v.faint_bg_color = p.bg_panel;
    v.code_bg_color = p.bg_input;
    v.hyperlink_color = p.accent;
    v.warn_fg_color = p.yellow;
    v.error_fg_color = p.red;
    v.selection.bg_fill = p.text_selection;
    v.selection.stroke = Stroke::new(1.0, p.focus);

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.bg_panel;
    w.noninteractive.weak_bg_fill = p.bg_panel;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border_light);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);

    w.inactive.bg_fill = p.bg_raised;
    w.inactive.weak_bg_fill = p.bg_raised;
    w.inactive.bg_stroke = line(p.widget_border);
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);

    w.hovered.bg_fill = p.widget_hover;
    w.hovered.weak_bg_fill = p.widget_hover;
    w.hovered.bg_stroke = Stroke::new(1.0, p.widget_hover_border);
    w.hovered.fg_stroke = Stroke::new(1.0, p.text_bright);

    w.active.bg_fill = p.widget_active;
    w.active.weak_bg_fill = p.widget_active;
    w.active.bg_stroke = Stroke::new(1.0, p.accent);
    w.active.fg_stroke = Stroke::new(1.0, p.text_bright);

    w.open.bg_fill = p.bg_raised;
    w.open.weak_bg_fill = p.bg_raised;
    w.open.bg_stroke = Stroke::new(1.0, p.border_light);
    w.open.fg_stroke = Stroke::new(1.0, p.text_bright);

    for visuals in [
        &mut w.noninteractive,
        &mut w.inactive,
        &mut w.hovered,
        &mut w.active,
        &mut w.open,
    ] {
        visuals.corner_radius = CornerRadius::same(4);
        visuals.expansion = 0.0;
    }
    v
}

/// A window icon drawn in code: three graph nodes on a dark tile.
pub fn app_icon() -> egui::IconData {
    const N: usize = 64;
    let mut rgba = vec![0u8; N * N * 4];
    let put = |rgba: &mut [u8], x: usize, y: usize, c: Color32, a: f32| {
        let i = (y * N + x) * 4;
        let blend = |dst: u8, src: u8| (dst as f32 + (src as f32 - dst as f32) * a) as u8;
        rgba[i] = blend(rgba[i], c.r());
        rgba[i + 1] = blend(rgba[i + 1], c.g());
        rgba[i + 2] = blend(rgba[i + 2], c.b());
        rgba[i + 3] = rgba[i + 3].max((a * 255.0) as u8);
    };

    // Distance from a point to a line segment.
    let seg = |p: (f32, f32), a: (f32, f32), b: (f32, f32)| {
        let (px, py) = (p.0 - a.0, p.1 - a.1);
        let (bx, by) = (b.0 - a.0, b.1 - a.1);
        let t = ((px * bx + py * by) / (bx * bx + by * by)).clamp(0.0, 1.0);
        ((px - bx * t).powi(2) + (py - by * t).powi(2)).sqrt()
    };
    let cover = |d: f32| (0.5 - d).clamp(0.0, 1.0);

    let nodes = [((20.0, 14.0), DARK.lanes[0]), ((20.0, 50.0), DARK.lanes[0]), ((44.0, 30.0), DARK.lanes[1])];
    for y in 0..N {
        for x in 0..N {
            let p = (x as f32 + 0.5, y as f32 + 0.5);

            // Rounded tile.
            let r = 13.0;
            let dx = (p.0 - 32.0).abs() - (32.0 - r);
            let dy = (p.1 - 32.0).abs() - (32.0 - r);
            let outside = dx.max(0.0).hypot(dy.max(0.0)) + dx.max(dy).min(0.0) - r;
            put(&mut rgba, x, y, DARK.bg_toolbar, cover(outside + 0.5));

            let trunk = seg(p, (20.0, 14.0), (20.0, 50.0)) - 2.5;
            put(&mut rgba, x, y, DARK.lanes[0], cover(trunk));
            let branch = seg(p, (20.0, 44.0), (44.0, 30.0)) - 2.5;
            put(&mut rgba, x, y, DARK.lanes[1], cover(branch));

            for (center, color) in nodes {
                let d = (p.0 - center.0).hypot(p.1 - center.1);
                put(&mut rgba, x, y, color, cover(d - 7.5));
                put(&mut rgba, x, y, DARK.bg_toolbar, cover(d - 3.5));
            }
        }
    }

    egui::IconData { rgba, width: N as u32, height: N as u32 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dark_style_is_what_it_was_before_themes() {
        // The colours the style used before they moved into the palette.
        let v = visuals(&DARK);
        assert_eq!(v.selection.bg_fill, rgb(0x1d5c7d));
        assert_eq!(v.selection.stroke, Stroke::new(1.0, rgb(0xf4f5f7)));
        assert_eq!(v.widgets.inactive.bg_stroke, Stroke::NONE);
        assert_eq!(v.widgets.hovered.bg_fill, rgb(0x3e434f));
        assert_eq!(v.widgets.hovered.bg_stroke, Stroke::new(1.0, rgb(0x596070)));
        assert_eq!(v.widgets.active.bg_fill, rgb(0x474d5b));
        assert_eq!(v.window_shadow, Visuals::dark().window_shadow);
        assert_eq!(v.popup_shadow, Visuals::dark().popup_shadow);
        assert!(v.dark_mode);
        assert!(!visuals(&LIGHT).dark_mode);
    }

    #[test]
    fn the_light_set_reads_on_white() {
        let p = &LIGHT;
        for (name, text, min) in [
            ("text", p.text, 7.0),
            ("text_bright", p.text_bright, 7.0),
            ("text_dim", p.text_dim, 4.5),
            ("text_faint", p.text_faint, 3.0),
            ("accent", p.accent, 4.5),
            ("green", p.green, 4.0),
            ("red", p.red, 4.5),
            ("blue", p.blue, 4.5),
            ("orange", p.orange, 3.5),
            ("yellow", p.yellow, 3.5),
        ] {
            for (bg_name, bg) in [("app", p.bg_app), ("panel", p.bg_panel), ("header", p.bg_header)] {
                let ratio = contrast(text, bg);
                assert!(ratio >= min, "{name} on {bg_name}: {ratio:.2}");
            }
        }
        // Selected rows keep their text readable and stand out from hovered ones.
        assert!(contrast(p.text_bright, p.bg_selected) >= 7.0);
        assert!(contrast(p.text_dim, p.bg_selected) >= 3.5);
        assert!(contrast(p.bg_selected, p.bg_app) >= 1.25);
        assert!(contrast(p.bg_selected, p.bg_hover) >= 1.15);
        for (i, lane) in p.lanes.iter().enumerate() {
            let ratio = contrast(*lane, p.bg_app);
            assert!(ratio >= 3.0, "lane {i}: {ratio:.2}");
        }
    }

    #[test]
    fn solid_fills_carry_white_text_in_both_sets() {
        for light in [false, true] {
            set_light(light);
            for color in [accent(), green(), red()] {
                let ratio = contrast(solid(color, 0.8), on_accent());
                assert!(ratio >= if light { 4.5 } else { 2.5 }, "{color:?} (light: {light}): {ratio:.2}");
            }
            for color in lanes() {
                let ratio = contrast(deep(color), on_accent());
                assert!(ratio >= if light { 4.0 } else { 2.0 }, "{color:?} (light: {light}): {ratio:.2}");
            }
        }
        set_light(false);
    }

    #[test]
    fn match_system_follows_the_os_and_defaults_to_dark() {
        assert!(!Choice::System.is_light(None));
        assert!(!Choice::System.is_light(Some(egui::Theme::Dark)));
        assert!(Choice::System.is_light(Some(egui::Theme::Light)));
        assert!(Choice::Light.is_light(Some(egui::Theme::Dark)));
        assert!(!Choice::Dark.is_light(Some(egui::Theme::Light)));
        assert_eq!(Choice::default(), Choice::Dark);
    }
}
