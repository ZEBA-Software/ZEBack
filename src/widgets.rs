//! Eigene Bedienelemente: Karten mit Eckwinkeln, Schaltflächen, Auswahlzeilen, Schalter usw.
//! Alle Übergänge (Hover, Auswahl, Drücken) sind animiert.

use std::f32::consts::{FRAC_PI_2, PI, TAU};
use std::sync::Arc;

use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, FontId, Galley, Margin, Pos2, Rect, Response,
    RichText, Sense, Shape, Stroke, StrokeKind, Ui, Vec2, pos2, vec2,
};

use crate::theme::{self, *};

// ---------------------------------------------------------------------------- Symbole

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Backup,
    Restore,
    Cloud,
    Log,
    Desktop,
    Documents,
    Downloads,
    Pictures,
    Music,
    Videos,
    Folder,
    Usb,
    Add,
    Delete,
    Check,
    Close,
    Warning,
    Error,
    Zip,
    OpenFile,
    Home,
    Sync,
    Settings,
    Key,
    User,
    Globe,
    Mail,
    Drive,
}

impl Icon {
    /// Zeichen in der Windows-Symbolschrift (Segoe Fluent Icons / MDL2) und Ersatzzeichen.
    fn glyphs(self) -> (char, &'static str) {
        match self {
            Icon::Backup => ('\u{E74E}', "💾"),
            Icon::Restore => ('\u{E81C}', "⟲"),
            Icon::Cloud => ('\u{E753}', "☁"),
            Icon::Log => ('\u{EA37}', "📋"),
            Icon::Desktop => ('\u{E7F4}', "🖥"),
            Icon::Documents => ('\u{E8A5}', "🗋"),
            Icon::Downloads => ('\u{E896}', "⬇"),
            Icon::Pictures => ('\u{E91B}', "🖼"),
            Icon::Music => ('\u{E8D6}', "🎵"),
            Icon::Videos => ('\u{E714}', "🎬"),
            Icon::Folder => ('\u{E8B7}', "📁"),
            Icon::Usb => ('\u{E88E}', "💽"),
            Icon::Add => ('\u{E710}', "➕"),
            Icon::Delete => ('\u{E74D}', "🗑"),
            Icon::Check => ('\u{E73E}', "✔"),
            Icon::Close => ('\u{E711}', "✖"),
            Icon::Warning => ('\u{E7BA}', "⚠"),
            Icon::Error => ('\u{E783}', "❗"),
            Icon::Zip => ('\u{F012}', "🗜"),
            Icon::OpenFile => ('\u{E8E5}', "📂"),
            Icon::Home => ('\u{E80F}', "🏠"),
            Icon::Sync => ('\u{E895}', "🔄"),
            Icon::Settings => ('\u{E713}', "⚙"),
            Icon::Key => ('\u{E8D7}', "🔑"),
            Icon::User => ('\u{E77B}', "👤"),
            Icon::Globe => ('\u{E774}', "🌐"),
            Icon::Mail => ('\u{E715}', "✉"),
            Icon::Drive => ('\u{EDA2}', "🖴"),
        }
    }
}

pub fn paint_icon(painter: &egui::Painter, center: Pos2, icon: Icon, size: f32, color: Color32) {
    let (glyph, fallback) = icon.glyphs();
    if theme::has_icon_font() {
        painter.text(center, Align2::CENTER_CENTER, glyph, theme::icons(size), color);
    } else {
        painter.text(center, Align2::CENTER_CENTER, fallback, theme::regular(size), color);
    }
}

/// Reserviert ein Quadrat im Textfluss und zeichnet ein Symbol hinein.
pub fn inline_icon(ui: &mut Ui, icon: Icon, size: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size + 6.0), Sense::hover());
    paint_icon(ui.painter(), rect.center(), icon, size, color);
}

// ---------------------------------------------------------------------------- Text

/// Einzeilige Schrift, die bei Platzmangel mit "…" endet.
pub fn elided(ui: &Ui, text: &str, font: FontId, color: Color32, max_width: f32) -> Arc<Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap.max_width = max_width.max(8.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    ui.painter().layout_job(job)
}

pub fn text(ui: &mut Ui, content: impl Into<String>, font: FontId, color: Color32) -> Response {
    ui.label(RichText::new(content).font(font).color(color))
}

/// Kleine Beschriftung über einem Eingabefeld.
pub fn field_label(ui: &mut Ui, content: &str) {
    text(ui, content, theme::regular(12.0), TEXT_MUTED);
}

/// Kleines Etikett mit dunklem Hintergrund (z. B. für Zähler).
pub fn chip(ui: &mut Ui, content: &str, color: Color32) {
    let galley = ui.painter().layout_no_wrap(content.to_owned(), theme::bold(12.0), color);
    let (rect, _) = ui.allocate_exact_size(galley.size() + vec2(14.0, 6.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(3), SURFACE_SEL);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, color);
}

/// Zeile "Bezeichnung …… Wert" (Wert rechtsbündig).
pub fn kv_row(ui: &mut Ui, key: &str, value: &str) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::hover());
    let painter = ui.painter();
    let key = painter.layout_no_wrap(key.to_owned(), theme::regular(13.0), TEXT_MUTED);
    let key_width = key.size().x;
    painter.galley(pos2(rect.left(), rect.center().y - key.size().y / 2.0), key, TEXT_MUTED);
    let value = elided(ui, value, theme::bold(13.0), TEXT_BRIGHT, rect.width() - key_width - 16.0);
    painter.galley(
        pos2(rect.right() - value.size().x, rect.center().y - value.size().y / 2.0),
        value,
        TEXT_BRIGHT,
    );
}

// ---------------------------------------------------------------------------- Karten

/// Winkel an den vier Ecken eines Rechtecks.
pub fn corner_brackets(painter: &egui::Painter, rect: Rect, length: f32, color: Color32) {
    let stroke = Stroke::new(1.0, color);
    let r = rect.expand(0.5);
    for (corner, dx, dy) in [
        (r.left_top(), 1.0, 1.0),
        (r.right_top(), -1.0, 1.0),
        (r.left_bottom(), 1.0, -1.0),
        (r.right_bottom(), -1.0, -1.0),
    ] {
        painter.line_segment([corner, corner + vec2(dx * length, 0.0)], stroke);
        painter.line_segment([corner, corner + vec2(0.0, dy * length)], stroke);
    }
}

pub fn card<R>(ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> R {
    let result = egui::Frame::new()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(2))
        .inner_margin(Margin::same(18))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add_contents(ui)
        });
    corner_brackets(ui.painter(), result.response.rect, 12.0, BRACKET);
    result.inner
}

/// Kopf einer Karte: Symbol, Titel, optional ein Etikett rechts und eine Erklärung darunter.
pub fn card_header(ui: &mut Ui, icon: Icon, title: &str, subtitle: &str, badge: Option<&str>) {
    ui.horizontal(|ui| {
        inline_icon(ui, icon, 16.0, TEXT_MUTED);
        text(ui, title, theme::bold(16.0), TEXT_BRIGHT);
        if let Some(badge) = badge {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                chip(ui, badge, TEXT);
            });
        }
    });
    if !subtitle.is_empty() {
        text(ui, subtitle, theme::regular(12.5), TEXT_MUTED);
    }
    ui.add_space(8.0);
}

pub fn divider(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().hline(rect.x_range(), rect.center().y, Stroke::new(1.0, BORDER));
}

// ---------------------------------------------------------------------------- Schaltflächen

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Primary,
    Secondary,
    Danger,
    Ghost,
}

const BUTTON_ICON_SPACE: f32 = 24.0;
const BUTTON_PADDING: f32 = 28.0;

fn button_font(kind: Kind) -> FontId {
    if kind == Kind::Primary { theme::bold(14.0) } else { theme::regular(14.0) }
}

/// Breite, die `button` für diese Beschriftung belegt (zum Aufteilen einer Zeile).
pub fn button_width(ui: &Ui, label: &str, kind: Kind, has_icon: bool) -> f32 {
    let galley = ui.painter().layout_no_wrap(label.to_owned(), button_font(kind), Color32::WHITE);
    galley.size().x + if has_icon { BUTTON_ICON_SPACE } else { 0.0 } + BUTTON_PADDING
}

pub fn button(ui: &mut Ui, label: &str, kind: Kind, icon: Option<Icon>) -> Response {
    let galley = ui.painter().layout_no_wrap(label.to_owned(), button_font(kind), Color32::WHITE);
    let icon_space = if icon.is_some() { BUTTON_ICON_SPACE } else { 0.0 };
    let width = galley.size().x + icon_space + BUTTON_PADDING;
    let (rect, response) = ui.allocate_exact_size(vec2(width, 34.0), Sense::click());

    let ctx = ui.ctx();
    let enabled = ui.is_enabled();
    let hover = ctx.animate_bool_with_time(response.id.with("hover"), response.hovered() && enabled, 0.1);
    let down = ctx.animate_bool_with_time(response.id.with("down"), response.is_pointer_button_down_on(), 0.06);

    if ui.is_rect_visible(rect) {
        let (fill, border, text_color) = match kind {
            Kind::Primary => (mix(mix(ACCENT, ACCENT_HI, hover), ACCENT_LO, down), Color32::TRANSPARENT, ON_ACCENT),
            Kind::Secondary => (mix(SURFACE, SURFACE_SEL, hover), mix(BORDER, BORDER_HI, hover), mix(TEXT, TEXT_BRIGHT, hover)),
            Kind::Danger => (mix(SURFACE, mix(SURFACE, RED, 0.18), hover), mix(mix(BORDER, RED, 0.5), RED, hover), RED),
            Kind::Ghost => (SURFACE_HI.gamma_multiply(hover), Color32::TRANSPARENT, mix(TEXT_MUTED, TEXT_BRIGHT, hover)),
        };
        // Beim Drücken gibt die Schaltfläche leicht nach.
        let body = rect.shrink(down);
        let painter = ui.painter();
        painter.rect(body, CornerRadius::same(4), fill, Stroke::new(1.0, border), StrokeKind::Inside);

        let mut x = body.center().x - (galley.size().x + icon_space) / 2.0;
        if let Some(icon) = icon {
            paint_icon(painter, pos2(x + 8.0, body.center().y), icon, 14.0, text_color);
            x += icon_space;
        }
        painter.galley(pos2(x, body.center().y - galley.size().y / 2.0), galley, text_color);
    }
    response.on_hover_cursor(CursorIcon::PointingHand)
}

/// Kleine Symbol-Schaltfläche (z. B. "Schließen").
pub fn icon_button(ui: &mut Ui, icon: Icon, tooltip: &str) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::click());
    let hover = ui.ctx().animate_bool_with_time(response.id.with("hover"), response.hovered(), 0.1);
    ui.painter().rect_filled(rect, CornerRadius::same(4), SURFACE_SEL.gamma_multiply(hover));
    paint_icon(ui.painter(), rect.center(), icon, 12.0, mix(TEXT_MUTED, TEXT_BRIGHT, hover));
    response.on_hover_cursor(CursorIcon::PointingHand).on_hover_text(tooltip)
}

// ---------------------------------------------------------------------------- Auswahl

fn paint_checkbox(painter: &egui::Painter, center: Pos2, on: f32, hover: f32) {
    // Kurzes "Aufploppen" beim Umschalten.
    let rect = Rect::from_center_size(center, Vec2::splat(16.0)).expand((on * PI).sin() * 1.4);
    painter.rect(
        rect,
        CornerRadius::same(3),
        ACCENT.gamma_multiply(on),
        Stroke::new(1.0, mix(mix(BORDER_HI, TEXT_MUTED, hover), ACCENT, on)),
        StrokeKind::Inside,
    );
    if on > 0.02 {
        let at = |x: f32, y: f32| pos2(rect.left() + rect.width() * x, rect.top() + rect.height() * y);
        let (a, b, c) = (at(0.24, 0.52), at(0.43, 0.70), at(0.77, 0.31));
        // Der Haken wird in zwei Zügen "gezeichnet".
        let first = (on * 2.0).min(1.0);
        let second = (on * 2.0 - 1.0).clamp(0.0, 1.0);
        let mut points = vec![a, a + (b - a) * first];
        if second > 0.0 {
            points.push(b + (c - b) * second);
        }
        painter.line(points, Stroke::new(1.8, ON_ACCENT));
    }
}

fn paint_radio(painter: &egui::Painter, center: Pos2, on: f32, hover: f32) {
    painter.circle_stroke(center, 7.5, Stroke::new(1.0, mix(mix(BORDER_HI, TEXT_MUTED, hover), ACCENT, on)));
    if on > 0.02 {
        painter.circle_filled(center, 4.0 * on, ACCENT);
    }
}

/// Listenzeile mit Häkchen, Symbol, Titel und Untertitel. Ein Klick irgendwo schaltet um.
/// `trailing` steht rechtsbündig in der Zeile (z. B. die Ordnergröße).
/// Rückgabe: `true`, wenn "Entfernen" geklickt wurde (nur bei `removable`).
pub fn check_row(
    ui: &mut Ui,
    checked: &mut bool,
    icon: Icon,
    title: &str,
    subtitle: &str,
    trailing: Option<&str>,
    removable: bool,
) -> bool {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 46.0), Sense::click());
    let remove_rect = Rect::from_center_size(pos2(rect.right() - 20.0, rect.center().y), Vec2::splat(28.0));
    let remove_response =
        removable.then(|| ui.interact(remove_rect, response.id.with("remove"), Sense::click()));
    let remove_clicked = remove_response.as_ref().is_some_and(|r| r.clicked());
    let remove_hovered = remove_response.as_ref().is_some_and(|r| r.hovered());
    if response.clicked() {
        *checked = !*checked;
    }

    let ctx = ui.ctx();
    let hover = ctx.animate_bool_with_time(response.id.with("hover"), response.hovered() || remove_hovered, 0.1);
    let on = ctx.animate_bool_with_time(response.id.with("on"), *checked, 0.16);

    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.rect_filled(rect, CornerRadius::same(3), SURFACE_HI.gamma_multiply(hover));
        paint_checkbox(painter, pos2(rect.left() + 18.0, rect.center().y), on, hover);
        paint_icon(painter, pos2(rect.left() + 46.0, rect.center().y), icon, 15.0, mix(TEXT_FAINT, ACCENT, on));

        let text_left = rect.left() + 66.0;
        let mut text_right = if removable { remove_rect.left() } else { rect.right() } - 10.0;
        if let Some(trailing) = trailing {
            let color = mix(TEXT_FAINT, TEXT, on);
            let galley = painter.layout_no_wrap(trailing.to_owned(), theme::regular(12.5), color);
            text_right -= galley.size().x;
            painter.galley(pos2(text_right, rect.center().y - galley.size().y / 2.0), galley, color);
            text_right -= 12.0;
        }
        let title_color = mix(TEXT_MUTED, TEXT_BRIGHT, on);
        let title = elided(ui, title, theme::bold(14.0), title_color, text_right - text_left);
        let subtitle = elided(ui, subtitle, theme::regular(12.0), TEXT_FAINT, text_right - text_left);
        painter.galley(pos2(text_left, rect.center().y - 16.0), title, title_color);
        painter.galley(pos2(text_left, rect.center().y + 2.0), subtitle, TEXT_FAINT);

        if removable {
            let remove_hover = ctx.animate_bool_with_time(response.id.with("remove_hover"), remove_hovered, 0.1);
            painter.rect_filled(remove_rect, CornerRadius::same(4), SURFACE_SEL.gamma_multiply(remove_hover));
            let color = mix(TEXT_FAINT, RED, remove_hover).gamma_multiply(0.3 + 0.7 * hover);
            paint_icon(painter, remove_rect.center(), Icon::Delete, 13.0, color);
        }
    }
    if let Some(r) = remove_response {
        r.on_hover_cursor(CursorIcon::PointingHand).on_hover_text(t!("Aus der Liste entfernen", "Remove from list"));
    }
    response.on_hover_cursor(CursorIcon::PointingHand);
    remove_clicked
}

/// Zeile im Datei-Browser einer Sicherung. Das Häkchen wählt aus, ein Klick auf einen Ordner
/// öffnet ihn. `locked`: über einen übergeordneten Ordner bereits ausgewählt.
/// Rückgabe: (Häkchen umgeschaltet, Ordner geöffnet).
pub fn browse_row(ui: &mut Ui, checked: bool, locked: bool, dir: bool, title: &str, trailing: &str) -> (bool, bool) {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::click());
    let check_rect = Rect::from_min_max(rect.min, pos2(rect.left() + 36.0, rect.bottom()));
    let check = ui.interact(check_rect, response.id.with("check"), Sense::click());
    let ctx = ui.ctx();
    let hover = ctx.animate_bool_with_time(response.id.with("hover"), response.hovered() || check.hovered(), 0.1);
    let on = ctx.animate_bool_with_time(response.id.with("on"), checked, 0.12);

    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.rect_filled(rect, CornerRadius::ZERO, SURFACE_HI.gamma_multiply(hover));
        // Über den Ordner darüber ausgewählt: Häkchen gesetzt, aber blass.
        let mut check_painter = painter.clone();
        check_painter.multiply_opacity(if locked { 0.45 } else { 1.0 });
        paint_checkbox(&check_painter, pos2(rect.left() + 18.0, rect.center().y), on, hover);
        let icon = if dir { Icon::Folder } else { Icon::Documents };
        paint_icon(painter, pos2(rect.left() + 46.0, rect.center().y), icon, 14.0, mix(TEXT_FAINT, ACCENT, on));

        let mut text_right = rect.right() - 10.0;
        if dir {
            let arrow = painter.layout_no_wrap(">".to_owned(), theme::bold(13.0), mix(TEXT_FAINT, ACCENT, hover));
            text_right -= arrow.size().x;
            painter.galley(pos2(text_right, rect.center().y - arrow.size().y / 2.0), arrow, TEXT_FAINT);
            text_right -= 12.0;
        }
        let trailing = painter.layout_no_wrap(trailing.to_owned(), theme::regular(12.0), TEXT_FAINT);
        text_right -= trailing.size().x;
        painter.galley(pos2(text_right, rect.center().y - trailing.size().y / 2.0), trailing, TEXT_FAINT);
        let text_left = rect.left() + 64.0;
        let color = mix(TEXT, TEXT_BRIGHT, on.max(hover));
        let font = if dir { theme::bold(13.5) } else { theme::regular(13.5) };
        let title = elided(ui, title, font, color, text_right - 12.0 - text_left);
        painter.galley(pos2(text_left, rect.center().y - title.size().y / 2.0), title, color);
    }
    let check_clicked = check.clicked();
    check.on_hover_cursor(CursorIcon::PointingHand);
    let row_clicked = response.clicked();
    response.on_hover_cursor(CursorIcon::PointingHand);
    let toggled = !locked && (check_clicked || (!dir && row_clicked));
    (toggled, dir && row_clicked && !check_clicked)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    /// Mehrfachauswahl
    Check,
    /// Entweder-oder
    Radio,
}

/// Umrandete Auswahlzeile mit Markierung, Symbol, Titel und Beschreibung.
pub fn option_row(
    ui: &mut Ui,
    selected: bool,
    mark: Mark,
    icon: Icon,
    title: &str,
    description: &str,
) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 60.0), Sense::click());
    let ctx = ui.ctx();
    let enabled = ui.is_enabled();
    let hover = ctx.animate_bool_with_time(response.id.with("hover"), response.hovered() && enabled, 0.1);
    let on = ctx.animate_bool_with_time(response.id.with("on"), selected, 0.16);
    let down = ctx.animate_bool_with_time(response.id.with("down"), response.is_pointer_button_down_on(), 0.06);

    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let body = rect.shrink(down);
        let fill = mix(mix(BG, SURFACE_HI, hover), ACCENT_BG, on);
        let border = mix(mix(BORDER, BORDER_HI, hover), ACCENT, on);
        painter.rect(body, CornerRadius::same(3), fill, Stroke::new(1.0, border), StrokeKind::Inside);

        let mark_center = pos2(body.left() + 20.0, body.center().y);
        match mark {
            Mark::Check => paint_checkbox(painter, mark_center, on, hover),
            Mark::Radio => paint_radio(painter, mark_center, on, hover),
        }
        paint_icon(painter, pos2(body.left() + 50.0, body.center().y), icon, 16.0, mix(TEXT_MUTED, ACCENT, on));

        let text_left = body.left() + 72.0;
        let width = body.right() - text_left - 12.0;
        let title = elided(ui, title, theme::bold(14.0), TEXT_BRIGHT, width);
        let description = elided(ui, description, theme::regular(12.0), TEXT_MUTED, width);
        painter.galley(pos2(text_left, body.center().y - 17.0), title, TEXT_BRIGHT);
        painter.galley(pos2(text_left, body.center().y + 3.0), description, TEXT_MUTED);
    }
    response.on_hover_cursor(CursorIcon::PointingHand)
}

/// Große Kachel mit gestricheltem Rand, die direkt eine Aktion auslöst.
pub fn action_tile(ui: &mut Ui, size: Vec2, icon: Icon, title: &str, description: &str) -> Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let ctx = ui.ctx();
    let enabled = ui.is_enabled();
    let hover = ctx.animate_bool_with_time(response.id.with("hover"), response.hovered() && enabled, 0.14);
    let down = ctx.animate_bool_with_time(response.id.with("down"), response.is_pointer_button_down_on(), 0.06);

    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let body = rect.shrink(down * 1.5);
        painter.rect_filled(body, CornerRadius::same(8), mix(BG, SURFACE, hover).gamma_multiply(0.85));

        // Gestrichelter, abgerundeter Rand
        const RADIUS: f32 = 8.0;
        let mut outline = Vec::new();
        for (center, start) in [
            (pos2(body.right() - RADIUS, body.top() + RADIUS), -FRAC_PI_2),
            (pos2(body.right() - RADIUS, body.bottom() - RADIUS), 0.0),
            (pos2(body.left() + RADIUS, body.bottom() - RADIUS), FRAC_PI_2),
            (pos2(body.left() + RADIUS, body.top() + RADIUS), PI),
        ] {
            for step in 0..=6 {
                let angle = start + step as f32 / 6.0 * FRAC_PI_2;
                outline.push(center + vec2(angle.cos(), angle.sin()) * RADIUS);
            }
        }
        outline.push(outline[0]);
        let stroke = Stroke::new(1.0, mix(BORDER_HI, ACCENT, hover));
        painter.extend(Shape::dashed_line(&outline, stroke, 5.0, 4.0));

        let center = pos2(body.center().x, body.center().y - 22.0);
        painter.circle(center, 19.0, mix(SURFACE_HI, ACCENT_BG, hover), Stroke::new(1.0, mix(BORDER, ACCENT, hover)));
        paint_icon(painter, center, icon, 16.0, mix(TEXT, ACCENT, hover));

        let width = body.width() - 24.0;
        let title = elided(ui, title, theme::bold(14.0), TEXT_BRIGHT, width);
        painter.galley(pos2(body.center().x - title.size().x / 2.0, center.y + 30.0), title, TEXT_BRIGHT);
        let description = elided(ui, description, theme::regular(12.0), TEXT_MUTED, width);
        painter.galley(pos2(body.center().x - description.size().x / 2.0, center.y + 51.0), description, TEXT_MUTED);
    }
    response.on_hover_cursor(CursorIcon::PointingHand)
}

/// Schalter (an/aus) mit gleitendem Knopf.
pub fn toggle(ui: &mut Ui, on: &mut bool) -> Response {
    let (rect, mut response) = ui.allocate_exact_size(vec2(36.0, 20.0), Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    let t = ui.ctx().animate_bool_with_time(response.id.with("on"), *on, 0.14);
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.rect(
            rect,
            CornerRadius::same(10),
            mix(SURFACE_SEL, ACCENT, t),
            Stroke::new(1.0, mix(BORDER_HI, ACCENT, t)),
            StrokeKind::Inside,
        );
        let x = egui::lerp((rect.left() + 10.0)..=(rect.right() - 10.0), t);
        painter.circle_filled(pos2(x, rect.center().y), 6.5, mix(TEXT_MUTED, ON_ACCENT, t));
    }
    response.on_hover_cursor(CursorIcon::PointingHand)
}

/// Einstellung mit Titel, Erklärung und Schalter rechts.
pub fn toggle_row(ui: &mut Ui, on: &mut bool, title: &str, description: &str) {
    let width = ui.available_width();
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.set_width((width - 56.0).max(120.0));
            ui.spacing_mut().item_spacing.y = 3.0;
            text(ui, title, theme::bold(14.0), TEXT_BRIGHT);
            text(ui, description, theme::regular(12.0), TEXT_MUTED);
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            toggle(ui, on);
        });
    });
}

/// Auswahlleiste mit gleitender Markierung; füllt die verfügbare Breite.
pub fn segmented<T: PartialEq + Copy>(ui: &mut Ui, salt: &str, value: &mut T, options: &[(T, &str)]) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::hover());
    let id = ui.make_persistent_id(salt);
    let inner = rect.shrink(3.0);
    let cell = inner.width() / options.len().max(1) as f32;
    let segment = |index: usize| {
        Rect::from_min_size(pos2(inner.left() + cell * index as f32, inner.top()), vec2(cell, inner.height()))
    };

    let painter = ui.painter().clone();
    painter.rect(rect, CornerRadius::same(4), BG, Stroke::new(1.0, BORDER), StrokeKind::Inside);
    if let Some(index) = options.iter().position(|(v, _)| v == value) {
        let left = ui.ctx().animate_value_with_time(id.with("left"), cell * index as f32, 0.16);
        let highlight = Rect::from_min_size(pos2(inner.left() + left, inner.top()), vec2(cell, inner.height()));
        painter.rect(highlight, CornerRadius::same(3), ACCENT_BG, Stroke::new(1.0, ACCENT), StrokeKind::Inside);
    }
    for (index, (option, label)) in options.iter().enumerate() {
        let response = ui
            .interact(segment(index), id.with(index), Sense::click())
            .on_hover_cursor(CursorIcon::PointingHand);
        if response.clicked() {
            *value = *option;
        }
        let selected = ui.ctx().animate_bool_with_time(id.with(("selected", index)), option == value, 0.16);
        let hover = ui.ctx().animate_bool_with_time(id.with(("hover", index)), response.hovered(), 0.1);
        let color = mix(mix(TEXT_MUTED, TEXT, hover), ACCENT_HI, selected);
        let galley = elided(ui, label, theme::regular(13.0), color, cell - 8.0);
        painter.galley(segment(index).center() - galley.size() / 2.0, galley, color);
    }
}

// ---------------------------------------------------------------------------- Eingabe

/// Eingabefeld; der Rand färbt sich, solange das Feld aktiv ist.
pub fn text_field(
    ui: &mut Ui,
    salt: &str,
    value: &mut String,
    hint: &str,
    password: bool,
    width: f32,
) -> Response {
    let id = ui.make_persistent_id(salt);
    let focused = ui.memory(|m| m.has_focus(id));
    let focus = ui.ctx().animate_bool_with_time(id.with("focus"), focused, 0.1);
    egui::Frame::new()
        .fill(BG)
        .corner_radius(CornerRadius::same(4))
        .stroke(Stroke::new(1.0, mix(BORDER, ACCENT, focus)))
        .inner_margin(Margin::symmetric(8, 1))
        .show(ui, |ui| {
            // Höhe wie die Schaltflächen (34), Text senkrecht mittig.
            ui.spacing_mut().interact_size.y = 30.0;
            ui.add(
                egui::TextEdit::singleline(value)
                    .id(id)
                    .frame(egui::Frame::NONE)
                    .margin(Margin::symmetric(2, 0))
                    .align(Align2::LEFT_CENTER)
                    .hint_text(RichText::new(hint.to_owned()).color(TEXT_FAINT))
                    .password(password)
                    .font(theme::regular(14.0))
                    .text_color(TEXT_BRIGHT)
                    .desired_width((width - 18.0).max(60.0)),
            )
        })
        .inner
}

// ---------------------------------------------------------------------------- Navigation

/// Eintrag in der Seitenleiste. Die Markierung des aktiven Eintrags zeichnet der Aufrufer
/// (sie gleitet zwischen den Einträgen).
pub fn nav_item(ui: &mut Ui, selected: bool, icon: Icon, label: &str, badge: Option<usize>) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::click());
    let ctx = ui.ctx();
    let hover = ctx.animate_bool_with_time(response.id.with("hover"), response.hovered() && !selected, 0.1);
    let on = ctx.animate_bool_with_time(response.id.with("on"), selected, 0.14);
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        // Eckig wie die Karten: beim Überfahren ein kantiger Rahmen.
        painter.rect(
            rect,
            CornerRadius::ZERO,
            SURFACE_HI.gamma_multiply(hover),
            Stroke::new(1.0, BORDER_HI.gamma_multiply(hover)),
            StrokeKind::Inside,
        );
        // Symbol in einem Kästchen; beim aktiven Eintrag gefüllt wie der aktuelle Schritt im Ablauf.
        let tile = Rect::from_center_size(pos2(rect.left() + 21.0, rect.center().y), Vec2::splat(24.0));
        painter.rect(
            tile,
            CornerRadius::ZERO,
            mix(SURFACE, ACCENT, on),
            Stroke::new(1.0, mix(mix(BORDER, BORDER_HI, hover), ACCENT, on)),
            StrokeKind::Inside,
        );
        let color = mix(mix(TEXT_MUTED, TEXT, hover), TEXT_BRIGHT, on);
        paint_icon(painter, tile.center(), icon, 13.0, mix(color, ON_ACCENT, on));
        let font = if selected { theme::bold(13.5) } else { theme::regular(13.5) };
        let galley = painter.layout_no_wrap(label.to_owned(), font, color);
        painter.galley(pos2(rect.left() + 43.0, rect.center().y - galley.size().y / 2.0), galley, color);

        if let Some(count) = badge.filter(|c| *c > 0) {
            let label = if count > 99 { "99+".to_owned() } else { count.to_string() };
            let galley = painter.layout_no_wrap(label, theme::bold(11.0), ON_ACCENT);
            let width = (galley.size().x + 10.0).max(18.0);
            let tag = Rect::from_center_size(pos2(rect.right() - 8.0 - width / 2.0, rect.center().y), vec2(width, 18.0));
            painter.rect_filled(tag, CornerRadius::ZERO, YELLOW);
            painter.galley(tag.center() - galley.size() / 2.0, galley, ON_ACCENT);
        }
    }
    response.on_hover_cursor(CursorIcon::PointingHand)
}

/// Überschrift einer Gruppe in der Seitenleiste: kleine Versalien mit Linie bis zum Rand.
pub fn nav_group(ui: &mut Ui, label: &str) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::hover());
    let painter = ui.painter();
    let galley = painter.layout_no_wrap(label.to_owned(), theme::bold(10.5), TEXT_FAINT);
    let line_from = rect.left() + 9.0 + galley.size().x + 8.0;
    painter.galley(pos2(rect.left() + 9.0, rect.center().y - galley.size().y / 2.0), galley, TEXT_FAINT);
    painter.hline(line_from..=rect.right(), rect.center().y, Stroke::new(1.0, BORDER));
}

/// Schrittanzeige eines geführten Ablaufs. Erledigte Schritte sind anklickbar;
/// Rückgabe: der angeklickte frühere Schritt.
pub fn stepper(ui: &mut Ui, salt: &str, labels: &[&str], current: usize) -> Option<usize> {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::hover());
    let id = ui.make_persistent_id(salt);
    let cell = rect.width() / labels.len().max(1) as f32;
    // Die farbige Verbindungslinie wächst animiert bis zum aktuellen Schritt.
    let progress = ui.ctx().animate_value_with_time(id.with("progress"), current as f32, 0.28);
    let painter = ui.painter().clone();
    let mut clicked = None;

    for (index, label) in labels.iter().enumerate() {
        let cell_rect = Rect::from_min_size(pos2(rect.left() + cell * index as f32, rect.top()), vec2(cell, rect.height()));
        let done = index < current;
        let active = ui.ctx().animate_bool_with_time(id.with(("active", index)), index == current, 0.18);
        let reached = ui.ctx().animate_bool_with_time(id.with(("done", index)), done, 0.18);

        let mut hover = 0.0;
        if done {
            let response = ui
                .interact(cell_rect.shrink2(vec2(4.0, 2.0)), id.with(index), Sense::click())
                .on_hover_cursor(CursorIcon::PointingHand);
            hover = ui.ctx().animate_bool_with_time(id.with(("hover", index)), response.hovered(), 0.1);
            if response.clicked() {
                clicked = Some(index);
            }
        }

        let chip = Rect::from_center_size(pos2(cell_rect.left() + 13.0, cell_rect.center().y), Vec2::splat(24.0));
        let fill = mix(mix(SURFACE_SEL, ACCENT_BG, reached), ACCENT, active);
        let border = mix(mix(BORDER_HI, ACCENT, reached), ACCENT, active);
        painter.rect(chip, CornerRadius::same(3), fill, Stroke::new(1.0, border), StrokeKind::Inside);
        if done {
            paint_icon(&painter, chip.center(), Icon::Check, 11.0, ACCENT);
        } else {
            painter.text(
                chip.center(),
                Align2::CENTER_CENTER,
                format!("{:02}", index + 1),
                theme::bold(11.5),
                mix(TEXT_MUTED, ON_ACCENT, active),
            );
        }

        let color = mix(mix(mix(TEXT_FAINT, TEXT, reached), TEXT_BRIGHT, hover), TEXT_BRIGHT, active);
        let font = if index == current { theme::bold(14.0) } else { theme::regular(14.0) };
        let galley = elided(ui, label, font, color, cell - 60.0);
        let label_width = galley.size().x;
        painter.galley(pos2(chip.right() + 10.0, cell_rect.center().y - galley.size().y / 2.0), galley, color);

        if index + 1 < labels.len() {
            let from = chip.right() + 10.0 + label_width + 12.0;
            let to = cell_rect.right() - 12.0;
            if to > from {
                let y = cell_rect.center().y;
                painter.hline(from..=to, y, Stroke::new(1.0, BORDER));
                let filled = (progress - index as f32).clamp(0.0, 1.0);
                if filled > 0.0 {
                    painter.hline(from..=(from + (to - from) * filled), y, Stroke::new(1.0, ACCENT));
                }
            }
        }
    }
    clicked
}

// ---------------------------------------------------------------------------- Fortschritt

/// Fortschrittsbalken mit wanderndem Glanz. `None` = Dauer unbekannt.
pub fn progress_bar(ui: &mut Ui, fraction: Option<f32>) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 6.0), Sense::hover());
    let painter = ui.painter().with_clip_rect(rect);
    let radius = CornerRadius::same(2);
    painter.rect_filled(rect, radius, SURFACE_SEL);
    let time = ui.input(|i| i.time) as f32;
    match fraction {
        Some(fraction) => {
            let shown = ui
                .ctx()
                .animate_value_with_time(ui.scope_id().with("progress"), fraction.clamp(0.0, 1.0), 0.3);
            let fill = Rect::from_min_max(rect.min, pos2(rect.left() + rect.width() * shown, rect.bottom()));
            if fill.width() > 1.0 {
                painter.rect_filled(fill, radius, ACCENT);
                const BAND: f32 = 120.0;
                let x = rect.left() - BAND + (time * 260.0) % (rect.width() + BAND * 2.0);
                let band = Rect::from_min_max(pos2(x, rect.top()), pos2(x + BAND, rect.bottom())).intersect(fill);
                if band.width() > 0.0 {
                    painter.rect_filled(band, radius, Color32::from_white_alpha(70));
                }
            }
        }
        None => {
            let width = rect.width() * 0.25;
            let phase = (time * 0.7).fract();
            let t = if phase < 0.5 { phase * 2.0 } else { 2.0 - phase * 2.0 };
            let x = rect.left() + (rect.width() - width) * (0.5 - 0.5 * (t * PI).cos());
            painter.rect_filled(Rect::from_min_size(pos2(x, rect.top()), vec2(width, rect.height())), radius, ACCENT);
        }
    }
    ui.ctx().request_repaint();
}

/// Kreisender Bogen als Zeichen für "es wird gearbeitet".
pub fn spinner(ui: &mut Ui, size: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let time = ui.input(|i| i.time) as f32;
    let radius = size / 2.0 - 2.0;
    let points: Vec<Pos2> = (0..=28)
        .map(|i| {
            let angle = time * 5.5 + i as f32 / 28.0 * TAU * 0.7;
            rect.center() + vec2(angle.cos(), angle.sin()) * radius
        })
        .collect();
    ui.painter().add(Shape::line(points, Stroke::new(2.0, color)));
    ui.ctx().request_repaint();
}
