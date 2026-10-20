//! Backdrop layers (`Styled::backdrop`): a frosted bar, a liquid glass card, and a lens that
//! follows the mouse, over a colorful scene. Only the Metal renderer (macOS and iOS) draws
//! backdrop layers; elsewhere the glass is just its translucent background.

#![cfg_attr(target_family = "wasm", no_main)]

#[path = "example_support/fonts.rs"]
mod example_support;

use gpui::{
    App, BackdropEffect, Bounds, Context, FontWeight, Hsla, LiquidGlass, MouseMoveEvent, Pixels,
    Point, Window, WindowBounds, WindowOptions, div, hsla, linear_color_stop, linear_gradient,
    point, prelude::*, px, size,
};
use gpui_platform::application;

const LENS_SIZE: f32 = 160.;

struct LiquidGlassExample {
    lens: Point<Pixels>,
}

/// Liquid glass whose rim bends light from `rim / 2` pixels outside its edge.
fn liquid_glass(rim: f32, magnify: f32, aberration: f32) -> BackdropEffect {
    BackdropEffect::LiquidGlass(LiquidGlass {
        refraction: px(rim * 0.5),
        rim: px(rim),
        magnify,
        aberration,
        tint: hsla(0., 0., 1., 0.08),
        rim_tone: 0.5,
    })
}

fn blob(x: f32, y: f32, diameter: f32, color: Hsla) -> impl IntoElement {
    div()
        .absolute()
        .left(px(x))
        .top(px(y))
        .size(px(diameter))
        .rounded_full()
        .bg(color)
}

impl Render for LiquidGlassExample {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rim_light = hsla(0., 0., 1., 0.35);
        let half_lens = px(LENS_SIZE / 2.);

        div()
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(linear_gradient(
                135.,
                linear_color_stop(hsla(0.62, 0.8, 0.55, 1.), 0.),
                linear_color_stop(hsla(0.9, 0.75, 0.6, 1.), 1.),
            ))
            .text_color(gpui::white())
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                this.lens = event.position;
                cx.notify();
            }))
            .child(blob(60., 90., 180., hsla(0.13, 0.95, 0.6, 1.)))
            .child(blob(380., 60., 140., hsla(0.45, 0.8, 0.5, 1.)))
            .child(blob(300., 250., 220., hsla(0.0, 0.85, 0.6, 1.)))
            .child(blob(520., 300., 90., hsla(0.75, 0.7, 0.35, 1.)))
            .child(
                div()
                    .absolute()
                    .left(px(48.))
                    .top(px(170.))
                    .text_size(px(56.))
                    .font_weight(FontWeight::BOLD)
                    .child("Liquid Glass"),
            )
            .child(
                div()
                    .absolute()
                    .top(px(16.))
                    .left(px(16.))
                    .right(px(16.))
                    .h(px(48.))
                    .rounded(px(24.))
                    .backdrop(px(8.), BackdropEffect::Frost)
                    .bg(hsla(0., 0., 1., 0.12))
                    .flex()
                    .items_center()
                    .px_5()
                    .child("Frost"),
            )
            .child(
                div()
                    .absolute()
                    .left(px(40.))
                    .bottom(px(40.))
                    .w(px(260.))
                    .h(px(120.))
                    .rounded(px(28.))
                    .backdrop(px(6.), liquid_glass(22., 1., 0.08))
                    .border_1()
                    .border_color(rim_light)
                    .p_5()
                    .child("LiquidGlass: blur, refraction and rim light"),
            )
            .child(
                div()
                    .absolute()
                    .left(self.lens.x - half_lens)
                    .top(self.lens.y - half_lens)
                    .size(px(LENS_SIZE))
                    .rounded_full()
                    .backdrop(px(1.5), liquid_glass(44., 1.15, 0.15))
                    .border_1()
                    .border_color(rim_light),
            )
    }
}

fn run_example() {
    application().run(|cx: &mut App| {
        if !example_support::load_fonts(cx) {
            return;
        }
        let bounds = Bounds::centered(None, size(px(640.), px(440.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| {
                cx.new(|_| LiquidGlassExample {
                    lens: point(px(460.), px(200.)),
                })
            },
        )
        .unwrap();
        cx.activate(true);
    });
}

#[cfg(not(target_family = "wasm"))]
fn main() {
    run_example();
}

#[cfg(target_family = "wasm")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn start() {
    gpui_platform::web_init();
    run_example();
}
