use gpui::{
    Bounds, Hsla, Pixels, Rgba, Size, Window, fill, hsla, linear_color_stop, linear_gradient,
    point, px, size,
};

use rayon::prelude::*;

use crate::theme::Theme;

/// A rounded rectangle in window coordinates (logical pixels).
#[derive(Clone, Copy)]
pub struct Shape {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub radius: f32,
    pub color: Hsla,
}

impl Shape {
    fn circle(cx: f32, cy: f32, r: f32, color: Hsla) -> Self {
        Self {
            x: cx - r,
            y: cy - r,
            w: r * 2.,
            h: r * 2.,
            radius: r,
            color,
        }
    }
}

/// The scene behind the glass, described as data. The GPU paints it for the
/// window; glass layers rasterize the region under themselves on the CPU,
/// because GPUI has no backdrop-filter or render-to-texture.
pub struct Backdrop {
    width: f32,
    height: f32,
    top: Hsla,
    bottom: Hsla,
    shapes: Vec<Shape>,
}

/// A region of the backdrop at `scale` pixels per logical pixel: opaque
/// sRGB, row-major. `(x0, y0)` is its top-left corner in the window, in
/// raster pixels.
pub struct Raster {
    pub x0: usize,
    pub y0: usize,
    pub width: usize,
    pub height: usize,
    pub scale: f32,
    pub pixels: Vec<[f32; 3]>,
}

/// A shape converted to raster pixels and clipped to one region.
struct RasterShape {
    x0: usize,
    x1: usize,
    y0: usize,
    y1: usize,
    cx: f32,
    cy: f32,
    hw: f32,
    hh: f32,
    r: f32,
    color: Rgba,
}

impl Backdrop {
    pub fn animated(viewport: Size<Pixels>, t: f32, theme: &Theme) -> Self {
        let w = f32::from(viewport.width);
        let h = f32::from(viewport.height);
        let min = w.min(h);
        let mut shapes = Vec::new();

        // (hue, radius as fraction of the short side, speed, phase)
        const BLOBS: [(f32, f32, f32, f32); 6] = [
            (0.92, 0.26, 0.21, 0.0),
            (0.08, 0.20, 0.17, 1.9),
            (0.52, 0.24, 0.13, 3.7),
            (0.76, 0.22, 0.19, 5.1),
            (0.30, 0.16, 0.23, 2.6),
            (0.60, 0.30, 0.09, 4.4),
        ];
        for (hue, radius, speed, phase) in BLOBS {
            let cx = w * (0.5 + 0.38 * (t * speed + phase).sin());
            let cy = h * (0.5 + 0.34 * (t * speed * 1.3 + phase * 1.7).cos());
            shapes.push(Shape::circle(
                cx,
                cy,
                radius * min,
                hsla(hue, theme.blob_saturation, theme.blob_lightness, 1.),
            ));
        }

        // A scrolling band of stripes: high-frequency detail makes the
        // refraction at the glass edges easy to see.
        let count = 28;
        let pitch = w / count as f32;
        let offset = (t * 24.) % pitch;
        for i in -1..=count {
            shapes.push(Shape {
                x: i as f32 * pitch + offset,
                y: h * 0.42,
                w: pitch * 0.42,
                h: h * 0.16,
                radius: 3.,
                color: theme.ink,
            });
        }

        // Two rows of dots near the top.
        let spacing = 36.;
        for row in 0..2 {
            let y = h * 0.2 + row as f32 * spacing;
            let shift = if row == 0 { 0. } else { spacing / 2. };
            let mut x = shift;
            while x < w + spacing {
                shapes.push(Shape::circle(x, y, 5., theme.ink.opacity(1.15)));
                x += spacing;
            }
        }

        Self {
            width: w,
            height: h,
            top: theme.backdrop_top,
            bottom: theme.backdrop_bottom,
            shapes,
        }
    }

    /// Paints the backdrop with the GPU, covering the whole window.
    pub fn paint(&self, window: &mut Window) {
        window.paint_quad(fill(
            rect(0., 0., self.width, self.height),
            linear_gradient(
                180.,
                linear_color_stop(self.top, 0.),
                linear_color_stop(self.bottom, 1.),
            ),
        ));
        for shape in &self.shapes {
            window.paint_quad(
                fill(rect(shape.x, shape.y, shape.w, shape.h), shape.color)
                    .corner_radii(px(shape.radius)),
            );
        }
    }

    /// Rasterizes the part of the backdrop inside `bounds` at `scale`
    /// pixels per logical pixel, clamped to the window. Rows are drawn in
    /// parallel. Returns `None` if nothing is left.
    pub fn rasterize(&self, bounds: Bounds<Pixels>, scale: f32) -> Option<Raster> {
        let max_x = (self.width * scale).ceil();
        let max_y = (self.height * scale).ceil();
        let clamp_x = |x: f32| x.clamp(0., max_x) as usize;
        let clamp_y = |y: f32| y.clamp(0., max_y) as usize;
        let x0 = clamp_x((f32::from(bounds.origin.x) * scale).floor());
        let y0 = clamp_y((f32::from(bounds.origin.y) * scale).floor());
        let x1 = clamp_x((f32::from(bounds.right()) * scale).ceil());
        let y1 = clamp_y((f32::from(bounds.bottom()) * scale).ceil());
        if x0 >= x1 || y0 >= y1 {
            return None;
        }
        let (width, height) = (x1 - x0, y1 - y0);
        let top = Rgba::from(self.top);
        let bottom = Rgba::from(self.bottom);

        let shapes: Vec<RasterShape> = self
            .shapes
            .iter()
            .filter_map(|shape| {
                let (x, y) = (shape.x * scale, shape.y * scale);
                let (w, h) = (shape.w * scale, shape.h * scale);
                let shape_x0 = clamp_x(x.floor()).max(x0);
                let shape_y0 = clamp_y(y.floor()).max(y0);
                let shape_x1 = clamp_x((x + w).ceil()).min(x1);
                let shape_y1 = clamp_y((y + h).ceil()).min(y1);
                (shape_x0 < shape_x1 && shape_y0 < shape_y1).then(|| RasterShape {
                    x0: shape_x0,
                    x1: shape_x1,
                    y0: shape_y0,
                    y1: shape_y1,
                    cx: x + w / 2.,
                    cy: y + h / 2.,
                    hw: w / 2.,
                    hh: h / 2.,
                    r: (shape.radius * scale).min(w / 2.).min(h / 2.),
                    color: Rgba::from(shape.color),
                })
            })
            .collect();

        let mut pixels = vec![[0.; 3]; width * height];
        pixels
            .par_chunks_mut(width)
            .enumerate()
            .for_each(|(row, line)| {
                let y = y0 + row;
                let t = (y as f32 + 0.5) / scale / self.height;
                line.fill([
                    top.r + (bottom.r - top.r) * t,
                    top.g + (bottom.g - top.g) * t,
                    top.b + (bottom.b - top.b) * t,
                ]);
                for shape in &shapes {
                    if y < shape.y0 || y >= shape.y1 {
                        continue;
                    }
                    let qy = y as f32 + 0.5 - shape.cy;
                    let color = shape.color;
                    for x in shape.x0..shape.x1 {
                        let qx = x as f32 + 0.5 - shape.cx;
                        let d = rounded_rect_sdf(qx, qy, shape.hw, shape.hh, shape.r);
                        let a = (0.5 - d).clamp(0., 1.) * color.a;
                        if a > 0. {
                            let p = &mut line[x - x0];
                            p[0] += (color.r - p[0]) * a;
                            p[1] += (color.g - p[1]) * a;
                            p[2] += (color.b - p[2]) * a;
                        }
                    }
                }
            });

        Some(Raster {
            x0,
            y0,
            width,
            height,
            scale,
            pixels,
        })
    }
}

/// Signed distance from `(qx, qy)`, relative to the center, to a rounded
/// rect with half extents `(hw, hh)` and corner radius `r`. Negative inside.
pub fn rounded_rect_sdf(qx: f32, qy: f32, hw: f32, hh: f32, r: f32) -> f32 {
    let kx = qx.abs() - hw + r;
    let ky = qy.abs() - hh + r;
    kx.max(0.).hypot(ky.max(0.)) + kx.max(ky).min(0.) - r
}

pub fn rect(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
    Bounds::new(point(px(x), px(y)), size(px(w), px(h)))
}
