//! Colours, fonts and the egui style.

use std::sync::Arc;

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Stroke,
    TextStyle, Vec2,
};

pub use egui_phosphor::regular as icons;

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

// Surfaces, darkest to lightest.
pub const BG_INPUT: Color32 = rgb(0x14161a);
pub const BG_APP: Color32 = rgb(0x1b1d23);
pub const BG_TABBAR: Color32 = rgb(0x1f2127);
pub const BG_PANEL: Color32 = rgb(0x22252c);
pub const BG_HEADER: Color32 = rgb(0x262930);
pub const BG_TOOLBAR: Color32 = rgb(0x2b2e36);
pub const BG_RAISED: Color32 = rgb(0x333741);
pub const BG_HOVER: Color32 = rgb(0x2a2e37);
pub const BG_SELECTED: Color32 = rgb(0x1d5a80);
pub const BG_PRESSED: Color32 = rgb(0x464c5a);
pub const BORDER: Color32 = rgb(0x121317);
pub const BORDER_LIGHT: Color32 = rgb(0x3a3f4a);

pub const TEXT: Color32 = rgb(0xd3d6dc);
pub const TEXT_BRIGHT: Color32 = rgb(0xf4f5f7);
pub const TEXT_DIM: Color32 = rgb(0x8b909b);
pub const TEXT_FAINT: Color32 = rgb(0x5d626d);

pub const ACCENT: Color32 = rgb(0x15a0bf);
pub const GREEN: Color32 = rgb(0x4cb963);
pub const GREEN_DARK: Color32 = rgb(0x1f3b28);
pub const RED: Color32 = rgb(0xe5534b);
pub const RED_DARK: Color32 = rgb(0x45201f);
pub const YELLOW: Color32 = rgb(0xe3b341);
pub const ORANGE: Color32 = rgb(0xf0883e);
pub const BLUE: Color32 = rgb(0x4d9de0);

pub const DIFF_ADD_BG: Color32 = rgb(0x1d3324);
pub const DIFF_DEL_BG: Color32 = rgb(0x3a1f22);
pub const DIFF_HUNK_BG: Color32 = rgb(0x242a36);

/// Branch line colours. Neighbours in this list are deliberately far apart in
/// hue, because consecutive entries end up side by side in the graph.
pub const LANES: [Color32; 10] = [
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
];

const _: () = assert!(LANES.len() == crate::git::graph::COLORS as usize);

/// Colour of the branch line with palette index `index`.
pub fn lane_color(index: u8) -> Color32 {
    LANES[index as usize % LANES.len()]
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
        v.panel_fill = BG_APP;
        v.window_fill = BG_TOOLBAR;
        v.window_stroke = Stroke::new(1.0, BORDER_LIGHT);
        v.window_corner_radius = CornerRadius::same(6);
        v.menu_corner_radius = CornerRadius::same(6);
        v.extreme_bg_color = BG_INPUT;
        v.text_edit_bg_color = Some(BG_INPUT);
        v.faint_bg_color = BG_PANEL;
        v.code_bg_color = BG_INPUT;
        v.hyperlink_color = ACCENT;
        v.warn_fg_color = YELLOW;
        v.error_fg_color = RED;
        v.selection.bg_fill = rgb(0x1d5c7d);
        v.selection.stroke = Stroke::new(1.0, TEXT_BRIGHT);

        let w = &mut v.widgets;
        w.noninteractive.bg_fill = BG_PANEL;
        w.noninteractive.weak_bg_fill = BG_PANEL;
        w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER_LIGHT);
        w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);

        w.inactive.bg_fill = BG_RAISED;
        w.inactive.weak_bg_fill = BG_RAISED;
        w.inactive.bg_stroke = Stroke::NONE;
        w.inactive.fg_stroke = Stroke::new(1.0, TEXT);

        w.hovered.bg_fill = rgb(0x3e434f);
        w.hovered.weak_bg_fill = rgb(0x3e434f);
        w.hovered.bg_stroke = Stroke::new(1.0, rgb(0x596070));
        w.hovered.fg_stroke = Stroke::new(1.0, TEXT_BRIGHT);

        w.active.bg_fill = rgb(0x474d5b);
        w.active.weak_bg_fill = rgb(0x474d5b);
        w.active.bg_stroke = Stroke::new(1.0, ACCENT);
        w.active.fg_stroke = Stroke::new(1.0, TEXT_BRIGHT);

        w.open.bg_fill = BG_RAISED;
        w.open.weak_bg_fill = BG_RAISED;
        w.open.bg_stroke = Stroke::new(1.0, BORDER_LIGHT);
        w.open.fg_stroke = Stroke::new(1.0, TEXT_BRIGHT);

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

    let nodes = [((20.0, 14.0), LANES[0]), ((20.0, 50.0), LANES[0]), ((44.0, 30.0), LANES[1])];
    for y in 0..N {
        for x in 0..N {
            let p = (x as f32 + 0.5, y as f32 + 0.5);

            // Rounded tile.
            let r = 13.0;
            let dx = (p.0 - 32.0).abs() - (32.0 - r);
            let dy = (p.1 - 32.0).abs() - (32.0 - r);
            let outside = dx.max(0.0).hypot(dy.max(0.0)) + dx.max(dy).min(0.0) - r;
            put(&mut rgba, x, y, BG_TOOLBAR, cover(outside + 0.5));

            let trunk = seg(p, (20.0, 14.0), (20.0, 50.0)) - 2.5;
            put(&mut rgba, x, y, LANES[0], cover(trunk));
            let branch = seg(p, (20.0, 44.0), (44.0, 30.0)) - 2.5;
            put(&mut rgba, x, y, LANES[1], cover(branch));

            for (center, color) in nodes {
                let d = (p.0 - center.0).hypot(p.1 - center.1);
                put(&mut rgba, x, y, color, cover(d - 7.5));
                put(&mut rgba, x, y, BG_TOOLBAR, cover(d - 3.5));
            }
        }
    }

    egui::IconData { rgba, width: N as u32, height: N as u32 }
}
