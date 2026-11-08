//! Minimal hand-painted line/area graphs (no plotting crate needed).

use eframe::egui::{self, Color32, Mesh, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Vec2, epaint::Vertex};

use crate::sampler::{HISTORY, History};

pub const CPU: Color32 = Color32::from_rgb(0x3b, 0x8e, 0xea);
pub const MEM: Color32 = Color32::from_rgb(0xa3, 0x5c, 0xe0);
pub const DISK: Color32 = Color32::from_rgb(0x3c, 0xb3, 0x71);
pub const NET: Color32 = Color32::from_rgb(0xe8, 0x8a, 0x2c);
pub const GPU: Color32 = Color32::from_rgb(0xdd, 0x4b, 0x5a);

pub struct Series<'a> {
    pub data: &'a History,
    pub color: Color32,
    pub dashed: bool,
}

impl<'a> Series<'a> {
    pub fn new(data: &'a History, color: Color32) -> Self {
        Series { data, color, dashed: false }
    }
    pub fn dashed(mut self) -> Self {
        self.dashed = true;
        self
    }
}

/// Draw a graph filling `size`. `max` is the y-axis ceiling (e.g. 100 for percentages).
pub fn graph(ui: &mut egui::Ui, size: Vec2, series: &[Series], max: f32, grid: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let painter = ui.painter_at(rect);
    let visuals = ui.visuals();
    let bg = if visuals.dark_mode { Color32::from_gray(22) } else { Color32::from_gray(250) };
    let base = series.first().map_or(CPU, |s| s.color);
    painter.rect_filled(rect, 2.0, bg);

    if grid {
        let gc = base.gamma_multiply(if visuals.dark_mode { 0.18 } else { 0.22 });
        let gs = Stroke::new(1.0, gc);
        for i in 1..4 {
            let y = rect.top() + rect.height() * i as f32 / 4.0;
            painter.hline(rect.x_range(), y, gs);
        }
        for i in 1..12 {
            let x = rect.left() + rect.width() * i as f32 / 12.0;
            painter.vline(x, rect.y_range(), gs);
        }
    }

    let max = if max <= 0.0 { 1.0 } else { max };
    let step = rect.width() / (HISTORY - 1) as f32;
    for s in series {
        let n = s.data.len();
        if n < 2 {
            continue;
        }
        let x0 = rect.right() - step * (n - 1) as f32;
        let pts: Vec<Pos2> = s
            .data
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let y = rect.bottom() - (v / max).clamp(0.0, 1.0) * (rect.height() - 1.0);
                Pos2::new(x0 + step * i as f32, y)
            })
            .collect();

        if !s.dashed {
            let fill = s.color.gamma_multiply(0.22);
            let mut mesh = Mesh::default();
            for p in &pts {
                mesh.vertices.push(Vertex { pos: *p, uv: egui::epaint::WHITE_UV, color: fill });
                mesh.vertices.push(Vertex {
                    pos: Pos2::new(p.x, rect.bottom()),
                    uv: egui::epaint::WHITE_UV,
                    color: fill,
                });
            }
            for i in 0..(pts.len() as u32 - 1) {
                let a = i * 2;
                mesh.add_triangle(a, a + 1, a + 2);
                mesh.add_triangle(a + 1, a + 3, a + 2);
            }
            painter.add(Shape::mesh(mesh));
            painter.add(Shape::line(pts, Stroke::new(1.5, s.color)));
        } else {
            painter.extend(Shape::dashed_line(&pts, Stroke::new(1.2, s.color), 4.0, 3.0));
        }
    }
    painter.rect_stroke(rect, 2.0, Stroke::new(1.0, base.gamma_multiply(0.6)), StrokeKind::Inside);
    resp
}

/// Thin horizontal usage bar, e.g. memory composition.
pub fn bar(ui: &mut egui::Ui, width: f32, parts: &[(f32, Color32)], total: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 18.0), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, color.gamma_multiply(0.08));
    let mut x = rect.left();
    for (v, c) in parts {
        let w = if total > 0.0 { rect.width() * (v / total).clamp(0.0, 1.0) } else { 0.0 };
        painter.rect_filled(Rect::from_min_max(Pos2::new(x, rect.top()), Pos2::new(x + w, rect.bottom())), 0.0, *c);
        x += w;
    }
    painter.rect_stroke(rect, 2.0, Stroke::new(1.0, color.gamma_multiply(0.6)), StrokeKind::Inside);
}

/// Graph over arbitrary-length series (all the same length, oldest first). Returns the response
/// and the index of the point under the mouse, for tooltips.
pub fn graph_series(
    ui: &mut egui::Ui,
    size: Vec2,
    series: &[(&[f32], Color32, bool)],
    max: f32,
) -> (egui::Response, Option<usize>) {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    let n = series.iter().map(|s| s.0.len()).max().unwrap_or(0);
    if !ui.is_rect_visible(rect) || n == 0 {
        return (resp, None);
    }
    let painter = ui.painter_at(rect);
    let dark = ui.visuals().dark_mode;
    let base = series.first().map_or(CPU, |s| s.1);
    painter.rect_filled(rect, 2.0, if dark { Color32::from_gray(22) } else { Color32::from_gray(250) });
    let gs = Stroke::new(1.0, base.gamma_multiply(if dark { 0.18 } else { 0.22 }));
    for i in 1..4 {
        painter.hline(rect.x_range(), rect.top() + rect.height() * i as f32 / 4.0, gs);
    }
    let max = if max <= 0.0 { 1.0 } else { max };
    let step = rect.width() / (n.max(2) - 1) as f32;
    let to_pt = |i: usize, v: f32| {
        Pos2::new(rect.left() + step * i as f32, rect.bottom() - (v / max).clamp(0.0, 1.0) * (rect.height() - 1.0))
    };
    for (data, color, dashed) in series {
        if data.len() < 2 {
            continue;
        }
        let pts: Vec<Pos2> = data.iter().enumerate().map(|(i, v)| to_pt(i, *v)).collect();
        if *dashed {
            painter.extend(Shape::dashed_line(&pts, Stroke::new(1.2, *color), 4.0, 3.0));
        } else {
            let fill = color.gamma_multiply(0.22);
            let mut mesh = Mesh::default();
            for p in &pts {
                mesh.vertices.push(Vertex { pos: *p, uv: egui::epaint::WHITE_UV, color: fill });
                mesh.vertices.push(Vertex {
                    pos: Pos2::new(p.x, rect.bottom()),
                    uv: egui::epaint::WHITE_UV,
                    color: fill,
                });
            }
            for i in 0..(pts.len() as u32 - 1) {
                let a = i * 2;
                mesh.add_triangle(a, a + 1, a + 2);
                mesh.add_triangle(a + 1, a + 3, a + 2);
            }
            painter.add(Shape::mesh(mesh));
            painter.add(Shape::line(pts, Stroke::new(1.5, *color)));
        }
    }
    let hovered = resp.hover_pos().map(|p| (((p.x - rect.left()) / step).round().max(0.0) as usize).min(n - 1));
    if let Some(i) = hovered {
        let x = rect.left() + step * i as f32;
        painter.vline(x, rect.y_range(), Stroke::new(1.0, ui.visuals().strong_text_color().gamma_multiply(0.6)));
    }
    painter.rect_stroke(rect, 2.0, Stroke::new(1.0, base.gamma_multiply(0.6)), StrokeKind::Inside);
    (resp, hovered)
}
