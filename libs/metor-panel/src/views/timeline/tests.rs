use super::*;
use gpui::{point, size};
use std::cell::RefCell;

#[gpui::test]
fn thin_range_drag_keeps_live_anchor_and_preview_does_not_commit(cx: &mut gpui::TestAppContext) {
    let temp = tempfile::tempdir().unwrap();
    let db = Arc::new(DB::create(temp.path().join("db")).unwrap());
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
        crate::theme::set_theme(cx, Arc::new(crate::theme::DARK.clone()));
        temporal::TemporalController::init(db.clone(), cx);
        temporal::dispatch(TimeAction::Range(TimeRangeSpec { start: TimeExpr::fixed(Timestamp(0)), end: TimeExpr::LIVE }), cx).unwrap();
        let edits = Arc::new(RefCell::new(Vec::new()));
        let sink = edits.clone();
        let timeline = cx.new(|cx| Timeline::preview(db.clone(), EditTarget::Start, Arc::new(move |action, _, _| sink.borrow_mut().push(action)), cx));
        timeline.update(cx, |t, cx| {
            t.area = Some(Bounds { origin: point(px(0.0), px(0.0)), size: size(px(500.0), px(31.0)) });
            t.viewport = Some(Interval::new(0, 500_000_000));
            let before = temporal::config(cx);
            t.pointer_down(&gpui::MouseDownEvent { button: MouseButton::Left, position: point(px(0.0), px(5.0)), click_count: 1, ..Default::default() }, window, cx);
            t.pointer_move(&gpui::MouseMoveEvent { position: point(px(100.0), px(5.0)), pressed_button: Some(MouseButton::Left), ..Default::default() }, cx);
            assert_eq!(t.draft.unwrap().end, TimeExpr::LIVE);
            assert_eq!(t.draft.unwrap().start, TimeExpr::fixed(Timestamp(100_000_000)));
            assert!(edits.borrow().is_empty(), "pointer movement waits for the frame flush");
            t.flush_range(window, cx);
            assert_eq!(edits.borrow().len(), 1, "the host receives the edit before release");
            t.flush_range(window, cx);
            assert_eq!(edits.borrow().len(), 1, "unchanged frames do not republish");
            t.release(window, cx);
            assert_eq!(edits.borrow().len(), 1, "release does not duplicate the final frame");
            assert_eq!(temporal::config(cx), before);
            assert!(matches!(edits.borrow().last(), Some(TimeAction::Range(r)) if r.end == TimeExpr::LIVE));
            assert!(!t.is_dragging());
        });
    });
}

#[gpui::test]
fn manual_zoom_is_local_and_double_click_does_not_seek(cx: &mut gpui::TestAppContext) {
    let temp = tempfile::tempdir().unwrap();
    let db = Arc::new(DB::create(temp.path().join("db")).unwrap());
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
        crate::theme::set_theme(cx, Arc::new(crate::theme::DARK.clone()));
        temporal::TemporalController::init(db.clone(), cx);
        temporal::dispatch(TimeAction::Seek(TimeExpr::fixed(Timestamp(50_000_000))), cx).unwrap();
        temporal::dispatch(
            TimeAction::Range(TimeRangeSpec::fixed(Timestamp(0)..Timestamp(100_000_000))),
            cx,
        )
        .unwrap();
        let timeline = cx.new(|cx| Timeline::from_config(TimelineConfig::default(), db, cx));
        timeline.update(cx, |t, cx| {
            t.area = Some(Bounds {
                origin: point(px(0.0), px(0.0)),
                size: size(px(500.0), px(31.0)),
            });
            t.update_view(0.03, cx);
            let before = temporal::config(cx);
            t.zoom(0.5, 0.25, cx);
            t.update_view(0.03, cx);
            let manual = t.viewport;
            assert!(matches!(t.config.navigation, Navigation::Manual(_)));
            temporal::dispatch(TimeAction::Seek(TimeExpr::fixed(Timestamp(80_000_000))), cx)
                .unwrap();
            t.update_view(0.03, cx);
            assert_eq!(manual, t.viewport);
            assert_eq!(before.range, temporal::config(cx).range);
            let view_time = temporal::config(cx).view;
            let mut down = gpui::MouseDownEvent {
                button: MouseButton::Left,
                position: point(px(250.0), px(22.0)),
                click_count: 1,
                ..Default::default()
            };
            t.pointer_down(&down, window, cx);
            t.release(window, cx);
            down.click_count = 2;
            t.pointer_down(&down, window, cx);
            t.release(window, cx);
            assert_eq!(temporal::config(cx).view, view_time);
            assert_eq!(t.config.navigation, Navigation::Fit);
            assert!(t.click.is_none());
        });
    });
}

#[gpui::test]
fn registry_supports_thin_dashboard_and_same_tile_config(cx: &mut gpui::TestAppContext) {
    cx.update(|cx| {
        crate::views::dashboard::WidgetRegistry::init(cx);
        let spec = cx
            .global::<crate::views::dashboard::WidgetRegistry>()
            .spec(&crate::views::dashboard::WidgetKind::new("timeline"))
            .unwrap();
        assert_eq!(spec.minimum_size, (80.0, 31.0));
        assert_eq!(
            spec.tile.as_ref().unwrap().serialization_key.as_ref(),
            "timeline"
        );
        let config = TimelineConfig {
            navigation: Navigation::Manual(Interval::new(100, 1_000_100)),
            snap: true,
            ..Default::default()
        };
        let encoded = serde_json::to_string(&config).unwrap();
        let restored: TimelineConfig = serde_json::from_str(&encoded).unwrap();
        assert_eq!(config.navigation, restored.navigation);
        assert!(restored.snap);
        assert!(TimelineConfig::default().snap);
        assert!(serde_json::from_str::<TimelineConfig>("{}").unwrap().snap);
        assert!(
            !serde_json::from_str::<TimelineConfig>(r#"{"snap":false}"#)
                .unwrap()
                .snap
        );
        assert_eq!(
            serde_json::from_str::<TimelineConfig>("{}")
                .unwrap()
                .navigation,
            Navigation::Fit
        );
    });
}

#[gpui::test]
fn wheel_zooms_time_control_pans_and_shift_scrolls_overflow(cx: &mut gpui::TestAppContext) {
    let temp = tempfile::tempdir().unwrap();
    let db = Arc::new(DB::create(temp.path().join("db")).unwrap());
    let cx = cx.add_empty_window();
    cx.update(|_, cx| {
        crate::theme::set_theme(cx, Arc::new(crate::theme::DARK.clone()));
        temporal::TemporalController::init(db.clone(), cx);
        let timeline = cx.new(|cx| Timeline::from_config(TimelineConfig::default(), db, cx));
        timeline.update(cx, |t, cx| {
            for id in ["a", "b", "c"] {
                t.set_events(id, id.into(), Vec::new(), cx);
            }
            t.area = Some(Bounds::new(
                point(px(0.0), px(0.0)),
                size(px(600.0), px(240.0)),
            ));
            t.viewport = Some(Interval::new(0, 100_000_000));
            let before = temporal::config(cx);
            let mut event = gpui::ScrollWheelEvent {
                position: point(px(300.0), px(70.0)),
                delta: gpui::ScrollDelta::Pixels(point(px(0.0), px(-20.0))),
                ..Default::default()
            };
            let before_view = t.viewport.unwrap();
            let anchor = t.fraction(event.position.x);
            t.scroll(&event, cx);
            t.update_view(0.03, cx);
            let zoomed = t.viewport.unwrap();
            assert!(
                zoomed.span() > before_view.span(),
                "negative delta zooms out like the plot"
            );
            assert!(
                (i128::from(zoomed.at(anchor)) - i128::from(before_view.at(anchor))).abs() <= 1,
                "zoom preserves time under the pointer"
            );
            assert_eq!(t.lane_scroll, 0);
            event.modifiers.control = true;
            t.scroll(&event, cx);
            t.update_view(0.03, cx);
            let panned = t.viewport.unwrap();
            assert!(panned.start > zoomed.start);
            assert_eq!(
                panned.span(),
                zoomed.span(),
                "Ctrl pans without changing scale"
            );
            event.modifiers.control = false;
            event.modifiers.shift = true;
            t.scroll(&event, cx);
            assert_eq!(t.lane_scroll, 0, "no vertical movement when lanes fit");
            t.area.as_mut().unwrap().size.height = px(88.0);
            for _ in 0..20 {
                t.scroll(&event, cx);
            }
            assert_eq!(
                t.lane_scroll,
                t.visible_lanes() - 1,
                "last lane fills the remaining row"
            );
            let old = t.lane_scroll;
            event.modifiers.shift = false;
            t.scroll(&event, cx);
            assert_eq!(
                t.lane_scroll, old,
                "ordinary scroll zooms time, even with overflow"
            );
            t.update_view(0.03, cx);
            event.modifiers.control = true;
            event.delta = gpui::ScrollDelta::Pixels(point(px(20.0), px(0.0)));
            let before_pan = t.viewport.unwrap();
            t.scroll(&event, cx);
            t.update_view(0.03, cx);
            assert!(
                t.viewport.unwrap().start < before_pan.start,
                "Ctrl also pans with horizontal trackpad input"
            );
            assert_eq!(t.viewport.unwrap().span(), before_pan.span());
            assert_eq!(temporal::config(cx), before);
            let area = t.time_area().unwrap();
            assert_eq!(t.fraction(area.origin.x), 0.0);
            assert_eq!(t.fraction(area.right()), 1.0);
        });
    });
}

#[gpui::test]
fn event_chip_body_hits_the_rendered_burst_and_span(cx: &mut gpui::TestAppContext) {
    let temp = tempfile::tempdir().unwrap();
    let db = Arc::new(DB::create(temp.path().join("db")).unwrap());
    cx.update(|cx| {
        crate::theme::set_theme(cx, Arc::new(crate::theme::DARK.clone()));
        temporal::TemporalController::init(db.clone(), cx);
    });
    let (timeline, cx) = cx.add_window_view(|_, cx| {
        let mut t = Timeline::from_config(
            TimelineConfig {
                sources: Vec::new(),
                navigation: Navigation::Manual(Interval::new(0, 100_000_000)),
                ..Default::default()
            },
            db,
            cx,
        );
        let event = |id, time, end: Option<i64>, priority| TimelineEvent {
            id,
            end: end.map(Timestamp),
            priority,
            event: crate::plot_events::PlotEvent {
                ts: Timestamp(time),
                color: crate::theme::theme(cx).text_primary,
                label: "Navigation warning".into(),
                short: "WARN navigation".into(),
                detail: crate::plot_events::EventDetail::Raw(0),
            },
        };
        t.set_events(
            "test",
            "Test events".into(),
            vec![
                event(1, 20_000_000, None::<i64>, 0),
                event(2, 20_000_001, None, 2),
                event(3, 60_000_000, Some(90_000_000), 0),
            ],
            cx,
        );
        t
    });
    cx.refresh().unwrap();
    cx.run_until_parked();
    cx.update(|_, cx| {
        let t = timeline.read(cx);
        let area = t.area.unwrap();
        let time_area = t.time_area().unwrap();
        let view = t.viewport.unwrap();
        let x = |time| time_area.origin.x + time_area.size.width * view.fraction(time) as f32;
        let chip_body = point(x(20_000_001) + px(35.0), area.origin.y + px(56.0));
        let (events, count) = t
            .hit_events(chip_body, 8)
            .expect("the label body is interactive, not only its timestamp rule");
        assert_eq!(count, 2);
        assert_eq!(events[0].id, 2, "the highest severity leads the tooltip");
        let span_body = point(x(80_000_000), area.origin.y + px(56.0));
        assert_eq!(t.event_at(span_body, cx).unwrap().id, 3);
        assert!(
            t.event_at(point(time_area.origin.x - px(10.0), chip_body.y), cx)
                .is_none()
        );
    });
    let (empty, flag) = cx.update(|_, cx| {
        let t = timeline.read(cx);
        let area = t.time_area().unwrap();
        (
            area.origin + point(px(5.0), px(24.0)),
            point(
                area.origin.x + area.size.width * 0.2 + px(35.0),
                area.origin.y + px(56.0),
            ),
        )
    });
    cx.simulate_mouse_move(empty, None, gpui::Modifiers::default());
    cx.refresh().unwrap();
    cx.update(|_, cx| {
        assert!(matches!(
            timeline.read(cx).readout(cx),
            Some(tooltip::Readout::Time(_))
        ))
    });
    cx.simulate_mouse_move(flag, None, gpui::Modifiers::default());
    cx.refresh().unwrap();
    cx.update(|_, cx| {
        assert!(matches!(
            timeline.read(cx).readout(cx),
            Some(tooltip::Readout::Events(_, 2))
        ))
    });
    cx.simulate_mouse_move(
        point(px(-10.0), px(-10.0)),
        None,
        gpui::Modifiers::default(),
    );
    cx.refresh().unwrap();
    cx.update(|_, cx| assert!(timeline.read(cx).readout(cx).is_none()));
}

#[gpui::test]
fn right_click_opens_shared_commands_and_can_execute_them(cx: &mut gpui::TestAppContext) {
    use crate::inspector::{InspectorMode, InspectorRequest, OpenInspectorGlobal};
    let temp = tempfile::tempdir().unwrap();
    let db = Arc::new(DB::create(temp.path().join("db")).unwrap());
    let requests = Rc::new(RefCell::new(Vec::<InspectorRequest>::new()));
    cx.update(|cx| {
        crate::theme::set_theme(cx, Arc::new(crate::theme::DARK.clone()));
        temporal::TemporalController::init(db.clone(), cx);
        let requests = requests.clone();
        cx.set_global(OpenInspectorGlobal(Arc::new(move |request, _, _| {
            requests.borrow_mut().push(request);
        })));
    });
    let (timeline, cx) = cx.add_window_view(|_, cx| {
        let mut timeline = Timeline::from_config(
            TimelineConfig {
                sources: Vec::new(),
                navigation: Navigation::Manual(Interval::new(0, 100_000_000)),
                ..Default::default()
            },
            db,
            cx,
        );
        timeline.set_events("test", "Test events".into(), Vec::new(), cx);
        timeline
    });
    cx.refresh().unwrap();
    cx.run_until_parked();
    let position =
        cx.update(|_, cx| timeline.read(cx).area.unwrap().origin + point(px(250.0), px(25.0)));
    cx.simulate_mouse_move(position, None, gpui::Modifiers::default());
    cx.refresh().unwrap();
    cx.update(|_, cx| assert!(timeline.read(cx).readout(cx).is_some()));
    cx.simulate_event(gpui::MouseDownEvent {
        button: MouseButton::Right,
        position,
        click_count: 1,
        ..Default::default()
    });
    cx.simulate_event(gpui::MouseUpEvent {
        button: MouseButton::Right,
        position,
        ..Default::default()
    });
    cx.refresh().unwrap();
    cx.update(|_, cx| {
        assert!(
            timeline.read(cx).readout(cx).is_none(),
            "right click hides the readout"
        )
    });
    let mut request = requests
        .borrow_mut()
        .pop()
        .expect("right click opens the inspector");
    assert!(matches!(request.mode, InspectorMode::Anchored(p) if p == position));
    cx.update(|window, cx| {
        let expected = Timeline::rows(timeline.clone(), cx);
        assert_eq!(
            request.rows.iter().map(|r| r.label()).collect::<Vec<_>>(),
            expected.iter().map(|r| r.label()).collect::<Vec<_>>(),
            "right click and command palette use the same rows"
        );
        request
            .rows
            .iter_mut()
            .find(|r| r.label() == "Fit context")
            .unwrap()
            .activate(window, cx);
        assert_eq!(timeline.read(cx).config.navigation, Navigation::Fit);
        request
            .rows
            .iter_mut()
            .find(|r| r.label() == "Snap to events and anchors")
            .unwrap()
            .activate(window, cx);
        assert!(!timeline.read(cx).config.snap);
        request
            .rows
            .iter_mut()
            .find(|r| r.label() == "Show Test events")
            .unwrap()
            .activate(window, cx);
        assert!(
            timeline
                .read(cx)
                .config
                .collapsed
                .contains(&"custom:test".to_string())
        );
    });
}

#[gpui::test]
fn range_drag_updates_global_before_release_and_coalesces_frames(cx: &mut gpui::TestAppContext) {
    let temp = tempfile::tempdir().unwrap();
    let db = Arc::new(DB::create(temp.path().join("db")).unwrap());
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
        temporal::TemporalController::init(db.clone(), cx);
        temporal::dispatch(
            TimeAction::Range(TimeRangeSpec {
                start: TimeExpr::fixed(Timestamp(0)),
                end: TimeExpr::LIVE,
            }),
            cx,
        )
        .unwrap();
        let timeline = cx.new(|cx| Timeline::from_config(TimelineConfig::default(), db, cx));
        timeline.update(cx, |t, cx| {
            t.area = Some(Bounds::new(
                point(px(0.0), px(0.0)),
                size(px(500.0), px(31.0)),
            ));
            t.viewport = Some(Interval::new(0, 500_000_000));
            t.pointer_down(
                &gpui::MouseDownEvent {
                    button: MouseButton::Left,
                    position: point(px(0.0), px(4.0)),
                    click_count: 1,
                    ..Default::default()
                },
                window,
                cx,
            );
            for x in [50.0, 100.0] {
                t.pointer_move(
                    &gpui::MouseMoveEvent {
                        position: point(px(x), px(4.0)),
                        pressed_button: Some(MouseButton::Left),
                        ..Default::default()
                    },
                    cx,
                );
            }
            assert_eq!(
                temporal::config(cx).range.start,
                TimeExpr::fixed(Timestamp(0))
            );
            t.flush_range(window, cx);
            assert!(t.is_dragging());
            assert_eq!(
                temporal::config(cx).range.start,
                TimeExpr::fixed(Timestamp(100_000_000))
            );
            assert_eq!(temporal::config(cx).range.end, TimeExpr::LIVE);
            let revision = cx.global::<temporal::TemporalRevision>().0;
            t.flush_range(window, cx);
            t.release(window, cx);
            assert_eq!(cx.global::<temporal::TemporalRevision>().0, revision);
        });
    });
}

#[gpui::test]
fn ruler_grabbers_snap_to_live_and_follow_the_head(cx: &mut gpui::TestAppContext) {
    let temp = tempfile::tempdir().unwrap();
    let db = Arc::new(DB::create(temp.path().join("db")).unwrap());
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
        let controller = temporal::TemporalController::init(db.clone(), cx);
        for height in [31.0, 112.0] {
            temporal::dispatch(
                TimeAction::Range(TimeRangeSpec::fixed(
                    Timestamp(20_000_000)..Timestamp(80_000_000),
                )),
                cx,
            )
            .unwrap();
            controller.update(cx, |c, _| {
                c.snapshot.context.extent = Some(Timestamp(0)..Timestamp(100_000_000));
                c.snapshot.context.live = Some(Timestamp(100_000_000));
            });
            let timeline = cx.new(|cx| {
                Timeline::from_config(
                    TimelineConfig {
                        snap: true,
                        ..Default::default()
                    },
                    db.clone(),
                    cx,
                )
            });
            timeline.update(cx, |t, cx| {
                t.area = Some(Bounds::new(
                    point(px(0.0), px(0.0)),
                    size(px(600.0), px(height)),
                ));
                t.viewport = Some(Interval::new(0, 120_000_000));
                let area = t.time_area().unwrap();
                let x = |time: f32| area.origin.x + area.size.width * (time / 120_000_000.0);
                t.pointer_down(
                    &gpui::MouseDownEvent {
                        button: MouseButton::Left,
                        position: point(x(80_000_000.0), px(25.0)),
                        click_count: 1,
                        ..Default::default()
                    },
                    window,
                    cx,
                );
                assert!(
                    matches!(t.drag.as_ref().map(|d| d.kind), Some(DragKind::End)),
                    "grabbers are interactive inside the time-label band"
                );
                t.pointer_move(
                    &gpui::MouseMoveEvent {
                        position: point(x(100_000_000.0), px(25.0)),
                        pressed_button: Some(MouseButton::Left),
                        ..Default::default()
                    },
                    cx,
                );
                assert_eq!(t.draft.unwrap().end, TimeExpr::LIVE);
                let mut context = temporal::snapshot(cx).unwrap().context;
                context.live = Some(Timestamp(110_000_000));
                assert_eq!(
                    t.draft.unwrap().resolve(&context).unwrap().end,
                    Timestamp(110_000_000)
                );
                assert!(
                    matches!(
                        t.snap(100_000_000, t.viewport.unwrap(), true, cx).1.anchor,
                        temporal::Anchor::Timestamp(_)
                    ),
                    "Alt bypass leaves an absolute timestamp"
                );
                t.release(window, cx);
                assert_eq!(temporal::config(cx).range.end, TimeExpr::LIVE);
            });
        }
    });
}

#[gpui::test]
fn moving_range_to_live_preserves_a_floating_duration(cx: &mut gpui::TestAppContext) {
    let temp = tempfile::tempdir().unwrap();
    let db = Arc::new(DB::create(temp.path().join("db")).unwrap());
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
        let controller = temporal::TemporalController::init(db.clone(), cx);
        temporal::dispatch(
            TimeAction::Range(TimeRangeSpec::fixed(
                Timestamp(20_000_000)..Timestamp(80_000_000),
            )),
            cx,
        )
        .unwrap();
        controller.update(cx, |c, _| {
            c.snapshot.context.extent = Some(Timestamp(0)..Timestamp(100_000_000));
            c.snapshot.context.live = Some(Timestamp(100_000_000));
        });
        let timeline = cx.new(|cx| {
            Timeline::from_config(
                TimelineConfig {
                    snap: true,
                    ..Default::default()
                },
                db,
                cx,
            )
        });
        timeline.update(cx, |t, cx| {
            t.area = Some(Bounds::new(
                point(px(0.0), px(0.0)),
                size(px(600.0), px(31.0)),
            ));
            t.viewport = Some(Interval::new(0, 120_000_000));
            t.pointer_down(
                &gpui::MouseDownEvent {
                    button: MouseButton::Left,
                    position: point(px(200.0), px(4.0)),
                    click_count: 1,
                    ..Default::default()
                },
                window,
                cx,
            );
            assert!(matches!(
                t.drag.as_ref().map(|d| d.kind),
                Some(DragKind::Move)
            ));
            t.pointer_move(
                &gpui::MouseMoveEvent {
                    position: point(px(300.0), px(4.0)),
                    pressed_button: Some(MouseButton::Left),
                    ..Default::default()
                },
                cx,
            );
            let range = t.draft.unwrap();
            assert_eq!(range.end, TimeExpr::LIVE);
            assert_eq!(
                range.start,
                TimeExpr::new(temporal::Anchor::Live, -60_000_000)
            );
            let mut context = temporal::snapshot(cx).unwrap().context;
            context.live = Some(Timestamp(110_000_000));
            assert_eq!(
                range.resolve(&context).unwrap(),
                Timestamp(50_000_000)..Timestamp(110_000_000)
            );
        });
    });
}
