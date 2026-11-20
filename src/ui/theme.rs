//! Design tokens and small widgets for the desktop app: one accent on dark neutrals, flat
//! cards with 1px borders, embedded Inter (UI) and JetBrains Mono (code).

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Response,
    RichText, Sense, Stroke, StrokeKind, TextStyle, Vec2,
};
use std::sync::Arc;

// Surfaces step up in lightness instead of using shadows.
pub const BG: Color32 = Color32::from_rgb(0x0B, 0x0D, 0x10);
pub const PANEL: Color32 = Color32::from_rgb(0x0E, 0x10, 0x13);
pub const CARD: Color32 = Color32::from_rgb(0x11, 0x14, 0x18);
pub const HOVER: Color32 = Color32::from_rgb(0x16, 0x1A, 0x1F);
pub const ACTIVE: Color32 = Color32::from_rgb(0x1C, 0x21, 0x28);
pub const BORDER: Color32 = Color32::from_rgb(0x1C, 0x20, 0x26);
pub const CONTROL: Color32 = Color32::from_rgb(0x26, 0x2B, 0x33);
pub const CONTROL_STRONG: Color32 = Color32::from_rgb(0x3A, 0x41, 0x4B);
pub const GRID_DOT: Color32 = Color32::from_rgb(0x1B, 0x1F, 0x25);

pub const TEXT: Color32 = Color32::from_rgb(0xE6, 0xE8, 0xEB);
pub const TEXT_2: Color32 = Color32::from_rgb(0xA6, 0xAD, 0xB8);
pub const TEXT_3: Color32 = Color32::from_rgb(0x8A, 0x92, 0x9E);

pub const ACCENT: Color32 = Color32::from_rgb(0x7B, 0x8C, 0xFF);
/// Accent at ~10% over the background: selected fills.
pub const ACCENT_SOFT: Color32 = Color32::from_rgba_premultiplied(12, 14, 26, 26);
/// Accent at ~55%: strokes of selected controls.
pub const ACCENT_LINE: Color32 = Color32::from_rgba_premultiplied(68, 77, 140, 140);

/// Agent activity (heat, trail, changed rings). Never used for anything else.
pub const HEAT: Color32 = Color32::from_rgb(0xFF, 0x8A, 0x3D);
pub const HEAT_INK: Color32 = Color32::from_rgb(0x1A, 0x0E, 0x05);
pub const SUCCESS: Color32 = Color32::from_rgb(0x4F, 0xC3, 0x8A);
pub const WARN: Color32 = Color32::from_rgb(0xE0, 0xA8, 0x4E);
pub const DANGER: Color32 = Color32::from_rgb(0xF2, 0x55, 0x5A);
pub const NOTE: Color32 = Color32::from_rgb(0xB9, 0xC2, 0xCF);

/// Community palette: similar lightness and chroma, none close to the accent or heat.
pub const COMMUNITY: [Color32; 10] = [
    Color32::from_rgb(0x45, 0xB5, 0xA5),
    Color32::from_rgb(0xC2, 0x8B, 0xE0),
    Color32::from_rgb(0xCF, 0xB4, 0x6A),
    Color32::from_rgb(0xE5, 0x7F, 0x9C),
    Color32::from_rgb(0x8C, 0x9A, 0xB0),
    Color32::from_rgb(0x94, 0xC2, 0x7A),
    Color32::from_rgb(0x6F, 0xB2, 0xD6),
    Color32::from_rgb(0xD8, 0xA0, 0xC0),
    Color32::from_rgb(0xA9, 0xB5, 0x65),
    Color32::from_rgb(0xB3, 0x9A, 0x86),
];

pub fn community_color(i: usize) -> Color32 {
    COMMUNITY[i % COMMUNITY.len()]
}

// Spacing scale (px).
pub const S1: f32 = 4.0;
pub const S2: f32 = 8.0;
pub const S3: f32 = 12.0;
pub const S4: f32 = 16.0;

// Radii.
pub const R_CONTROL: u8 = 6;
pub const R_CARD: u8 = 8;
pub const R_PILL: u8 = 12;

const SEMIBOLD: &str = "semibold";

pub fn semibold_family() -> FontFamily {
    FontFamily::Name(SEMIBOLD.into())
}

pub fn ui_font(size: f32) -> FontId {
    FontId::proportional(size)
}

pub fn strong_font(size: f32) -> FontId {
    FontId::new(size, semibold_family())
}

pub fn mono_font(size: f32) -> FontId {
    FontId::monospace(size)
}

/// Semibold text (egui has no bold weight of its own).
pub fn strong(text: impl Into<String>, size: f32) -> RichText {
    RichText::new(text).family(semibold_family()).size(size)
}

pub fn mono(text: impl Into<String>, size: f32) -> RichText {
    RichText::new(text).monospace().size(size)
}

/// Fonts are embedded in the binary; egui's defaults stay behind them as glyph fallbacks.
pub fn font_definitions() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "inter".into(),
        Arc::new(FontData::from_static(include_bytes!(
            "../../assets/fonts/Inter-Regular.ttf"
        ))),
    );
    fonts.font_data.insert(
        "inter-semibold".into(),
        Arc::new(FontData::from_static(include_bytes!(
            "../../assets/fonts/Inter-SemiBold.ttf"
        ))),
    );
    fonts.font_data.insert(
        "jetbrains-mono".into(),
        Arc::new(FontData::from_static(include_bytes!(
            "../../assets/fonts/JetBrainsMono-Regular.ttf"
        ))),
    );
    let fallbacks = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    if let Some(list) = fonts.families.get_mut(&FontFamily::Proportional) {
        list.insert(0, "inter".into());
    }
    if let Some(list) = fonts.families.get_mut(&FontFamily::Monospace) {
        list.insert(0, "jetbrains-mono".into());
    }
    let mut semibold = vec!["inter-semibold".to_string()];
    semibold.extend(fallbacks);
    fonts.families.insert(semibold_family(), semibold);
    fonts
}

/// Installs fonts, type scale, spacing and visuals. Call once at startup.
pub fn install(ctx: &egui::Context) {
    ctx.set_fonts(font_definitions());
    ctx.style_mut(|style| {
        style.text_styles = [
            (TextStyle::Small, ui_font(11.0)),
            (TextStyle::Body, ui_font(13.0)),
            (TextStyle::Button, ui_font(13.0)),
            (TextStyle::Heading, strong_font(18.0)),
            (TextStyle::Monospace, mono_font(12.0)),
        ]
        .into();
        style.spacing.item_spacing = Vec2::new(S2, 6.0);
        style.spacing.button_padding = Vec2::new(10.0, 4.0);
        style.spacing.interact_size.y = 26.0;
        style.spacing.window_margin = Margin::same(16);
        style.spacing.menu_margin = Margin::same(6);

        let v = &mut style.visuals;
        *v = egui::Visuals::dark();
        v.panel_fill = BG;
        v.window_fill = PANEL;
        v.extreme_bg_color = PANEL;
        v.faint_bg_color = CARD;
        v.code_bg_color = BG;
        v.hyperlink_color = ACCENT;
        v.warn_fg_color = WARN;
        v.error_fg_color = DANGER;
        v.window_corner_radius = CornerRadius::same(10);
        v.menu_corner_radius = CornerRadius::same(R_CARD);
        v.window_stroke = Stroke::new(1.0_f32, CONTROL);
        let shadow = egui::Shadow {
            offset: [0, 6],
            blur: 18,
            spread: 0,
            color: Color32::from_black_alpha(110),
        };
        v.window_shadow = shadow;
        v.popup_shadow = shadow;
        v.selection.bg_fill = ACCENT_SOFT;
        v.selection.stroke = Stroke::new(1.0_f32, ACCENT);
        let cr = CornerRadius::same(R_CONTROL);
        let w = &mut v.widgets;
        w.noninteractive.bg_fill = CARD;
        w.noninteractive.weak_bg_fill = CARD;
        w.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
        w.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT_2);
        w.noninteractive.corner_radius = cr;
        w.inactive.bg_fill = CARD;
        w.inactive.weak_bg_fill = CARD;
        w.inactive.bg_stroke = Stroke::new(1.0_f32, CONTROL);
        w.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT_2);
        w.inactive.corner_radius = cr;
        w.hovered.bg_fill = HOVER;
        w.hovered.weak_bg_fill = HOVER;
        w.hovered.bg_stroke = Stroke::new(1.0_f32, CONTROL_STRONG);
        w.hovered.fg_stroke = Stroke::new(1.0_f32, TEXT);
        w.hovered.corner_radius = cr;
        w.hovered.expansion = 0.0;
        w.active.bg_fill = ACTIVE;
        w.active.weak_bg_fill = ACTIVE;
        w.active.bg_stroke = Stroke::new(1.0_f32, ACCENT_LINE);
        w.active.fg_stroke = Stroke::new(1.0_f32, TEXT);
        w.active.corner_radius = cr;
        w.active.expansion = 0.0;
        w.open = w.active;
    });
}

/// A flat card: card fill, 1px border, 8px radius.
pub fn card() -> egui::Frame {
    egui::Frame::NONE
        .fill(CARD)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .corner_radius(CornerRadius::same(R_CARD))
        .inner_margin(Margin::same(14))
}

/// A tinted card for one semantic tone (agent activity, success, danger).
pub fn tinted_card(fill: Color32, border: Color32) -> egui::Frame {
    card().fill(fill).stroke(Stroke::new(1.0_f32, border))
}

pub fn section_label(ui: &mut egui::Ui, text: &str) {
    ui.label(
        RichText::new(text.to_uppercase())
            .size(11.0)
            .color(TEXT_3)
            .extra_letter_spacing(0.6),
    );
}

/// KPI card: label, large mono value, sub line.
pub fn kpi_card(ui: &mut egui::Ui, label: &str, value: &str, sub: &str, value_color: Color32) {
    card().inner_margin(Margin::same(16)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(label).size(12.0).color(TEXT_3));
        ui.add_space(S1);
        ui.label(mono(value, 22.0).color(value_color));
        ui.add_space(2.0);
        ui.label(RichText::new(sub).size(11.0).color(TEXT_3));
    });
}

/// A secondary button: transparent, 1px control border.
pub fn button(text: impl Into<RichText>) -> egui::Button<'static> {
    egui::Button::new(text.into().size(12.0))
        .fill(Color32::TRANSPARENT)
        .stroke(Stroke::new(1.0_f32, CONTROL))
        .corner_radius(CornerRadius::same(R_CONTROL))
        .min_size(Vec2::new(0.0, 28.0))
}

/// The one primary action per surface.
pub fn primary_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(strong(text, 12.0).color(BG))
        .fill(ACCENT)
        .stroke(Stroke::new(1.0_f32, ACCENT))
        .corner_radius(CornerRadius::same(R_CONTROL))
        .min_size(Vec2::new(0.0, 28.0))
}

/// Underlined tab with an optional count.
pub fn tab(ui: &mut egui::Ui, label: &str, count: Option<String>, active: bool) -> Response {
    let font = strong_font(13.0);
    let color = if active { TEXT } else { TEXT_3 };
    let g = ui.painter().layout_no_wrap(label.to_string(), font, color);
    let cg = count.map(|c| ui.painter().layout_no_wrap(c, mono_font(11.0), TEXT_3));
    let w = g.size().x + cg.as_ref().map_or(0.0, |c| c.size().x + 6.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 40.0), Sense::click());
    let color = if resp.hovered() && !active {
        TEXT
    } else {
        color
    };
    let p = ui.painter();
    let y = rect.center().y - g.size().y * 0.5;
    p.galley(egui::pos2(rect.left(), y), g, color);
    if let Some(c) = cg {
        let x = rect.right() - c.size().x;
        p.galley(egui::pos2(x, rect.center().y - c.size().y * 0.5), c, TEXT_3);
    }
    if active {
        p.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(rect.left(), rect.bottom() - 2.0),
                rect.right_bottom(),
            ),
            CornerRadius::same(1),
            ACCENT,
        );
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Segmented control. Returns the clicked index, if any.
pub fn segmented(ui: &mut egui::Ui, options: &[&str], selected: usize) -> Option<usize> {
    let mut clicked = None;
    egui::Frame::NONE
        .fill(PANEL)
        .stroke(Stroke::new(1.0_f32, CONTROL))
        .corner_radius(CornerRadius::same(7))
        .inner_margin(Margin::same(2))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.horizontal(|ui| {
                for (i, label) in options.iter().enumerate() {
                    let on = i == selected;
                    let text = RichText::new(*label)
                        .size(12.0)
                        .family(semibold_family())
                        .color(if on { TEXT } else { TEXT_3 });
                    let b = egui::Button::new(text)
                        .fill(if on { ACTIVE } else { Color32::TRANSPARENT })
                        .stroke(Stroke::NONE)
                        .corner_radius(CornerRadius::same(5))
                        .min_size(Vec2::new(0.0, 24.0));
                    if ui.add(b).clicked() {
                        clicked = Some(i);
                    }
                }
            });
        });
    clicked
}

/// A toggle chip with a status dot.
pub fn toggle_chip(ui: &mut egui::Ui, label: &str, on: bool, dot: Color32) -> Response {
    let text = RichText::new(label)
        .size(12.0)
        .color(if on { TEXT } else { TEXT_3 });
    let g = ui
        .painter()
        .layout_no_wrap(label.to_string(), ui_font(12.0), TEXT);
    let size = Vec2::new(g.size().x + 34.0, 28.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let p = ui.painter();
    let fill = if resp.hovered() {
        HOVER
    } else {
        Color32::TRANSPARENT
    };
    p.rect(
        rect,
        CornerRadius::same(R_CONTROL),
        fill,
        Stroke::new(1.0_f32, if on { CONTROL_STRONG } else { CONTROL }),
        StrokeKind::Inside,
    );
    p.circle_filled(
        egui::pos2(rect.left() + 14.0, rect.center().y),
        3.5,
        if on { dot } else { CONTROL_STRONG },
    );
    let galley = ui.painter().layout_no_wrap(
        text.text().to_string(),
        ui_font(12.0),
        if on { TEXT } else { TEXT_3 },
    );
    ui.painter().galley(
        egui::pos2(rect.left() + 24.0, rect.center().y - galley.size().y * 0.5),
        galley,
        TEXT,
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A small coloured dot followed by text, for legends and statuses.
pub fn dot_label(ui: &mut egui::Ui, color: Color32, text: impl Into<RichText>) -> Response {
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
        ui.painter().circle_filled(r.center(), 4.0, color);
        ui.label(text.into())
    })
    .inner
}
