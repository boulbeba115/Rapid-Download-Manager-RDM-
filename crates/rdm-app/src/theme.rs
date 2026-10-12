//! Classic desktop look: compact, flat, light by default.

use eframe::egui::{self, Color32, FontId, TextStyle};

pub const ACCENT: Color32 = Color32::from_rgb(0x1E, 0x7F, 0xD6);
pub const GREEN: Color32 = Color32::from_rgb(0x2E, 0x9E, 0x4F);
pub const RED: Color32 = Color32::from_rgb(0xD1, 0x3B, 0x3B);
pub const ORANGE: Color32 = Color32::from_rgb(0xE0, 0x8A, 0x1E);

pub fn apply(ctx: &egui::Context, dark: bool) {
    ctx.set_theme(if dark { egui::Theme::Dark } else { egui::Theme::Light });
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(6.0, 4.0);
        style.spacing.button_padding = egui::vec2(8.0, 3.0);
        style.spacing.menu_margin = egui::Margin::same(4);
        style.spacing.interact_size.y = 20.0;
        // Rows are clickable; text selection would swallow those clicks.
        style.interaction.selectable_labels = false;
        style.text_styles = [
            (TextStyle::Small, FontId::proportional(11.0)),
            (TextStyle::Body, FontId::proportional(13.0)),
            (TextStyle::Button, FontId::proportional(13.0)),
            (TextStyle::Heading, FontId::proportional(17.0)),
            (TextStyle::Monospace, FontId::monospace(12.0)),
        ]
        .into();
        style.visuals.selection.bg_fill = if style.visuals.dark_mode {
            Color32::from_rgb(0x1F, 0x4E, 0x79)
        } else {
            Color32::from_rgb(0xCC, 0xE4, 0xFB)
        };
        style.visuals.selection.stroke.color = if style.visuals.dark_mode { Color32::WHITE } else { Color32::from_rgb(0x10, 0x30, 0x50) };
        style.visuals.window_corner_radius = egui::CornerRadius::same(4);
        style.visuals.menu_corner_radius = egui::CornerRadius::same(3);
    });
}

/// Colour used for a category's little file badge.
pub fn category_color(category: &str) -> Color32 {
    match category {
        "Compressed" => Color32::from_rgb(0xC9, 0x8A, 0x1B),
        "Documents" => Color32::from_rgb(0x3B, 0x7D, 0xD8),
        "Music" => Color32::from_rgb(0x9B, 0x4F, 0xC4),
        "Programs" => Color32::from_rgb(0x4A, 0x90, 0x5A),
        "Video" => Color32::from_rgb(0xD0, 0x4A, 0x4A),
        _ => Color32::from_rgb(0x8A, 0x8F, 0x98),
    }
}
