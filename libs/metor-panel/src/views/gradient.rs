//! Colouring a value by where it falls in a range, as a heat map does.
//!
//! A producer asks for it with the `gradient` metadata key on a component,
//! and a host can override that per view. The setting is text everywhere it
//! is stored — `off`, or a span and an optional palette, as `auto`,
//! `0..100 heat` or `auto series3` — so metadata, saved layouts and the
//! inspector share one grammar. A span on one side of zero shades by
//! distance from its low end; one spanning zero diverges, positives one hue
//! and negatives another, by distance from zero.

use metor_proto_wkt::ComponentMetadata;

/// The metadata key a producer sets to colour a component by value.
pub const GRADIENT_KEY: &str = "gradient";

/// What a component or view asks for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Gradient {
    /// No colouring, even where metadata asks for it.
    Off,
    On {
        span: Span,
        palette: Palette,
    },
}

/// The values a gradient runs across.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Span {
    /// Fitted to the values being coloured.
    Auto,
    Fixed(f64, f64),
}

/// The theme colours a gradient shades toward. Every palette is built from
/// theme tokens, so a gradient re-colours with the theme.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Palette {
    /// The "on" colour for positives, the error colour for negatives.
    #[default]
    Status,
    /// The theme's intensity ramp, by magnitude.
    Heat,
    /// One of the theme's eight plot colours, with the error colour for
    /// negatives.
    Series(u8),
}

impl Palette {
    /// Every preset, in menu order.
    pub fn all() -> impl Iterator<Item = Self> {
        [Self::Status, Self::Heat]
            .into_iter()
            .chain((0..8).map(Self::Series))
    }

    /// The name a setting spells it with.
    pub fn key(&self) -> String {
        match self {
            Self::Status => "status".into(),
            Self::Heat => "heat".into(),
            Self::Series(i) => format!("series{}", i + 1),
        }
    }

    /// What a menu calls it.
    pub fn title(&self) -> String {
        match self {
            Self::Status => "Status".into(),
            Self::Heat => "Heat".into(),
            Self::Series(i) => format!("Series {}", i + 1),
        }
    }

    fn parse(word: &str) -> Option<Self> {
        match word {
            "status" => Some(Self::Status),
            "heat" => Some(Self::Heat),
            _ => {
                let n: u8 = word.strip_prefix("series")?.parse().ok()?;
                (1..=8).contains(&n).then(|| Self::Series(n - 1))
            }
        }
    }
}

impl Span {
    /// `auto` or `lo..hi`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if text.eq_ignore_ascii_case("auto") {
            return Ok(Self::Auto);
        }
        let (lo, hi) = text
            .split_once("..")
            .ok_or_else(|| format!("expected auto or lo..hi, not `{text}`"))?;
        let number = |s: &str| {
            s.trim()
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite())
                .ok_or_else(|| format!("`{}` is not a number", s.trim()))
        };
        let (lo, hi) = (number(lo)?, number(hi)?);
        if lo >= hi {
            return Err(format!("the range {lo}..{hi} is empty"));
        }
        Ok(Self::Fixed(lo, hi))
    }

    pub fn text(&self) -> String {
        match self {
            Self::Auto => "auto".into(),
            Self::Fixed(lo, hi) => format!("{}..{}", short(*lo), short(*hi)),
        }
    }
}

impl Gradient {
    /// `off`, or a span and an optional palette in either order; a palette
    /// alone fits its span. Blank is no setting.
    pub fn parse(text: &str) -> Result<Option<Self>, String> {
        let text = text.trim().to_ascii_lowercase();
        if text.is_empty() {
            return Ok(None);
        }
        if text == "off" || text == "none" {
            return Ok(Some(Self::Off));
        }
        let mut span = None;
        let mut palette = None;
        for word in text.split_whitespace() {
            if let Some(p) = Palette::parse(word) {
                palette = Some(p);
            } else if span.is_none() {
                span = Some(Span::parse(word)?);
            } else {
                return Err(format!("`{word}` is not a palette"));
            }
        }
        Ok(Some(Self::On {
            span: span.unwrap_or(Span::Auto),
            palette: palette.unwrap_or_default(),
        }))
    }

    pub fn text(&self) -> String {
        match self {
            Self::Off => "off".into(),
            Self::On { span, palette } if *palette == Palette::Status => span.text(),
            Self::On { span, palette } => format!("{} {}", span.text(), palette.key()),
        }
    }

    /// The same setting with another span, coloured as before; from off,
    /// in the default palette.
    pub fn with_span(self, span: Span) -> Self {
        match self {
            Self::On { palette, .. } => Self::On { span, palette },
            Self::Off => Self::On {
                span,
                palette: Palette::default(),
            },
        }
    }

    /// The same setting in another palette; from off, fitted.
    pub fn with_palette(self, palette: Palette) -> Self {
        match self {
            Self::On { span, .. } => Self::On { span, palette },
            Self::Off => Self::On {
                span: Span::Auto,
                palette,
            },
        }
    }

    pub fn span(&self) -> Option<Span> {
        match self {
            Self::On { span, .. } => Some(*span),
            Self::Off => None,
        }
    }

    pub fn palette(&self) -> Option<Palette> {
        match self {
            Self::On { palette, .. } => Some(*palette),
            Self::Off => None,
        }
    }

    /// What the producer asked for, ignoring a value that doesn't parse.
    pub fn from_metadata(meta: &ComponentMetadata) -> Option<Self> {
        Self::parse(meta.metadata.get(GRADIENT_KEY)?).ok().flatten()
    }

    /// The scale and palette this setting colours `values` with; `fitted`
    /// is a range a host fitted over more values than these, which an
    /// `auto` span prefers.
    pub fn scale(
        &self,
        fitted: Option<Scale>,
        values: impl IntoIterator<Item = f64>,
    ) -> Option<(Scale, Palette)> {
        let Self::On { span, palette } = *self else {
            return None;
        };
        let scale = match span {
            Span::Fixed(lo, hi) => Scale { lo, hi },
            Span::Auto => fitted.or_else(|| Scale::fit(values))?,
        };
        Some((scale, palette))
    }
}

/// A resolved range to colour on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scale {
    lo: f64,
    hi: f64,
}

impl Scale {
    /// A range covering `values`, anchored at zero as Perspective's is, so
    /// the shade reads as magnitude: `0..max` when all are positive,
    /// `min..0` when all are negative, `-m..m` when they straddle zero.
    /// Nothing to colour when there are no values, or all are zero.
    pub fn fit(values: impl IntoIterator<Item = f64>) -> Option<Self> {
        let (mut lo, mut hi) = (0.0f64, 0.0f64);
        for v in values.into_iter().filter(|v| v.is_finite()) {
            lo = lo.min(v);
            hi = hi.max(v);
        }
        if lo < 0.0 && hi > 0.0 {
            let m = hi.max(-lo);
            (lo, hi) = (-m, m);
        }
        (lo < hi).then_some(Self { lo, hi })
    }

    /// The range as a fixed span reads.
    pub fn text(&self) -> String {
        Span::Fixed(self.lo, self.hi).text()
    }

    /// Where `value` sits: 0 at the plain end, toward 1 for the positive hue
    /// and toward -1 for the negative one.
    pub fn position(&self, value: f64) -> f32 {
        if !value.is_finite() {
            return 0.0;
        }
        let t = if self.lo < 0.0 && self.hi > 0.0 {
            if value >= 0.0 {
                value / self.hi
            } else {
                -(value / self.lo)
            }
        } else if self.hi <= 0.0 {
            // All negative: deepest at the low end.
            -((self.hi - value) / (self.hi - self.lo))
        } else {
            (value - self.lo) / (self.hi - self.lo)
        };
        t.clamp(-1.0, 1.0) as f32
    }
}

/// A bound as a person would type it: four significant digits, no float
/// noise.
fn short(v: f64) -> String {
    if v == 0.0 || !v.is_finite() {
        return format!("{v}");
    }
    let digits = 4 - 1 - v.abs().log10().floor() as i32;
    // Dividing by a power of ten, never multiplying by a fraction of one,
    // keeps the result exact where it can be.
    let rounded = if digits >= 0 {
        let scale = 10f64.powi(digits);
        (v * scale).round() / scale
    } else {
        let scale = 10f64.powi(-digits);
        (v / scale).round() * scale
    };
    format!("{rounded}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on(span: Span, palette: Palette) -> Option<Gradient> {
        Some(Gradient::On { span, palette })
    }

    #[test]
    fn settings_parse_and_print_back() {
        assert_eq!(Gradient::parse(" ").unwrap(), None);
        assert_eq!(Gradient::parse("off").unwrap(), Some(Gradient::Off));
        assert_eq!(
            Gradient::parse("AUTO").unwrap(),
            on(Span::Auto, Palette::Status)
        );
        assert_eq!(
            Gradient::parse("heat").unwrap(),
            on(Span::Auto, Palette::Heat)
        );
        let ranged = Gradient::parse("-1.5..2 series3").unwrap();
        assert_eq!(ranged, on(Span::Fixed(-1.5, 2.0), Palette::Series(2)));
        for g in [ranged.unwrap(), Gradient::parse("auto").unwrap().unwrap()] {
            assert_eq!(Gradient::parse(&g.text()).unwrap(), Some(g));
        }
        assert!(Gradient::parse("3..1").is_err());
        assert!(Gradient::parse("warm").is_err());
        assert!(Gradient::parse("auto series9").is_err());
    }

    #[test]
    fn spans_and_palettes_change_independently() {
        let g = Gradient::Off.with_palette(Palette::Heat);
        assert_eq!(Some(g), on(Span::Auto, Palette::Heat));
        let g = g.with_span(Span::Fixed(0.0, 5.0));
        assert_eq!(Some(g), on(Span::Fixed(0.0, 5.0), Palette::Heat));
        assert_eq!(g.text(), "0..5 heat");
    }

    #[test]
    fn bounds_print_without_float_noise() {
        assert_eq!(short(4.199999999), "4.2");
        assert_eq!(short(1_234_567.0), "1235000");
        assert_eq!(short(-0.000123456), "-0.0001235");
    }

    #[test]
    fn a_fitted_range_is_anchored_at_zero() {
        let pos = Scale::fit([2.0, 8.0, 4.0]).unwrap();
        assert_eq!(pos.position(8.0), 1.0);
        assert_eq!(pos.position(4.0), 0.5);
        let both = Scale::fit([-2.0, 1.0]).unwrap();
        assert_eq!(both.position(-2.0), -1.0);
        assert_eq!(both.position(1.0), 0.5);
        let neg = Scale::fit([-4.0, -1.0]).unwrap();
        assert_eq!(neg.position(-4.0), -1.0);
        assert_eq!(neg.position(0.0), 0.0);
        assert_eq!(Scale::fit([0.0, f64::NAN]), None);
    }

    #[test]
    fn a_fixed_span_shades_from_its_low_end() {
        let fixed = Gradient::On {
            span: Span::Fixed(10.0, 20.0),
            palette: Palette::Status,
        };
        let (scale, _) = fixed.scale(None, []).unwrap();
        assert_eq!(scale.position(10.0), 0.0);
        assert_eq!(scale.position(15.0), 0.5);
        assert_eq!(scale.position(99.0), 1.0);
        // Auto prefers the host's wider fit to its own values.
        let fitted = Scale::fit([100.0]);
        let auto = fixed.with_span(Span::Auto);
        let (scale, _) = auto.scale(fitted, [1.0]).unwrap();
        assert_eq!(scale.position(50.0), 0.5);
        assert_eq!(Gradient::Off.scale(fitted, [1.0]), None);
    }
}
