//! Colouring a value by where it falls in a range, as a heat map does.
//!
//! A producer asks for it with the `gradient` metadata key on a component —
//! `auto`, or a `lo..hi` range — and a host can override that per view. The
//! setting is text everywhere it is stored so metadata, saved layouts and the
//! inspector share one grammar. A range on one side of zero shades one hue
//! by distance from its low end; one spanning zero diverges, positives one
//! hue and negatives another, by distance from zero.

use metor_proto_wkt::ComponentMetadata;

/// The metadata key a producer sets to colour a component by value.
pub const GRADIENT_KEY: &str = "gradient";

/// What a component or view asks for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Gradient {
    /// No colouring, even where metadata asks for it.
    Off,
    /// A range fitted to the values being coloured.
    Auto,
    /// A fixed range.
    Range(f64, f64),
}

impl Gradient {
    /// `off`, `auto` or `lo..hi`; blank is no setting.
    pub fn parse(text: &str) -> Result<Option<Self>, String> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(None);
        }
        match text.to_ascii_lowercase().as_str() {
            "off" | "none" => return Ok(Some(Self::Off)),
            "auto" => return Ok(Some(Self::Auto)),
            _ => {}
        }
        let (lo, hi) = text
            .split_once("..")
            .ok_or_else(|| format!("expected auto, off or lo..hi, not `{text}`"))?;
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
        Ok(Some(Self::Range(lo, hi)))
    }

    pub fn text(&self) -> String {
        match self {
            Self::Off => "off".into(),
            Self::Auto => "auto".into(),
            Self::Range(lo, hi) => format!("{lo}..{hi}"),
        }
    }

    /// What the producer asked for, ignoring a value that doesn't parse.
    pub fn from_metadata(meta: &ComponentMetadata) -> Option<Self> {
        Self::parse(meta.metadata.get(GRADIENT_KEY)?).ok().flatten()
    }

    /// The scale this setting colours `values` on; `fitted` is a range a
    /// host fitted over more values than these, which `auto` prefers.
    pub fn scale(
        &self,
        fitted: Option<Scale>,
        values: impl IntoIterator<Item = f64>,
    ) -> Option<Scale> {
        match *self {
            Self::Off => None,
            Self::Range(lo, hi) => Some(Scale { lo, hi }),
            Self::Auto => fitted.or_else(|| Scale::fit(values)),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_parse_and_print_back() {
        assert_eq!(Gradient::parse(" ").unwrap(), None);
        assert_eq!(Gradient::parse("AUTO").unwrap(), Some(Gradient::Auto));
        assert_eq!(Gradient::parse("off").unwrap(), Some(Gradient::Off));
        let range = Gradient::parse("-1.5 .. 2").unwrap().unwrap();
        assert_eq!(range, Gradient::Range(-1.5, 2.0));
        assert_eq!(Gradient::parse(&range.text()).unwrap(), Some(range));
        assert!(Gradient::parse("3..1").is_err());
        assert!(Gradient::parse("warm").is_err());
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
    fn a_fixed_range_shades_from_its_low_end() {
        let scale = Gradient::Range(10.0, 20.0).scale(None, []).unwrap();
        assert_eq!(scale.position(10.0), 0.0);
        assert_eq!(scale.position(15.0), 0.5);
        assert_eq!(scale.position(99.0), 1.0);
        // Auto prefers the host's wider fit to its own values.
        let fitted = Scale::fit([100.0]);
        let auto = Gradient::Auto.scale(fitted, [1.0]).unwrap();
        assert_eq!(auto.position(50.0), 0.5);
        assert_eq!(Gradient::Off.scale(fitted, [1.0]), None);
    }
}
