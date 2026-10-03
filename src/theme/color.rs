//! Colour math. No gpui types here — this layer is pure so it can be tested
//! without a window, and so the palette can be checked for contrast in CI.

/// A colour in the Oklch space: perceptual lightness, chroma, hue in degrees.
///
/// Palettes are authored here rather than in sRGB because equal steps in `l`
/// look like equal steps to the eye. Equal steps in sRGB do not.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Oklch {
    pub l: f32,
    pub c: f32,
    pub h: f32,
}

/// A colour in gamma-encoded sRGB, each channel in `0.0..=1.0`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Srgb {
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

/// An sRGB colour with an alpha channel. Tokens carry alpha because hairline
/// borders and washes are defined as a tint over whatever sits behind them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Rgba {
    /// Flatten against a known backdrop, giving the colour actually displayed.
    pub fn flatten(self, backdrop: Srgb) -> Srgb {
        Srgb::new(self.r, self.g, self.b).over(backdrop, self.a)
    }
}

impl Oklch {
    pub const fn new(l: f32, c: f32, h: f32) -> Self {
        Self { l, c, h }
    }

    /// Convert to sRGB. A colour the display cannot show keeps its lightness
    /// and hue and gives up chroma until it fits: clipping each channel instead
    /// skews the hue, which turns a deep orange red.
    pub fn to_srgb(self) -> Srgb {
        let fits = |c: f32| {
            Self { c, ..self }
                .linear()
                .iter()
                .all(|v| (-1e-4..=1.0 + 1e-4).contains(v))
        };
        let c = if fits(self.c) {
            self.c
        } else {
            let (mut inside, mut outside) = (0.0, self.c);
            for _ in 0..20 {
                let mid = (inside + outside) / 2.0;
                if fits(mid) {
                    inside = mid
                } else {
                    outside = mid
                }
            }
            inside
        };
        let [r, g, b] = Self { c, ..self }.linear();
        Srgb {
            r: gamma_encode(r),
            g: gamma_encode(g),
            b: gamma_encode(b),
        }
    }

    fn linear(self) -> [f32; 3] {
        let (sin_h, cos_h) = self.h.to_radians().sin_cos();
        let a = self.c * cos_h;
        let b = self.c * sin_h;

        // Oklab -> LMS, cube-rooted (Björn Ottosson's matrices).
        let l_ = self.l + 0.396_337_78 * a + 0.215_803_76 * b;
        let m_ = self.l - 0.105_561_346 * a - 0.063_854_17 * b;
        let s_ = self.l - 0.089_484_18 * a - 1.291_485_5 * b;

        let (l, m, s) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);

        [
            4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s,
            -1.268_438 * l + 2.609_757_4 * m - 0.341_319_38 * s,
            -0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s,
        ]
    }
}

impl Srgb {
    pub const fn new(r: f32, g: f32, b: f32) -> Self {
        Self { r, g, b }
    }

    /// Read a `0xRRGGBB` literal. The dark palette is transcribed from a
    /// published colourscheme, and hex is the form it is published in —
    /// re-deriving each swatch in Oklch would only invite transcription drift.
    pub fn from_hex(rgb: u32) -> Self {
        let channel = |shift: u32| ((rgb >> shift) & 0xff) as f32 / 255.0;
        Self {
            r: channel(16),
            g: channel(8),
            b: channel(0),
        }
    }

    pub const fn opaque(self) -> Rgba {
        Rgba {
            r: self.r,
            g: self.g,
            b: self.b,
            a: 1.0,
        }
    }

    pub const fn alpha(self, a: f32) -> Rgba {
        Rgba {
            r: self.r,
            g: self.g,
            b: self.b,
            a,
        }
    }

    pub fn hex(self) -> String {
        let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
        format!(
            "#{:02x}{:02x}{:02x}",
            channel(self.r),
            channel(self.g),
            channel(self.b)
        )
    }

    /// WCAG 2.1 relative luminance.
    pub fn relative_luminance(self) -> f32 {
        0.2126 * gamma_decode(self.r)
            + 0.7152 * gamma_decode(self.g)
            + 0.0722 * gamma_decode(self.b)
    }

    /// Composite `self` over `backdrop` at `alpha`, in gamma-encoded space.
    ///
    /// Blending in gamma space is not physically correct, but it is what every
    /// GPU compositor and CSS engine does, so it matches what the display will
    /// actually show for our hairline borders and washes.
    pub fn over(self, backdrop: Srgb, alpha: f32) -> Srgb {
        let a = alpha.clamp(0.0, 1.0);
        Srgb {
            r: self.r * a + backdrop.r * (1.0 - a),
            g: self.g * a + backdrop.g * (1.0 - a),
            b: self.b * a + backdrop.b * (1.0 - a),
        }
    }
}

/// WCAG 2.1 contrast ratio, always `>= 1.0` regardless of argument order.
///
/// AA wants 4.5 for body text and 3.0 for large text; AAA wants 7.0.
///
/// Test-only by design: the spec requires contrast to be *checked*, not painted
/// with. Nothing at runtime should be picking colours by measuring them.
#[cfg(test)]
pub fn contrast_ratio(a: Srgb, b: Srgb) -> f32 {
    let (l1, l2) = (a.relative_luminance(), b.relative_luminance());
    let (hi, lo) = if l1 > l2 { (l1, l2) } else { (l2, l1) };
    (hi + 0.05) / (lo + 0.05)
}

fn gamma_encode(c: f32) -> f32 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.003_130_8 {
        12.92 * c
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

fn gamma_decode(c: f32) -> f32 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32, tol: f32) -> bool {
        (a - b).abs() < tol
    }

    #[test]
    fn a_colour_past_the_gamut_keeps_its_hue() {
        // A deep orange the display cannot show. Clipped per channel it loses
        // nearly all its green and lands on a red; trading chroma keeps it
        // orange.
        let orange = Oklch::new(0.35, 0.15, 55.0).to_srgb();
        assert!(orange.g / orange.r > 0.4, "{}", orange.hex());
        assert!(approx(orange.b, 0.0, 0.02), "{}", orange.hex());
    }

    #[test]
    fn oklch_white_and_black_round_trip() {
        let white = Oklch::new(1.0, 0.0, 0.0).to_srgb();
        assert!(approx(white.r, 1.0, 0.01) && approx(white.g, 1.0, 0.01));

        let black = Oklch::new(0.0, 0.0, 0.0).to_srgb();
        assert!(approx(black.r, 0.0, 0.01) && approx(black.g, 0.0, 0.01));
    }

    #[test]
    fn lightness_is_monotonic() {
        // The property the whole palette rests on: raising `l` never darkens.
        let mut previous = -1.0;
        for step in 0..=20 {
            let luminance = Oklch::new(step as f32 / 20.0, 0.01, 260.0)
                .to_srgb()
                .relative_luminance();
            assert!(
                luminance > previous,
                "luminance dropped at l={}",
                step as f32 / 20.0
            );
            previous = luminance;
        }
    }

    #[test]
    fn contrast_matches_wcag_reference_values() {
        let white = Srgb::new(1.0, 1.0, 1.0);
        let black = Srgb::new(0.0, 0.0, 0.0);
        assert!(approx(contrast_ratio(white, black), 21.0, 0.05));
        assert!(approx(contrast_ratio(white, white), 1.0, 0.001));
        // Order must not matter.
        assert!(approx(
            contrast_ratio(white, black),
            contrast_ratio(black, white),
            0.001
        ));
    }

    #[test]
    fn srgb_formats_as_hex() {
        assert_eq!(Srgb::new(1.0, 0.5, 0.0).hex(), "#ff8000");
    }

    #[test]
    fn hex_survives_the_round_trip() {
        // The palette is written as hex literals, so a channel swapped here
        // would silently repaint every token.
        assert_eq!(Srgb::from_hex(0x0fc5ed).hex(), "#0fc5ed");
    }

    #[test]
    fn alpha_composite_endpoints() {
        let white = Srgb::new(1.0, 1.0, 1.0);
        let black = Srgb::new(0.0, 0.0, 0.0);
        assert_eq!(white.over(black, 1.0), white);
        assert_eq!(white.over(black, 0.0), black);

        let half = white.over(black, 0.5);
        assert!(approx(half.r, 0.5, 0.001));
    }
}
