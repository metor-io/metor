//! Turning per-system run records into laid-out bars.
//!
//! The FSW publishes *how long* a step took, never when it began: the
//! coordinator steps its systems serially, so a cycle's start is one number
//! and each system's offset within it is the sum of the durations ahead of it.
//! [`layout_cycles`] is that prefix sum, and it is the whole reason the row
//! order comes from the wiring IR rather than from the database.
//!
//! Reads go straight at the memory-mapped nodes (timestamps + raw bytes), the
//! way the plot's GPU planner does — `iter_values` would allocate a view per
//! sample, and a cycle-rate frame produces a lot of samples.

use std::ops::Range;

use metor_db::Component;
use metor_proto::types::Timestamp;

use crate::dynamic::tensor::read_f64_at;
use crate::views::time_series::PlotBounds;

/// Stand-in state code for a duration sample whose `state` partner is missing
/// — a leaf that hasn't resolved, or a cycle the two frames disagree on. Falls
/// outside the FSW's `SlotState::code` range, so the theme paints it neutral
/// rather than claiming a lifecycle the target never reported.
pub(crate) const UNKNOWN_STATE: u8 = u8::MAX;

/// One system's record for one cycle: the cycle's shared timestamp, how long
/// the step took, and what state the slot was in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CycleSample {
    pub ts_us: i64,
    pub dur_us: u64,
    pub state: u8,
}

/// One drawn interval on a lane.
///
/// In raw mode this is a single step: `[start_us, start_us + dur_us)`. In
/// summarized mode (see [`summarize`]) it is a fixed screen bucket whose
/// `dur_us` is the *busy* time inside it, which the painter reads as a duty
/// cycle rather than as an extent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Bar {
    pub start_us: i64,
    pub dur_us: u64,
    pub state: u8,
    /// The cycle this bar belongs to. Carrying it is what lets the data-flow
    /// connectors join a producer's bar to its consumer's *within one cycle*
    /// without indexing by position — a row that skipped a cycle would break
    /// that alignment. Equal to `start_us` for an unmatched (async) bar.
    pub cycle_ts: i64,
}

/// Everything paint needs for one repaint, computed off the main thread and
/// handed over whole. Paint never touches an `App`, matching the event-flag
/// `ClusterPaint` idiom.
pub(crate) struct GanttFrame {
    /// Per row, in row order, sorted by start.
    pub bars: Vec<Vec<Bar>>,
    /// The coordinator's own `(timestamp, whole-cycle duration)` pairs, drawn
    /// as a context band behind every lane so host overhead and the cycle
    /// budget stay visible.
    pub cycle: Vec<(i64, u64)>,
    /// True when [`summarize`] collapsed the bars into screen buckets.
    pub summarized: bool,
    /// Bucket width used while `summarized`; `0` otherwise.
    pub bucket_us: i64,
    /// Timestamp of the newest record the target has published *anywhere*, not
    /// just inside this window. It has to be global: a window zoomed into one
    /// past cycle contains exactly one record, and treating that as the end of
    /// all data would dim the very cycle being examined.
    pub data_end: Option<i64>,
    /// Spacing between consecutive cycles, measured when the window holds two
    /// and nominal otherwise. Drives the stale threshold and the connector
    /// density test, both of which need "how wide is a cycle" at any zoom.
    pub period_us: i64,
}

/// Narrowest window the timeline will zoom to.
///
/// Timestamps are integer microseconds, so below one there is nothing further
/// to resolve. The floor is not cosmetic: an unbounded zoom drives the window
/// toward zero width, which collapses the scan's integer range to nothing and
/// sends the paint transform's scale to infinity — the pane goes blank.
pub(crate) const MIN_WINDOW_US: f64 = 1.0;

/// Hold a pan/zoom result to a window that still means something, keeping the
/// centre the gesture left it on.
pub(crate) fn clamp_window(view: PlotBounds) -> PlotBounds {
    let width = view.max_x - view.min_x;
    if width >= MIN_WINDOW_US {
        return view;
    }
    let mid = view.min_x + width / 2.0;
    PlotBounds {
        min_x: mid - MIN_WINDOW_US / 2.0,
        max_x: mid + MIN_WINDOW_US / 2.0,
        ..view
    }
}

/// Where the stale tail begins, in axis units, or `None` when the whole window
/// is backed by data.
///
/// Dimming marks the stretch where a record *should* have arrived and has not,
/// so it opens one full period past the newest record rather than at it: inside
/// that period the current cycle is still legitimately in flight. Anything
/// earlier — a window parked over history, or a scope-triggered window sitting
/// on the newest cycle — is fully backed and never dims.
pub(crate) fn stale_start(
    data_end: Option<i64>,
    period_us: i64,
    min_x: f64,
    max_x: f64,
) -> Option<f64> {
    let edge = data_end?.saturating_add(period_us.max(0)) as f64;
    (edge < max_x).then(|| edge.max(min_x))
}

/// Spacing between consecutive cycle records, as the median of the gaps.
///
/// The median rather than the mean: one long gap (a stalled target, a window
/// straddling a downtime) must not stretch the notion of a cycle.
pub(crate) fn measured_period(cycle: &[(i64, u64)]) -> Option<i64> {
    let mut gaps: Vec<i64> = cycle.windows(2).map(|w| w[1].0 - w[0].0).collect();
    if gaps.is_empty() {
        return None;
    }
    gaps.sort_unstable();
    Some(gaps[gaps.len() / 2]).filter(|g| *g > 0)
}

/// Every scalar sample of `component` in `range`, as `(timestamp, value)`
/// sorted oldest-first. Nodes iterate newest-first and a range can straddle
/// several, so the sort is not optional.
fn scalars(component: &Component, range: Range<Timestamp>) -> Vec<(i64, f64)> {
    let mut out = Vec::new();
    let sample_size = component.schema.size();
    if sample_size == 0 {
        return out;
    }
    let Some(slice) = component.time_series.get_range(range) else {
        return out;
    };
    for node in slice.as_iter() {
        let timestamps = node.timestamps();
        let data = node.data();
        for (i, ts) in timestamps.iter().enumerate() {
            let base = i * sample_size;
            let Some(buf) = data.get(base..base + sample_size) else {
                continue;
            };
            out.push((ts.0, read_f64_at(buf, component.schema.prim_type, 0)));
        }
    }
    out.sort_unstable_by_key(|(ts, _)| *ts);
    out
}

/// One row's cycle records over `range`.
///
/// Both leaves are written from the same frame under the coordinator's shared
/// per-cycle `now`, so the join is on an exact timestamp match; a duration with
/// no state partner degrades to [`UNKNOWN_STATE`] rather than being dropped —
/// the timing is the point, the colour is the annotation.
pub(crate) fn read_row(
    duration: &Component,
    state: Option<&Component>,
    range: Range<Timestamp>,
) -> Vec<CycleSample> {
    let durations = scalars(duration, range.clone());
    let states = state.map(|c| scalars(c, range)).unwrap_or_default();
    let mut cursor = 0usize;
    durations
        .into_iter()
        .map(|(ts_us, dur)| {
            while cursor < states.len() && states[cursor].0 < ts_us {
                cursor += 1;
            }
            let state = match states.get(cursor) {
                Some((ts, v)) if *ts == ts_us => *v as u8,
                _ => UNKNOWN_STATE,
            };
            CycleSample {
                ts_us,
                dur_us: dur.max(0.0) as u64,
                state,
            }
        })
        .collect()
}

/// Lay every row's samples out along the shared time axis.
///
/// `rows` are in step order and each is sorted oldest-first; `cycle_ts` is the
/// coordinator's timestamp list, the authoritative cycle set. Within a cycle a
/// row's bar starts at the running sum of the durations of the rows ahead of
/// it, so a row that published nothing that cycle contributes zero width and
/// never shifts its successors.
///
/// A sample matching no cycle — a free-running `AsyncSystem` publishing through
/// its own `StatusPort` — becomes a duration-only bar anchored at its own
/// timestamp. An empty `cycle_ts` (no coordinator record yet) falls back to the
/// union of the rows' timestamps.
///
/// `envelope` names the row whose record *spans* the cycle rather than taking a
/// step in it (the coordinator, or a system marked
/// [`encompassing`](metor_fsw_2::ir::SystemSpec::encompassing)). Its bar anchors
/// at the cycle start and, crucially, contributes nothing to the running sum —
/// it is the container the other bars sit inside, so adding its duration would
/// push every sibling out by a whole cycle.
pub(crate) fn layout_cycles(
    rows: &[Vec<CycleSample>],
    cycle_ts: &[i64],
    envelope: Option<usize>,
) -> Vec<Vec<Bar>> {
    let union;
    let cycles: &[i64] = if cycle_ts.is_empty() {
        union = {
            let mut all: Vec<i64> = rows.iter().flatten().map(|s| s.ts_us).collect();
            all.sort_unstable();
            all.dedup();
            all
        };
        &union
    } else {
        cycle_ts
    };

    let mut out: Vec<Vec<Bar>> = vec![Vec::new(); rows.len()];
    let mut cursor = vec![0usize; rows.len()];
    let mut matched: Vec<Vec<bool>> = rows.iter().map(|r| vec![false; r.len()]).collect();

    for &ts in cycles {
        let mut acc = 0u64;
        for (r, samples) in rows.iter().enumerate() {
            while cursor[r] < samples.len() && samples[cursor[r]].ts_us < ts {
                cursor[r] += 1;
            }
            let Some(sample) = samples.get(cursor[r]).filter(|s| s.ts_us == ts) else {
                continue;
            };
            matched[r][cursor[r]] = true;
            let is_envelope = envelope == Some(r);
            out[r].push(Bar {
                start_us: if is_envelope {
                    ts
                } else {
                    ts.saturating_add(acc as i64)
                },
                dur_us: sample.dur_us,
                state: sample.state,
                cycle_ts: ts,
            });
            if !is_envelope {
                acc = acc.saturating_add(sample.dur_us);
            }
        }
    }

    for (r, samples) in rows.iter().enumerate() {
        for (i, sample) in samples.iter().enumerate() {
            if !matched[r][i] {
                out[r].push(Bar {
                    start_us: sample.ts_us,
                    dur_us: sample.dur_us,
                    state: sample.state,
                    cycle_ts: sample.ts_us,
                });
            }
        }
        out[r].sort_unstable_by_key(|b| b.start_us);
    }
    out
}

/// How alarming a `SlotState::code` is, for picking a summary bucket's colour.
///
/// A bucket takes the *worst* state in it, never the last one: stride-sampling
/// a state channel silently erases the one cycle that went wrong, which is
/// exactly the cycle the operator zoomed out to find.
pub(crate) fn state_severity(code: u8) -> u8 {
    match code {
        3 => 0, // running
        1 => 1, // loaded
        2 => 2, // loading
        4 => 3, // done
        0 => 4, // empty
        _ => 5, // stopped, or a code this panel doesn't know
    }
}

/// Operator-facing name of a `SlotState::code`, for the hover readout.
pub(crate) fn state_name(code: u8) -> &'static str {
    match code {
        0 => "empty",
        1 => "loaded",
        2 => "loading",
        3 => "running",
        4 => "done",
        5 => "stopped",
        _ => "unknown",
    }
}

/// Collapse one row's bars into fixed buckets of `bucket_us` across `window`.
///
/// Each surviving bucket keeps its busy time in `dur_us` — the painter fills
/// the whole bucket and reads that as a duty cycle — and the worst state seen
/// inside it.
pub(crate) fn summarize(bars: &[Bar], window: Range<i64>, bucket_us: i64) -> Vec<Bar> {
    if bucket_us <= 0 || window.end <= window.start {
        return Vec::new();
    }
    let count = ((window.end - window.start) / bucket_us + 1) as usize;
    let mut busy = vec![0u64; count];
    let mut worst = vec![None::<u8>; count];
    for bar in bars {
        let offset = bar.start_us - window.start;
        if offset < 0 || offset >= window.end - window.start {
            continue;
        }
        let i = (offset / bucket_us) as usize;
        busy[i] = busy[i].saturating_add(bar.dur_us);
        worst[i] = Some(match worst[i] {
            Some(prev) if state_severity(prev) >= state_severity(bar.state) => prev,
            _ => bar.state,
        });
    }
    (0..count)
        .filter_map(|i| {
            let start_us = window.start + i as i64 * bucket_us;
            Some(Bar {
                start_us,
                dur_us: busy[i],
                state: worst[i]?,
                cycle_ts: start_us,
            })
        })
        .collect()
}
