//! Image comparison viewer: a movable split divider, side by side or a grid of variants, all sharing one zoom and
//! pan. Mouse wheel zooms at the cursor, dragging pans (or moves the divider when grabbed near it), double-click
//! fits. Images of different resolutions (an original and its ×4 upscale) are mapped onto the same texture space,
//! so the original shows as its own texels.

use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke, TextureId, Vec2};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewState {
    /// Screen points per texel of the target resolution.
    pub zoom: f32,
    /// Centre of the view in texture coordinates (0..1).
    pub center: Vec2,
    /// Divider position across the image in split mode (0..1 of the cell width).
    pub split: f32,
    /// Zoom follows the cell size.
    pub fit: bool,
    dragging_split: bool,
}

impl Default for ViewState {
    fn default() -> Self {
        ViewState { zoom: 1.0, center: Vec2::splat(0.5), split: 0.5, fit: true, dragging_split: false }
    }
}

impl ViewState {
    pub fn reset(&mut self) {
        *self = ViewState { split: self.split, ..ViewState::default() };
    }
    /// 1 texel = 1 screen pixel.
    pub fn actual_pixels(&mut self, ppp: f32) {
        self.fit = false;
        self.zoom = 1.0 / ppp;
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// Two images in one cell, left of the divider and right of it.
    Split,
    /// Each image in its own cell, `n` per row.
    Grid(usize),
}

pub struct Pane<'a> {
    pub label: &'a str,
    pub tex: Option<TextureId>,
    /// Shown when `tex` is not ready.
    pub waiting: &'a str,
}

fn checker(painter: &egui::Painter, rect: Rect) {
    let s = 12.0;
    painter.rect_filled(rect, 0.0, Color32::from_gray(70));
    let mut y = rect.min.y;
    let mut row = 0;
    while y < rect.max.y {
        let mut x = rect.min.x + if row % 2 == 0 { 0.0 } else { s };
        while x < rect.max.x {
            let r = Rect::from_min_size(Pos2::new(x, y), Vec2::splat(s)).intersect(rect);
            painter.rect_filled(r, 0.0, Color32::from_gray(100));
            x += 2.0 * s;
        }
        y += s;
        row += 1;
    }
}

/// Screen rect and uv rect of the visible part of the texture in `cell` for the view.
fn mapping(cell: Rect, dims: Vec2, st: &ViewState) -> (Rect, Rect) {
    let span_uv = cell.size() / (dims * st.zoom); // uv per cell
    let uv_min = st.center - span_uv * 0.5;
    let uv = Rect::from_min_size(uv_min.to_pos2(), span_uv);
    let vis = uv.intersect(Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)));
    if vis.width() <= 0.0 || vis.height() <= 0.0 {
        return (Rect::NOTHING, Rect::NOTHING);
    }
    let to_screen = |p: Pos2| cell.min + (p - uv.min) / span_uv * cell.size();
    (Rect::from_min_max(to_screen(vis.min), to_screen(vis.max)), vis)
}

/// Draw the panes in `height` points and handle zoom/pan/divider input. `dims` is the target resolution in texels.
pub fn show(ui: &mut egui::Ui, st: &mut ViewState, dims: [u32; 2], panes: &[Pane], layout: Layout, height: f32) {
    let dims = Vec2::new(dims[0].max(1) as f32, dims[1].max(1) as f32);
    let (outer, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::click_and_drag());
    let painter = ui.painter_at(outer);
    painter.rect_filled(outer, 0.0, Color32::from_gray(25));
    let cells: Vec<Rect> = match layout {
        Layout::Split => vec![outer.shrink(2.0)],
        Layout::Grid(n) => {
            let n = n.max(1).min(panes.len().max(1));
            let rows = panes.len().div_ceil(n).max(1);
            let (cw, ch) = (outer.width() / n as f32, outer.height() / rows as f32);
            (0..panes.len()).map(|i| Rect::from_min_size(outer.min + Vec2::new((i % n) as f32 * cw, (i / n) as f32 * ch), Vec2::new(cw, ch)).shrink(3.0)).collect()
        }
    };
    let cell0 = cells.first().copied().unwrap_or(outer);
    if st.fit {
        st.zoom = (cell0.width() / dims.x).min(cell0.height() / dims.y);
        st.center = Vec2::splat(0.5);
    }

    // Input: double-click fits; wheel zooms at the cursor; drag pans or moves the divider.
    if resp.double_clicked() {
        st.fit = true;
    }
    if let Some(pos) = resp.hover_pos() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll != 0.0 {
            let cell = cells.iter().copied().find(|c| c.contains(pos)).unwrap_or(cell0);
            let before = st.center + (pos - cell.center()) / (dims * st.zoom);
            st.fit = false;
            st.zoom = (st.zoom * (scroll * 0.002).exp()).clamp(0.02, 64.0);
            let after = st.center + (pos - cell.center()) / (dims * st.zoom);
            st.center += before - after;
        }
    }
    if resp.drag_started() {
        st.dragging_split = layout == Layout::Split
            && resp.interact_pointer_pos().is_some_and(|p| (p.x - (cell0.min.x + cell0.width() * st.split)).abs() < 10.0);
    }
    if resp.dragged() {
        if st.dragging_split {
            if let Some(p) = resp.interact_pointer_pos() {
                st.split = ((p.x - cell0.min.x) / cell0.width()).clamp(0.0, 1.0);
            }
        } else {
            st.fit = false;
            st.center -= resp.drag_delta() / (dims * st.zoom);
        }
    }
    if !resp.dragged() {
        st.dragging_split = false;
    }
    st.center = st.center.clamp(Vec2::ZERO, Vec2::splat(1.0));

    let label = |p: &egui::Painter, pos: Pos2, align: egui::Align2, text: &str| {
        let galley_rect = p.text(pos, align, text, egui::FontId::proportional(13.0), Color32::WHITE);
        p.rect_filled(galley_rect.expand(3.0), 3.0, Color32::from_black_alpha(160));
        p.text(pos, align, text, egui::FontId::proportional(13.0), Color32::WHITE);
    };
    match layout {
        Layout::Split => {
            let cell = cell0;
            let (scr, uv) = mapping(cell, dims, st);
            let sp = cell.painter_clip(&painter);
            checker(&sp, scr);
            let split_x = cell.min.x + cell.width() * st.split;
            for (k, pane) in panes.iter().take(2).enumerate() {
                let clip = if k == 0 { Rect::from_min_max(cell.min, Pos2::new(split_x, cell.max.y)) } else { Rect::from_min_max(Pos2::new(split_x, cell.min.y), cell.max) };
                let p = painter.with_clip_rect(clip);
                match pane.tex {
                    Some(t) => {
                        p.image(t, scr, uv, Color32::WHITE);
                    }
                    None => {
                        p.text(clip.center(), egui::Align2::CENTER_CENTER, pane.waiting, egui::FontId::proportional(14.0), Color32::GRAY);
                    }
                }
            }
            painter.vline(split_x, cell.y_range(), Stroke::new(2.0, Color32::YELLOW));
            painter.circle_filled(Pos2::new(split_x, cell.center().y), 6.0, Color32::YELLOW);
            if let Some(a) = panes.first() {
                label(&painter, cell.min + Vec2::new(6.0, 6.0), egui::Align2::LEFT_TOP, a.label);
            }
            if let Some(b) = panes.get(1) {
                label(&painter, Pos2::new(cell.max.x - 6.0, cell.min.y + 6.0), egui::Align2::RIGHT_TOP, b.label);
            }
        }
        Layout::Grid(_) => {
            for (pane, cell) in panes.iter().zip(&cells) {
                let p = painter.with_clip_rect(*cell);
                let (scr, uv) = mapping(*cell, dims, st);
                checker(&p, scr);
                match pane.tex {
                    Some(t) => {
                        p.image(t, scr, uv, Color32::WHITE);
                    }
                    None => {
                        p.text(cell.center(), egui::Align2::CENTER_CENTER, pane.waiting, egui::FontId::proportional(14.0), Color32::GRAY);
                    }
                }
                p.rect_stroke(*cell, 0.0, Stroke::new(1.0, Color32::from_gray(60)), egui::StrokeKind::Inside);
                label(&p, cell.min + Vec2::new(6.0, 6.0), egui::Align2::LEFT_TOP, pane.label);
            }
        }
    }
    let texel = 1.0 / st.zoom.max(1e-6) / ui.ctx().pixels_per_point();
    let note = if texel >= 1.0 { format!("1 screen px = {texel:.1} texels") } else { format!("1 texel = {:.1} screen px", 1.0 / texel) };
    painter.text(outer.max - Vec2::new(6.0, 6.0), egui::Align2::RIGHT_BOTTOM, note, egui::FontId::proportional(12.0), Color32::from_gray(200));
}

trait PainterClip {
    fn painter_clip(&self, p: &egui::Painter) -> egui::Painter;
}

impl PainterClip for Rect {
    fn painter_clip(&self, p: &egui::Painter) -> egui::Painter {
        p.with_clip_rect(*self)
    }
}
