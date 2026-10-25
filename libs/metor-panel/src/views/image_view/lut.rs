//! Display settings and the code-to-colour table they build.

use serde::{Deserialize, Serialize};

/// The range [`Curve::Asinh`] clamps its softening to.
const SOFTENING: (f32, f32) = (1e-6, 1e6);

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
    /// Percentiles of the histogram, each in `0..=1`.
    Auto {
        low: f64,
        high: f64,
    },
}

impl Default for Levels {
    fn default() -> Self {
        Self::Auto {
            low: 0.005,
            high: 0.9995,
        }
    }
}

/// The transfer curve applied between the levels.
#[derive(Clone, Copy, Debug, Default, PartialEq, facet::Facet, Serialize, Deserialize)]
#[repr(u8)]
pub enum Curve {
    #[default]
    Linear,
    /// `asinh(t / s) / asinh(1 / s)`, `s` clamped to `1e-6..=1e6`; a
    /// non-finite or non-positive `s` is linear.
    Asinh { softening: f32 },
}

impl Curve {
    /// Maps `t` in `0..=1` onto `0..=1`, fixing both ends.
    pub fn apply(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Self::Linear => t,
            Self::Asinh { softening } if softening.is_finite() && softening > 0.0 => {
                let s = softening.clamp(SOFTENING.0, SOFTENING.1);
                (t / s).asinh() / s.recip().asinh()
            }
            Self::Asinh { .. } => t,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, facet::Facet, Serialize, Deserialize)]
#[repr(u8)]
pub enum Colormap {
    #[default]
    Gray,
    Inferno,
}

impl Colormap {
    /// `t` in `0..=1` as packed BGRA.
    pub fn bgra(self, t: f32) -> u32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Self::Gray => {
                let y = (t * 255.0).round() as u8;
                pack(y, y, y)
            }
            Self::Inferno => {
                let c = colorous::INFERNO.eval_continuous(f64::from(t));
                pack(c.r, c.g, c.b)
            }
        }
    }
}

fn pack(r: u8, g: u8, b: u8) -> u32 {
    u32::from_le_bytes([b, g, r, 0xff])
}

/// One packed BGRA colour per sample code.
#[derive(Clone, Debug, PartialEq)]
pub struct Lut {
    entries: Vec<u32>,
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
        Self { entries }
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
}

/// The levels, or `full` when their span is not finite.
fn finite_levels(black: f64, white: f64, full: (f64, f64)) -> (f64, f64) {
    match (white - black).is_finite() {
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
    const CURVES: [Curve; 2] = [Curve::Linear, Curve::Asinh { softening: 0.1 }];
    const COLORMAPS: [Colormap; 2] = [Colormap::Gray, Colormap::Inferno];

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
    fn bad_curve_parameters_are_linear() {
        for curve in [
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
            Curve::Asinh { softening: 1e-40 },
            Curve::Asinh { softening: 1e30 },
        ] {
            let mid = curve.apply(0.5);
            assert!(mid.is_finite() && (0.0..=1.0).contains(&mid), "{curve:?}");
            assert_eq!(curve.apply(0.0), 0.0, "{curve:?}");
            assert!((curve.apply(1.0) - 1.0).abs() < 1e-5, "{curve:?}");
        }
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
}
