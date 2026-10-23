//! Display settings and the code-to-colour table they build.

use serde::{Deserialize, Serialize};

/// The bit depth a `mono_f32` frame is quantised to before its LUT.
pub const F32_BITS: u8 = 12;
/// Strength of [`Curve::Log`]: `ln(1 + a·t) / ln(1 + a)`.
const LOG_STRENGTH: f32 = 1000.0;
/// The range [`Curve::Asinh`] clamps its softening to.
const SOFTENING: (f32, f32) = (1e-6, 1e6);
/// The range [`Curve::Gamma`] clamps γ to.
const GAMMA: (f32, f32) = (1e-2, 1e2);

/// How an image component's samples become colours.
#[derive(Clone, Copy, Debug, Default, PartialEq, facet::Facet, Serialize, Deserialize)]
pub struct Display {
    pub levels: Levels,
    pub curve: Curve,
    pub colormap: Colormap,
    /// Paints the top code in the theme's saturation colour.
    pub saturation: bool,
}

/// Where black and white sit, in raw sample values.
#[derive(Clone, Copy, Debug, PartialEq, facet::Facet, Serialize, Deserialize)]
#[repr(u8)]
pub enum Levels {
    Manual {
        black: f64,
        white: f64,
    },
    /// Percentiles of the histogram, each in `0..=1`. `lock` holds the last
    /// levels instead of following new frames.
    Auto {
        low: f64,
        high: f64,
        lock: bool,
    },
    /// Histogram equalisation: the LUT follows the cumulative histogram.
    Equalize,
}

impl Default for Levels {
    fn default() -> Self {
        Self::Auto {
            low: 0.005,
            high: 0.9995,
            lock: false,
        }
    }
}

/// The transfer curve applied between the levels.
#[derive(Clone, Copy, Debug, Default, PartialEq, facet::Facet, Serialize, Deserialize)]
#[repr(u8)]
pub enum Curve {
    #[default]
    Linear,
    Sqrt,
    Log,
    /// `asinh(t / s) / asinh(1 / s)`, `s` clamped to `1e-6..=1e6`; a
    /// non-finite or non-positive `s` is linear.
    Asinh {
        softening: f32,
    },
    /// `t^(1/γ)`, so `γ > 1` brightens. `γ` is clamped to `0.01..=100`; a
    /// non-finite or non-positive `γ` is linear.
    Gamma(f32),
}

impl Curve {
    /// Maps `t` in `0..=1` onto `0..=1`, fixing both ends.
    pub fn apply(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Self::Linear => t,
            Self::Sqrt => t.sqrt(),
            Self::Log => (LOG_STRENGTH * t).ln_1p() / LOG_STRENGTH.ln_1p(),
            Self::Asinh { softening } => match usable(softening, SOFTENING) {
                Some(s) => (t / s).asinh() / s.recip().asinh(),
                None => t,
            },
            Self::Gamma(gamma) => match usable(gamma, GAMMA) {
                Some(g) => t.powf(g.recip()),
                None => t,
            },
        }
    }
}

/// A finite, positive curve parameter clamped to `range`.
fn usable(value: f32, (min, max): (f32, f32)) -> Option<f32> {
    (value.is_finite() && value > 0.0).then(|| value.clamp(min, max))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, facet::Facet, Serialize, Deserialize)]
#[repr(u8)]
pub enum Colormap {
    #[default]
    Gray,
    Inferno,
    Viridis,
    Magma,
    Cividis,
    /// Diverging, for signed fields: auto levels sit symmetric about zero.
    RedBlue,
}

impl Colormap {
    /// `t` in `0..=1` as packed BGRA.
    pub fn bgra(self, t: f32) -> u32 {
        let gradient = match self {
            Self::Gray => {
                let y = (t.clamp(0.0, 1.0) * 255.0).round() as u8;
                return pack(y, y, y);
            }
            Self::Inferno => colorous::INFERNO,
            Self::Viridis => colorous::VIRIDIS,
            Self::Magma => colorous::MAGMA,
            Self::Cividis => colorous::CIVIDIS,
            // Reversed so low is blue and high is red.
            Self::RedBlue => return reversed(colorous::RED_BLUE, t),
        };
        let c = gradient.eval_continuous(f64::from(t.clamp(0.0, 1.0)));
        pack(c.r, c.g, c.b)
    }
}

fn reversed(gradient: colorous::Gradient, t: f32) -> u32 {
    let c = gradient.eval_continuous(f64::from(1.0 - t.clamp(0.0, 1.0)));
    pack(c.r, c.g, c.b)
}

fn pack(r: u8, g: u8, b: u8) -> u32 {
    u32::from_le_bytes([b, g, r, 0xff])
}

/// One packed BGRA colour per sample code.
#[derive(Clone, Debug, PartialEq)]
pub struct Lut {
    entries: Vec<u32>,
    /// The affine map from an `f32` sample to a code: `(offset, scale)`.
    f32_codes: (f32, f32),
}

impl Lut {
    /// `2^bits` entries, `bits` clamped to `1..=16`. `black == white` is a
    /// step at `black`; `black > white` inverts; non-finite levels span every
    /// code. `saturated` is BGRA.
    pub fn build(bits: u8, black: f64, white: f64, display: &Display, saturated: [u8; 4]) -> Self {
        let codes = 1usize << bits.clamp(1, 16);
        let (black, white) = finite_levels(black, white, (0.0, (codes - 1) as f64));
        let mut entries: Vec<u32> = (0..codes)
            .map(|code| {
                let t = level(code as f64, black, white);
                display.colormap.bgra(display.curve.apply(t))
            })
            .collect();
        if display.saturation
            && let Some(top) = entries.last_mut()
        {
            *top = u32::from_le_bytes(saturated);
        }
        Self {
            entries,
            f32_codes: (0.0, 1.0),
        }
    }

    /// A [`F32_BITS`] table for `mono_f32`, with `black` and `white` in
    /// sample units mapped onto the first and last code; non-finite levels
    /// read as `0..=1`. Floats do not saturate, so `saturation` is ignored.
    pub fn build_f32(black: f32, white: f32, display: &Display) -> Self {
        let top = ((1u32 << F32_BITS) - 1) as f32;
        let display = Display {
            saturation: false,
            ..*display
        };
        let mut lut = Self::build(F32_BITS, 0.0, f64::from(top), &display, [0; 4]);
        let (black, white) = finite_levels(black, white, (0.0, 1.0));
        let span = white - black;
        let scale = if span == 0.0 {
            f32::INFINITY
        } else {
            top / span
        };
        lut.f32_codes = (black, scale);
        lut
    }

    /// Replaces the entries with the cumulative histogram through the curve
    /// and colormap; the top code keeps the saturation colour when enabled.
    pub fn equalized(
        mut self,
        histogram: &super::histogram::Histogram,
        display: &Display,
        saturated: [u8; 4],
    ) -> Self {
        let cdf = histogram.cdf();
        let codes = self.entries.len();
        for (code, entry) in self.entries.iter_mut().enumerate() {
            let t = cdf
                .get(code * cdf.len() / codes.max(1))
                .copied()
                .unwrap_or(0.0);
            *entry = display.colormap.bgra(display.curve.apply(t));
        }
        if display.saturation
            && let Some(top) = self.entries.last_mut()
        {
            *top = u32::from_le_bytes(saturated);
        }
        self
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The colour of `code`; codes past the table read as the top entry.
    pub fn get(&self, code: u32) -> u32 {
        let last = self.entries.len().saturating_sub(1);
        self.entries
            .get((code as usize).min(last))
            .copied()
            .unwrap_or(0)
    }

    /// The code of an `f32` sample; NaN reads as code 0.
    pub fn f32_code(&self, v: f32) -> u32 {
        let (offset, scale) = self.f32_codes;
        let top = (self.entries.len().saturating_sub(1)) as f32;
        let code = (v - offset) * scale;
        // A step LUT (`scale` infinite) sends `v == offset` to NaN.
        if code.is_nan() {
            return if v.is_nan() { 0 } else { top as u32 };
        }
        code.clamp(0.0, top) as u32
    }
}

/// The levels, or `full` when their span is not finite.
fn finite_levels<T: Into<f64> + Copy>(black: T, white: T, full: (T, T)) -> (T, T) {
    match (white.into() - black.into()).is_finite() {
        true => (black, white),
        false => full,
    }
}

/// `v` placed between the levels of a `codes`-code table, in `0..=1`;
/// non-finite levels span every code.
pub(crate) fn level_of(v: f64, black: f64, white: f64, codes: usize) -> f32 {
    let (black, white) = finite_levels(black, white, (0.0, codes.saturating_sub(1) as f64));
    level(v, black, white)
}

/// `v` placed between the levels, in `0..=1`.
fn level(v: f64, black: f64, white: f64) -> f32 {
    let span = white - black;
    if span == 0.0 {
        return if v < black { 0.0 } else { 1.0 };
    }
    ((v - black) / span).clamp(0.0, 1.0) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAT: [u8; 4] = [0, 0, 255, 255];
    const CURVES: [Curve; 6] = [
        Curve::Linear,
        Curve::Sqrt,
        Curve::Log,
        Curve::Asinh { softening: 0.1 },
        Curve::Gamma(2.2),
        Curve::Gamma(0.5),
    ];
    const COLORMAPS: [Colormap; 5] = [
        Colormap::Gray,
        Colormap::Inferno,
        Colormap::Viridis,
        Colormap::Magma,
        Colormap::Cividis,
    ];

    fn display(curve: Curve, colormap: Colormap) -> Display {
        Display {
            curve,
            colormap,
            ..Display::default()
        }
    }

    fn luma(bgra: u32) -> f32 {
        let [b, g, r, _] = bgra.to_le_bytes();
        0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b)
    }

    #[test]
    fn levels_map_to_the_colormap_ends_and_clamp() {
        for colormap in COLORMAPS {
            let lut = Lut::build(12, 200.0, 4000.0, &display(Curve::Linear, colormap), SAT);
            let first = colormap.bgra(0.0);
            let last = colormap.bgra(1.0);
            assert_eq!(lut.get(200), first);
            assert_eq!(lut.get(4000), last);
            assert_eq!(lut.get(0), first);
            assert_eq!(lut.get(4095), last);
        }
    }

    #[test]
    fn every_curve_is_monotonic_for_every_colormap() {
        for colormap in COLORMAPS {
            for curve in CURVES {
                let lut = Lut::build(12, 0.0, 4095.0, &display(curve, colormap), SAT);
                let lumas: Vec<f32> = (0..4096).map(|c| luma(lut.get(c))).collect();
                for pair in lumas.windows(2) {
                    // Perceptual maps are monotonic in lightness; the
                    // tolerance covers 8-bit rounding.
                    assert!(pair[1] >= pair[0] - 1.0, "{colormap:?} {curve:?}");
                }
                assert!(lumas[4095] > lumas[0], "{colormap:?} {curve:?}");
            }
        }
    }

    #[test]
    fn curves_fix_both_ends() {
        for curve in CURVES {
            assert_eq!(curve.apply(0.0), 0.0, "{curve:?}");
            assert!((curve.apply(1.0) - 1.0).abs() < 1e-6, "{curve:?}");
            assert_eq!(curve.apply(-1.0), 0.0, "{curve:?}");
            assert!((curve.apply(2.0) - 1.0).abs() < 1e-6, "{curve:?}");
        }
    }

    #[test]
    fn gamma_one_is_linear() {
        let linear = Lut::build(12, 0.0, 4095.0, &Display::default(), SAT);
        let gamma = Lut::build(
            12,
            0.0,
            4095.0,
            &display(Curve::Gamma(1.0), Colormap::Gray),
            SAT,
        );
        assert_eq!(linear, gamma);
    }

    #[test]
    fn bad_curve_parameters_are_linear() {
        for curve in [
            Curve::Gamma(0.0),
            Curve::Gamma(-2.0),
            Curve::Gamma(f32::NAN),
            Curve::Gamma(f32::INFINITY),
            Curve::Asinh { softening: 0.0 },
            Curve::Asinh {
                softening: f32::NAN,
            },
            Curve::Asinh {
                softening: f32::INFINITY,
            },
        ] {
            assert_eq!(curve.apply(0.25), 0.25, "{curve:?}");
        }
    }

    #[test]
    fn extreme_curve_parameters_stay_graded() {
        for curve in [
            Curve::Gamma(1e-45),
            Curve::Gamma(1e30),
            Curve::Asinh { softening: 1e-40 },
            Curve::Asinh { softening: 1e30 },
        ] {
            let mid = curve.apply(0.5);
            assert!(mid.is_finite() && (0.0..=1.0).contains(&mid), "{curve:?}");
            assert_eq!(curve.apply(0.0), 0.0, "{curve:?}");
            assert!((curve.apply(1.0) - 1.0).abs() < 1e-5, "{curve:?}");
        }
        let gamma = display(Curve::Gamma(1e-45), Colormap::Gray);
        let lut = Lut::build(12, 0.0, 4095.0, &gamma, SAT);
        assert_ne!(lut.get(4094), lut.get(0));
    }

    #[test]
    fn non_finite_levels_span_every_code() {
        let full = Lut::build(8, 0.0, 255.0, &Display::default(), SAT);
        for (black, white) in [
            (f64::NAN, 100.0),
            (0.0, f64::INFINITY),
            (f64::NEG_INFINITY, f64::INFINITY),
            (f64::NAN, f64::NAN),
        ] {
            let lut = Lut::build(8, black, white, &Display::default(), SAT);
            assert_eq!(lut, full, "{black} {white}");
        }
        let lut = Lut::build_f32(f32::NAN, 2.0, &Display::default());
        assert_eq!((lut.f32_code(0.0), lut.f32_code(1.0)), (0, 4095));
    }

    #[test]
    fn f32_ignores_saturation() {
        let display = Display {
            saturation: true,
            ..Display::default()
        };
        let lut = Lut::build_f32(0.0, 1.0, &display);
        assert_eq!(lut.get(4095), pack(255, 255, 255));
    }

    #[test]
    fn length_is_two_to_the_bits() {
        for (bits, len) in [
            (1, 2),
            (8, 256),
            (12, 4096),
            (16, 65536),
            (0, 2),
            (20, 65536),
        ] {
            let lut = Lut::build(bits, 0.0, 1.0, &Display::default(), SAT);
            assert_eq!(lut.len(), len, "{bits}");
        }
    }

    #[test]
    fn one_bit_is_black_and_white() {
        let lut = Lut::build(1, 0.0, 1.0, &Display::default(), SAT);
        assert_eq!(
            (lut.get(0), lut.get(1)),
            (pack(0, 0, 0), pack(255, 255, 255))
        );
    }

    #[test]
    fn saturation_replaces_only_the_top_code() {
        let off = Lut::build(8, 0.0, 200.0, &Display::default(), SAT);
        let on_display = Display {
            saturation: true,
            ..Display::default()
        };
        let on = Lut::build(8, 0.0, 200.0, &on_display, SAT);
        assert_eq!(on.get(255), u32::from_le_bytes(SAT));
        assert_eq!(off.get(255), pack(255, 255, 255));
        assert_eq!(on.get(254), off.get(254));
        assert_eq!(on.get(1000), u32::from_le_bytes(SAT));
    }

    #[test]
    fn equal_levels_are_a_step() {
        let lut = Lut::build(8, 100.0, 100.0, &Display::default(), SAT);
        assert_eq!(lut.get(99), pack(0, 0, 0));
        assert_eq!(lut.get(100), pack(255, 255, 255));
        assert_eq!(lut.get(255), pack(255, 255, 255));
    }

    #[test]
    fn inverted_levels_invert() {
        let lut = Lut::build(8, 200.0, 50.0, &Display::default(), SAT);
        assert_eq!(lut.get(255), pack(0, 0, 0));
        assert_eq!(lut.get(200), pack(0, 0, 0));
        assert_eq!(lut.get(50), pack(255, 255, 255));
        assert_eq!(lut.get(0), pack(255, 255, 255));
        assert!(luma(lut.get(100)) > luma(lut.get(150)));
    }

    #[test]
    fn f32_codes_span_the_levels() {
        let lut = Lut::build_f32(-1.0, 3.0, &Display::default());
        assert_eq!(lut.len(), 4096);
        assert_eq!(lut.f32_code(-1.0), 0);
        assert_eq!(lut.f32_code(3.0), 4095);
        assert_eq!(lut.f32_code(1.0), 2047);
        assert_eq!(lut.f32_code(-50.0), 0);
        assert_eq!(lut.f32_code(f32::INFINITY), 4095);
        assert_eq!(lut.f32_code(f32::NAN), 0);
    }

    #[test]
    fn f32_equal_levels_are_a_step() {
        let lut = Lut::build_f32(0.5, 0.5, &Display::default());
        assert_eq!(lut.f32_code(0.4), 0);
        assert_eq!(lut.f32_code(0.5), 4095);
        assert_eq!(lut.f32_code(0.6), 4095);
    }

    #[test]
    fn an_equalized_lut_is_monotonic_and_spans_the_range() {
        use crate::views::image_view::Rect;
        use crate::views::image_view::histogram::Histogram;
        use zerocopy::byteorder::{LE, U16};
        // Skewed: most samples dark, a few bright.
        let samples: Vec<U16<LE>> = (0..1000u16)
            .map(|i| U16::new(if i < 900 { i % 50 } else { 3000 + i }))
            .collect();
        let hist = Histogram::of_u16(&samples, 1000, 12, Rect::new(0, 0, 1000, 1));
        let lut = Lut::build(12, 0.0, 4095.0, &Display::default(), SAT).equalized(
            &hist,
            &Display::default(),
            SAT,
        );
        let lumas: Vec<f32> = (0..4096).map(|c| luma(lut.get(c))).collect();
        assert!(lumas.windows(2).all(|w| w[1] >= w[0]));
        assert_eq!(lut.get(4095), pack(255, 255, 255));
        assert!(luma(lut.get(0)) < 10.0);
        // The dark half of the codes takes most of the output range.
        assert!(luma(lut.get(49)) > 200.0);
    }

    #[test]
    fn red_blue_runs_blue_to_red() {
        let [b0, _, r0, _] = Colormap::RedBlue.bgra(0.0).to_le_bytes();
        let [b1, _, r1, _] = Colormap::RedBlue.bgra(1.0).to_le_bytes();
        assert!(b0 > r0 && r1 > b1);
    }
}
