use super::*;

#[gpui::test]
fn editing_range_rebases_playback_or_pauses_without_seeking(cx: &mut gpui::TestAppContext) {
    let temp = tempfile::tempdir().unwrap();
    let db = Arc::new(DB::create(temp.path().join("db")).unwrap());
    cx.update(|cx| {
        TemporalController::init(db, cx);
        dispatch(
            TimeAction::Range(TimeRangeSpec::fixed(Timestamp(0)..Timestamp(100_000_000))),
            cx,
        )
        .unwrap();
        dispatch(TimeAction::Seek(TimeExpr::fixed(Timestamp(10_000_000))), cx).unwrap();
        dispatch(TimeAction::Play { from_start: false }, cx).unwrap();
        dispatch(
            TimeAction::Range(TimeRangeSpec::fixed(
                Timestamp(5_000_000)..Timestamp(20_000_000),
            )),
            cx,
        )
        .unwrap();
        let controller = controller(cx).unwrap();
        assert_eq!(
            controller.read(cx).playback.as_ref().unwrap().bounds.end,
            Timestamp(20_000_000)
        );
        let current = snapshot(cx).unwrap().context.view;
        dispatch(
            TimeAction::Range(TimeRangeSpec::fixed(
                Timestamp(30_000_000)..Timestamp(40_000_000),
            )),
            cx,
        )
        .unwrap();
        assert!(!snapshot(cx).unwrap().playing);
        assert_eq!(snapshot(cx).unwrap().context.view, current);
    });
}
use model::{ParseContext, duration, parse_instant, parse_range};

fn context() -> TimeContext {
    TimeContext {
        extent: Some(Timestamp(0)..Timestamp(120_000_000)),
        live: Some(Timestamp(600_000_000)),
        view: Some(Timestamp(60_000_000)),
    }
}

#[test]
fn anchors_keep_their_meaning_when_view_time_pauses() {
    let parse = ParseContext::utc();
    let mut clock = context();
    let first = parse_range("The first 5M", &parse).unwrap();
    assert_eq!(
        first.resolve(&clock).unwrap(),
        Timestamp(0)..Timestamp(300_000_000)
    );
    let last = parse_range("The last 2.5m", &parse).unwrap();
    assert_eq!(
        last.resolve(&clock).unwrap(),
        Timestamp(450_000_000)..Timestamp(600_000_000)
    );
    clock.live = Some(Timestamp(660_000_000));
    assert_eq!(
        last.resolve(&clock).unwrap(),
        Timestamp(510_000_000)..Timestamp(660_000_000)
    );
    assert_eq!(
        parse_range("1m ending at view time", &parse)
            .unwrap()
            .resolve(&clock)
            .unwrap(),
        Timestamp(0)..Timestamp(60_000_000)
    );
    let old: crate::views::time_series::TimeRangeBehavior = "LAST 5m".parse().unwrap();
    assert_eq!(TimeRangeSpec::from(old).end.anchor, Anchor::DataEnd);
}

#[test]
fn exact_duration_precision_and_invalid_inputs() {
    assert_eq!(duration("2.5M").unwrap(), 150_000_000);
    assert_eq!(duration("1h 30m 0.125s").unwrap(), 5_400_125_000);
    assert_eq!(duration("0.000001s").unwrap(), 1);
    for text in [
        "-1s",
        "NaN",
        "1month",
        "0.0000001s",
        "999999999999999999999999999999d",
        "1.2.3s",
    ] {
        assert!(duration(text).is_err(), "{text}");
    }
    for text in ["first 0m", "last -1m", "first NaNm"] {
        assert!(parse_range(text, &ParseContext::utc()).is_err());
    }
    assert!(
        TimeExpr::new(Anchor::Timestamp(i64::MAX), 1)
            .resolve(&context())
            .is_err()
    );
    assert!(parse_instant("view time", &ParseContext::utc(), false).is_err());
}

#[test]
fn timestamp_and_anchor_text_round_trip_without_precision_loss() {
    let parse = ParseContext::utc();
    for expr in [
        TimeExpr::fixed(Timestamp(1_780_000_060_123_457)),
        TimeExpr::new(Anchor::Live, -150_000_000),
        TimeExpr::new(Anchor::DataStart, 1),
        TimeExpr::new(Anchor::View, 30_000_000),
    ] {
        assert_eq!(
            parse_instant(&expr.to_string(), &parse, true).unwrap(),
            expr,
            "{expr}"
        );
    }
    for text in [
        "full range",
        "first 5m",
        "last 2.5m",
        "last 5m of data",
        "1m around view time",
        "2026-09-05T14:00:00.123457Z .. live",
    ] {
        let range = parse_range(text, &parse).unwrap();
        assert_eq!(parse_range(&range.to_string(), &parse).unwrap(), range);
    }
}

#[test]
fn civil_days_and_dst_require_explicit_disambiguation() {
    let parse =
        ParseContext::new("America/Los_Angeles", Timestamp::now(), Timestamp::now()).unwrap();
    let spring = parse_range("day 2026-03-08", &parse)
        .unwrap()
        .resolve(&context())
        .unwrap();
    assert_eq!(spring.end.0 - spring.start.0, 23 * 3_600_000_000);
    let fall = parse_range("day 2026-11-01", &parse)
        .unwrap()
        .resolve(&context())
        .unwrap();
    assert_eq!(fall.end.0 - fall.start.0, 25 * 3_600_000_000);
    assert!(parse_instant("2026-03-08 02:30:00", &parse, false).is_err());
    assert!(parse_instant("2026-11-01 01:30:00", &parse, false).is_err());
    let a = parse_instant("2026-11-01T01:30:00-07:00", &parse, false).unwrap();
    let b = parse_instant("2026-11-01T01:30:00-08:00", &parse, false).unwrap();
    assert_eq!(
        b.resolve(&context()).unwrap().0 - a.resolve(&context()).unwrap().0,
        3_600_000_000
    );
}

#[test]
fn invalid_ranges_do_not_fall_back_to_full_history() {
    let r = TimeRangeSpec::fixed(Timestamp(500)..Timestamp(100));
    assert!(r.resolve(&context()).is_err());
    let outside = TimeRangeSpec::fixed(Timestamp(-100)..Timestamp(-50));
    assert_eq!(
        outside.resolve(&context()).unwrap(),
        Timestamp(-100)..Timestamp(-50)
    );
    let one = TimeContext {
        extent: Some(Timestamp(10)..Timestamp(10)),
        ..context()
    };
    assert_eq!(
        TimeRangeSpec::FULL.resolve(&one).unwrap(),
        Timestamp(10)..Timestamp(11)
    );
}

#[gpui::test]
fn transport_and_layout_keep_range_and_view_time_independent(cx: &mut gpui::TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let db = Arc::new(DB::create(tmp.path().join("db")).unwrap());
    cx.update(|cx| {
        let c = TemporalController::init(db, cx);
        let range = TimeRangeSpec::fixed(Timestamp(100)..Timestamp(10_000_100));
        dispatch(TimeAction::Range(range), cx).unwrap();
        dispatch(TimeAction::Play { from_start: true }, cx).unwrap();
        assert!(snapshot(cx).unwrap().playing);
        dispatch(TimeAction::Display(TimeDisplay::Elapsed), cx).unwrap();
        dispatch(TimeAction::T0(Some(1_234_567)), cx).unwrap();
        let saved = save_layout(cx);
        let mut legacy = serde_json::to_value(&saved).unwrap();
        legacy.as_object_mut().unwrap().remove("elapsed_display");
        legacy.as_object_mut().unwrap().remove("t0");
        let legacy: metor_proto_wkt::TemporalLayout = serde_json::from_value(legacy).unwrap();
        assert!(!legacy.elapsed_display);
        assert_eq!(legacy.t0, None);
        restore_layout(&saved, cx);
        assert!(!snapshot(cx).unwrap().playing);
        assert!(!is_live(cx));
        assert_eq!(config(cx).range, range);
        assert_eq!(config(cx).display, TimeDisplay::Elapsed);
        assert_eq!(config(cx).t0, Some(1_234_567));
        dispatch(TimeAction::Live, cx).unwrap();
        assert!(is_live(cx));
        assert_eq!(config(cx).range, range);
        dispatch(TimeAction::Seek(TimeExpr::fixed(Timestamp(500))), cx).unwrap();
        dispatch(TimeAction::StepSize(100), cx).unwrap();
        dispatch(TimeAction::Step(-1), cx).unwrap();
        assert_eq!(view_time(cx), Some(Timestamp(400)));
        c.update(cx, |c, cx| {
            c.apply(TimeAction::Play { from_start: true }, cx).unwrap();
            let end = c.playback.as_ref().unwrap().started + Duration::from_secs(100);
            c.tick(end, cx);
            assert!(c.playback.is_none());
            assert_eq!(c.snapshot.context.view, Some(Timestamp(10_000_100)));
        });
    });
}

#[test]
fn compact_time_display_preserves_exact_values_and_dst_labels() {
    let mut config = TemporalConfig::default();
    let clock = context();
    let parse = ParseContext::utc();
    let t = parse_instant("2026-09-05 12:34:56.123456 UTC", &parse, false)
        .unwrap()
        .resolve(&clock)
        .unwrap();
    assert_eq!(
        display::timestamp(t, &config, &clock),
        "2026-09-05 12:34:56.123 UTC"
    );
    assert_eq!(
        parse_instant(&model::timestamp_text(t, "UTC"), &parse, false)
            .unwrap()
            .resolve(&clock)
            .unwrap(),
        t
    );
    config.timezone = "America/Los_Angeles".into();
    assert_eq!(
        display::timestamp(t, &config, &clock),
        "2026-09-05 05:34:56.123 PDT"
    );
    let winter = parse_instant("2026-01-05 12:34:56 UTC", &parse, false)
        .unwrap()
        .resolve(&clock)
        .unwrap();
    assert_eq!(
        display::timestamp(winter, &config, &clock),
        "2026-01-05 04:34:56.000 PST"
    );
    config.display = TimeDisplay::Elapsed;
    assert_eq!(
        display::timestamp(Timestamp(90_000_000), &config, &clock),
        "T+01:30"
    );
    config.t0 = Some(120_000_000);
    assert_eq!(
        display::timestamp(Timestamp(90_000_000), &config, &clock),
        "T-30"
    );
    assert_eq!(
        display::timestamp(Timestamp(360_120_000_000), &config, &clock),
        "T+100:00:00"
    );
    assert_eq!(
        display::range(
            TimeRangeSpec::fixed(Timestamp(90_000_000)..Timestamp(150_000_000)),
            &config,
            &clock
        ),
        "T-30 → T+30"
    );
}

#[test]
fn compact_elapsed_labels_round_trip_at_millisecond_precision() {
    let config = TemporalConfig {
        display: TimeDisplay::Elapsed,
        t0: Some(0),
        ..Default::default()
    };
    let clock = context();
    for (micros, expected) in [
        (0, "T+0"),
        (1_000_000, "T+01"),
        (1_550_000, "T+1.550s"),
        (1_000, "T+0.001s"),
        (59_999_000, "T+59.999s"),
        (60_000_000, "T+01:00"),
        (61_000_000, "T+01:01"),
        (61_550_000, "T+01:01.550"),
        (3_600_000_000, "T+01:00:00"),
        (3_661_550_000, "T+01:01:01.550"),
        (-1_000_000, "T-01"),
        (-1_550_000, "T-1.550s"),
        (-61_000_000, "T-01:01"),
    ] {
        let label = display::timestamp(Timestamp(micros), &config, &clock);
        assert_eq!(label, expected);
        let input = display::expand_input(&label, &config, &clock).unwrap();
        assert_eq!(
            parse_instant(&input, &ParseContext::utc(), false)
                .unwrap()
                .resolve(&clock)
                .unwrap(),
            Timestamp(micros),
        );
    }
    assert_eq!(display::timestamp(Timestamp(-999), &config, &clock), "T+0");
}

#[test]
fn elapsed_input_resolves_exact_instants_and_range_endpoints() {
    let config = TemporalConfig {
        t0: Some(120_000_123),
        ..Default::default()
    };
    let clock = context();
    let parse = ParseContext::utc();
    for (input, expected) in [
        ("T0", 120_000_123),
        ("T0 + 2.5m", 270_000_123),
        ("T-30s", 90_000_123),
        ("T+00:01:30", 210_000_123),
    ] {
        let text = display::expand_input(input, &config, &clock).unwrap();
        assert_eq!(
            parse_instant(&text, &parse, false)
                .unwrap()
                .resolve(&clock)
                .unwrap(),
            Timestamp(expected)
        );
    }
    let text = display::expand_input("T-30s .. T+1m", &config, &clock).unwrap();
    assert_eq!(
        parse_range(&text, &parse).unwrap().resolve(&clock).unwrap(),
        Timestamp(90_000_123)..Timestamp(180_000_123)
    );
    for bad in [
        "T+00:60:00",
        "T+00:00:60",
        "T+9999999999999999999999h",
        "T-",
        "T0 potato",
    ] {
        assert!(display::expand_input(bad, &config, &clock).is_err());
    }
    assert_eq!(
        display::expand_input("today 12:00", &config, &clock).unwrap(),
        "today 12:00"
    );
    let empty = TimeContext {
        extent: None,
        ..clock
    };
    assert!(display::expand_input("T0", &TemporalConfig::default(), &empty).is_err());
}

#[gpui::test]
fn disjoint_panels_share_bounds_and_clock_names_use_registered_ids(cx: &mut gpui::TestAppContext) {
    use crate::views::time_series::{Override, TimeRangeBehavior};
    use metor_db::{ComponentSchema, manifest::SpanSource};
    let tmp = tempfile::tempdir().unwrap();
    let db = Arc::new(DB::create(tmp.path().join("db")).unwrap());
    for (id, name, stamps) in [
        (ComponentId(42), "a.clock", [10, 20]),
        (ComponentId(43), "b.value", [100, 200]),
    ] {
        db.with_state_mut(|s| {
            s.insert_component(
                id,
                ComponentSchema::new(metor_proto::types::PrimType::F64, &[][..]),
                &db.path,
            )
        })
        .unwrap();
        db.with_state_mut(|s| {
            s.set_component_metadata(
                metor_proto_wkt::ComponentMetadata {
                    component_id: id,
                    name: name.into(),
                    metadata: Default::default(),
                },
                &db.path,
            )
        })
        .unwrap();
        let c = db.with_state(|s| s.get_component(id).cloned()).unwrap();
        let bytes = 1f64.to_le_bytes();
        c.time_series
            .install_samples(
                8,
                stamps.into_iter().map(|t| (Timestamp(t), bytes.as_slice())),
                SpanSource::RemoteFetch,
            )
            .unwrap();
    }
    let c = cx.update(|cx| TemporalController::init(db, cx));
    cx.run_until_parked();
    cx.update(|cx| {
        assert_eq!(
            c.read(cx).clock_source("a.clock").unwrap(),
            Some(ComponentId(42))
        );
        let a = Timestamp(10)..Timestamp(20);
        let b = Timestamp(100)..Timestamp(200);
        assert_eq!(
            resolve_range(&Override::Auto, a.clone(), cx),
            Some(Timestamp(10)..Timestamp(201))
        );
        assert_eq!(
            resolve_range(&Override::Auto, b, cx),
            Some(Timestamp(10)..Timestamp(201))
        );
        assert_eq!(
            resolve_range(&Override::Custom(TimeRangeBehavior::FULL), a, cx),
            Some(Timestamp(10)..Timestamp(21))
        );
        dispatch(TimeAction::Clock(Some(ComponentId(42))), cx).unwrap();
        assert_eq!(view_time(cx), Some(Timestamp(20)));
    });
}

#[gpui::test]
fn telemetry_ahead_of_wall_time_can_monitor_pause_resume_and_replay(cx: &mut gpui::TestAppContext) {
    use metor_db::{ComponentSchema, manifest::SpanSource};
    let temp = tempfile::tempdir().unwrap();
    let db = Arc::new(DB::create(temp.path().join("db")).unwrap());
    let id = ComponentId::new("clocked.value");
    db.with_state_mut(|s| {
        s.insert_component(
            id,
            ComponentSchema::new(metor_proto::types::PrimType::F64, &[][..]),
            &db.path,
        )
    })
    .unwrap();
    db.with_state_mut(|s| {
        s.set_component_metadata(
            metor_proto_wkt::ComponentMetadata {
                component_id: id,
                name: "clocked.value".into(),
                metadata: Default::default(),
            },
            &db.path,
        )
    })
    .unwrap();
    let component = db.with_state(|s| s.get_component(id).cloned()).unwrap();
    let latest = Timestamp(Timestamp::now().0 + 600_000_000);
    let first = Timestamp(latest.0 - 120_000_000);
    let bytes = 42f64.to_le_bytes();
    component
        .time_series
        .install_samples(
            8,
            [first, latest].into_iter().map(|t| (t, bytes.as_slice())),
            SpanSource::RemoteFetch,
        )
        .unwrap();
    let (controller, reader) = cx.update(|cx| {
        (
            TemporalController::init(db.clone(), cx),
            samples::acquire(db, id, cx),
        )
    });
    cx.run_until_parked();
    cx.update(|cx| {
        assert_eq!(view_time(cx), Some(latest));
        assert_eq!(
            reader.read(cx).selection.sample.as_ref().unwrap().timestamp,
            latest
        );
        dispatch(
            TimeAction::Range(parse_range("last 5m", &ParseContext::utc()).unwrap()),
            cx,
        )
        .unwrap();
        assert_eq!(
            snapshot(cx).unwrap().range,
            Some(Timestamp(latest.0 - 300_000_000)..latest)
        );
        dispatch(TimeAction::Pause, cx).unwrap();
        dispatch(TimeAction::Play { from_start: false }, cx).unwrap();
        assert!(is_live(cx));
        dispatch(TimeAction::Play { from_start: true }, cx).unwrap();
        controller.update(cx, |c, cx| {
            assert_eq!(c.playback.as_ref().unwrap().base, first);
            c.tick(
                c.playback.as_ref().unwrap().started + Duration::from_secs(1),
                cx,
            );
            assert_eq!(
                c.snapshot.context.view,
                Some(Timestamp(first.0 + 1_000_000))
            );
            assert!(c.snapshot.playing);
        });
    });
    cx.run_until_parked();
    cx.update(|cx| {
        assert_eq!(
            reader.read(cx).selection.sample.as_ref().unwrap().timestamp,
            first
        )
    });
}

#[gpui::test]
fn full_range_follows_samples_between_history_scans(cx: &mut gpui::TestAppContext) {
    use metor_db::{ComponentSchema, manifest::SpanSource};
    let temp = tempfile::tempdir().unwrap();
    let db = Arc::new(DB::create(temp.path().join("db")).unwrap());
    let id = ComponentId::new("stream.value");
    db.with_state_mut(|s| {
        s.insert_component(
            id,
            ComponentSchema::new(metor_proto::types::PrimType::F64, &[][..]),
            &db.path,
        )
    })
    .unwrap();
    db.with_state_mut(|s| {
        s.set_component_metadata(
            metor_proto_wkt::ComponentMetadata {
                component_id: id,
                name: "stream.value".into(),
                metadata: Default::default(),
            },
            &db.path,
        )
    })
    .unwrap();
    let component = db.with_state(|s| s.get_component(id).cloned()).unwrap();
    let bytes = 42f64.to_le_bytes();
    let append = |t| {
        component
            .time_series
            .install_samples(
                8,
                [(Timestamp(t), bytes.as_slice())],
                SpanSource::RemoteFetch,
            )
            .unwrap();
    };
    append(100);
    let controller = cx.update(|cx| TemporalController::init(db, cx));
    cx.run_until_parked();
    cx.update(|cx| {
        controller.update(cx, |c, cx| {
            let scanned_at = Instant::now();
            c.last_bounds = scanned_at;
            let revision = cx.global::<TemporalRevision>().0;
            c.tick(scanned_at + Duration::from_millis(1), cx);
            assert_eq!(cx.global::<TemporalRevision>().0, revision);
            for (frame, head) in [(1, 200), (2, 300), (3, 400)] {
                append(head);
                c.tick(scanned_at + Duration::from_millis(frame * 33), cx);
                assert_eq!(c.last_bounds, scanned_at);
                assert_eq!(c.snapshot.context.view, Some(Timestamp(head)));
                assert_eq!(c.snapshot.range, Some(Timestamp(100)..Timestamp(head + 1)));
            }

            c.apply(TimeAction::Pause, cx).unwrap();
            append(500);
            c.tick(scanned_at + Duration::from_millis(132), cx);
            assert_eq!(c.snapshot.context.view, Some(Timestamp(400)));
            assert_eq!(c.snapshot.range, Some(Timestamp(100)..Timestamp(501)));

            c.apply(TimeAction::WallClock, cx).unwrap();
            assert_eq!(c.snapshot.range, Some(Timestamp(100)..Timestamp(501)));
        });
    });
}
