//! Colours, fonts and the egui style.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Stroke,
    TextStyle, Vec2,
};

pub use egui_phosphor::regular as icons;

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// Every colour the UI paints with. [`DARK`] and [`LIGHT`] are the two sets;
/// [`set_light`] chooses one and the functions below read the chosen set, so
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
    pub border: Color32,
    pub border_light: Color32,

    pub text: Color32,
    pub text_bright: Color32,
    pub text_dim: Color32,
    pub text_faint: Color32,

    pub accent: Color32,
    pub green: Color32,
    pub green_dark: Color32,
    pub red: Color32,
    pub red_dark: Color32,
    pub yellow: Color32,
    pub orange: Color32,
    pub blue: Color32,

    pub diff_add_bg: Color32,
    pub diff_del_bg: Color32,
    pub diff_hunk_bg: Color32,

    /// Branch line colours. Neighbours in this list are deliberately far
    /// apart in hue, because consecutive entries end up side by side in the
    /// graph.
    pub lanes: [Color32; crate::git::graph::COLORS as usize],
}

pub const DARK: Palette = Palette {
    dark: true,
    // Surfaces, darkest to lightest (in the dark set).
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
    border: rgb(0x121317),
    border_light: rgb(0x3a3f4a),

    text: rgb(0xd3d6dc),
    text_bright: rgb(0xf4f5f7),
    text_dim: rgb(0x8b909b),
    text_faint: rgb(0x5d626d),

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

/// The light set. Placeholder: until it is designed it is the dark set.
pub const LIGHT: Palette = Palette { dark: false, ..DARK };

static LIGHT_ON: AtomicBool = AtomicBool::new(false);

/// The set in use.
pub fn palette() -> &'static Palette {
    if LIGHT_ON.load(Ordering::Relaxed) { &LIGHT } else { &DARK }
}

/// Switch sets. The egui style still has to be applied again afterwards.
pub fn set_light(light: bool) {
    LIGHT_ON.store(light, Ordering::Relaxed);
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

accessors! {
    bg_input,
    bg_app,
    bg_tabbar,
    bg_panel,
    bg_header,
    bg_toolbar,
    bg_raised,
    bg_hover,
    bg_selected,
    bg_pressed,
    border,
    border_light,
    text,
    text_bright,
    text_dim,
    text_faint,
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

pub fn install(ctx: &egui::Context) {
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
    ctx.set_theme(egui::ThemePreference::Dark);

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

        let v = &mut style.visuals;
        v.dark_mode = true;
        v.override_text_color = None;
        v.panel_fill = bg_app();
        v.window_fill = bg_toolbar();
        v.window_stroke = Stroke::new(1.0, border_light());
        v.window_corner_radius = CornerRadius::same(6);
        v.menu_corner_radius = CornerRadius::same(6);
        v.extreme_bg_color = bg_input();
        v.text_edit_bg_color = Some(bg_input());
        v.faint_bg_color = bg_panel();
        v.code_bg_color = bg_input();
        v.hyperlink_color = accent();
        v.warn_fg_color = yellow();
        v.error_fg_color = red();
        v.selection.bg_fill = rgb(0x1d5c7d);
        v.selection.stroke = Stroke::new(1.0, text_bright());

        let w = &mut v.widgets;
        w.noninteractive.bg_fill = bg_panel();
        w.noninteractive.weak_bg_fill = bg_panel();
        w.noninteractive.bg_stroke = Stroke::new(1.0, border_light());
        w.noninteractive.fg_stroke = Stroke::new(1.0, text());

        w.inactive.bg_fill = bg_raised();
        w.inactive.weak_bg_fill = bg_raised();
        w.inactive.bg_stroke = Stroke::NONE;
        w.inactive.fg_stroke = Stroke::new(1.0, text());

        w.hovered.bg_fill = rgb(0x3e434f);
        w.hovered.weak_bg_fill = rgb(0x3e434f);
        w.hovered.bg_stroke = Stroke::new(1.0, rgb(0x596070));
        w.hovered.fg_stroke = Stroke::new(1.0, text_bright());

        w.active.bg_fill = rgb(0x474d5b);
        w.active.weak_bg_fill = rgb(0x474d5b);
        w.active.bg_stroke = Stroke::new(1.0, accent());
        w.active.fg_stroke = Stroke::new(1.0, text_bright());

        w.open.bg_fill = bg_raised();
        w.open.weak_bg_fill = bg_raised();
        w.open.bg_stroke = Stroke::new(1.0, border_light());
        w.open.fg_stroke = Stroke::new(1.0, text_bright());

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
    });
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
