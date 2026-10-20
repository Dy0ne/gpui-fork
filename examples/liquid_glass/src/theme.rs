use gpui::{Hsla, Rgba, WindowAppearance, hsla};

/// Colors that differ between light and dark mode. `Theme::at` blends the
/// two so switching modes can crossfade.
#[derive(Clone, Copy)]
pub struct Theme {
    pub backdrop_top: Hsla,
    pub backdrop_bottom: Hsla,
    pub blob_saturation: f32,
    pub blob_lightness: f32,
    /// Stripes and dots in the backdrop.
    pub ink: Hsla,
    /// Multiplier on the rim tone. In the dark the bright rim is the main
    /// thing that shows glass, so it needs more strength.
    pub rim_gain: f32,
    /// Text and controls drawn on top of the glass.
    pub foreground: Hsla,
}

impl Theme {
    const LIGHT: Theme = Theme {
        backdrop_top: hsla(0.58, 0.55, 0.9, 1.),
        backdrop_bottom: hsla(0.07, 0.65, 0.92, 1.),
        blob_saturation: 0.9,
        blob_lightness: 0.66,
        ink: hsla(0.66, 0.35, 0.2, 0.35),
        rim_gain: 1.,
        foreground: hsla(0.66, 0.3, 0.12, 1.),
    };

    const DARK: Theme = Theme {
        backdrop_top: hsla(0.70, 0.55, 0.16, 1.),
        backdrop_bottom: hsla(0.53, 0.60, 0.12, 1.),
        blob_saturation: 0.85,
        blob_lightness: 0.58,
        ink: hsla(0., 0., 1., 0.6),
        rim_gain: 1.7,
        foreground: hsla(0., 0., 1., 1.),
    };

    /// The theme `dark` of the way from light (0) to dark (1).
    pub fn at(dark: f32) -> Self {
        let (l, d) = (Self::LIGHT, Self::DARK);
        let t = dark.clamp(0., 1.);
        let num = |a: f32, b: f32| a + (b - a) * t;
        Self {
            backdrop_top: mix(l.backdrop_top, d.backdrop_top, t),
            backdrop_bottom: mix(l.backdrop_bottom, d.backdrop_bottom, t),
            blob_saturation: num(l.blob_saturation, d.blob_saturation),
            blob_lightness: num(l.blob_lightness, d.blob_lightness),
            ink: mix(l.ink, d.ink, t),
            rim_gain: num(l.rim_gain, d.rim_gain),
            foreground: mix(l.foreground, d.foreground, t),
        }
    }

    /// The foreground at `alpha`.
    pub fn fg(&self, alpha: f32) -> Hsla {
        self.foreground.opacity(alpha)
    }
}

pub fn is_dark(appearance: WindowAppearance) -> bool {
    matches!(
        appearance,
        WindowAppearance::Dark | WindowAppearance::VibrantDark
    )
}

/// Blends in sRGB, which avoids hue sweeps between unrelated colors.
fn mix(a: Hsla, b: Hsla, t: f32) -> Hsla {
    let (a, b) = (Rgba::from(a), Rgba::from(b));
    let lerp = |x: f32, y: f32| x + (y - x) * t;
    Hsla::from(Rgba {
        r: lerp(a.r, b.r),
        g: lerp(a.g, b.g),
        b: lerp(a.b, b.b),
        a: lerp(a.a, b.a),
    })
}
