use image::RgbaImage;
use tiny_skia::{
    Color, FillRule, LineCap, LineJoin, Paint, PathBuilder, PixmapMut, Rect, Stroke, Transform,
};

use crate::capture::VirtualCapture;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ToolKind {
    Select,
    Pen,
    Line,
    Arrow,
    Rect,
    Blur,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SwatchColor {
    pub name: &'static str,
    pub rgba: [u8; 4],
}

pub const PALETTE: [SwatchColor; 6] = [
    SwatchColor { name: "Red", rgba: [239, 68, 68, 255] },
    SwatchColor { name: "Yellow", rgba: [250, 204, 21, 255] },
    SwatchColor { name: "Green", rgba: [34, 197, 94, 255] },
    SwatchColor { name: "Cyan", rgba: [34, 211, 238, 255] },
    SwatchColor { name: "White", rgba: [255, 255, 255, 255] },
    SwatchColor { name: "Black", rgba: [17, 24, 39, 255] },
];

pub const STROKE_WIDTHS: [f32; 3] = [2.5, 5.0, 9.0];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PixelRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl PixelRect {
    pub fn from_points(x0: i32, y0: i32, x1: i32, y1: i32) -> Self {
        let min_x = x0.min(x1);
        let min_y = y0.min(y1);
        let max_x = x0.max(x1);
        let max_y = y0.max(y1);
        Self {
            x: min_x,
            y: min_y,
            w: max_x - min_x,
            h: max_y - min_y,
        }
    }

    pub fn right(&self) -> i32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }

    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.right() && py >= self.y && py < self.bottom()
    }

    pub fn intersects(&self, other: PixelRect) -> bool {
        self.x < other.right()
            && self.right() > other.x
            && self.y < other.bottom()
            && self.bottom() > other.y
    }

    pub fn clamped_to(self, max_w: i32, max_h: i32) -> Self {
        let x0 = self.x.clamp(0, max_w);
        let y0 = self.y.clamp(0, max_h);
        let x1 = self.right().clamp(0, max_w);
        let y1 = self.bottom().clamp(0, max_h);
        Self {
            x: x0,
            y: y0,
            w: (x1 - x0).max(0),
            h: (y1 - y0).max(0),
        }
    }
}

/// Immutable annotation command stored in the undo history stack.
#[derive(Clone, Debug)]
pub enum Annotation {
    Pen {
        points: Vec<(f32, f32)>,
        color: [u8; 4],
        width: f32,
    },
    Line {
        start: (f32, f32),
        end: (f32, f32),
        color: [u8; 4],
        width: f32,
    },
    Arrow {
        start: (f32, f32),
        end: (f32, f32),
        color: [u8; 4],
        width: f32,
    },
    Rect {
        start: (f32, f32),
        end: (f32, f32),
        color: [u8; 4],
        width: f32,
    },
    Blur {
        start: (f32, f32),
        end: (f32, f32),
        radius: usize,
    },
}

/// Immutable command stack for annotations and undo support.
#[derive(Default)]
pub struct AnnotationStack {
    pub items: Vec<Annotation>,
    pub active: Option<Annotation>,
}

impl AnnotationStack {
    pub fn push(&mut self, ann: Annotation) {
        self.items.push(ann);
    }

    pub fn undo(&mut self) -> bool {
        if self.active.is_some() {
            self.active = None;
            return true;
        }
        self.items.pop().is_some()
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.active = None;
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty() && self.active.is_none()
    }
}

/// Reusable scratch buffers for zero-allocation O(W*H) privacy blur.
#[derive(Default)]
pub struct BlurScratch {
    buf_a: Vec<u8>,
    buf_b: Vec<u8>,
    col_r: Vec<u32>,
    col_g: Vec<u32>,
    col_b: Vec<u32>,
}

/// Extracts `sel` from `capture`'s per-monitor XRGB buffers into an `RgbaImage` and
/// composites all committed (and optional active) annotations.
pub fn composite_region(
    capture: &VirtualCapture,
    sel: PixelRect,
    items: &[Annotation],
    active: Option<&Annotation>,
) -> RgbaImage {
    let mut blur_scratch = BlurScratch::default();
    composite_region_with_scratch(capture, sel, items, active, &mut blur_scratch)
}

/// Same as `composite_region`, reusing `blur_scratch` across repeated calls.
pub fn composite_region_with_scratch(
    capture: &VirtualCapture,
    sel: PixelRect,
    items: &[Annotation],
    active: Option<&Annotation>,
    blur_scratch: &mut BlurScratch,
) -> RgbaImage {
    let sel = sel.clamped_to(capture.width as i32, capture.height as i32);
    let w = sel.w.max(1) as u32;
    let h = sel.h.max(1) as u32;
    let mut out = RgbaImage::new(w, h);

    if sel.w > 0 && sel.h > 0 {
        capture.copy_rect_to_rgba(sel, out.as_mut());
    }

    apply_annotations_onto_with_scratch(
        &mut out,
        sel.x as f32,
        sel.y as f32,
        items,
        active,
        blur_scratch,
    );
    out
}

/// Applies committed and optional active annotations onto an existing `RgbaImage`
/// using `blur_scratch` to avoid per-frame allocations during Blur dragging.
pub fn apply_annotations_onto_with_scratch(
    target: &mut RgbaImage,
    offset_x: f32,
    offset_y: f32,
    items: &[Annotation],
    active: Option<&Annotation>,
    blur_scratch: &mut BlurScratch,
) {
    let w = target.width();
    let h = target.height();
    if w == 0 || h == 0 {
        return;
    }

    for ann in items.iter().chain(active.into_iter()) {
        match ann {
            Annotation::Blur { start, end, radius } => {
                let x0 = (start.0.min(end.0) - offset_x).floor() as i32;
                let y0 = (start.1.min(end.1) - offset_y).floor() as i32;
                let x1 = (start.0.max(end.0) - offset_x).ceil() as i32;
                let y1 = (start.1.max(end.1) - offset_y).ceil() as i32;
                let rect = PixelRect::from_points(x0, y0, x1, y1).clamped_to(w as i32, h as i32);
                if rect.w > 1 && rect.h > 1 {
                    apply_privacy_blur_with_scratch(target, rect, *radius, blur_scratch);
                }
            }
            _ => {
                if let Some(mut pixmap) = PixmapMut::from_bytes(target.as_mut(), w, h) {
                    draw_vector_annotation(&mut pixmap, ann, offset_x, offset_y);
                }
            }
        }
    }
}

/// Draws a single line segment onto `target` with round caps (used for O(1) incremental Pen dragging).
pub fn draw_pen_segment(
    target: &mut RgbaImage,
    p0: (f32, f32),
    p1: (f32, f32),
    offset_x: f32,
    offset_y: f32,
    color: [u8; 4],
    width: f32,
) {
    let (w, h) = target.dimensions();
    if w == 0 || h == 0 {
        return;
    }
    if let Some(mut pixmap) = PixmapMut::from_bytes(target.as_mut(), w, h) {
        let paint = make_paint(color);
        let mut pb = PathBuilder::new();
        pb.move_to(p0.0 - offset_x, p0.1 - offset_y);
        pb.line_to(p1.0 - offset_x, p1.1 - offset_y);
        if let Some(path) = pb.finish() {
            let stroke = Stroke {
                width,
                line_cap: LineCap::Round,
                line_join: LineJoin::Round,
                ..Stroke::default()
            };
            pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
    }
}

/// Convenience wrapper for `apply_privacy_blur_with_scratch`.
#[allow(dead_code)]
pub fn apply_privacy_blur(img: &mut RgbaImage, rect: PixelRect, radius: usize) {
    let mut scratch = BlurScratch::default();
    apply_privacy_blur_with_scratch(img, rect, radius, &mut scratch);
}

/// Single-threaded, cache-linear O(W*H) 2-pass separable box blur over `rect` inside `img`,
/// reusing `scratch` buffers with zero per-frame allocations.
pub fn apply_privacy_blur_with_scratch(
    img: &mut RgbaImage,
    rect: PixelRect,
    radius: usize,
    scratch: &mut BlurScratch,
) {
    let img_w = img.width() as i32;
    let img_h = img.height() as i32;
    let rect = rect.clamped_to(img_w, img_h);
    if rect.w <= 1 || rect.h <= 1 {
        return;
    }

    let rw = rect.w as usize;
    let rh = rect.h as usize;
    let rx = rect.x as usize;
    let ry = rect.y as usize;
    let row_bytes = rw * 4;
    let needed_bytes = rh * row_bytes;
    let img_stride = img.width() as usize * 4;
    let r = radius.max(2);
    let win = (2 * r + 1) as u32;

    scratch.buf_a.resize(needed_bytes, 0);
    scratch.buf_b.resize(needed_bytes, 0);
    scratch.col_r.resize(rw, 0);
    scratch.col_g.resize(rw, 0);
    scratch.col_b.resize(rw, 0);

    let buf_a = &mut scratch.buf_a[..needed_bytes];
    let buf_b = &mut scratch.buf_b[..needed_bytes];
    let col_r = &mut scratch.col_r[..rw];
    let col_g = &mut scratch.col_g[..rw];
    let col_b = &mut scratch.col_b[..rw];

    {
        let raw = img.as_raw();
        for row in 0..rh {
            let src_start = (ry + row) * img_stride + rx * 4;
            let dst_start = row * row_bytes;
            buf_a[dst_start..dst_start + row_bytes]
                .copy_from_slice(&raw[src_start..src_start + row_bytes]);
        }
    }

    // Two separable H+V passes yield a smooth quadratic/Gaussian-like privacy blur.
    for _pass in 0..2 {
        // Horizontal sliding-window pass: buf_a -> buf_b
        for y in 0..rh {
            let row_off = y * row_bytes;
            let src_row = &buf_a[row_off..row_off + row_bytes];
            let dst_row = &mut buf_b[row_off..row_off + row_bytes];

            let right_init = r.min(rw - 1);
            let mut sum_r = src_row[0] as u32 * r as u32;
            let mut sum_g = src_row[1] as u32 * r as u32;
            let mut sum_b = src_row[2] as u32 * r as u32;

            for i in 0..=right_init {
                let idx = i * 4;
                sum_r += src_row[idx] as u32;
                sum_g += src_row[idx + 1] as u32;
                sum_b += src_row[idx + 2] as u32;
            }
            if right_init < r {
                let pad = (r - right_init) as u32;
                let last = (rw - 1) * 4;
                sum_r += src_row[last] as u32 * pad;
                sum_g += src_row[last + 1] as u32 * pad;
                sum_b += src_row[last + 2] as u32 * pad;
            }

            for x in 0..rw {
                let o = x * 4;
                dst_row[o] = (sum_r / win) as u8;
                dst_row[o + 1] = (sum_g / win) as u8;
                dst_row[o + 2] = (sum_b / win) as u8;
                dst_row[o + 3] = 255;

                let sub_x = x.saturating_sub(r);
                let add_x = (x + r + 1).min(rw - 1);
                let si = sub_x * 4;
                let ai = add_x * 4;
                sum_r = sum_r + src_row[ai] as u32 - src_row[si] as u32;
                sum_g = sum_g + src_row[ai + 1] as u32 - src_row[si + 1] as u32;
                sum_b = sum_b + src_row[ai + 2] as u32 - src_row[si + 2] as u32;
            }
        }

        // Row-major O(W*H) vertical sliding-window pass: buf_b -> buf_a
        let first_row = &buf_b[..row_bytes];
        for x in 0..rw {
            let o = x * 4;
            col_r[x] = first_row[o] as u32 * r as u32;
            col_g[x] = first_row[o + 1] as u32 * r as u32;
            col_b[x] = first_row[o + 2] as u32 * r as u32;
        }
        let bottom_init = r.min(rh - 1);
        for y in 0..=bottom_init {
            let row = &buf_b[y * row_bytes..(y + 1) * row_bytes];
            for x in 0..rw {
                let o = x * 4;
                col_r[x] += row[o] as u32;
                col_g[x] += row[o + 1] as u32;
                col_b[x] += row[o + 2] as u32;
            }
        }
        if bottom_init < r {
            let pad = (r - bottom_init) as u32;
            let last_row = &buf_b[(rh - 1) * row_bytes..rh * row_bytes];
            for x in 0..rw {
                let o = x * 4;
                col_r[x] += last_row[o] as u32 * pad;
                col_g[x] += last_row[o + 1] as u32 * pad;
                col_b[x] += last_row[o + 2] as u32 * pad;
            }
        }

        for y in 0..rh {
            let dst_row = &mut buf_a[y * row_bytes..(y + 1) * row_bytes];
            for x in 0..rw {
                let o = x * 4;
                dst_row[o] = (col_r[x] / win) as u8;
                dst_row[o + 1] = (col_g[x] / win) as u8;
                dst_row[o + 2] = (col_b[x] / win) as u8;
                dst_row[o + 3] = 255;
            }

            if y + 1 < rh {
                let sub_y = y.saturating_sub(r);
                let add_y = (y + r + 1).min(rh - 1);
                let sub_row = &buf_b[sub_y * row_bytes..(sub_y + 1) * row_bytes];
                let add_row = &buf_b[add_y * row_bytes..(add_y + 1) * row_bytes];
                for x in 0..rw {
                    let o = x * 4;
                    col_r[x] = col_r[x] + add_row[o] as u32 - sub_row[o] as u32;
                    col_g[x] = col_g[x] + add_row[o + 1] as u32 - sub_row[o + 1] as u32;
                    col_b[x] = col_b[x] + add_row[o + 2] as u32 - sub_row[o + 2] as u32;
                }
            }
        }
    }

    let img_raw = img.as_mut();
    for row in 0..rh {
        let dst_start = (ry + row) * img_stride + rx * 4;
        let src_start = row * row_bytes;
        img_raw[dst_start..dst_start + row_bytes]
            .copy_from_slice(&buf_a[src_start..src_start + row_bytes]);
    }
}

fn make_paint(rgba: [u8; 4]) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color(Color::from_rgba8(rgba[0], rgba[1], rgba[2], rgba[3]));
    paint.anti_alias = true;
    paint
}

/// Renders a single vector annotation (`Pen`, `Line`, `Arrow`, `Rect`) onto `pixmap`.
pub fn draw_vector_annotation(
    pixmap: &mut PixmapMut,
    ann: &Annotation,
    offset_x: f32,
    offset_y: f32,
) {
    match ann {
        Annotation::Pen { points, color, width } => {
            if points.is_empty() {
                return;
            }
            let paint = make_paint(*color);
            let mut pb = PathBuilder::new();
            let p0 = (points[0].0 - offset_x, points[0].1 - offset_y);
            pb.move_to(p0.0, p0.1);
            if points.len() == 1 {
                pb.line_to(p0.0 + 0.1, p0.1 + 0.1);
            } else if points.len() == 2 {
                pb.line_to(points[1].0 - offset_x, points[1].1 - offset_y);
            } else {
                for i in 1..points.len() - 1 {
                    let cx = points[i].0 - offset_x;
                    let cy = points[i].1 - offset_y;
                    let nx = points[i + 1].0 - offset_x;
                    let ny = points[i + 1].1 - offset_y;
                    pb.quad_to(cx, cy, (cx + nx) * 0.5, (cy + ny) * 0.5);
                }
                let last = points[points.len() - 1];
                pb.line_to(last.0 - offset_x, last.1 - offset_y);
            }
            if let Some(path) = pb.finish() {
                let stroke = Stroke {
                    width: *width,
                    line_cap: LineCap::Round,
                    line_join: LineJoin::Round,
                    ..Stroke::default()
                };
                pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
            }
        }
        Annotation::Line { start, end, color, width } => {
            let paint = make_paint(*color);
            let mut pb = PathBuilder::new();
            pb.move_to(start.0 - offset_x, start.1 - offset_y);
            pb.line_to(end.0 - offset_x, end.1 - offset_y);
            if let Some(path) = pb.finish() {
                let stroke = Stroke {
                    width: *width,
                    line_cap: LineCap::Round,
                    ..Stroke::default()
                };
                pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
            }
        }
        Annotation::Arrow { start, end, color, width } => {
            let sx = start.0 - offset_x;
            let sy = start.1 - offset_y;
            let ex = end.0 - offset_x;
            let ey = end.1 - offset_y;
            let dx = ex - sx;
            let dy = ey - sy;
            let len = (dx * dx + dy * dy).sqrt();
            if len < 2.0 {
                return;
            }
            let ux = dx / len;
            let uy = dy / len;
            let px = -uy;
            let py = ux;

            let head_len = (*width * 3.6).clamp(14.0, 32.0).min(len * 0.65);
            let head_half_w = head_len * 0.48;
            let shaft_end_x = ex - ux * (head_len * 0.68);
            let shaft_end_y = ey - uy * (head_len * 0.68);

            let paint = make_paint(*color);
            let mut shaft_pb = PathBuilder::new();
            shaft_pb.move_to(sx, sy);
            shaft_pb.line_to(shaft_end_x, shaft_end_y);
            if let Some(path) = shaft_pb.finish() {
                let stroke = Stroke {
                    width: *width,
                    line_cap: LineCap::Round,
                    ..Stroke::default()
                };
                pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
            }

            let base_x = ex - ux * head_len;
            let base_y = ey - uy * head_len;
            let notch_x = ex - ux * (head_len * 0.78);
            let notch_y = ey - uy * (head_len * 0.78);

            let mut head_pb = PathBuilder::new();
            head_pb.move_to(ex, ey);
            head_pb.line_to(base_x + px * head_half_w, base_y + py * head_half_w);
            head_pb.line_to(notch_x, notch_y);
            head_pb.line_to(base_x - px * head_half_w, base_y - py * head_half_w);
            head_pb.close();
            if let Some(head_path) = head_pb.finish() {
                pixmap.fill_path(&head_path, &paint, FillRule::Winding, Transform::identity(), None);
            }
        }
        Annotation::Rect { start, end, color, width } => {
            let x0 = start.0.min(end.0) - offset_x;
            let y0 = start.1.min(end.1) - offset_y;
            let x1 = start.0.max(end.0) - offset_x;
            let y1 = start.1.max(end.1) - offset_y;
            if let Some(rect) = Rect::from_ltrb(x0, y0, x1.max(x0 + 1.0), y1.max(y0 + 1.0)) {
                let path = PathBuilder::from_rect(rect);
                let paint = make_paint(*color);
                let stroke = Stroke {
                    width: *width,
                    line_join: LineJoin::Round,
                    ..Stroke::default()
                };
                pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
            }
        }
        Annotation::Blur { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::CapturedMonitor;
    use image::Rgba;

    #[test]
    fn undo_stack_behaves_like_lifo() {
        let mut stack = AnnotationStack::default();
        assert!(stack.is_empty());
        stack.push(Annotation::Line {
            start: (0.0, 0.0),
            end: (10.0, 10.0),
            color: PALETTE[0].rgba,
            width: 3.0,
        });
        assert!(!stack.is_empty());
        assert!(stack.undo());
        assert!(stack.is_empty());
        assert!(!stack.undo());
    }

    #[test]
    fn privacy_blur_softens_sharp_edge_with_linear_pass() {
        let mut img = RgbaImage::from_pixel(40, 40, Rgba([0, 0, 0, 255]));
        for y in 0..40 {
            for x in 20..40 {
                img.put_pixel(x, y, Rgba([255, 255, 255, 255]));
            }
        }
        apply_privacy_blur(
            &mut img,
            PixelRect {
                x: 5,
                y: 5,
                w: 30,
                h: 30,
            },
            6,
        );
        let p = img.get_pixel(20, 20)[0];
        assert!(p > 20 && p < 235, "expected blurred mid-tone, got {p}");
        assert_eq!(img.get_pixel(0, 0)[0], 0);
        assert_eq!(img.get_pixel(39, 39)[0], 255);
    }

    #[test]
    fn composite_region_draws_arrow_onto_cropped_output() {
        let px = (20u32 << 16) | (20u32 << 8) | 20u32;
        let cap = VirtualCapture {
            width: 100,
            height: 100,
            monitors: vec![CapturedMonitor {
                screen_x: 0,
                screen_y: 0,
                bounds: PixelRect {
                    x: 0,
                    y: 0,
                    w: 100,
                    h: 100,
                },
                pixels_xrgb: vec![px; 100 * 100],
            }],
        };
        let ann = Annotation::Arrow {
            start: (20.0, 50.0),
            end: (80.0, 50.0),
            color: [239, 68, 68, 255],
            width: 5.0,
        };
        let out = composite_region(
            &cap,
            PixelRect {
                x: 10,
                y: 10,
                w: 80,
                h: 80,
            },
            &[ann],
            None,
        );
        let px = out.get_pixel(40, 40);
        assert!(px[0] > 200 && px[1] < 100, "expected red stroke, got {px:?}");
    }
}
