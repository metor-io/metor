//! Scalar reductions over a closed range of samples.
//!
//! A measurement cursor selects a `[t_start, t_end]` range and a focused
//! trace. Each [`MeasurementKind`] consumes the samples in that range and
//! emits either a single scalar (most kinds) or a vector spectrum (FFT). The
//! computations are one-shot — invoked from the inspector row builder when
//! the cursor's range or focused trace changes — and live outside the
//! streaming dynamic-node graph because a closed range has no producer.

use std::ops::Range;
use std::sync::Arc;

use facet::Facet;
use gpui::SharedString;
use metor_db::Component;
use metor_proto::types::Timestamp;
use smallvec::SmallVec;

use crate::dynamic::tensor::read_f64_at;

/// One measurement reduction available to a [`MeasurementCursor`].
///
/// `facet::Facet` so the set of enabled kinds round-trips through the panel
/// layout serialization.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Facet)]
#[repr(u8)]
pub enum MeasurementKind {
    DeltaX,
    DeltaY,
    Mean,
    Min,
    Max,
    PeakToPeak,
    Rms,
    StdDev,
    Integral,
}

impl MeasurementKind {
    pub const ALL: [MeasurementKind; 9] = [
        MeasurementKind::DeltaX,
        MeasurementKind::DeltaY,
        MeasurementKind::Mean,
        MeasurementKind::Min,
        MeasurementKind::Max,
        MeasurementKind::PeakToPeak,
        MeasurementKind::Rms,
        MeasurementKind::StdDev,
        MeasurementKind::Integral,
    ];

    pub fn label(self) -> &'static str {
        match self {
            MeasurementKind::DeltaX => "Δx",
            MeasurementKind::DeltaY => "Δy",
            MeasurementKind::Mean => "Mean",
            MeasurementKind::Min => "Min",
            MeasurementKind::Max => "Max",
            MeasurementKind::PeakToPeak => "Peak-to-peak",
            MeasurementKind::Rms => "RMS",
            MeasurementKind::StdDev => "Std dev",
            MeasurementKind::Integral => "Integral",
        }
    }

    /// Kinds whose value is read from the focused trace (vs trace-independent
    /// like ΔX). Drives whether the cursor inspector cascades a "per trace"
    /// sub-page or shows a single readout.
    pub fn applies_to_focused_trace(self) -> bool {
        !matches!(self, MeasurementKind::DeltaX)
    }
}

/// Pull every `(timestamp, value)` pair in `range` for one element of
/// `component`, materialising into a `Vec` so reductions and the FFT can
/// share the same buffer.
///
/// Returns `None` when the component schema can't address `element_index` or
/// when the range falls entirely outside the recorded samples.
pub fn collect_samples(
    component: &Component,
    element_index: usize,
    range: Range<Timestamp>,
) -> Option<Arc<Vec<(Timestamp, f64)>>> {
    let schema = &component.schema;
    let elements: usize = schema.dim.iter().product::<usize>().max(1);
    if element_index >= elements {
        return None;
    }
    let slice = component.time_series.get_range(range)?;
    let mut out: Vec<(Timestamp, f64)> = Vec::new();
    // Newest-first node order, reversed so reductions and the FFT see
    // chronological samples.
    let node_slices: Vec<_> = slice.as_iter().collect();
    for node in node_slices.iter().rev() {
        let timestamps = node.timestamps();
        let data = node.data();
        let elem_size = schema.size();
        if elem_size == 0 {
            continue;
        }
        for (i, ts) in timestamps.iter().enumerate() {
            let base = i * elem_size;
            let Some(buf) = data.get(base..base + elem_size) else {
                continue;
            };
            let v = read_f64_at(buf, schema.prim_type, element_index);
            out.push((*ts, v));
        }
    }
    if out.is_empty() {
        return None;
    }
    Some(Arc::new(out))
}

/// Reductions that collapse a sample range to a single scalar.
pub fn reduce(kind: MeasurementKind, samples: &[(Timestamp, f64)]) -> Option<f64> {
    match kind {
        MeasurementKind::DeltaX => {
            let first = samples.first()?.0;
            let last = samples.last()?.0;
            Some((last.0 - first.0) as f64)
        }
        MeasurementKind::DeltaY => {
            let first = samples.first()?.1;
            let last = samples.last()?.1;
            Some(last - first)
        }
        MeasurementKind::Mean => Some(mean(samples)),
        MeasurementKind::Min => samples.iter().map(|(_, v)| *v).reduce(f64::min),
        MeasurementKind::Max => samples.iter().map(|(_, v)| *v).reduce(f64::max),
        MeasurementKind::PeakToPeak => {
            let mut lo = f64::INFINITY;
            let mut hi = f64::NEG_INFINITY;
            for (_, v) in samples {
                if *v < lo {
                    lo = *v;
                }
                if *v > hi {
                    hi = *v;
                }
            }
            (lo.is_finite() && hi.is_finite()).then_some(hi - lo)
        }
        MeasurementKind::Rms => {
            if samples.is_empty() {
                return None;
            }
            let s: f64 = samples.iter().map(|(_, v)| v * v).sum();
            Some((s / samples.len() as f64).sqrt())
        }
        MeasurementKind::StdDev => {
            if samples.len() < 2 {
                return Some(0.0);
            }
            let m = mean(samples);
            let s: f64 = samples.iter().map(|(_, v)| (v - m).powi(2)).sum();
            Some((s / (samples.len() - 1) as f64).sqrt())
        }
        MeasurementKind::Integral => Some(trapezoidal_integral(samples)),
    }
}

fn mean(samples: &[(Timestamp, f64)]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let s: f64 = samples.iter().map(|(_, v)| *v).sum();
    s / samples.len() as f64
}

/// Trapezoidal integration in (sample, value) coordinates. Timestamps are
/// treated as microseconds (the panel-wide convention); the result is in
/// `value · seconds`.
fn trapezoidal_integral(samples: &[(Timestamp, f64)]) -> f64 {
    if samples.len() < 2 {
        return 0.0;
    }
    let mut acc = 0.0;
    for win in samples.windows(2) {
        let (t0, v0) = win[0];
        let (t1, v1) = win[1];
        let dt = (t1.0 - t0.0) as f64 * 1e-6;
        acc += 0.5 * (v0 + v1) * dt;
    }
    acc
}

/// Render a scalar measurement value, choosing units appropriate to the
/// kind and magnitude.
///
/// Δx is formatted as a time duration; the rest are unitless f64s in the
/// trace's value space.
pub fn format_value(kind: MeasurementKind, value: f64) -> SharedString {
    match kind {
        MeasurementKind::DeltaX => format_duration_us(value).into(),
        _ => super::format_value_label(value).into(),
    }
}

/// Format a microsecond duration with the same rounding rules used for plot
/// axis labels.
fn format_duration_us(us: f64) -> String {
    if us.abs() < 1.0 {
        return "0".to_string();
    }
    let dur = hifitime::Duration::from_microseconds(us);
    format!("{}", dur)
}

/// `SmallVec` alias matching the cursor's enabled-set inline capacity. The
/// inline capacity is the most-common cursor (Δx + Δy + Mean) plus a few
/// extras users toggle on.
pub type MeasurementKindList = SmallVec<[MeasurementKind; 8]>;
