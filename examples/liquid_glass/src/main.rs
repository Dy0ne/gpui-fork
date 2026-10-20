mod assets;
mod backdrop;
mod glass;
mod perf;
mod theme;

use std::{
    cell::Cell,
    rc::Rc,
    time::{Instant, SystemTime},
};

use gpui::{
    App, Bounds, Context, Div, FontWeight, Hsla, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, Render, SharedString, Subscription, TitlebarOptions, Window,
    WindowBounds, WindowOptions, canvas, div, hsla, linear_color_stop, linear_gradient, point,
    prelude::*, px, relative, rgb, size, svg,
};

use assets::Assets;
use backdrop::Backdrop;
use glass::{GlassRenderer, GlassStyle, liquid_glass};
use perf::Perf;
use theme::Theme;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    NavBar,
    Controls,
    Dock,
    Lens,
    Stats,
}

impl Kind {
    fn id(self) -> &'static str {
        match self {
            Kind::NavBar => "nav-bar",
            Kind::Controls => "controls",
            Kind::Dock => "dock",
            Kind::Lens => "lens",
            Kind::Stats => "stats",
        }
    }
}

struct Panel {
    kind: Kind,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    radius: f32,
    /// Smoothed drag velocity, used to squash and stretch the glass.
    vx: f32,
    vy: f32,
}

impl Panel {
    fn new(kind: Kind, x: f32, y: f32, w: f32, h: f32, radius: f32) -> Self {
        Self {
            kind,
            x,
            y,
            w,
            h,
            radius,
            vx: 0.,
            vy: 0.,
        }
    }
}

/// The dragged panel is always the last one in `Demo::panels`.
struct Drag {
    start: Point<f32>,
    grab: Point<f32>,
    last: Point<f32>,
}

/// Sliders that run from white (1) through clear (0) to black (-1).
#[derive(Clone, Copy, PartialEq)]
enum ToneSlider {
    Tint,
    Rim,
}

/// The glass settings the shuffle button randomizes.
#[derive(Clone, Copy)]
struct Settings {
    blur: f32,
    refraction: f32,
    magnify: f32,
    tint: f32,
    rim_tone: f32,
}

/// Glass tint opacity at the white or black end of the tint slider.
const TINT_MAX_ALPHA: f32 = 0.5;

const TABS: [&str; 5] = ["Library", "For You", "Browse", "Radio", "Search"];

struct Demo {
    panels: Vec<Panel>,
    drag: Option<Drag>,
    /// Whether the last press moved its panel, so releasing it isn't a click.
    drag_moved: bool,
    tab: usize,
    /// Position of the selection pill in tabs, sprung toward `tab`.
    tab_pos: f32,
    tab_vel: f32,
    time: f32,
    last_frame: Instant,
    paused: bool,
    blur: f32,
    refraction: f32,
    magnify: f32,
    tint: f32,
    rim_tone: f32,
    /// Where each tone slider's track was last laid out, for mapping mouse
    /// positions to values. Indexed by `ToneSlider`.
    tone_tracks: [Rc<Cell<Bounds<Pixels>>>; 2],
    tone_dragging: Option<ToneSlider>,
    /// Settings the controls are gliding toward after a shuffle.
    shuffle_target: Option<Settings>,
    rng: u64,
    /// The backdrop was pressed; the next drag moves the window.
    window_move_pending: bool,
    perf: Perf,
    /// Whether the last frame asked for another; when not, the app is idle
    /// and the performance numbers are frozen.
    animating: bool,
    dark: bool,
    renderer: GlassRenderer,
    _appearance: Subscription,
    /// How far the colors have faded from light (0) to dark (1).
    dark_mix: f32,
}

impl Demo {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let dark = theme::is_dark(cx.window_appearance());
        // Follow the system appearance; the switch overrides it until the
        // system changes again.
        let appearance = cx.observe_window_appearance(window, |this, window, cx| {
            this.dark = theme::is_dark(window.appearance());
            cx.notify();
        });
        Self {
            panels: vec![
                Panel::new(Kind::NavBar, 320., 28., 460., 56., 28.),
                Panel::new(Kind::Controls, 32., 296., 300., 408., 28.),
                Panel::new(Kind::Dock, 380., 612., 400., 80., 30.),
                Panel::new(Kind::Lens, 640., 220., 180., 180., 90.),
                Panel::new(Kind::Stats, 846., 24., 230., 352., 24.),
            ],
            drag: None,
            drag_moved: false,
            tab: 0,
            tab_pos: 0.,
            tab_vel: 0.,
            time: 0.,
            last_frame: Instant::now(),
            paused: false,
            blur: 6.,
            refraction: 0.25,
            magnify: 1.3,
            tint: 0.3,
            rim_tone: 0.5,
            tone_tracks: Default::default(),
            tone_dragging: None,
            shuffle_target: None,
            window_move_pending: false,
            perf: Perf::new(),
            animating: true,
            rng: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0x9e37_79b9_7f4a_7c15, |d| d.as_nanos() as u64)
                | 1,
            dark,
            renderer: GlassRenderer::default_for_platform(),
            _appearance: appearance,
            dark_mix: if dark { 1. } else { 0. },
        }
    }

    fn tick(&mut self) -> bool {
        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f32().min(0.05);
        self.last_frame = now;
        if !self.paused {
            self.time += dt;
        }
        let decay = 0.82_f32.powf(dt * 60.);
        let mut moving = false;
        for panel in &mut self.panels {
            panel.vx *= decay;
            panel.vy *= decay;
            if panel.vx.abs() < 0.05 && panel.vy.abs() < 0.05 {
                panel.vx = 0.;
                panel.vy = 0.;
            } else {
                moving = true;
            }
        }

        // Slightly underdamped spring, stepped finely enough to stay stable.
        const STIFFNESS: f32 = 300.;
        const DAMPING: f32 = 26.;
        let target = self.tab as f32;
        let steps = (dt * 240.).ceil().max(1.);
        let h = dt / steps;
        for _ in 0..steps as usize {
            let accel = STIFFNESS * (target - self.tab_pos) - DAMPING * self.tab_vel;
            self.tab_vel += accel * h;
            self.tab_pos += self.tab_vel * h;
        }
        if (target - self.tab_pos).abs() < 0.001 && self.tab_vel.abs() < 0.01 {
            self.tab_pos = target;
            self.tab_vel = 0.;
        } else {
            moving = true;
        }

        if let Some(target) = self.shuffle_target {
            let k = 1. - (-dt * 8.).exp();
            let mut settled = true;
            for (value, target) in [
                (&mut self.blur, target.blur),
                (&mut self.refraction, target.refraction),
                (&mut self.magnify, target.magnify),
                (&mut self.tint, target.tint),
                (&mut self.rim_tone, target.rim_tone),
            ] {
                *value += (target - *value) * k;
                if (target - *value).abs() > 0.002 {
                    settled = false;
                }
            }
            if settled {
                self.apply(target);
                self.shuffle_target = None;
            } else {
                moving = true;
            }
        }

        let target = if self.dark { 1. } else { 0. };
        self.dark_mix += (target - self.dark_mix) * (1. - (-dt * 7.).exp());
        if (target - self.dark_mix).abs() < 0.002 {
            self.dark_mix = target;
        } else {
            moving = true;
        }
        moving
    }

    fn apply(&mut self, settings: Settings) {
        self.blur = settings.blur;
        self.refraction = settings.refraction;
        self.magnify = settings.magnify;
        self.tint = settings.tint;
        self.rim_tone = settings.rim_tone;
    }

    /// A uniform random number in `[0, 1)` (xorshift64).
    fn random(&mut self) -> f32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng >> 40) as f32 / (1u64 << 24) as f32
    }

    /// Picks random settings that still look like glass, snapped to the
    /// controls' steps, and starts gliding toward them.
    fn shuffle(&mut self) {
        // A dark rim disappears on a dark backdrop, so keep it light there.
        let rim_low = if self.dark { 0.2 } else { -0.6 };
        let mut pick = |lo: f32, hi: f32, step: f32| {
            let steps = ((hi - lo) / step).round();
            lo + (self.random() * (steps + 1.)).floor().min(steps) * step
        };
        let blur = pick(0., 14., 2.);
        let refraction = pick(0.1, 0.5, 0.05);
        let magnify = pick(1., 1.8, 0.1);
        let tint = pick(-0.7, 0.7, 0.01);
        let rim_tone = pick(rim_low, 1., 0.01);
        self.shuffle_target = Some(Settings {
            blur,
            refraction,
            magnify,
            tint,
            rim_tone,
        });
    }

    fn theme(&self) -> Theme {
        Theme::at(self.dark_mix)
    }

    fn begin_drag(&mut self, ix: usize, position: Point<f32>) {
        let panel = self.panels.remove(ix);
        self.drag_moved = false;
        self.drag = Some(Drag {
            start: position,
            grab: Point::new(position.x - panel.x, position.y - panel.y),
            last: position,
        });
        self.panels.push(panel);
    }

    fn drag_to(&mut self, position: Point<f32>) {
        let (Some(drag), Some(panel)) = (self.drag.as_mut(), self.panels.last_mut()) else {
            return;
        };
        panel.x = position.x - drag.grab.x;
        panel.y = position.y - drag.grab.y;
        panel.vx = panel.vx * 0.6 + (position.x - drag.last.x) * 0.4;
        panel.vy = panel.vy * 0.6 + (position.y - drag.last.y) * 0.4;
        drag.last = position;
        if (position.x - drag.start.x).hypot(position.y - drag.start.y) > 3. {
            self.drag_moved = true;
        }
    }

    fn glass_for(&self, kind: Kind) -> GlassStyle {
        let theme = self.theme();
        let base = GlassStyle {
            blur: self.blur,
            refraction: self.refraction,
            tint: hsla(
                0.,
                0.,
                if self.tint >= 0. { 1. } else { 0. },
                self.tint.abs() * TINT_MAX_ALPHA,
            ),
            rim_tone: (self.rim_tone * theme.rim_gain).clamp(-1., 1.),
            ..GlassStyle::default()
        };
        match kind {
            Kind::NavBar => GlassStyle { rim: 16., ..base },
            Kind::Controls | Kind::Dock | Kind::Stats => GlassStyle { rim: 22., ..base },
            Kind::Lens => GlassStyle {
                blur: self.blur * 0.25,
                magnify: self.magnify,
                rim: 44.,
                aberration: 0.15,
                ..base
            },
        }
    }

    fn adjust(
        cx: &mut Context<Self>,
        f: impl Fn(&mut Self) + 'static,
    ) -> impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static {
        cx.listener(move |this, _: &MouseDownEvent, _, cx| {
            cx.stop_propagation();
            this.shuffle_target = None;
            f(this);
            cx.notify();
        })
    }

    fn tone(&mut self, slider: ToneSlider) -> &mut f32 {
        match slider {
            ToneSlider::Tint => &mut self.tint,
            ToneSlider::Rim => &mut self.rim_tone,
        }
    }

    /// Sets a tone from a mouse x position over its slider track: the left
    /// end is white, the middle clear, the right end black.
    fn set_tone(&mut self, slider: ToneSlider, x: f32) {
        let track = self.tone_tracks[slider as usize].get();
        let width = f32::from(track.size.width);
        if width <= 0. {
            return;
        }
        let fraction = ((x - f32::from(track.origin.x)) / width).clamp(0., 1.);
        let tone = 1. - 2. * fraction;
        self.shuffle_target = None;
        // A small detent at the middle makes "clear" easy to hit.
        *self.tone(slider) = if tone.abs() < 0.05 { 0. } else { tone };
    }

    fn tone_slider(&self, slider: ToneSlider, label: &'static str, cx: &mut Context<Self>) -> Div {
        let tone = match slider {
            ToneSlider::Tint => self.tint,
            ToneSlider::Rim => self.rim_tone,
        };
        let value = match tone {
            0. => "Clear".to_string(),
            t if t > 0. => format!("White {:.0}%", t * 100.),
            t => format!("Black {:.0}%", -t * 100.),
        };
        let theme = self.theme();
        let white = |a| hsla(0., 0., 1., a);
        let black = |a| hsla(0., 0., 0., a);
        let half = || div().absolute().top_0().bottom_0().w(relative(0.5));
        let track = self.tone_tracks[slider as usize].clone();

        div()
            .flex()
            .flex_col()
            .gap_1()
            .text_sm()
            .child(
                div()
                    .flex()
                    .justify_between()
                    .child(label)
                    .child(div().text_color(theme.fg(0.75)).child(value)),
            )
            .child(
                div()
                    .relative()
                    .h(px(20.))
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            this.tone_dragging = Some(slider);
                            this.set_tone(slider, f32::from(event.position.x));
                            cx.notify();
                        }),
                    )
                    .child(
                        canvas(move |bounds, _, _| track.set(bounds), |_, _, _, _| {})
                            .absolute()
                            .size_full(),
                    )
                    .child(
                        div()
                            .relative()
                            .w_full()
                            .h(px(6.))
                            .rounded_full()
                            .border_1()
                            .border_color(theme.fg(0.25))
                            .child(half().left_0().rounded_l_full().bg(linear_gradient(
                                90.,
                                linear_color_stop(white(0.95), 0.),
                                linear_color_stop(white(0.), 1.),
                            )))
                            .child(half().right_0().rounded_r_full().bg(linear_gradient(
                                90.,
                                linear_color_stop(black(0.), 0.),
                                linear_color_stop(black(0.95), 1.),
                            ))),
                    )
                    .child(
                        div()
                            .absolute()
                            .top(px(1.))
                            .left(relative((1. - tone) / 2.))
                            .ml(px(-9.))
                            .size(px(18.))
                            .rounded_full()
                            .bg(hsla(0., 0., 0.5 + tone * 0.45, 1.))
                            .border_2()
                            .border_color(white(0.9))
                            .shadow(vec![
                                gpui::BoxShadow::new(px(0.), px(1.), black(0.35))
                                    .blur_radius(px(3.)),
                            ]),
                    ),
            )
    }

    fn shuffle_button(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme();
        let hover = theme.fg(0.24);
        div()
            .size(px(30.))
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .bg(theme.fg(0.12))
            .hover(move |style| style.bg(hover))
            .on_mouse_down(MouseButton::Left, Self::adjust(cx, Self::shuffle))
            .child(
                svg()
                    .path("icons/shuffle.svg")
                    .size(px(16.))
                    .text_color(theme.fg(0.9)),
            )
    }

    fn dark_mode_switch(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme();
        let on = hsla(0.38, 0.62, 0.48, 1.);
        div()
            .flex()
            .justify_between()
            .items_center()
            .text_sm()
            .child("Dark mode")
            .child(
                div()
                    .relative()
                    .w(px(40.))
                    .h(px(22.))
                    .rounded_full()
                    .cursor_pointer()
                    .bg(if self.dark { on } else { theme.fg(0.18) })
                    .on_mouse_down(
                        MouseButton::Left,
                        Self::adjust(cx, |this| this.dark = !this.dark),
                    )
                    .child(
                        div()
                            .absolute()
                            .top(px(2.))
                            .left(px(2. + 18. * self.dark_mix))
                            .size(px(18.))
                            .rounded_full()
                            .bg(gpui::white())
                            .shadow(vec![
                                gpui::BoxShadow::new(px(0.), px(1.), hsla(0., 0., 0., 0.3))
                                    .blur_radius(px(2.)),
                            ]),
                    ),
            )
    }

    fn content(&self, kind: Kind, cx: &mut Context<Self>) -> Div {
        let theme = self.theme();
        match kind {
            Kind::NavBar => {
                let count = TABS.len() as f32;
                // The pill widens with speed, like a droplet being pulled.
                let stretch = (self.tab_vel.abs() * 0.04).min(0.5);
                let rim = self.glass_for(Kind::NavBar);
                div().size_full().p(px(8.)).child(
                    div()
                        .relative()
                        .size_full()
                        .flex()
                        .child(
                            div()
                                .absolute()
                                .top_0()
                                .bottom_0()
                                .left(relative((self.tab_pos - stretch / 2.) / count))
                                .w(relative((1. + stretch) / count))
                                .rounded_full()
                                .bg(rim.rim_color(0.4))
                                .border_1()
                                .border_color(rim.rim_color(0.36)),
                        )
                        .children(TABS.into_iter().enumerate().map(|(ix, label)| {
                            let selected = ix == self.tab;
                            div()
                                .id(("tab", ix))
                                .flex_1()
                                .h_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .cursor_pointer()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme.fg(if selected { 1. } else { 0.72 }))
                                .hover(|style| style.text_color(theme.fg(1.)))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if !this.drag_moved {
                                        this.tab = ix;
                                        cx.notify();
                                    }
                                }))
                                .child(label)
                        })),
                )
            }
            Kind::Controls => div()
                .size_full()
                .flex()
                .flex_col()
                .gap_2()
                .p_5()
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .items_center()
                        .child(
                            div()
                                .text_lg()
                                .font_weight(FontWeight::BOLD)
                                .child("Liquid Glass"),
                        )
                        .child(self.shuffle_button(cx)),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.fg(0.7))
                        .mb_1()
                        .child("GPUI demo · drag any layer"),
                )
                .child(stepper(
                    &theme,
                    "Blur",
                    format!("{:.0}px", self.blur),
                    Self::adjust(cx, |this| this.blur = (this.blur - 2.).max(0.)),
                    Self::adjust(cx, |this| this.blur = (this.blur + 2.).min(24.)),
                ))
                .child(stepper(
                    &theme,
                    "Refraction",
                    format!("{:.2}", self.refraction),
                    Self::adjust(cx, |this| {
                        this.refraction = (this.refraction - 0.05).max(0.)
                    }),
                    Self::adjust(cx, |this| {
                        this.refraction = (this.refraction + 0.05).min(0.6)
                    }),
                ))
                .child(stepper(
                    &theme,
                    "Magnify",
                    format!("{:.1}×", self.magnify),
                    Self::adjust(cx, |this| this.magnify = (this.magnify - 0.1).max(0.5)),
                    Self::adjust(cx, |this| this.magnify = (this.magnify + 0.1).min(2.5)),
                ))
                .child(self.tone_slider(ToneSlider::Tint, "Tint", cx))
                .child(self.tone_slider(ToneSlider::Rim, "Rim", cx))
                .child(self.dark_mode_switch(cx))
                .when(GlassRenderer::GPU_AVAILABLE, |card| {
                    card.child(
                        div()
                            .flex()
                            .justify_between()
                            .items_center()
                            .text_sm()
                            .child("Renderer")
                            .child(
                                pill(
                                    &theme,
                                    match self.renderer {
                                        GlassRenderer::Gpu => "Metal",
                                        GlassRenderer::Cpu => "CPU",
                                    },
                                )
                                .px_3()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    Self::adjust(cx, |this| {
                                        this.renderer = match this.renderer {
                                            GlassRenderer::Gpu => GlassRenderer::Cpu,
                                            GlassRenderer::Cpu => GlassRenderer::Gpu,
                                        }
                                    }),
                                ),
                            ),
                    )
                })
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .items_center()
                        .text_sm()
                        .child("Animation")
                        .child(
                            pill(&theme, if self.paused { "Play" } else { "Pause" })
                                .px_3()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    Self::adjust(cx, |this| this.paused = !this.paused),
                                ),
                        ),
                ),
            Kind::Dock => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .gap_3()
                .children(
                    [
                        ("F", 0.58, 0.62),
                        ("M", 0.33, 0.40),
                        ("P", 0.95, 0.03),
                        ("N", 0.12, 0.08),
                        ("C", 0.75, 0.85),
                        ("S", 0.0, 0.0),
                    ]
                    .into_iter()
                    .map(|(letter, from, to)| {
                        let saturation = if letter == "S" { 0. } else { 0.75 };
                        div()
                            .size(px(52.))
                            .rounded(px(14.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_xl()
                            .font_weight(FontWeight::BOLD)
                            .text_color(gpui::white())
                            .bg(linear_gradient(
                                160.,
                                linear_color_stop(hsla(from, saturation, 0.62, 1.), 0.),
                                linear_color_stop(hsla(to, saturation, 0.45, 1.), 1.),
                            ))
                            .child(letter)
                    }),
                ),
            Kind::Lens => div()
                .size_full()
                .flex()
                .items_end()
                .justify_center()
                .pb_5()
                .text_xs()
                .text_color(theme.fg(0.85))
                .child("drag me"),
            Kind::Stats => self.stats(&theme),
        }
    }

    fn stats(&self, theme: &Theme) -> Div {
        // Status colors, always shown with a label.
        let good = Hsla::from(rgb(0x0ca30c));
        let warning = Hsla::from(rgb(0xfab219));
        let serious = Hsla::from(rgb(0xec835a));
        let critical = Hsla::from(rgb(0xd03b3b));

        // While idle no frames are drawn, so the last rate no longer applies.
        let fps = if self.animating { self.perf.fps() } else { 0. };
        let (status, status_color) = if !self.animating {
            ("Idle", theme.fg(0.45))
        } else if fps >= 55. {
            ("Smooth", good)
        } else if fps >= 30. {
            ("OK", warning)
        } else {
            ("Slow", critical)
        };

        let frame = match self.perf.frame_avg_max() {
            Some((avg, max)) => format!("{avg:.1} ms · max {max:.1}"),
            None => "—".into(),
        };
        let cpu = self
            .perf
            .cpu_percent
            .map_or("—".into(), |cpu| format!("{cpu:.0}%"));
        let memory = self.perf.memory_bytes.map_or("—".into(), |bytes| {
            let mb = bytes as f64 / (1024. * 1024.);
            if mb >= 1024. {
                format!("{:.2} GB", mb / 1024.)
            } else {
                format!("{mb:.1} MB")
            }
        });
        let threads = self.perf.threads.map_or("—".into(), |n| n.to_string());
        let gpu_frame = self.perf.gpu.map_or("—".into(), |gpu| {
            format!("{:.2} ms · copy {:.2}", gpu.frame_ms, gpu.copy_ms)
        });
        let glass = match (self.renderer, self.perf.gpu) {
            (GlassRenderer::Gpu, Some(gpu)) => format!(
                "{:.2} ms GPU ({} layers)",
                gpu.blur_ms + gpu.composite_ms,
                gpu.layers
            ),
            (GlassRenderer::Gpu, None) => "Metal shader".into(),
            (GlassRenderer::Cpu, _) => format!("{:.1} ms CPU", self.perf.glass_ms),
        };
        let glass_split = match (self.renderer, self.perf.gpu) {
            (GlassRenderer::Gpu, Some(gpu)) => {
                format!("blur {:.2} · lens {:.2}", gpu.blur_ms, gpu.composite_ms)
            }
            _ => "—".into(),
        };

        // Frame times above this miss a 60 Hz refresh.
        const BUDGET_MS: f32 = 1000. / 60.;
        let history: Vec<f32> = self.perf.frame_ms.iter().copied().collect();
        let bar = theme.fg(0.4);
        let latest = theme.fg(0.9);
        let guide = theme.fg(0.3);
        let sparkline = canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                let (x0, y0) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
                let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
                let scale = history.iter().copied().fold(BUDGET_MS * 2., f32::max) * 1.1;
                let y_of = |ms: f32| y0 + h - (ms / scale).min(1.) * h;

                let gap = 1.;
                let slot = (w + gap) / perf::HISTORY as f32;
                // Newest frame on the right; missing history leaves the left empty.
                let offset = perf::HISTORY - history.len();
                for (i, &ms) in history.iter().enumerate() {
                    let top = y_of(ms);
                    let color = if ms > BUDGET_MS * 1.1 {
                        serious
                    } else if i + 1 == history.len() {
                        latest
                    } else {
                        bar
                    };
                    window.paint_quad(
                        gpui::fill(
                            backdrop::rect(
                                x0 + (offset + i) as f32 * slot,
                                top,
                                slot - gap,
                                (y0 + h - top).max(1.),
                            ),
                            color,
                        )
                        .corner_radii(gpui::Corners {
                            top_left: px(1.),
                            top_right: px(1.),
                            ..Default::default()
                        }),
                    );
                }
                window.paint_quad(gpui::fill(
                    backdrop::rect(x0, y_of(BUDGET_MS), w, 1.),
                    guide,
                ));
            },
        )
        .w_full()
        .h(px(40.))
        // A canvas has no content size, so flex would squeeze it to nothing.
        .flex_none();

        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_1()
            .p_4()
            .text_sm()
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.fg(0.75))
                            .child("Performance"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .text_xs()
                            .text_color(theme.fg(0.75))
                            .child(div().size(px(8.)).rounded_full().bg(status_color))
                            .child(status),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .gap_1()
                    .child(
                        div()
                            .text_3xl()
                            .font_weight(FontWeight::BOLD)
                            .child(format!("{fps:.0}")),
                    )
                    .child(div().text_color(theme.fg(0.6)).child("fps")),
            )
            .child(sparkline)
            .child(
                div()
                    .flex()
                    .justify_between()
                    .mb_1()
                    .text_xs()
                    .text_color(theme.fg(0.5))
                    .child(format!("frame time · last {}", perf::HISTORY))
                    .child("line = 16.7 ms"),
            )
            .child(stat_row(theme, "Frame", frame))
            .child(stat_row(theme, "GPU", gpu_frame))
            .child(stat_row(theme, "Glass", glass))
            .child(stat_row(theme, "", glass_split))
            .child(stat_row(theme, "CPU", cpu))
            .child(stat_row(theme, "Memory", memory))
            .child(stat_row(theme, "Threads", threads))
    }
}

fn stat_row(theme: &Theme, label: &'static str, value: String) -> Div {
    div()
        .flex()
        .justify_between()
        .child(div().text_color(theme.fg(0.7)).child(label))
        .child(value)
}

fn pill(theme: &Theme, label: impl Into<SharedString>) -> Div {
    pill_base(theme).child(label.into())
}

fn icon_pill(theme: &Theme, icon: &'static str) -> Div {
    pill_base(theme).child(svg().path(icon).size(px(14.)).text_color(theme.fg(0.9)))
}

fn pill_base(theme: &Theme) -> Div {
    let hover = theme.fg(0.24);
    div()
        .min_w(px(28.))
        .h(px(24.))
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .text_sm()
        .font_weight(FontWeight::SEMIBOLD)
        .cursor_pointer()
        .bg(theme.fg(0.12))
        .hover(move |style| style.bg(hover))
}

fn stepper(
    theme: &Theme,
    label: &'static str,
    value: String,
    minus: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    plus: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .text_sm()
        .child(div().flex_1().child(label))
        .child(
            div()
                .w(px(52.))
                .text_right()
                .text_color(theme.fg(0.75))
                .child(value),
        )
        .child(icon_pill(theme, "icons/minus.svg").on_mouse_down(MouseButton::Left, minus))
        .child(icon_pill(theme, "icons/plus.svg").on_mouse_down(MouseButton::Left, plus))
}

/// Squash and stretch: glass widens along the direction it's flung.
fn stretch(vx: f32, vy: f32) -> (f32, f32) {
    let x = (vx.abs() * 0.008).min(0.2);
    let y = (vy.abs() * 0.008).min(0.2);
    (1. + x - y * 0.5, 1. + y - x * 0.5)
}

impl Render for Demo {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let moving = self.tick();
        self.animating = !self.paused || moving;
        if self.animating {
            window.request_animation_frame();
        }
        self.perf.frame(Instant::now());
        // The first call turns GPU timing on; Metal only.
        self.perf.gpu_frame(window.gpu_frame_stats());
        self.perf.log(Instant::now());

        let theme = self.theme();
        let backdrop = Rc::new(Backdrop::animated(
            window.viewport_size(),
            self.time,
            &theme,
        ));
        let panels = self
            .panels
            .iter()
            .enumerate()
            .map(|(ix, panel)| {
                let (sx, sy) = stretch(panel.vx, panel.vy);
                let w = panel.w * sx;
                let h = panel.h * sy;
                liquid_glass(panel.kind.id(), backdrop.clone())
                    .glass(self.glass_for(panel.kind))
                    .renderer(self.renderer)
                    .radius(px(panel.radius.min(w / 2.).min(h / 2.)))
                    .absolute()
                    .left(px(panel.x + (panel.w - w) / 2.))
                    .top(px(panel.y + (panel.h - h) / 2.))
                    .w(px(w))
                    .h(px(h))
                    .text_color(theme.fg(1.))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            // Keep the press from reaching the background,
                            // which would move the window instead.
                            cx.stop_propagation();
                            this.begin_drag(ix, to_f32(event.position));
                            cx.notify();
                        }),
                    )
                    .child(self.content(panel.kind, cx))
            })
            .collect::<Vec<_>>();

        div()
            .size_full()
            .relative()
            .child(
                canvas(|_, _, _| {}, move |_, _, window, _| backdrop.paint(window))
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full(),
            )
            .children(panels)
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                if this.window_move_pending {
                    this.window_move_pending = false;
                    if event.pressed_button == Some(MouseButton::Left) {
                        // macOS drags from the event being handled, so this
                        // has to start on a drag event, not the press.
                        window.start_window_move();
                    }
                    return;
                }
                if let Some(slider) = this.tone_dragging {
                    if event.pressed_button == Some(MouseButton::Left) {
                        this.set_tone(slider, f32::from(event.position.x));
                    } else {
                        this.tone_dragging = None;
                    }
                    cx.notify();
                    return;
                }
                if this.drag.is_none() {
                    return;
                }
                if event.pressed_button == Some(MouseButton::Left) {
                    this.drag_to(to_f32(event.position));
                } else {
                    this.drag = None;
                }
                cx.notify();
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, _| {
                    this.drag = None;
                    this.tone_dragging = None;
                    this.window_move_pending = false;
                }),
            )
            // There's no title bar, so the empty backdrop stands in for it:
            // drag it to move the window, double-click it to zoom.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, _| {
                    if event.click_count == 2 {
                        window.titlebar_double_click();
                    } else {
                        this.window_move_pending = true;
                    }
                }),
            )
    }
}

fn to_f32(point: Point<gpui::Pixels>) -> Point<f32> {
    Point::new(f32::from(point.x), f32::from(point.y))
}

fn main() {
    gpui_platform::application()
        .with_assets(Assets)
        .run(|cx: &mut App| {
            // macOS apps keep running with no windows by default; this demo
            // is a single window, so closing it quits.
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let bounds = Bounds::centered(None, size(px(1100.), px(720.)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    // Draw the scene under a transparent title bar; only the
                    // traffic lights remain, floating over the backdrop.
                    titlebar: Some(TitlebarOptions {
                        title: Some("Liquid Glass".into()),
                        appears_transparent: true,
                        traffic_light_position: Some(point(px(16.), px(16.))),
                    }),
                    ..Default::default()
                },
                |window, cx| cx.new(|cx| Demo::new(window, cx)),
            )
            .unwrap();
            cx.activate(true);
        });
}
