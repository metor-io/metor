//! Calibrations a mux mapping applies to a scalar `f64`.

mod its90;

use schemars::JsonSchema;
use serde::Deserialize;

/// A thermocouple type, by its letter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TcKind {
    K,
    J,
    T,
    E,
}

/// A calibration from a source value to an engineering value.
#[derive(Clone, Debug, PartialEq)]
pub enum Cal {
    /// Ascending coefficients: `c0 + c1·x + ..`.
    Poly(Box<[f64]>),
    /// Points in strictly increasing `x`, interpolated and clamped at the ends.
    Table(Box<[(f64, f64)]>),
    /// Cold-junction-compensated volts to °C.
    Thermocouple { kind: TcKind },
    /// `x / excitation` scaled from `span` to `eng`; `excitation` is the byte
    /// offset of an `f64` in the source record.
    Ratiometric {
        excitation: u32,
        span: (f64, f64),
        eng: (f64, f64),
    },
}

impl Cal {
    /// `None` when empty or a coefficient is not finite.
    pub fn poly(coeffs: &[f64]) -> Option<Self> {
        let finite = coeffs.iter().all(|c| c.is_finite());
        (finite && !coeffs.is_empty()).then(|| Self::Poly(coeffs.into()))
    }

    /// `None` when empty, not strictly increasing in `x`, or not finite.
    pub fn table(points: &[(f64, f64)]) -> Option<Self> {
        let finite = points.iter().all(|(x, y)| x.is_finite() && y.is_finite());
        let increasing = points.windows(2).all(|w| matches!(w, [a, b] if a.0 < b.0));
        (finite && increasing && !points.is_empty()).then(|| Self::Table(points.into()))
    }

    /// `None` when `span` has zero width or a value is not finite.
    pub fn ratiometric(excitation: u32, span: (f64, f64), eng: (f64, f64)) -> Option<Self> {
        let finite = [span.0, span.1, eng.0, eng.1].iter().all(|v| v.is_finite());
        (finite && span.0 != span.1).then_some(Self::Ratiometric {
            excitation,
            span,
            eng,
        })
    }

    /// Evaluates at `x`; `record` is the source record, read by `Ratiometric`.
    pub fn eval(&self, x: f64, record: &[u8]) -> f64 {
        match self {
            Self::Poly(coeffs) => poly(coeffs, x),
            Self::Table(points) => table(points, x),
            Self::Thermocouple { kind } => thermocouple(*kind, x),
            Self::Ratiometric {
                excitation,
                span,
                eng,
            } => {
                let excitation = read_f64(record, *excitation).unwrap_or(f64::NAN);
                ratiometric(x, excitation, *span, *eng)
            }
        }
    }
}

/// Horner, ascending coefficients.
pub(crate) fn poly(coeffs: &[f64], x: f64) -> f64 {
    coeffs.iter().rev().fold(0.0, |acc, c| acc * x + c)
}

/// Linear interpolation, clamped to the end points; NaN for a NaN `x`.
pub(crate) fn table(points: &[(f64, f64)], x: f64) -> f64 {
    match segment(points, x) {
        Some(Segment::End(y)) => y,
        Some(Segment::Between(a, b)) => lerp(a, b, x),
        None => f64::NAN,
    }
}

/// Where `x` falls in a table.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Segment {
    /// At or past an end point, which gives `y`.
    End(f64),
    /// Between two neighbors, `a.0 < x <= b.0`.
    Between((f64, f64), (f64, f64)),
}

/// The segment of `points` holding `x`; `None` when empty or `x` is NaN.
fn segment(points: &[(f64, f64)], x: f64) -> Option<Segment> {
    let (&first, &last) = (points.first()?, points.last()?);
    if x.is_nan() {
        return None;
    }
    if x <= first.0 {
        return Some(Segment::End(first.1));
    }
    if x >= last.0 {
        return Some(Segment::End(last.1));
    }
    let i = points.partition_point(|p| p.0 < x);
    let (&a, &b) = (points.get(i.checked_sub(1)?)?, points.get(i)?);
    Some(Segment::Between(a, b))
}

fn lerp((x0, y0): (f64, f64), (x1, y1): (f64, f64), x: f64) -> f64 {
    let t = (x - x0) / (x1 - x0);
    y0 * (1.0 - t) + y1 * t
}

/// The ITS-90 inverse for `kind`; NaN outside its ranges.
pub(crate) fn thermocouple(kind: TcKind, volts: f64) -> f64 {
    let mv = volts * 1000.0;
    its90::ranges(kind)
        .iter()
        .find(|r| r.lo <= mv && mv <= r.hi)
        .map_or(f64::NAN, |r| poly(r.d, mv))
}

/// `v / excitation` mapped linearly from `span` to `eng`.
pub(crate) fn ratiometric(v: f64, excitation: f64, span: (f64, f64), eng: (f64, f64)) -> f64 {
    let t = (v / excitation - span.0) / (span.1 - span.0);
    eng.0 + t * (eng.1 - eng.0)
}

/// The size of an `f64`, which a calibration reads and writes.
pub(crate) const F64_LEN: u32 = 8;

/// The byte range of an `f64` at `at`.
pub(crate) fn f64_at(at: u32) -> Option<core::ops::Range<usize>> {
    byte_range(at, F64_LEN)
}

/// The byte range of `len` bytes at `at`; `None` when it overflows.
pub(crate) fn byte_range(at: u32, len: u32) -> Option<core::ops::Range<usize>> {
    let start = usize::try_from(at).ok()?;
    Some(start..start.checked_add(usize::try_from(len).ok()?)?)
}

/// An `f64` at `at`, little-endian.
pub(crate) fn read_f64(record: &[u8], at: u32) -> Option<f64> {
    let bytes = record.get(f64_at(at)?)?;
    Some(f64::from_le_bytes(bytes.try_into().ok()?))
}

#[cfg(test)]
mod tests;

#[cfg(kani)]
mod proofs {
    use super::*;

    #[kani::proof]
    #[kani::unwind(4)]
    fn poly_bounded_is_finite() {
        let coeffs: [f64; 3] = kani::any();
        let (len, x): (usize, f64) = (kani::any(), kani::any());
        kani::assume(len <= 3);
        kani::assume(coeffs.iter().chain([&x]).all(|v| v.abs() <= 1e3));
        assert!(poly(&coeffs[..len], x).is_finite());
    }

    #[kani::proof]
    #[kani::unwind(4)]
    fn table_never_panics() {
        let points: [(f64, f64); 3] = kani::any();
        let (len, x): (usize, f64) = (kani::any(), kani::any());
        kani::assume((1..=3).contains(&len));
        let points = &points[..len];
        kani::assume(points.iter().all(|(x, y)| x.is_finite() && y.is_finite()));
        kani::assume(points.windows(2).all(|w| w[0].0 < w[1].0));
        let (first, last) = (points[0], points[len - 1]);
        match segment(points, x) {
            None => assert!(x.is_nan()),
            Some(Segment::End(y)) if x <= first.0 => assert!(y == first.1),
            Some(Segment::End(y)) => assert!(x >= last.0 && y == last.1),
            Some(Segment::Between(a, b)) => {
                assert!(first.0 < x && x < last.0);
                assert!(a.0 < x && x <= b.0);
                assert!(points.windows(2).any(|w| w[0] == a && w[1] == b));
            }
        }
    }

    #[kani::proof]
    fn read_f64_in_bounds() {
        let record: [u8; 16] = kani::any();
        let at: u32 = kani::any();
        assert_eq!(
            read_f64(&record, at).is_some(),
            u64::from(at) + u64::from(F64_LEN) <= 16
        );
    }
}
