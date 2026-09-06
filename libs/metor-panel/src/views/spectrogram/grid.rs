//! Bucketing a vector component's history into a `[time × bin]` grid.
//!
//! Pure CPU work, deliberately free of gpui and wgpu so the reduction rules —
//! which are the part that can silently lie about the data — are testable on
//! their own. The grid is rebuilt every frame from the visible window, which
//! is what lets zooming re-bucket instead of resampling an image.

use std::ops::Range;

use metor_db::Component;
use metor_proto::types::Timestamp;

use crate::dynamic::tensor::read_f64_at;
use crate::views::time_series::{EMPTY_INTENSITY, EMPTY_THRESHOLD, IntensityScale};

/// Widest grid built for one frame. Columns follow the plot's pixel width;
/// past this a column is narrower than a pixel and buys nothing.
pub(crate) const MAX_COLS: usize = 2048;

/// Elements a raw-path frame may read before the view falls back to the LoD
/// companion. Bins multiply sample count, so a 257-bin spectrum over a
/// million samples is a quarter-billion reads — the budget is on the product.
pub(crate) const ELEMENT_SCAN_BUDGET: u64 = 1 << 22;

/// One frame's bucketed spectrum.
///
/// `values` is row-major `rows × cols` with row 0 holding bin 0, matching the
/// tonemap pass's bottom-up row addressing. Cells no sample covered hold
/// [`EMPTY_INTENSITY`].
pub(crate) struct SpectrogramGrid {
    pub values: Vec<f32>,
    pub cols: usize,
    pub rows: usize,
    pub t0: i64,
    pub t1: i64,
    /// Extremes over the finite cells, in the scale's display units.
    pub lo: f32,
    pub hi: f32,
}

impl Default for SpectrogramGrid {
    fn default() -> Self {
        Self {
            values: Vec::new(),
            cols: 0,
            rows: 0,
            t0: 0,
            t1: 0,
            lo: 0.0,
            hi: 1.0,
        }
    }
}

impl SpectrogramGrid {
    /// The value at a grid position, or `None` for an uncovered cell.
    pub(crate) fn value_at(&self, col: usize, row: usize) -> Option<f32> {
        if col >= self.cols || row >= self.rows {
            return None;
        }
        let v = self.values[row * self.cols + col];
        (v > EMPTY_THRESHOLD).then_some(v)
    }

    /// Timestamp at the center of `col`, for hover readouts.
    pub(crate) fn column_time(&self, col: usize) -> i64 {
        if self.cols == 0 {
            return self.t0;
        }
        let span = (self.t1 - self.t0) as f64;
        self.t0 + (span * (col as f64 + 0.5) / self.cols as f64) as i64
    }

    fn reset(&mut self, cols: usize, rows: usize, range: &Range<Timestamp>) {
        self.cols = cols;
        self.rows = rows;
        self.t0 = range.start.0;
        self.t1 = range.end.0;
        self.lo = 0.0;
        self.hi = 1.0;
        self.values.clear();
        self.values.resize(cols * rows, EMPTY_INTENSITY);
    }
}

/// Accumulator shared by the raw and LoD walks.
struct Fold<'a> {
    grid: &'a mut SpectrogramGrid,
    covered: Vec<bool>,
    span: f64,
    lo: f32,
    hi: f32,
    any: bool,
}

impl<'a> Fold<'a> {
    fn new(grid: &'a mut SpectrogramGrid) -> Self {
        let cols = grid.cols;
        let span = (grid.t1 - grid.t0).max(1) as f64;
        Self {
            grid,
            covered: vec![false; cols],
            span,
            lo: f32::INFINITY,
            hi: f32::NEG_INFINITY,
            any: false,
        }
    }

    fn column_of(&self, ts: i64) -> Option<usize> {
        if ts < self.grid.t0 {
            return None;
        }
        let col = ((ts - self.grid.t0) as f64 / self.span * self.grid.cols as f64) as usize;
        (col < self.grid.cols).then_some(col)
    }

    /// Fold one bin into a column with `max`: a one-frame tone has to survive
    /// a zoom-out that puts thousands of samples in a column, so the reduction
    /// can never be "last wins" or an average.
    fn push(&mut self, col: usize, row: usize, value: f64) {
        if !value.is_finite() {
            return;
        }
        let v = value as f32;
        if !v.is_finite() {
            return;
        }
        let cell = &mut self.grid.values[row * self.grid.cols + col];
        if *cell <= EMPTY_THRESHOLD || v > *cell {
            *cell = v;
        }
        self.covered[col] = true;
        self.lo = self.lo.min(v);
        self.hi = self.hi.max(v);
        self.any = true;
    }

    /// Carry each covered column across the empty ones that follow it, up to
    /// the last column any sample reached — the waterfall's held spectrum.
    /// Columns before the first sample stay empty rather than being back-filled
    /// with a spectrum that had not happened yet.
    fn finish(self) -> bool {
        let Fold {
            grid,
            covered,
            lo,
            hi,
            any,
            ..
        } = self;
        if !any {
            return false;
        }
        let first = covered.iter().position(|c| *c).unwrap_or(0);
        let last = covered.iter().rposition(|c| *c).unwrap_or(0);
        for (col, filled) in covered.iter().enumerate().take(last + 1).skip(first + 1) {
            if *filled {
                continue;
            }
            for row in 0..grid.rows {
                grid.values[row * grid.cols + col] = grid.values[row * grid.cols + col - 1];
            }
        }
        grid.lo = lo;
        grid.hi = if hi > lo { hi } else { lo + 1.0 };
        true
    }
}

/// Bucket `component`'s raw samples over `range` into `out`.
///
/// Returns `false` when nothing finite landed in the grid, which is the
/// caller's cue to skip the frame rather than paint an all-empty field.
pub(crate) fn build_grid(
    component: &Component,
    len: usize,
    range: Range<Timestamp>,
    cols: usize,
    scale: IntensityScale,
    out: &mut SpectrogramGrid,
) -> bool {
    out.reset(cols, len, &range);
    if len == 0 || cols == 0 || range.end.0 <= range.start.0 {
        return false;
    }
    let schema = &component.schema;
    let sample_size = schema.size();
    let prim_size = schema.prim_type.size();
    if sample_size == 0 || prim_size == 0 || len * prim_size > sample_size {
        return false;
    }
    let Some(slice) = component.time_series.get_range(range) else {
        return false;
    };

    let mut fold = Fold::new(out);
    // Nodes iterate newest-first; the fold is order-independent, but walking
    // oldest-first keeps the forward-fill's notion of "previous column" honest
    // for anyone reading this alongside the line plot's planner.
    for node in slice.as_iter().collect::<Vec<_>>().iter().rev() {
        let timestamps = node.timestamps();
        let data = node.data();
        for (i, ts) in timestamps.iter().enumerate() {
            let Some(col) = fold.column_of(ts.0) else {
                continue;
            };
            let base = i * sample_size;
            let Some(buf) = data.get(base..base + sample_size) else {
                continue;
            };
            for bin in 0..len {
                let v = read_f64_at(buf, schema.prim_type, bin);
                fold.push(col, bin, scale.apply(v));
            }
        }
    }
    fold.finish()
}

/// Bucket a min/max LoD companion's maxima over `range` into `out`.
///
/// LoD samples are `[2, ..shape]` f32 buckets — minima then maxima. Only the
/// maxima are read: a spectrogram's whole job is showing where energy was, and
/// max-of-maxes is exactly what the raw path's max-fold would have produced.
pub(crate) fn build_grid_from_lod(
    lod: &Component,
    len: usize,
    range: Range<Timestamp>,
    cols: usize,
    scale: IntensityScale,
    out: &mut SpectrogramGrid,
) -> bool {
    out.reset(cols, len, &range);
    if len == 0 || cols == 0 || range.end.0 <= range.start.0 {
        return false;
    }
    let schema = &lod.schema;
    let sample_size = schema.size();
    let n_elements = schema.dim.iter().skip(1).product::<usize>().max(1);
    let bins = len.min(n_elements);
    if sample_size == 0 || (n_elements + bins) * schema.prim_type.size() > sample_size {
        return false;
    }
    let Some(slice) = lod.time_series.get_range(range) else {
        return false;
    };

    let mut fold = Fold::new(out);
    for node in slice.as_iter().collect::<Vec<_>>().iter().rev() {
        let timestamps = node.timestamps();
        let data = node.data();
        for (i, ts) in timestamps.iter().enumerate() {
            let Some(col) = fold.column_of(ts.0) else {
                continue;
            };
            let base = i * sample_size;
            let Some(buf) = data.get(base..base + sample_size) else {
                continue;
            };
            for bin in 0..bins {
                let v = read_f64_at(buf, schema.prim_type, n_elements + bin);
                fold.push(col, bin, scale.apply(v));
            }
        }
    }
    fold.finish()
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use metor_db::disruptor::Disruptor;
    use metor_db::time_series::{TimeSeries, TimeSeriesNode};
    use metor_db::{Component, ComponentSchema};
    use metor_proto::types::{ComponentId, PrimType, Timestamp};
    use stellarator::util::AtomicCell;

    use super::*;

    /// A component whose samples are `[len]` f64 vectors, written straight to
    /// disk the way `gpu.rs`'s fixture does — no DB, no runtime.
    fn synth_vector_component(
        dir: &Path,
        len: usize,
        samples: usize,
        start_us: i64,
        step_us: i64,
        value: impl Fn(usize, usize) -> f64,
    ) -> Component {
        let schema = ComponentSchema::new(PrimType::F64, &[len][..]);
        let node = TimeSeriesNode::create(
            dir.join(start_us.to_string()),
            Timestamp(start_us),
            len as u64 * 8,
        )
        .unwrap();
        for i in 0..samples {
            let ts = start_us + i as i64 * step_us;
            let mut bytes = Vec::with_capacity(len * 8);
            for bin in 0..len {
                bytes.extend_from_slice(&value(i, bin).to_le_bytes());
            }
            node.data.write(&bytes).unwrap();
            node.index.write(&Timestamp(ts).to_le_bytes()).unwrap();
        }
        Component {
            component_id: ComponentId(1),
            time_series: TimeSeries::open(dir).unwrap(),
            wal: Disruptor::new(1024),
            schema,
            last_timestamp: Arc::new(AtomicCell::new(Timestamp(0))),
        }
    }

    /// One sample per two columns: every column from the first sample onward
    /// carries a spectrum, and the columns before it stay empty rather than
    /// inventing history.
    #[test]
    fn sparse_samples_forward_fill_without_back_filling() {
        let dir = tempfile::tempdir().unwrap();
        let component = synth_vector_component(dir.path(), 4, 5, 2_000, 1_000, |i, bin| {
            (i * 10 + bin) as f64
        });
        let mut grid = SpectrogramGrid::default();
        let built = build_grid(
            &component,
            4,
            Timestamp(0)..Timestamp(8_000),
            8,
            IntensityScale::Linear,
            &mut grid,
        );
        assert!(built);
        assert_eq!((grid.cols, grid.rows), (8, 4));
        // Samples land at 2 000..6 000 µs, i.e. columns 2..6.
        for col in 0..2 {
            assert_eq!(grid.value_at(col, 0), None, "column {col} back-filled");
        }
        for col in 2..7 {
            assert!(grid.value_at(col, 0).is_some(), "column {col} uncovered");
        }
        // Column 7 sits past the newest sample: nothing is held forward into
        // time that has not happened.
        assert_eq!(grid.value_at(7, 0), None);
    }

    /// Sub-column samples must not average away a single loud frame — the
    /// whole point of a waterfall is spotting the one-off tone.
    #[test]
    fn an_impulse_survives_a_max_fold() {
        let dir = tempfile::tempdir().unwrap();
        let component = synth_vector_component(dir.path(), 3, 1_000, 0, 1_000, |i, bin| {
            if i == 517 && bin == 2 { 900.0 } else { 1.0 }
        });
        let mut grid = SpectrogramGrid::default();
        assert!(build_grid(
            &component,
            3,
            Timestamp(0)..Timestamp(1_000_000),
            8,
            IntensityScale::Linear,
            &mut grid,
        ));
        let hot: Vec<f32> = (0..8).filter_map(|c| grid.value_at(c, 2)).collect();
        assert!(
            hot.iter().any(|v| (*v - 900.0).abs() < 1e-3),
            "impulse folded away: {hot:?}"
        );
        assert!((grid.hi - 900.0).abs() < 1e-3, "hi missed the impulse");
        assert!((grid.lo - 1.0).abs() < 1e-3, "lo picked up the sentinel");
    }

    /// A zero-magnitude bin is normal (an empty FFT bin), and must land at the
    /// dB floor rather than at `-inf`, which would poison the auto range.
    #[test]
    fn log_scale_floors_zero_magnitudes() {
        let dir = tempfile::tempdir().unwrap();
        let component = synth_vector_component(dir.path(), 2, 4, 0, 1_000, |_, bin| bin as f64);
        let mut grid = SpectrogramGrid::default();
        assert!(build_grid(
            &component,
            2,
            Timestamp(0)..Timestamp(4_000),
            4,
            IntensityScale::Log,
            &mut grid,
        ));
        let floor = grid.value_at(0, 0).unwrap();
        assert!(floor.is_finite(), "zero magnitude produced {floor}");
        assert!((floor - (-120.0)).abs() < 1e-3, "unexpected floor {floor}");
        assert!((grid.value_at(0, 1).unwrap() - 0.0).abs() < 1e-3);
    }

    /// The LoD companion's maxima live one bin-block past its minima; reading
    /// the wrong half would silently plot the quiet edge of every bucket.
    #[test]
    fn lod_grids_read_the_max_half() {
        let dir = tempfile::tempdir().unwrap();
        let len = 3;
        let schema = ComponentSchema::new(PrimType::F32, &[2, len][..]);
        let node =
            TimeSeriesNode::create(dir.path().join("0"), Timestamp(0), len as u64 * 2 * 4).unwrap();
        for i in 0..4 {
            let mut bytes = Vec::new();
            for bin in 0..len {
                bytes.extend_from_slice(&(-(bin as f32) - i as f32).to_le_bytes());
            }
            for bin in 0..len {
                bytes.extend_from_slice(&((bin as f32 + 1.0) * 10.0).to_le_bytes());
            }
            node.data.write(&bytes).unwrap();
            node.index
                .write(&Timestamp(i as i64 * 1_000).to_le_bytes())
                .unwrap();
        }
        let lod = Component {
            component_id: ComponentId(2),
            time_series: TimeSeries::open(dir.path()).unwrap(),
            wal: Disruptor::new(1024),
            schema,
            last_timestamp: Arc::new(AtomicCell::new(Timestamp(0))),
        };
        let mut grid = SpectrogramGrid::default();
        assert!(build_grid_from_lod(
            &lod,
            len,
            Timestamp(0)..Timestamp(4_000),
            4,
            IntensityScale::Linear,
            &mut grid,
        ));
        for bin in 0..len {
            let expected = (bin as f32 + 1.0) * 10.0;
            assert!(
                (grid.value_at(0, bin).unwrap() - expected).abs() < 1e-3,
                "bin {bin} read the minima half"
            );
        }
        assert!((grid.lo - 10.0).abs() < 1e-3);
        assert!((grid.hi - 30.0).abs() < 1e-3);
    }

    /// A window that predates every sample has nothing to draw, and must say
    /// so rather than handing back an all-sentinel grid to normalize.
    #[test]
    fn an_empty_window_builds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let component = synth_vector_component(dir.path(), 2, 4, 100_000, 1_000, |_, _| 1.0);
        let mut grid = SpectrogramGrid::default();
        assert!(!build_grid(
            &component,
            2,
            Timestamp(0)..Timestamp(1_000),
            4,
            IntensityScale::Linear,
            &mut grid,
        ));
    }
}
