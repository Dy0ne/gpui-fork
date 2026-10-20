use std::{
    rc::Rc,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use rayon::{ThreadPool, ThreadPoolBuilder, prelude::*};

use gpui::{
    AnyElement, App, BackdropEffect, Bounds, BoxShadow, Corners, Div, ElementId, Hsla,
    InteractiveElement, Interactivity, IntoElement, ParentElement, Pixels, RenderImage, RenderOnce,
    Rgba, StyleRefinement, Styled, Window, canvas, div, hsla, linear_color_stop, linear_gradient,
    prelude::FluentBuilder, px,
};

use crate::backdrop::{Backdrop, Raster, rounded_rect_sdf};

/// Worker threads for rendering glass. Measured on an 8-core Mac with the
/// demo's five panels: 2 threads render them in ~4.2 ms at ~56% process CPU,
/// while rayon's default 8 take ~3.3 ms at ~145%, mostly workers spinning
/// between the many small jobs.
const THREADS: usize = 2;

/// The glass rendering pool, or `None` if its threads couldn't be started
/// (rendering then runs on the calling thread's default pool).
fn pool() -> Option<&'static ThreadPool> {
    static POOL: OnceLock<Option<ThreadPool>> = OnceLock::new();
    POOL.get_or_init(|| {
        ThreadPoolBuilder::new()
            .num_threads(THREADS)
            .thread_name(|i| format!("glass-{i}"))
            .build()
            .ok()
    })
    .as_ref()
}

/// Total nanoseconds spent rendering glass images, for profiling. Readers
/// swap it back to zero.
pub static RENDER_NANOS: AtomicU64 = AtomicU64::new(0);

/// Pixels per logical pixel that glass is rendered at; the GPU scales the
/// image to the display. Frosted glass has no fine detail left to lose, so
/// it renders at half resolution, a quarter of the pixels.
fn resolution(glass: &GlassStyle) -> f32 {
    if glass.blur >= 3. { 0.5 } else { 1. }
}

/// Optical parameters of a glass layer.
#[derive(Clone, Copy)]
pub struct GlassStyle {
    /// Frost: Gaussian sigma applied to the backdrop, in pixels.
    pub blur: f32,
    /// Magnification of the backdrop, around the glass center.
    pub magnify: f32,
    /// How far the rim bends light: at the very edge the glass shows the
    /// backdrop from `refraction * rim * 2` pixels outside itself.
    pub refraction: f32,
    /// Width of the curved rim; the interior inside it is flat.
    pub rim: f32,
    /// Per-channel spread of the refraction, for color fringes at the rim.
    pub aberration: f32,
    pub tint: Hsla,
    /// Color of the rim light, glow, borders and sheen: 1 is white, 0 is
    /// none, -1 is black.
    pub rim_tone: f32,
}

impl GlassStyle {
    /// The rim tone's color at `alpha` for full strength.
    pub fn rim_color(&self, alpha: f32) -> Hsla {
        let lightness = if self.rim_tone >= 0. { 1. } else { 0. };
        hsla(0., 0., lightness, alpha * self.rim_tone.abs().min(1.))
    }
}

impl Default for GlassStyle {
    fn default() -> Self {
        Self {
            blur: 6.,
            magnify: 1.,
            refraction: 0.25,
            rim: 18.,
            aberration: 0.08,
            tint: hsla(0., 0., 1., 0.08),
            rim_tone: 0.5,
        }
    }
}

/// How glass is drawn.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GlassRenderer {
    /// A Metal backdrop layer (this fork's `Styled::backdrop`): it bends whatever was
    /// painted behind it, including text and other glass. macOS and iOS only.
    Gpu,
    /// Shaded per pixel on the CPU from the `Backdrop` shapes alone.
    Cpu,
}

impl GlassRenderer {
    /// Only GPUI's Metal renderer draws backdrop layers.
    pub const GPU_AVAILABLE: bool = cfg!(any(target_os = "macos", target_os = "ios"));

    /// `LIQUID_GLASS=cpu` starts on the CPU renderer, for comparing the two.
    pub fn default_for_platform() -> Self {
        let cpu_requested = std::env::var("LIQUID_GLASS").is_ok_and(|value| value == "cpu");
        if Self::GPU_AVAILABLE && !cpu_requested {
            Self::Gpu
        } else {
            Self::Cpu
        }
    }
}

/// A "Liquid Glass" layer: it renders the backdrop beneath itself through a
/// refracting, frosted lens, then adds sheen and a lit rim.
///
/// It forwards `Styled`, `InteractiveElement` and `ParentElement` to an
/// inner `div`, so it lays out and handles events like any other element.
#[derive(IntoElement)]
pub struct LiquidGlass {
    id: ElementId,
    base: Div,
    backdrop: Rc<Backdrop>,
    glass: GlassStyle,
    renderer: GlassRenderer,
    radius: Pixels,
    children: Vec<AnyElement>,
}

/// The glass image painted last frame, kept so its texture can be released
/// once a newer one replaces it.
#[derive(Default)]
struct PaintedImage(Option<Arc<RenderImage>>);

pub fn liquid_glass(id: impl Into<ElementId>, backdrop: Rc<Backdrop>) -> LiquidGlass {
    LiquidGlass {
        id: id.into(),
        base: div().relative().shadow(vec![
            BoxShadow::new(px(0.), px(14.), hsla(0., 0., 0., 0.28)).blur_radius(px(32.)),
            BoxShadow::new(px(0.), px(1.), hsla(0., 0., 0., 0.14)).blur_radius(px(3.)),
        ]),
        backdrop,
        glass: GlassStyle::default(),
        renderer: GlassRenderer::default_for_platform(),
        radius: px(24.),
        children: Vec::new(),
    }
}

impl LiquidGlass {
    pub fn glass(mut self, glass: GlassStyle) -> Self {
        self.glass = glass;
        self
    }

    pub fn radius(mut self, radius: Pixels) -> Self {
        self.radius = radius;
        self
    }

    /// Falls back to the CPU where the GPU path isn't available.
    pub fn renderer(mut self, renderer: GlassRenderer) -> Self {
        self.renderer = if GlassRenderer::GPU_AVAILABLE {
            renderer
        } else {
            GlassRenderer::Cpu
        };
        self
    }
}

impl Styled for LiquidGlass {
    fn style(&mut self) -> &mut StyleRefinement {
        self.base.style()
    }
}

impl InteractiveElement for LiquidGlass {
    fn interactivity(&mut self) -> &mut Interactivity {
        self.base.interactivity()
    }
}

impl ParentElement for LiquidGlass {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl RenderOnce for LiquidGlass {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let painted = window.use_keyed_state(self.id, cx, |_, _| PaintedImage::default());
        let radius = self.radius;
        let glass = self.glass;
        let renderer = self.renderer;
        let backdrop = self.backdrop;
        let edge = |a| glass.rim_color(a);
        let layer = || {
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .rounded(radius)
        };

        if renderer == GlassRenderer::Gpu {
            // A CPU image left from before a switch would stay in the atlas.
            if let Some(previous) = painted.update(cx, |painted, _| painted.0.take()) {
                window.drop_image(previous).ok();
            }
        }

        self.base
            .rounded(radius)
            .when(renderer == GlassRenderer::Gpu, |this| {
                this.backdrop(px(glass.blur), backdrop_effect(glass, f32::from(radius)))
            })
            .when(renderer == GlassRenderer::Cpu, |this| {
                this.child(
                    canvas(
                        |_, _, _| {},
                        move |bounds, _, window, cx| {
                            let r = radius
                                .min(bounds.size.width / 2.)
                                .min(bounds.size.height / 2.);
                            let started = Instant::now();
                            let backdrop: &Backdrop = &backdrop;
                            let render = || render_glass(bounds, f32::from(r), backdrop, glass);
                            let image = match pool() {
                                Some(pool) => pool.install(render),
                                None => render(),
                            };
                            RENDER_NANOS
                                .fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
                            let Some(image) = image else {
                                return;
                            };
                            let image = Arc::new(image);
                            if window
                                .paint_image(
                                    bounds,
                                    bounds,
                                    Corners::all(r),
                                    image.clone(),
                                    0,
                                    false,
                                )
                                .is_err()
                            {
                                return;
                            }
                            let previous =
                                painted.update(cx, |painted, _| painted.0.replace(image));
                            if let Some(previous) = previous {
                                window.drop_image(previous).ok();
                            }
                        },
                    )
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full(),
                )
            })
            .child(layer().bg(linear_gradient(
                180.,
                linear_color_stop(edge(0.12), 0.),
                linear_color_stop(edge(0.), 0.5),
            )))
            // Light caught inside the curved rim: bright from above, a
            // fainter bounce from below.
            .child(layer().shadow(vec![
                BoxShadow::new(px(0.), px(2.), edge(0.22))
                    .blur_radius(px(6.))
                    .inset(),
                BoxShadow::new(px(0.), px(-2.), edge(0.1))
                    .blur_radius(px(8.))
                    .inset(),
            ]))
            .child(layer().border_1().border_color(edge(0.18)))
            .child(layer().border_t(px(1.)).border_color(edge(0.45)))
            .children(self.children)
    }
}

impl Raster {
    /// Three box blur passes per axis approximate a Gaussian of `sigma`
    /// raster pixels. Columns are blurred as rows of the transpose, so both
    /// axes run in parallel over contiguous memory.
    fn blur(&mut self, sigma: f32) {
        let radius = (((4. * sigma * sigma + 1.).sqrt() - 1.) / 2.).round() as usize;
        if radius == 0 {
            return;
        }
        let (width, height) = (self.width, self.height);
        box_blur_rows(&mut self.pixels, width, radius);
        let mut columns = transpose(&self.pixels, width, height);
        box_blur_rows(&mut columns, height, radius);
        self.pixels = transpose(&columns, height, width);
    }

    /// Bilinear sample at window coordinates (logical pixels), clamped to
    /// the edges.
    fn sample(&self, x: f32, y: f32) -> [f32; 3] {
        let fx = (x * self.scale - self.x0 as f32 - 0.5).clamp(0., (self.width - 1) as f32);
        let fy = (y * self.scale - self.y0 as f32 - 0.5).clamp(0., (self.height - 1) as f32);
        let ix = fx as usize;
        let iy = fy as usize;
        let ix1 = (ix + 1).min(self.width - 1);
        let iy1 = (iy + 1).min(self.height - 1);
        let tx = fx - ix as f32;
        let ty = fy - iy as f32;
        let p = |x: usize, y: usize| self.pixels[y * self.width + x];
        let (a, b, c, d) = (p(ix, iy), p(ix1, iy), p(ix, iy1), p(ix1, iy1));
        std::array::from_fn(|i| {
            let top = a[i] + (b[i] - a[i]) * tx;
            let bottom = c[i] + (d[i] - c[i]) * tx;
            top + (bottom - top) * ty
        })
    }
}

/// Three passes of a box blur along every row, rows in parallel.
fn box_blur_rows(pixels: &mut [[f32; 3]], width: usize, radius: usize) {
    pixels.par_chunks_mut(width).for_each_init(
        || vec![[0.; 3]; width],
        |scratch, row| {
            for _ in 0..3 {
                box_blur_row(row, scratch, radius);
                row.copy_from_slice(scratch);
            }
        },
    );
}

/// Running-sum box blur of one row, clamping at the ends.
fn box_blur_row(src: &[[f32; 3]], dst: &mut [[f32; 3]], radius: usize) {
    let last = src.len() as isize - 1;
    let at = |i: isize| src[i.clamp(0, last) as usize];
    let r = radius as isize;
    let norm = 1. / (2 * radius + 1) as f32;
    let mut sum = [0.; 3];
    for i in -r..=r {
        add(&mut sum, at(i), 1.);
    }
    for (i, out) in dst.iter_mut().enumerate() {
        let i = i as isize;
        *out = sum.map(|s| s * norm);
        add(&mut sum, at(i + r + 1), 1.);
        add(&mut sum, at(i - r), -1.);
    }
}

fn transpose(pixels: &[[f32; 3]], width: usize, height: usize) -> Vec<[f32; 3]> {
    let mut out = vec![[0.; 3]; width * height];
    out.par_chunks_mut(height)
        .enumerate()
        .for_each(|(x, column)| {
            for (y, p) in column.iter_mut().enumerate() {
                *p = pixels[y * width + x];
            }
        });
    out
}

fn add(sum: &mut [f32; 3], p: [f32; 3], sign: f32) {
    for c in 0..3 {
        sum[c] += p[c] * sign;
    }
}

/// Outward unit normal of the rounded rect's distance field at `(qx, qy)`.
/// Continuous within `r` of the edge, which is why the rim is capped at `r`.
fn rounded_rect_normal(qx: f32, qy: f32, hw: f32, hh: f32, r: f32) -> (f32, f32) {
    let kx = qx.abs() - hw + r;
    let ky = qy.abs() - hh + r;
    if kx > 0. && ky > 0. {
        let len = kx.hypot(ky);
        (qx.signum() * kx / len, qy.signum() * ky / len)
    } else if kx > ky {
        (qx.signum(), 0.)
    } else {
        (0., qy.signum())
    }
}

/// The glass as a Metal backdrop effect. The shader caps the rim at the
/// corner radius it is drawn with; capping it at `radius` here as well keeps
/// the refraction the same as `render_glass`.
fn backdrop_effect(glass: GlassStyle, radius: f32) -> BackdropEffect {
    let rim = glass.rim.min(radius);
    BackdropEffect::LiquidGlass(gpui::LiquidGlass {
        refraction: px(glass.refraction * rim * 2.),
        rim: px(rim),
        magnify: glass.magnify,
        aberration: glass.aberration,
        tint: glass.tint,
        rim_tone: glass.rim_tone,
    })
}

/// Renders the glass surface for `bounds` at `resolution(glass)`.
fn render_glass(
    bounds: Bounds<Pixels>,
    radius: f32,
    backdrop: &Backdrop,
    glass: GlassStyle,
) -> Option<RenderImage> {
    let scale = resolution(&glass);
    let ox = f32::from(bounds.origin.x);
    let oy = f32::from(bounds.origin.y);
    let w = f32::from(bounds.size.width);
    let h = f32::from(bounds.size.height);
    let device_w = (w * scale).ceil() as usize;
    let device_h = (h * scale).ceil() as usize;
    if device_w == 0 || device_h == 0 {
        return None;
    }

    let rim = glass.rim.min(radius);
    let max_shift = glass.refraction * rim * 2.;
    let margin = (max_shift * (1. + glass.aberration) + glass.blur * 3. + 2.).ceil();
    let mut source = backdrop.rasterize(bounds.dilate(px(margin)), scale)?;
    if glass.blur >= 0.5 {
        source.blur(glass.blur * scale);
    }

    let tint = Rgba::from(glass.tint);
    let (hw, hh) = (w / 2., h / 2.);
    let (cx, cy) = (ox + hw, oy + hh);
    // Light from the top left; squaring the dot product also lights the
    // opposite rim, as on real glass.
    let (light_x, light_y) = (-0.6, -0.8);

    let shade = |px: usize, py: usize| -> [f32; 3] {
        let lx = (px as f32 + 0.5) / scale;
        let ly = (py as f32 + 0.5) / scale;
        let (qx, qy) = (lx - hw, ly - hh);
        let inside = -rounded_rect_sdf(qx, qy, hw, hh, radius);
        let t = if rim > 0. {
            (inside.max(0.) / rim).min(1.)
        } else {
            1.
        };
        let edge = (1. - t) * (1. - t);
        let (nx, ny) = rounded_rect_normal(qx, qy, hw, hh, radius);
        let shift = max_shift * edge;

        let at = |k: f32| {
            let sx = ox + lx + nx * shift * k;
            let sy = oy + ly + ny * shift * k;
            source.sample(
                cx + (sx - cx) / glass.magnify,
                cy + (sy - cy) / glass.magnify,
            )
        };
        let mut color = if shift * glass.aberration > 0.05 {
            let (r, g, b) = (at(1. + glass.aberration), at(1.), at(1. - glass.aberration));
            [r[0], g[1], b[2]]
        } else {
            at(1.)
        };

        let tint_rgb = [tint.r, tint.g, tint.b];
        let dot = nx * light_x + ny * light_y;
        // Directional highlights plus a faint glow all around, since glass
        // reflects more at grazing angles.
        let light = edge * (1. - t) * (dot * dot * 0.55 + 0.2) * glass.rim_tone.abs().min(1.);
        let light_target = if glass.rim_tone >= 0. { 1. } else { 0. };
        for c in 0..3 {
            color[c] += (tint_rgb[c] - color[c]) * tint.a;
            color[c] += (light_target - color[c]) * light;
        }
        color
    };

    let mut bytes = vec![255u8; device_w * device_h * 4];
    bytes
        .par_chunks_mut(device_w * 4)
        .enumerate()
        .for_each(|(py, row)| {
            for (px, pixel) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let [r, g, b] = shade(px, py);
                // GPUI images are BGRA with straight alpha.
                pixel[0] = (b.clamp(0., 1.) * 255. + 0.5) as u8;
                pixel[1] = (g.clamp(0., 1.) * 255. + 0.5) as u8;
                pixel[2] = (r.clamp(0., 1.) * 255. + 0.5) as u8;
            }
        });

    let buffer = image::RgbaImage::from_raw(device_w as u32, device_h as u32, bytes)?;
    Some(RenderImage::new(vec![image::Frame::new(buffer)]))
}
