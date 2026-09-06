use metor_fsw_2::ir::{
    ClockSpec, CoordinatorSpec, DOWNLINK_TYPE, EdgeKind, EdgeSpec, IR_VERSION, ParamSource,
    SlotSpec, SystemSpec, Wiring,
};
use metor_proto::types::ComponentId;

use super::paint::{BAR_GAP_X, Projection, bar_rect, bar_span};
use super::rows::{RowKind, derive_edges, derive_rows, status_prefix};
use super::scan::{
    Bar, CycleSample, GanttFrame, MIN_WINDOW_US, clamp_window, layout_cycles, measured_period,
    stale_start,
};
use crate::views::time_series::PlotBounds;

#[gpui::test]
fn slow_scans_coalesce_live_updates_and_discard_frames_after_edits(cx: &mut gpui::TestAppContext) {
    use super::ExecTimeline;
    use gpui::AppContext;
    use std::sync::Arc;
    use stellarator::util::AtomicCell;

    let temp = tempfile::tempdir().unwrap();
    let db = Arc::new(metor_db::DB::create(temp.path().join("db")).unwrap());
    let started = Arc::new(AtomicCell::new(0u64));
    let release = Arc::new(AtomicCell::new(0u64));
    let timeline = cx.new(|cx| {
        let mut timeline = ExecTimeline::new(db, cx);
        timeline.view_override = Some(PlotBounds::new(0.0, 0.0, 100.0, 1.0));
        let started = started.clone();
        let release = release.clone();
        timeline.scan_task = Some(ExecTimeline::spawn_scan(
            timeline.scan_requests.clone(),
            cx,
            move |input| {
                let started = started.clone();
                let release = release.clone();
                async move {
                    let number = started.latest() + 1;
                    started.store(number);
                    release.wait_for(|allowed| allowed >= number).await;
                    let end = input.window.end;
                    let mut frame = super::build_frame(input);
                    frame.data_end = Some(end);
                    frame
                }
            },
        ));
        timeline
    });
    cx.run_until_parked();
    assert_eq!(started.latest(), 1);
    for end in [200.0, 300.0, 400.0, 500.0] {
        timeline.update(cx, |timeline, cx| {
            timeline.view_override = Some(PlotBounds::new(0.0, 0.0, end, 1.0));
            timeline.request_scan(cx);
        });
        cx.run_until_parked();
        assert_eq!(started.latest(), 1, "a slow scan must not be replaced");
        timeline.read_with(cx, |timeline, _| assert!(timeline.frame.is_none()));
    }
    release.store(1);
    cx.run_until_parked();
    assert_eq!(started.latest(), 2);
    release.store(2);
    cx.run_until_parked();
    timeline.read_with(cx, |timeline, _| {
        assert_eq!(timeline.frame.as_ref().unwrap().data_end, Some(500))
    });
    assert_eq!(
        started.latest(),
        2,
        "the pending updates should produce one scan"
    );

    timeline.update(cx, |timeline, cx| {
        timeline.view_override = Some(PlotBounds::new(0.0, 0.0, 600.0, 1.0));
        timeline.request_scan(cx);
    });
    cx.run_until_parked();
    timeline.update(cx, |timeline, cx| {
        timeline.view_override = Some(PlotBounds::new(0.0, 0.0, 900.0, 1.0));
        timeline.restart_scan(cx);
    });
    release.store(3);
    cx.run_until_parked();
    timeline.read_with(cx, |timeline, _| {
        assert_eq!(timeline.frame.as_ref().unwrap().data_end, Some(500))
    });
    release.store(4);
    cx.run_until_parked();
    timeline.read_with(cx, |timeline, _| {
        assert_eq!(timeline.frame.as_ref().unwrap().data_end, Some(900))
    });
    assert_eq!(started.latest(), 4);
}

fn base_wiring() -> Wiring {
    Wiring {
        ir_version: IR_VERSION,
        coordinator: CoordinatorSpec {
            cycle_rate: 100.0,
            default_depth: None,
            clock: ClockSpec::Wall,
            namespace: None,
            wasm_fuel_per_poll: None,
            wasm_memory_limit_bytes: None,
        },
        artifacts: Vec::new(),
        states: Vec::new(),
        systems: Vec::new(),
        slots: Vec::new(),
        edges: Vec::new(),
        scopes: Vec::new(),
        program: None,
    }
}

fn system(name: &str, ty: &str) -> SystemSpec {
    SystemSpec {
        name: name.to_string(),
        ty: Some(ty.to_string()),
        artifact: None,
        params: ParamSource::None,
        process: false,
        src: None,
        scope: None,
        attach: None,
        layout: None,
        status: None,
        encompassing: false,
    }
}

fn slot(name: &str) -> SlotSpec {
    SlotSpec {
        name: name.to_string(),
        inputs: Vec::new(),
        outputs: Vec::new(),
        allow: Vec::new(),
        initial: None,
        process: false,
        src: None,
        scope: None,
        status: None,
    }
}

fn edge(from: &str, out: &str, to: &str, in_: &str) -> EdgeSpec {
    EdgeSpec {
        from: from.to_string(),
        out: out.to_string(),
        to: to.to_string(),
        in_: in_.to_string(),
        delayed: false,
        kind: EdgeKind::Frame,
        src: None,
    }
}

#[test]
fn status_prefix_follows_the_framework_convention() {
    assert_eq!(status_prefix(None, "nav", None), "nav.system_status");
    assert_eq!(
        status_prefix(Some("sat1"), "nav", None),
        "sat1.nav.system_status"
    );
}

/// An override is taken literally, namespace included: leaf ids hash the full
/// component name, so a consumer can never re-derive one from a shortened form.
#[test]
fn status_override_wins_verbatim() {
    assert_eq!(
        status_prefix(Some("sat1"), "nav", Some("other.probe")),
        "other.probe"
    );
    assert_eq!(
        status_prefix(None, "nav", Some("other.probe")),
        "other.probe"
    );
}

/// The envelope leads, then the coordinator's step order: ordinary systems,
/// then slots, then the receive-all downlink `resolve` defers behind both.
#[test]
fn derive_rows_follows_resolve_step_order() {
    let mut w = base_wiring();
    w.systems = vec![
        system("plant", "Plant"),
        system("downlink", DOWNLINK_TYPE),
        system("nav", "Nav"),
    ];
    w.slots = vec![slot("mode")];

    let rows = derive_rows(&w);
    let names: Vec<&str> = rows.iter().map(|r| r.name.as_ref()).collect();
    assert_eq!(names, ["coordinator", "plant", "nav", "mode", "downlink"]);
    assert_eq!(rows[0].kind, RowKind::Coordinator);
    assert_eq!(rows[1].kind, RowKind::System);
    assert_eq!(rows[3].kind, RowKind::Slot);
}

#[test]
fn derive_rows_prefixes_the_namespace_and_honors_overrides() {
    let mut w = base_wiring();
    w.coordinator.namespace = Some("sat1".into());
    let nav = system("nav", "Nav");
    let mut plant = system("plant", "Plant");
    plant.status = Some("foreign.timer".into());
    w.systems = vec![nav, plant];

    let rows = derive_rows(&w);
    assert_eq!(
        rows[1].duration_id,
        ComponentId::new("sat1.nav.system_status.last_execute_us")
    );
    assert_eq!(
        rows[1].state_id,
        ComponentId::new("sat1.nav.system_status.state")
    );
    assert_eq!(
        rows[2].duration_id,
        ComponentId::new("foreign.timer.last_execute_us")
    );
    // The coordinator's own record is namespaced like any other instance.
    assert_eq!(
        rows[0].duration_id,
        ComponentId::new("sat1.coordinator.system_status.last_execute_us")
    );
}

/// A marked system replaces the synthesized coordinator row: it leads as the
/// envelope and does not also appear as an ordinary lane.
#[test]
fn derive_rows_promotes_the_encompassing_system() {
    let mut w = base_wiring();
    let mut host = system("host", "Host");
    host.encompassing = true;
    w.systems = vec![system("plant", "Plant"), host, system("nav", "Nav")];

    let rows = derive_rows(&w);
    let names: Vec<&str> = rows.iter().map(|r| r.name.as_ref()).collect();
    assert_eq!(names, ["host", "plant", "nav"]);
    assert_eq!(rows[0].kind, RowKind::Coordinator);
    assert_eq!(
        rows[0].duration_id,
        ComponentId::new("host.system_status.last_execute_us")
    );
}

/// With nothing marked, the framework's own coordinator record is synthesized —
/// it is not a `SystemSpec`, so there is nothing else to promote.
#[test]
fn derive_rows_synthesizes_the_coordinator_when_unmarked() {
    let mut w = base_wiring();
    w.systems = vec![system("nav", "Nav")];

    let rows = derive_rows(&w);
    let names: Vec<&str> = rows.iter().map(|r| r.name.as_ref()).collect();
    assert_eq!(names, ["coordinator", "nav"]);
    assert_eq!(rows[0].kind, RowKind::Coordinator);
}

/// Several marks is a config mistake; the first wins and the rest stay
/// ordinary lanes rather than vanishing.
#[test]
fn derive_rows_honors_only_the_first_encompassing_mark() {
    let mut w = base_wiring();
    let mut first = system("host_a", "Host");
    first.encompassing = true;
    let mut second = system("host_b", "Host");
    second.encompassing = true;
    w.systems = vec![first, second, system("nav", "Nav")];

    let rows = derive_rows(&w);
    let names: Vec<&str> = rows.iter().map(|r| r.name.as_ref()).collect();
    assert_eq!(names, ["host_a", "host_b", "nav"]);
    assert_eq!(rows[0].kind, RowKind::Coordinator);
    assert_eq!(rows[1].kind, RowKind::System);
}

/// Every port between one pair of instances collapses into a single connector
/// carrying all of them.
#[test]
fn derive_edges_groups_ports_per_instance_pair() {
    let mut w = base_wiring();
    w.systems = vec![system("plant", "Plant"), system("nav", "Nav")];
    w.edges = vec![
        edge("plant", "sensors", "nav", "sensors"),
        edge("plant", "gps", "nav", "gps"),
        edge("nav", "estimate", "plant", "estimate"),
    ];
    let rows = derive_rows(&w);

    // Row 0 is the envelope, so plant and nav are lanes 1 and 2.
    let edges = derive_edges(&w, &rows);
    assert_eq!(edges.len(), 2, "one connector per ordered instance pair");
    assert_eq!((edges[0].from, edges[0].to), (1, 2));
    assert_eq!(
        edges[0].ports,
        ["sensors → sensors", "gps → gps"],
        "grouped ports keep declaration order"
    );
    assert!(!edges[0].delayed);
    assert_eq!((edges[1].from, edges[1].to), (2, 1));
    assert_eq!(edges[1].ports, ["estimate → estimate"]);
}

/// Delay is part of the grouping key: a pair wired both forward and delayed
/// gets two connectors, because the delayed one reaches back a cycle and cannot
/// share geometry with the forward one. Message edges — which the FSW itself
/// excludes from cycle ordering — are not drawn at all, and edges naming an
/// instance with no lane, or naming one twice, are dropped.
#[test]
fn derive_edges_splits_delayed_and_skips_unrepresentable_edges() {
    let mut w = base_wiring();
    w.systems = vec![system("plant", "Plant"), system("nav", "Nav")];
    let mut delayed = edge("nav", "torque", "plant", "torque");
    delayed.delayed = true;
    let mut msg = edge("plant", "Command", "nav", "Command");
    msg.kind = EdgeKind::Msg;
    w.edges = vec![
        edge("nav", "estimate", "plant", "estimate"),
        delayed,
        msg,
        edge("ghost", "out", "nav", "in"),
        edge("nav", "loop", "nav", "loop"),
    ];
    let rows = derive_rows(&w);

    let edges = derive_edges(&w, &rows);
    assert_eq!(
        edges.len(),
        2,
        "forward and delayed are separate connectors"
    );
    assert!(!edges[0].delayed);
    assert_eq!(edges[0].ports, ["estimate → estimate"]);
    assert!(edges[1].delayed);
    assert_eq!(edges[1].ports, ["torque → torque"]);
}

fn sample(ts: i64, dur: u64) -> CycleSample {
    CycleSample {
        ts_us: ts,
        dur_us: dur,
        state: 3,
    }
}

fn bar(start: i64, dur: u64, cycle: i64) -> Bar {
    Bar {
        start_us: start,
        dur_us: dur,
        state: 3,
        cycle_ts: cycle,
    }
}

/// Two rows over two cycles: the second row starts where the first one ended.
#[test]
fn layout_cycles_prefix_sums_within_a_cycle() {
    let rows = vec![
        vec![sample(0, 100), sample(1000, 150)],
        vec![sample(0, 40), sample(1000, 60)],
    ];
    let bars = layout_cycles(&rows, &[0, 1000], None);
    assert_eq!(bars[0], vec![bar(0, 100, 0), bar(1000, 150, 1000)]);
    assert_eq!(bars[1], vec![bar(100, 40, 0), bar(1150, 60, 1000)]);
}

/// The envelope row spans the cycle rather than stepping in it: its bar anchors
/// at the cycle start and adds nothing to the running sum, so the lanes behind
/// it are not pushed out by a whole cycle's width.
#[test]
fn layout_cycles_envelope_row_does_not_consume_the_cycle() {
    let rows = vec![
        vec![sample(0, 900)],
        vec![sample(0, 100)],
        vec![sample(0, 40)],
    ];
    let bars = layout_cycles(&rows, &[0], Some(0));
    assert_eq!(
        bars[0],
        vec![bar(0, 900, 0)],
        "the envelope starts the cycle"
    );
    assert_eq!(
        bars[1],
        vec![bar(0, 100, 0)],
        "the first step starts with it"
    );
    assert_eq!(bars[2], vec![bar(100, 40, 0)]);
}

/// A row that published nothing that cycle contributes zero width and must not
/// shift the rows behind it.
#[test]
fn layout_cycles_missing_row_contributes_zero() {
    let rows = vec![vec![sample(0, 100)], Vec::new(), vec![sample(0, 25)]];
    let bars = layout_cycles(&rows, &[0], None);
    assert!(bars[1].is_empty());
    assert_eq!(bars[2], vec![bar(100, 25, 0)]);
}

/// A sample matching no cycle — a free-running async system publishing on its
/// own cadence — anchors at its own timestamp instead of joining the sum.
#[test]
fn layout_cycles_anchors_unmatched_samples() {
    let rows = vec![vec![sample(0, 100)], vec![sample(500, 20)]];
    let bars = layout_cycles(&rows, &[0, 1000], None);
    assert_eq!(bars[1], vec![bar(500, 20, 500)]);
}

/// With no coordinator record the cycle set is the union of the rows' own
/// timestamps, so the prefix sum still works.
#[test]
fn layout_cycles_falls_back_to_the_union() {
    let rows = vec![vec![sample(0, 100)], vec![sample(0, 40)]];
    let bars = layout_cycles(&rows, &[], None);
    assert_eq!(bars[0], vec![bar(0, 100, 0)]);
    assert_eq!(bars[1], vec![bar(100, 40, 0)]);
}

/// A window zoomed inside one cycle — its record sitting to the left of the
/// window — still produces that cycle's bars, which paint clips. This is the
/// layout half of the deep-zoom blank-out; the read half is the scan's
/// lookback.
#[test]
fn layout_cycles_keeps_a_cycle_straddling_the_left_edge() {
    // The record is at t=0; the operator is looking at [200, 300].
    let rows = vec![vec![sample(0, 900)], vec![sample(0, 500)]];
    let bars = layout_cycles(&rows, &[0], Some(0));
    assert_eq!(bars[1], vec![bar(0, 500, 0)]);
    let end = bars[1][0].start_us + bars[1][0].dur_us as i64;
    assert!(
        bars[1][0].start_us < 200 && end > 300,
        "the bar spans the whole window it is being viewed through"
    );
}

/// Dimming marks unreported time only. A window parked over history, or one
/// triggered onto the newest cycle, is fully backed by data and must not dim —
/// that regression made a fully zoomed-in cycle look dead.
#[test]
fn stale_start_never_dims_a_window_backed_by_data() {
    // Zoomed into a past cycle: the newest record is far to the right.
    assert_eq!(stale_start(Some(9_000), 1_000, 100.0, 200.0), None);
    // Triggered onto the newest cycle: the window is that cycle, and the next
    // record is not late yet.
    assert_eq!(stale_start(Some(1_000), 1_000, 1_000.0, 1_250.0), None);
    // Following live past the point a record should have arrived.
    assert_eq!(
        stale_start(Some(1_000), 1_000, 0.0, 9_000.0),
        Some(2_000.0),
        "the tail opens one period past the newest record"
    );
    // The cue never starts left of the window.
    assert_eq!(stale_start(Some(0), 0, 500.0, 900.0), Some(500.0));
    assert_eq!(stale_start(None, 1_000, 0.0, 9_000.0), None);
}

/// A realistic wall-clock timestamp: epoch microseconds are around 1.7e15,
/// which is the magnitude that breaks a naive `f32` transform.
const EPOCH_US: i64 = 1_700_000_000_000_000;

fn lane_bounds(width: f32) -> gpui::Bounds<gpui::Pixels> {
    gpui::Bounds::new(
        gpui::point(gpui::px(0.0), gpui::px(0.0)),
        gpui::size(gpui::px(width), gpui::px(100.0)),
    )
}

/// Deep zoom at epoch scale: a 100 µs window 1.7e15 µs from zero still projects
/// to exact, on-screen, sub-pixel-accurate coordinates.
///
/// `f32` resolves 1.7e15 only to about a minute, so any timestamp narrowed
/// before the window origin is subtracted collapses every bar onto one pixel.
/// The rebase happens in `i64` precisely so this holds.
#[test]
fn projection_is_exact_at_epoch_timestamps() {
    let view = PlotBounds::new(EPOCH_US as f64, 0.0, (EPOCH_US + 100) as f64, 1.0);
    let proj = Projection::new(&view, lane_bounds(1000.0));

    // 100 µs across 1000 px is 10 px/µs.
    for (offset, expected) in [(0i64, 0.0f32), (1, 10.0), (50, 500.0), (100, 1000.0)] {
        let x = f32::from(proj.x(EPOCH_US + offset));
        assert!(
            (x - expected).abs() < 0.01,
            "t0+{offset} projected to {x}, expected {expected}"
        );
    }
    // The inverse rebases too, or the hover readout reads the wrong bar.
    assert_eq!(proj.time_at(gpui::px(500.0)), EPOCH_US + 50);
    assert_eq!(proj.time_at(gpui::px(0.0)), EPOCH_US);
}

/// A bar far wider than the window still yields a finite rectangle covering it.
///
/// This is the deep-zoom regime that went blank: zoomed to a single
/// microsecond, a cycle-length bar projects nine million pixels wide, and a quad
/// that size is not reliably drawn. Clamping keeps the rect small enough to
/// render while still covering the window edge to edge.
#[test]
fn bar_rect_covers_a_window_zoomed_inside_one_bar() {
    let bounds = lane_bounds(1000.0);
    // A 1 µs window 4000 µs into a 9000 µs bar: 1000 px/µs, so unclamped the
    // rect would run from -4e6 to +5e6.
    let start = EPOCH_US;
    let view = PlotBounds::new((start + 4_000) as f64, 0.0, (start + 4_001) as f64, 1.0);
    let proj = Projection::new(&view, bounds);
    let frame = GanttFrame {
        bars: Vec::new(),
        cycle: Vec::new(),
        summarized: false,
        bucket_us: 0,
        data_end: None,
        period_us: 1_000,
    };
    let rect = bar_rect(
        &bar(start, 9_000, start),
        &frame,
        &proj,
        bounds,
        0,
        gpui::px(30.0),
    )
    .expect("a bar spanning the window is drawn, not culled");

    let (left, right) = (
        f32::from(rect.origin.x),
        f32::from(rect.origin.x + rect.size.width),
    );
    assert!(left.is_finite() && right.is_finite(), "{left}..{right}");
    assert!(left < 0.0, "the bar starts before the window: {left}");
    assert!(right > 1000.0, "and ends after it: {right}");
    assert!(
        right - left < 1.0e5,
        "the rect stays a size the renderer can take, not the 9e6 px the raw \
         projection gives: {}",
        right - left
    );
}

/// A bar wholly outside the window collapses against one edge and is culled,
/// rather than being clamped into view.
#[test]
fn bar_rect_culls_a_bar_left_of_the_window() {
    let bounds = lane_bounds(1000.0);
    let view = PlotBounds::new(
        (EPOCH_US + 10_000) as f64,
        0.0,
        (EPOCH_US + 10_100) as f64,
        1.0,
    );
    let proj = Projection::new(&view, bounds);
    let frame = GanttFrame {
        bars: Vec::new(),
        cycle: Vec::new(),
        summarized: false,
        bucket_us: 0,
        data_end: None,
        period_us: 1_000,
    };
    assert!(
        bar_rect(
            &bar(EPOCH_US, 5, EPOCH_US),
            &frame,
            &proj,
            bounds,
            0,
            gpui::px(30.0),
        )
        .is_none()
    );
}

/// Consecutive steps are exactly contiguous in time, so the gap that separates
/// them on screen — and gives the connectors somewhere to land — is taken in
/// pixels. A bar too narrow to spare it keeps its full width instead of being
/// inset out of existence.
#[test]
fn bar_span_gaps_neighbours_without_erasing_slivers() {
    let bounds = lane_bounds(1000.0);
    // 1000 µs window over 1000 px: 1 px/µs.
    let view = PlotBounds::new(EPOCH_US as f64, 0.0, (EPOCH_US + 1_000) as f64, 1.0);
    let proj = Projection::new(&view, bounds);
    let frame = GanttFrame {
        bars: Vec::new(),
        cycle: Vec::new(),
        summarized: false,
        bucket_us: 0,
        data_end: None,
        period_us: 1_000,
    };

    // Two steps the prefix sum made contiguous at t+100.
    let first = bar_span(&bar(EPOCH_US, 100, EPOCH_US), &frame, &proj);
    let second = bar_span(&bar(EPOCH_US + 100, 100, EPOCH_US), &frame, &proj);
    let gap = f32::from(second.0 - first.1);
    assert!(
        (gap - BAR_GAP_X).abs() < 0.01,
        "expected a {BAR_GAP_X} px gap, got {gap}"
    );

    // A 2 µs step is narrower than the gap; it keeps its full extent.
    let sliver = bar_span(&bar(EPOCH_US, 2, EPOCH_US), &frame, &proj);
    assert!(
        f32::from(sliver.1 - sliver.0) >= 2.0,
        "a sliver must not be inset away: {:?}",
        sliver
    );
}

/// Zoom is unbounded, so the window is held above the timestamp quantum: a
/// zero-width window takes the scan's integer range and the paint scale with it.
#[test]
fn clamp_window_holds_the_zoom_floor() {
    let collapsed = PlotBounds::new(EPOCH_US as f64, 0.0, EPOCH_US as f64, 1.0);
    let held = clamp_window(collapsed);
    assert!(held.width() >= MIN_WINDOW_US);
    assert!(
        (held.min_x + held.width() / 2.0 - EPOCH_US as f64).abs() < 1.0,
        "the centre the gesture chose is kept"
    );
    // A window already above the floor passes through untouched.
    let wide = PlotBounds::new(EPOCH_US as f64, 0.0, (EPOCH_US + 5_000) as f64, 1.0);
    assert_eq!(clamp_window(wide).bits(), wide.bits());
}

/// The cycle period is the median gap, so one stalled stretch does not stretch
/// the panel's notion of a cycle.
#[test]
fn measured_period_is_the_median_gap() {
    let cycle = [(0i64, 1u64), (1_000, 1), (2_000, 1), (99_000, 1)];
    assert_eq!(measured_period(&cycle), Some(1_000));
    assert_eq!(measured_period(&[(0, 1)]), None);
    assert_eq!(measured_period(&[]), None);
}
