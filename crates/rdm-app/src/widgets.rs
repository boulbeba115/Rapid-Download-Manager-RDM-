//! Custom-painted pieces: toolbar buttons with vector icons, the file badge,
//! and the segment map.

use eframe::egui::{pos2, vec2, Align2, Color32, CornerRadius, FontId, Painter, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Ui};
use rdm_core::segment::SegSnapshot;

use crate::theme::{ACCENT, GREEN, ORANGE, RED};

#[derive(Clone, Copy, PartialEq)]
pub enum Icon {
    Add,
    Resume,
    Pause,
    StopAll,
    Delete,
    DeleteDone,
    Options,
    Scheduler,
    StartQueue,
    StopQueue,
    Batch,
}

/// IDM-style toolbar button: icon on top, caption underneath.
pub fn tool_button(ui: &mut Ui, icon: Icon, caption: &str, enabled: bool) -> bool {
    let font = FontId::proportional(11.5);
    let text_w = ui.painter().layout_no_wrap(caption.to_string(), font.clone(), Color32::WHITE).size().x;
    let size = vec2((text_w + 14.0).max(52.0), 54.0);
    let (rect, resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    if !ui.is_rect_visible(rect) {
        return false;
    }
    let visuals = ui.visuals();
    if enabled && resp.hovered() {
        let fill = if resp.is_pointer_button_down_on() { visuals.widgets.active.bg_fill } else { visuals.widgets.hovered.bg_fill };
        ui.painter().rect_filled(rect, CornerRadius::same(3), fill);
        ui.painter().rect_stroke(rect, CornerRadius::same(3), visuals.widgets.hovered.bg_stroke, StrokeKind::Inside);
    }
    let icon_rect = Rect::from_center_size(pos2(rect.center().x, rect.top() + 20.0), vec2(26.0, 26.0));
    paint_icon(ui.painter(), icon_rect, icon, enabled, visuals.dark_mode);
    let color = if enabled { visuals.text_color() } else { visuals.weak_text_color() };
    ui.painter().text(pos2(rect.center().x, rect.bottom() - 4.0), Align2::CENTER_BOTTOM, caption, font, color);
    enabled && resp.clicked()
}

fn paint_icon(p: &Painter, r: Rect, icon: Icon, enabled: bool, dark: bool) {
    let grey = if dark { Color32::from_gray(110) } else { Color32::from_gray(175) };
    let tint = |c: Color32| if enabled { c } else { grey };
    let ink = tint(if dark { Color32::from_gray(210) } else { Color32::from_gray(70) });
    let c = r.center();
    let s = r.width() / 2.0;
    let at = |dx: f32, dy: f32| pos2(c.x + dx * s, c.y + dy * s);
    let tri = |a: Pos2, b: Pos2, d: Pos2, col: Color32| {
        p.add(Shape::convex_polygon(vec![a, b, d], col, Stroke::NONE));
    };
    let list_lines = |col: Color32| {
        for dy in [-0.55, 0.0, 0.55] {
            p.line_segment([at(0.05, dy), at(0.95, dy)], Stroke::new(2.0, col));
        }
    };
    match icon {
        Icon::Add => {
            p.circle_filled(c, s * 0.95, tint(GREEN));
            p.line_segment([at(-0.5, 0.0), at(0.5, 0.0)], Stroke::new(3.0, Color32::WHITE));
            p.line_segment([at(0.0, -0.5), at(0.0, 0.5)], Stroke::new(3.0, Color32::WHITE));
        }
        Icon::Resume => tri(at(-0.6, -0.8), at(0.8, 0.0), at(-0.6, 0.8), tint(GREEN)),
        Icon::Pause => {
            for dx in [-0.45, 0.45] {
                p.rect_filled(Rect::from_center_size(at(dx, 0.0), vec2(s * 0.5, s * 1.6)), CornerRadius::same(1), tint(ORANGE));
            }
        }
        Icon::StopAll => {
            p.rect_filled(Rect::from_center_size(c, vec2(s * 1.5, s * 1.5)), CornerRadius::same(2), tint(RED));
        }
        Icon::Delete => {
            let st = Stroke::new(3.5, tint(RED));
            p.line_segment([at(-0.65, -0.65), at(0.65, 0.65)], st);
            p.line_segment([at(-0.65, 0.65), at(0.65, -0.65)], st);
        }
        Icon::DeleteDone => {
            let check = Stroke::new(3.0, tint(GREEN));
            p.line_segment([at(-0.9, -0.1), at(-0.45, 0.4)], check);
            p.line_segment([at(-0.45, 0.4), at(0.25, -0.6)], check);
            let x = Stroke::new(2.5, tint(RED));
            p.line_segment([at(0.2, 0.15), at(0.9, 0.85)], x);
            p.line_segment([at(0.2, 0.85), at(0.9, 0.15)], x);
        }
        Icon::Options => {
            for i in 0..8 {
                let a = i as f32 * std::f32::consts::TAU / 8.0;
                let (sin, cos) = a.sin_cos();
                p.line_segment([at(cos * 0.55, sin * 0.55), at(cos * 0.95, sin * 0.95)], Stroke::new(4.0, ink));
            }
            p.circle_stroke(c, s * 0.5, Stroke::new(3.5, ink));
        }
        Icon::Scheduler => {
            p.circle_filled(c, s * 0.95, tint(ACCENT));
            p.circle_filled(c, s * 0.72, if dark { Color32::from_gray(40) } else { Color32::WHITE });
            p.line_segment([c, at(0.0, -0.5)], Stroke::new(2.0, ink));
            p.line_segment([c, at(0.38, 0.12)], Stroke::new(2.0, ink));
        }
        Icon::StartQueue => {
            tri(at(-0.95, -0.7), at(-0.15, 0.0), at(-0.95, 0.7), tint(GREEN));
            list_lines(ink);
        }
        Icon::StopQueue => {
            p.rect_filled(Rect::from_center_size(at(-0.55, 0.0), vec2(s * 0.8, s * 0.8)), CornerRadius::same(1), tint(RED));
            list_lines(ink);
        }
        Icon::Batch => {
            for (i, dy) in [-0.55f32, 0.0, 0.55].into_iter().enumerate() {
                p.circle_filled(at(-0.75, dy), 2.2, tint([ACCENT, GREEN, ORANGE][i]));
                p.line_segment([at(-0.35, dy), at(0.9, dy)], Stroke::new(2.0, ink));
            }
        }
    }
}

/// Small "document" badge in the category colour, drawn left of a file name.
pub fn file_badge(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(12.0, 15.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(2), color);
    let fold = 4.0;
    let corner = rect.right_top();
    p.add(Shape::convex_polygon(
        vec![corner, pos2(corner.x - fold, corner.y), pos2(corner.x, corner.y + fold)],
        ui.visuals().panel_fill,
        Stroke::NONE,
    ));
    for dy in [6.0, 9.0, 12.0] {
        p.line_segment([pos2(rect.left() + 2.5, rect.top() + dy), pos2(rect.right() - 2.5, rect.top() + dy)], Stroke::new(1.0, Color32::from_white_alpha(170)));
    }
}

/// The classic segment map: what has arrived (solid), what each connection
/// is working on (light), and where the segment boundaries are.
pub fn segment_bar(ui: &mut Ui, size: Option<u64>, downloaded: u64, segments: &[SegSnapshot], height: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    let p = ui.painter();
    let dark = ui.visuals().dark_mode;
    let bg = if dark { Color32::from_gray(45) } else { Color32::WHITE };
    p.rect_filled(rect, CornerRadius::same(2), bg);

    let total = size.filter(|s| *s > 0);
    let inner = rect.shrink(1.0);
    match total {
        Some(total) if !segments.is_empty() => {
            let x = |off: u64| inner.left() + inner.width() * (off.min(total) as f32 / total as f32);
            let pending = if dark { Color32::from_rgb(0x2A, 0x3F, 0x55) } else { Color32::from_rgb(0xDD, 0xEC, 0xFA) };
            for s in segments {
                let (x0, xp, x1) = (x(s.start), x(s.pos), x(s.end));
                if s.owned && x1 > xp {
                    p.rect_filled(Rect::from_min_max(pos2(xp, inner.top()), pos2(x1, inner.bottom())), CornerRadius::ZERO, pending);
                }
                if xp > x0 {
                    p.rect_filled(Rect::from_min_max(pos2(x0, inner.top()), pos2(xp.max(x0 + 1.0), inner.bottom())), CornerRadius::ZERO, ACCENT);
                }
            }
            // Boundaries on top so adjacent finished segments stay distinguishable.
            let tick = if dark { Color32::from_gray(20) } else { Color32::from_white_alpha(200) };
            for s in segments.iter().skip(1) {
                p.vline(x(s.start), inner.y_range(), Stroke::new(1.0, tick));
            }
        }
        Some(total) => {
            // Not running: all we know is how much is done in total.
            let frac = (downloaded as f32 / total as f32).clamp(0.0, 1.0);
            let done = Rect::from_min_max(inner.min, pos2(inner.left() + inner.width() * frac, inner.bottom()));
            p.rect_filled(done, CornerRadius::ZERO, ACCENT);
        }
        None => {}
    }
    p.rect_stroke(rect, CornerRadius::same(2), ui.visuals().widgets.noninteractive.bg_stroke, StrokeKind::Inside);
}

/// Marker for "this download is in a queue".
pub fn queue_dot(ui: &mut Ui) -> eframe::egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(12.0, 14.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 3.5, ACCENT);
    resp
}

/// Small triangle next to the column the list is sorted by.
pub fn sort_arrow(ui: &mut Ui, ascending: bool) {
    let (rect, _) = ui.allocate_exact_size(vec2(9.0, 12.0), Sense::hover());
    let (c, h) = (rect.center(), 3.5);
    let dir = if ascending { -1.0 } else { 1.0 };
    let points = vec![pos2(c.x - 4.0, c.y - dir * h), pos2(c.x + 4.0, c.y - dir * h), pos2(c.x, c.y + dir * h)];
    ui.painter().add(Shape::convex_polygon(points, ui.visuals().text_color(), Stroke::NONE));
}

/// Thin progress bar used inside table rows.
pub fn mini_progress(ui: &mut Ui, frac: f32, color: Color32, text: &str) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width().max(40.0), 15.0), Sense::hover());
    let p = ui.painter();
    let track = if ui.visuals().dark_mode { Color32::from_gray(55) } else { Color32::from_gray(228) };
    p.rect_filled(rect, CornerRadius::same(2), track);
    let mut fill = rect;
    fill.set_width(rect.width() * frac.clamp(0.0, 1.0));
    p.rect_filled(fill, CornerRadius::same(2), color);
    p.text(rect.center(), Align2::CENTER_CENTER, text, FontId::proportional(11.0), ui.visuals().strong_text_color());
}
