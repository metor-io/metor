//! Level-of-detail selection and gap bands for the plots that read a
//! component's time-series history (time series, spectrogram).
//!
//! Both decide per source, every frame, whether the visible window is cheap
//! enough to read raw or must come from a min/max LoD companion
//! (`metor_db::lod`), and both overlay the stretches of that window that are
//! not resident — remote-only history the hydrator is fetching, or
//! expression output nobody has computed yet.

use std::ops::Range;

use gpui::{Div, Hsla, Styled, div, relative};
use metor_db::{Component, DB};
use metor_proto::types::{ComponentId, Timestamp};
use smallvec::SmallVec;

use crate::dynamic::ops::replay::ReplayPlan;
use crate::views::time_series::resolve_lod_levels;

/// Missing stretches of the plot as `(start, width)` fractions of its width.
pub(crate) type GapBands = SmallVec<[(f32, f32); 4]>;

/// One source's raw-versus-LoD state.
#[derive(Default)]
pub(crate) struct LodTracking {
    /// The source's LoD companions, finest first. Re-resolved when the DB's
    /// component set changes.
    levels: Vec<Component>,
    resolved_gen: Option<u64>,
    /// True while the visible window costs more than the budget raw; the
    /// source then renders from `selected` instead.
    over_budget: bool,
    /// Index into `levels` while over budget; `None` when no level has been
    /// published yet (the plot shows gap bands instead).
    selected: Option<usize>,
}

impl LodTracking {
    /// The LoD component to render from, when over budget and one exists.
    pub fn selected(&self) -> Option<&Component> {
        self.levels.get(self.selected?)
    }

    pub fn over_budget(&self) -> bool {
        self.over_budget
    }

    /// Decide raw versus LoD for `range`. A raw sample costs
    /// `elements_per_sample` against `budget`; an LoD sample carries both
    /// halves of its bucket, so it costs twice that.
    ///
    /// Hysteresis keeps the boundary from flapping while panning near the
    /// budget; over budget the finest level that fits wins, falling back to
    /// the coarsest. A level with no data at all (still backfilling, or its
    /// first bucket hasn't completed) can't render and is skipped.
    pub fn select(
        &mut self,
        db: &DB,
        source: ComponentId,
        component: &Component,
        range: Range<Timestamp>,
        budget: u64,
        elements_per_sample: u64,
    ) {
        let enter = budget + budget / 4;
        let exit = budget * 4 / 5;
        let vtable_gen = db.vtable_gen.latest();
        if self.resolved_gen != Some(vtable_gen) {
            self.levels = resolve_lod_levels(db, source);
            self.resolved_gen = Some(vtable_gen);
        }
        let cost = component
            .time_series
            .estimate_samples(range.clone())
            .saturating_mul(elements_per_sample);
        self.over_budget = if self.over_budget {
            cost >= exit
        } else {
            cost > enter
        };
        if !self.over_budget {
            self.selected = None;
            return;
        }
        let mut selected = None;
        for (i, lod) in self.levels.iter().enumerate() {
            let has_data =
                lod.time_series.latest().is_some() || !lod.time_series.manifest().spans.is_empty();
            if !has_data {
                continue;
            }
            selected = Some(i);
            if lod
                .time_series
                .estimate_samples(range.clone())
                .saturating_mul(elements_per_sample * 2)
                <= budget
            {
                break;
            }
        }
        self.selected = selected;
    }
}

/// Accumulates [`GapBands`] across a plot's sources for one visible window,
/// asking the installed hydrator (if any) for remote-only stretches on the
/// way past — bounded, deduplicated, cheap to call per frame.
pub(crate) struct GapCollector {
    min_x: f64,
    span: f64,
    visible: Range<Timestamp>,
    hydrator: Option<crate::hydration::Hydrators>,
    gaps: metor_db::manifest::GapVec,
    bands: GapBands,
}

impl GapCollector {
    /// Start collecting over the visible window `[min_x, max_x]`, in
    /// microseconds.
    pub fn new(min_x: f64, max_x: f64, cx: &gpui::App) -> Self {
        Self {
            min_x,
            span: (max_x - min_x).max(1.0),
            visible: Timestamp(min_x as i64)..Timestamp(max_x as i64),
            hydrator: crate::hydration::hydrator(cx),
            gaps: metor_db::manifest::GapVec::new(),
            bands: GapBands::new(),
        }
    }

    /// Add one source's gaps. Over-budget sources never hydrate raw nodes (a
    /// year-wide view must not pull the archive): gaps come from, and
    /// hydration targets, the selected LoD companion, which is ~`2/ratio`
    /// the size.
    pub fn add(
        &mut self,
        component: &Component,
        source: ComponentId,
        lod: &LodTracking,
        plan: &Option<ReplayPlan>,
        cx: &gpui::App,
    ) {
        let (series, hydrate_id, hydrate) = match lod.selected() {
            Some(level) => (&level.time_series, level.component_id, true),
            // Over budget with no published level yet: show raw gaps but
            // never pull the raw archive for a wide view.
            None if lod.over_budget() => (&component.time_series, source, false),
            None => (&component.time_series, source, true),
        };
        self.gaps.clear();
        series.coverage(self.visible.clone(), &mut self.gaps);
        let (min_x, span) = (self.min_x, self.span);
        let bands = &mut self.bands;
        let mut band = |range: &Range<Timestamp>| {
            let start = ((range.start.0 as f64 - min_x) / span).clamp(0.0, 1.0) as f32;
            let end = ((range.end.0 as f64 - min_x) / span).clamp(0.0, 1.0) as f32;
            if end > start {
                bands.push((start, end - start));
            }
        };
        for gap in &self.gaps {
            if hydrate
                && gap.state == metor_db::manifest::SpanState::RemoteOnly
                && let Some(hydrator) = &self.hydrator
            {
                hydrator.request(hydrate_id, gap.range.clone());
            }
            band(&gap.range);
        }
        // What an expression has never computed is a gap of the other kind:
        // nothing to fetch, something to compute. Always against the raw
        // series — a LoD level is derived from it afterwards.
        let history = crate::data_binding::BoundHistory {
            component: component.clone(),
            plan: plan.clone(),
        };
        for range in history.request_replay(self.visible.clone(), cx) {
            band(&range);
        }
    }

    pub fn bands(self) -> GapBands {
        self.bands
    }

    /// The bands with anything closer than ~half a pixel merged. A year-wide
    /// view over a sparse archive yields hundreds of sub-pixel gaps; this
    /// keeps the overlay a handful of divs.
    pub fn merged(self) -> GapBands {
        let mut bands = self.bands;
        bands.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
        let mut merged = GapBands::new();
        for (start, width) in bands {
            match merged.last_mut() {
                Some((last_start, last_width)) if start - (*last_start + *last_width) < 0.0005 => {
                    *last_width = (start + width - *last_start).max(*last_width);
                }
                _ => merged.push((start, width)),
            }
        }
        merged
    }
}

/// One shaded div per band, for the plot's relative container.
///
/// History that lives in a store but not on disk yet; the hydrator has been
/// asked for it and the band disappears when the data lands and wakes the
/// trackers.
pub(crate) fn gap_band_overlay(bands: GapBands, color: Hsla) -> impl Iterator<Item = Div> {
    bands.into_iter().map(move |(start, width)| {
        div()
            .absolute()
            .top_0()
            .bottom_0()
            .left(relative(start))
            .w(relative(width))
            .bg(color)
    })
}
