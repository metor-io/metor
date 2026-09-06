use super::*;
use crate::inspector::registry::InspectorRegistry;
use metor_db::ComponentSchema;
use metor_proto::types::PrimType;
use metor_proto_wkt::ComponentMetadata;

fn fixture(cx: &mut gpui::TestAppContext) -> (tempfile::TempDir, Arc<DB>, Entity<LinePlot>) {
    let temp = tempfile::tempdir().unwrap();
    let db = Arc::new(DB::create(temp.path().join("db")).unwrap());
    register(&db, "signal");
    let plot = cx.update(|cx| {
        crate::theme::set_theme(cx, Arc::new(crate::theme::DARK.clone()));
        InspectorRegistry::init(db.clone(), cx);
        cx.new(|cx| {
            let mut plot = LinePlot::new(db.clone(), cx);
            plot.bind_traces(vec![trace("signal", 0)], cx);
            plot
        })
    });
    cx.run_until_parked();
    (temp, db, plot)
}

fn register(db: &DB, name: &str) {
    db.with_state_mut(|state| {
        state
            .insert_component(
                ComponentId::new(name),
                ComponentSchema::new(PrimType::F64, &[2][..]),
                &db.path,
            )
            .unwrap();
        state
            .set_component_metadata(
                ComponentMetadata {
                    component_id: ComponentId::new(name),
                    name: name.into(),
                    metadata: Default::default(),
                },
                &db.path,
            )
            .unwrap();
    });
}

fn trace(name: &str, element: usize) -> Trace {
    Trace::new(
        ComponentId::new(name),
        element,
        crate::theme::DARK.line_colors[0],
    )
}

fn edit_field<T: facet::Facet<'static> + 'static, V: facet::Facet<'static> + 'static>(
    entity: &Entity<T>,
    name: &str,
    value: V,
    cx: &mut gpui::App,
) {
    let facet::Type::User(facet::UserType::Struct(shape)) = T::SHAPE.ty else {
        panic!("expected a struct");
    };
    let index = shape
        .fields
        .iter()
        .position(|field| field.name == name)
        .unwrap();
    crate::inspector::reflect::set_field(&entity.clone().into_any(), index, value, cx);
}

fn zoom(plot: &mut LinePlot, cx: &mut Context<LinePlot>) {
    plot.set_view_override(
        Some(PlotView {
            x: (20.0, 30.0),
            axes: smallvec::smallvec![(2.0, 3.0)],
        }),
        cx,
    );
}

#[gpui::test]
fn live_samples_update_bounds_without_reconciling_configuration(cx: &mut gpui::TestAppContext) {
    let (_temp, db, plot) = fixture(cx);
    let component = db.with_state(|state| {
        state
            .get_component(ComponentId::new("signal"))
            .unwrap()
            .clone()
    });
    let mut writer = component.time_series.writer().unwrap();
    let before = cx.update(|cx| {
        let plot = plot.read(cx);
        (plot.configuration_passes, plot.title_rebuilds)
    });
    for i in 1..=8 {
        writer
            .push_buf(Timestamp(i), bytemuck::cast_slice(&[i as f64, 100.0]))
            .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            let plot = plot.read(cx);
            assert_eq!(
                plot.tracking[&plot.traces[0].entity_id()].y_bounds,
                Some((1.0, i as f64))
            );
            assert_eq!((plot.configuration_passes, plot.title_rebuilds), before);
        });
    }
    cx.update(|cx| plot.update(cx, zoom));
    cx.run_until_parked();
    cx.update(|cx| {
        let plot = plot.read(cx);
        assert_eq!((plot.configuration_passes, plot.title_rebuilds), before);
        assert_eq!(plot.effective_view(cx).unwrap().x, (20.0, 30.0));
    });
}

#[gpui::test]
fn trace_edits_rebind_and_only_title_inputs_rebuild_the_title(cx: &mut gpui::TestAppContext) {
    let (_temp, db, plot) = fixture(cx);
    let trace = cx.update(|cx| plot.read(cx).traces[0].clone());
    let title_rebuilds = cx.update(|cx| plot.read(cx).title_rebuilds);
    cx.update(|cx| {
        plot.update(cx, zoom);
        edit_field(&trace, "stroke_width", 3.0_f32, cx);
    });
    cx.run_until_parked();
    cx.update(|cx| {
        assert_eq!(plot.read(cx).title_rebuilds, title_rebuilds);
        assert!(plot.read(cx).view_override.is_some());
        edit_field(&trace, "element_index", 1_usize, cx);
    });
    cx.run_until_parked();
    cx.update(|cx| {
        let plot = plot.read(cx);
        assert_eq!(
            plot.tracking[&trace.entity_id()].cached_element_index,
            Some(1)
        );
        assert_eq!(plot.title_rebuilds, title_rebuilds + 1);
        assert_eq!(plot.title(), derive_title(&plot.traces, &db, cx));
    });
    register(&db, "other");
    cx.update(|cx| {
        edit_field(
            &trace,
            "source",
            crate::data_binding::Binding::from(ComponentId::new("other")),
            cx,
        )
    });
    cx.run_until_parked();
    cx.update(|cx| {
        assert_eq!(
            plot.read(cx)
                .component_for_trace(&trace, cx)
                .unwrap()
                .component_id,
            ComponentId::new("other")
        );
        assert!(plot.read(cx).title().starts_with("other"));
    });
}

#[gpui::test]
fn reflected_plot_and_axis_edits_survive_coalesced_data_notifications(
    cx: &mut gpui::TestAppContext,
) {
    let (_temp, _db, plot) = fixture(cx);
    cx.update(|cx| {
        plot.update(cx, |plot, cx| {
            zoom(plot, cx);
            cx.notify();
        });
        edit_field(
            &plot,
            "custom_title",
            Override::Custom(SharedString::from("custom")),
            cx,
        );
        // GPUI may merge this notification with the edit's pending notification.
        plot.update(cx, |_, cx| cx.notify());
    });
    cx.run_until_parked();
    cx.update(|cx| {
        assert_eq!(plot.read(cx).title(), "custom");
        assert!(plot.read(cx).view_override.is_some());
        let axis = plot.read(cx).axes[0].clone();
        edit_field(&axis, "y_min_override", Override::Custom(-10.0_f64), cx);
    });
    cx.run_until_parked();
    cx.update(|cx| assert!(plot.read(cx).view_override.is_none()));
    cx.update(|cx| {
        plot.update(cx, zoom);
        edit_field(
            &plot,
            "x_range",
            Override::Custom(TimeRangeBehavior {
                start: super::super::time_range::Offset::Fixed(Timestamp(100)),
                end: super::super::time_range::Offset::Fixed(Timestamp(200)),
            }),
            cx,
        );
        edit_field(&plot, "custom_title", Override::<SharedString>::Auto, cx);
    });
    cx.run_until_parked();
    cx.update(|cx| {
        assert!(plot.read(cx).title().starts_with("signal"));
        assert!(plot.read(cx).view_override.is_none());
    });
}

#[gpui::test]
fn inspector_list_actions_invalidate_axis_membership(cx: &mut gpui::TestAppContext) {
    use crate::inspector::rows::RowAction;
    use facet::Facet;

    let (_temp, db, plot) = fixture(cx);
    let cx = cx.add_empty_window();
    let axis_rows = |window: &mut Window, cx: &mut gpui::App| {
        let handler = cx
            .global::<InspectorRegistry>()
            .entity_list_handler(<Vec<Entity<YAxis>>>::SHAPE.id)
            .unwrap()
            .clone();
        let mut row = handler(plot.clone().into_any(), "Axes".into(), &db, cx);
        let RowAction::Cascade(rows) = row.activate(window, cx) else {
            panic!("expected axis list");
        };
        rows
    };
    cx.update(|window, cx| {
        let mut rows = axis_rows(window, cx);
        rows.iter_mut()
            .find(|row| row.label() == "Add")
            .unwrap()
            .activate(window, cx);
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        assert_eq!(plot.read(cx).axes.len(), 2);
        assert_eq!(plot.read(cx).axis_subscriptions.len(), 2);
        let mut rows = axis_rows(window, cx);
        let RowAction::Cascade(mut rows) = rows[1].activate(window, cx) else {
            panic!("expected axis inspector");
        };
        rows.iter_mut()
            .find(|row| row.label() == "Remove")
            .unwrap()
            .activate(window, cx);
    });
    cx.run_until_parked();
    cx.update(|_, cx| {
        assert_eq!(plot.read(cx).axes.len(), 1);
        assert_eq!(plot.read(cx).axis_subscriptions.len(), 1);
    });
}

#[gpui::test]
fn structural_edits_keep_tracker_identity_and_observe_replacement_axes(
    cx: &mut gpui::TestAppContext,
) {
    let (_temp, _db, plot) = fixture(cx);
    let first = cx.update(|cx| plot.read(cx).traces[0].clone());
    cx.update(|cx| {
        let second = cx.new(|_| trace("signal", 1));
        let axis = cx.new(|_| YAxis::new("replacement"));
        edit_field(&plot, "traces", vec![first.clone(), second], cx);
        edit_field(&plot, "axes", vec![axis], cx);
    });
    cx.run_until_parked();
    cx.update(|cx| {
        let p = plot.read(cx);
        assert_eq!(p.tracking.len(), 2);
        assert_eq!(p.title(), "signal");
        assert_eq!(p.axis_subscriptions.len(), 1);
        let mut reordered = p.traces.clone();
        reordered.reverse();
        // A cached bound acts as evidence that reordering preserves the tracker.
        plot.update(cx, |p, _| {
            p.tracking.get_mut(&first.entity_id()).unwrap().y_bounds = Some((7.0, 9.0))
        });
        edit_field(&plot, "traces", reordered, cx);
    });
    cx.run_until_parked();
    cx.update(|cx| {
        assert_eq!(
            plot.read(cx).tracking[&first.entity_id()].y_bounds,
            Some((7.0, 9.0))
        );
        edit_field(&plot, "traces", vec![first.clone()], cx);
    });
    cx.run_until_parked();
    cx.update(|cx| {
        assert_eq!(plot.read(cx).tracking.len(), 1);
        assert_eq!(plot.read(cx).tasks.len(), 1);
        plot.update(cx, zoom);
        let axis = plot.read(cx).axes[0].clone();
        edit_field(&axis, "y_max_override", Override::Custom(50.0_f64), cx);
    });
    cx.run_until_parked();
    cx.update(|cx| assert!(plot.read(cx).view_override.is_none()));
}

#[gpui::test]
fn metadata_renames_and_late_registration_refresh_idle_plots(cx: &mut gpui::TestAppContext) {
    let (_temp, db, plot) = fixture(cx);
    let before = db.metadata_gen.latest();
    db.with_state_mut(|state| {
        state.set_component_metadata(
            ComponentMetadata {
                component_id: ComponentId::new("signal"),
                name: "renamed".into(),
                metadata: Default::default(),
            },
            &db.path,
        )
    })
    .unwrap();
    assert!(db.metadata_gen.latest() > before);
    cx.run_until_parked();
    cx.update(|cx| {
        assert!(plot.read(cx).title().starts_with("renamed"));
        plot.update(cx, |p, cx| p.bind_traces(vec![trace("late", 0)], cx));
    });
    cx.run_until_parked();
    register(&db, "late");
    db.vtable_gen
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    cx.run_until_parked();
    cx.update(|cx| {
        let p = plot.read(cx);
        assert_eq!(
            p.component_for_trace(&p.traces[0], cx)
                .unwrap()
                .component_id,
            ComponentId::new("late")
        );
        assert!(p.title().starts_with("late"));
    });
}
