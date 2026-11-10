//! Farben, Schriften und Grundeinstellungen der Oberfläche:
//! fast schwarzer Hintergrund mit feinem Raster, Festbreitenschrift, orange Akzentfarbe.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, TextStyle,
};

// Flächen
pub const BG: Color32 = Color32::from_rgb(0x0B, 0x0B, 0x0C);
pub const SURFACE: Color32 = Color32::from_rgb(0x11, 0x11, 0x13);
pub const SURFACE_HI: Color32 = Color32::from_rgb(0x1A, 0x1A, 0x1D);
pub const SURFACE_SEL: Color32 = Color32::from_rgb(0x24, 0x24, 0x28);
pub const GRID: Color32 = Color32::from_rgb(0x15, 0x15, 0x17);

// Linien
pub const BORDER: Color32 = Color32::from_rgb(0x27, 0x27, 0x2B);
pub const BORDER_HI: Color32 = Color32::from_rgb(0x40, 0x40, 0x46);
pub const BRACKET: Color32 = Color32::from_rgb(0x6A, 0x6A, 0x72);

// Akzent und Signalfarben
pub const ACCENT: Color32 = Color32::from_rgb(0xF2, 0x6A, 0x45);
pub const ACCENT_HI: Color32 = Color32::from_rgb(0xFF, 0x86, 0x62);
pub const ACCENT_LO: Color32 = Color32::from_rgb(0xD2, 0x55, 0x33);
pub const ACCENT_BG: Color32 = Color32::from_rgb(0x2B, 0x17, 0x10);
pub const ON_ACCENT: Color32 = Color32::from_rgb(0x14, 0x0B, 0x07);
pub const GREEN: Color32 = Color32::from_rgb(0x3E, 0xCF, 0x8E);
pub const RED: Color32 = Color32::from_rgb(0xF0, 0x5B, 0x56);
pub const YELLOW: Color32 = Color32::from_rgb(0xE9, 0xB4, 0x3A);

// Text
pub const TEXT_BRIGHT: Color32 = Color32::from_rgb(0xF4, 0xF4, 0xF5);
pub const TEXT: Color32 = Color32::from_rgb(0xCF, 0xCF, 0xD4);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(0x8F, 0x8F, 0x98);
pub const TEXT_FAINT: Color32 = Color32::from_rgb(0x5F, 0x5F, 0x68);

const FAMILY_BOLD: &str = "bold";
const FAMILY_ICONS: &str = "icons";

static HAS_ICON_FONT: AtomicBool = AtomicBool::new(false);

pub fn regular(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

pub fn bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(FAMILY_BOLD.into()))
}

pub fn icons(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(FAMILY_ICONS.into()))
}

/// Ob die Windows-Symbolschrift gefunden wurde (sonst werden Ersatzzeichen verwendet).
pub fn has_icon_font() -> bool {
    HAS_ICON_FONT.load(Ordering::Relaxed)
}

/// Mischt zwei Farben (t = 0 → `a`, t = 1 → `b`).
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let channel = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_premultiplied(
        channel(a.r(), b.r()),
        channel(a.g(), b.g()),
        channel(a.b(), b.b()),
        channel(a.a(), b.a()),
    )
}

/// Weiches Auslaufen einer Bewegung.
pub fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

fn windows_font(names: &[&str]) -> Option<Vec<u8>> {
    let dir = PathBuf::from(std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into())).join("Fonts");
    names.iter().find_map(|name| std::fs::read(dir.join(name)).ok())
}

/// Richtet Schriften, Farben und Abstände ein. Einmal beim Start aufrufen.
pub fn install(ctx: &egui::Context) {
    // Eingebettete Festbreitenschrift JetBrains Mono (normal und fett, SIL Open Font License,
    // siehe assets/fonts/OFL.txt); dahinter die mit egui gelieferten Schriften als Rückfall
    // für Zeichen, die JetBrains Mono nicht enthält.
    let mut fonts = FontDefinitions::default();
    let mut fallback = fonts.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();
    for name in fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default() {
        if !fallback.contains(&name) {
            fallback.push(name);
        }
    }
    let with_fallback = |first: &str| {
        let mut list = vec![first.to_owned()];
        list.extend(fallback.iter().cloned());
        list
    };
    fonts.font_data.insert(
        "mono".to_owned(),
        Arc::new(FontData::from_static(include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf"))),
    );
    fonts.font_data.insert(
        "mono-bold".to_owned(),
        Arc::new(FontData::from_static(include_bytes!("../assets/fonts/JetBrainsMono-Bold.ttf"))),
    );
    fonts.families.insert(FontFamily::Proportional, with_fallback("mono"));
    fonts.families.insert(FontFamily::Monospace, with_fallback("mono"));
    fonts.families.insert(FontFamily::Name(FAMILY_BOLD.into()), with_fallback("mono-bold"));

    // Symbole kommen aus der Symbolschrift von Windows (Segoe Fluent Icons bzw. MDL2).
    let mut icon_list = fallback.clone();
    let icons = match windows_font(&["SegoeIcons.ttf", "segmdl2.ttf"]) {
        Some(bytes) => {
            fonts.font_data.insert("segoe-icons".to_owned(), Arc::new(FontData::from_owned(bytes)));
            icon_list.insert(0, "segoe-icons".to_owned());
            true
        }
        None => false,
    };
    fonts.families.insert(FontFamily::Name(FAMILY_ICONS.into()), icon_list);
    HAS_ICON_FONT.store(icons, Ordering::Relaxed);
    ctx.set_fonts(fonts);

    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Small, regular(12.0)),
            (TextStyle::Body, regular(14.0)),
            (TextStyle::Button, regular(14.0)),
            (TextStyle::Heading, bold(18.0)),
            (TextStyle::Monospace, regular(13.0)),
        ]
        .into();
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(12.0, 7.0);
        style.spacing.interact_size.y = 30.0;
        style.spacing.scroll = egui::style::ScrollStyle::thin();
        style.interaction.selectable_labels = false;
        style.animation_time = 0.12;

        let v = &mut style.visuals;
        v.dark_mode = true;
        v.override_text_color = Some(TEXT);
        v.panel_fill = BG;
        v.window_fill = SURFACE;
        v.window_stroke = Stroke::new(1.0, BORDER);
        v.window_corner_radius = CornerRadius::same(4);
        v.menu_corner_radius = CornerRadius::same(4);
        v.extreme_bg_color = BG;
        v.text_edit_bg_color = Some(BG);
        v.faint_bg_color = SURFACE;
        v.hyperlink_color = ACCENT;
        v.warn_fg_color = YELLOW;
        v.error_fg_color = RED;
        v.selection.bg_fill = mix(BG, ACCENT, 0.45);
        v.selection.stroke = Stroke::new(1.0, TEXT_BRIGHT);

        let radius = CornerRadius::same(4);
        let w = &mut v.widgets;
        w.noninteractive.bg_fill = SURFACE;
        w.noninteractive.weak_bg_fill = SURFACE;
        w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
        w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        w.noninteractive.corner_radius = radius;
        for (state, fill) in [
            (&mut w.inactive, SURFACE_HI),
            (&mut w.hovered, SURFACE_SEL),
            (&mut w.active, SURFACE_SEL),
            (&mut w.open, SURFACE_HI),
        ] {
            state.bg_fill = fill;
            state.weak_bg_fill = fill;
            state.bg_stroke = Stroke::new(1.0, BORDER);
            state.fg_stroke = Stroke::new(1.0, TEXT_BRIGHT);
            state.corner_radius = radius;
            state.expansion = 0.0;
        }
    });
}
