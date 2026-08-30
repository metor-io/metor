//! Built-in widgets and overrides registered at startup.
//!
//! Populates the registry with coverage for primitive types (`Hsla`,
//! `SharedString`, `ComponentId`), generic shapes (`Override<T>`,
//! `TimeRangeBehavior`), list-of-entity fields, and the views whose
//! inspector pages need custom rows.

use std::sync::Arc;

use gpui::{AppContext as _, Entity, Hsla, SharedString};
use metor_db::DB;
use metor_proto::types::ComponentId;

use crate::inspector::rows::{
    BoolRow, ColorRow, CommandRow, InspectorRow, NavRow, ScalarRow, TextRow,
};
use crate::views::column_browser::filter_bar_row;
use crate::views::list_plot::{ListLinePlot, ListTrace};
use crate::views::time_series::time_range::TimeRangeBehavior;
use crate::views::time_series::{EventOverlay, LinePlot, Override, Trace, YAxis};
use crate::views::viewer_3d::Viewer3d;
use crate::views::xy_plot::{XyLinePlot, XyTrace};

use super::{AddBehavior, InspectorRegistry, builders};

/// A "Source" row: the trace's current channel as its summary, and beneath
/// it the same wizard the palette adds a trace with, opened on that channel
/// so it can be edited rather than retyped.
fn source_row(
    summary: SharedString,
    query: String,
    wizard: Box<dyn Fn(&gpui::App) -> Vec<Box<dyn InspectorRow>>>,
) -> Box<dyn InspectorRow> {
    Box::new(NavRow::new(SharedString::new_static("Source"), summary, wizard).with_query(query))
}

/// What to put in the picker's field to edit a binding: the expression with
/// its sigil, so the field opens in expression mode, or the component's name.
fn binding_query(db: &DB, component: ComponentId) -> String {
    crate::dynamic::expressions::binding_text(db, component).unwrap_or_else(|| {
        crate::inspector::trace_picker::list_components(db)
            .into_iter()
            .find(|(id, _)| *id == component)
            .map(|(_, name)| name)
            .unwrap_or_default()
    })
}

/// What the add wizard produced, in place of the trace being edited.
///
/// The first new trace inherits how the old one was drawn — colour, style,
/// width, axis, visibility — so editing a source does not restyle it; an
/// expression over a vector yields one trace per element, and the rest
/// follow it in the plot's list with the palette colours the wizard chose.
fn replace_trace(trace: &Entity<Trace>, picked: Vec<Trace>, cx: &mut gpui::App) {
    let Some(lp) = trace.read(cx).line_plot.clone().and_then(|w| w.upgrade()) else {
        return;
    };
    lp.update(cx, |lp, cx| {
        let Some(at) = lp
            .traces
            .iter()
            .position(|t| t.entity_id() == trace.entity_id())
        else {
            return;
        };
        let old = trace.read(cx);
        let (color, style, stroke_width, axis_index, visible) = (
            old.color,
            old.style,
            old.stroke_width,
            old.axis_index,
            old.visible,
        );
        let replacements: Vec<Entity<Trace>> = picked
            .into_iter()
            .enumerate()
            .map(|(i, mut t)| {
                if i == 0 {
                    t.color = color;
                    t.style = style;
                    t.stroke_width = stroke_width;
                }
                t.axis_index = axis_index;
                t.visible = visible;
                cx.new(|_| t)
            })
            .collect();
        lp.traces.splice(at..at + 1, replacements);
        cx.notify();
    });
}

/// How a trace's channel reads in a summary: an expression's text, or
/// `component.element` the way the trace was labelled when it was picked.
fn channel_summary(db: &DB, component: ComponentId, element: usize) -> SharedString {
    if let Some(text) = crate::dynamic::expressions::binding_text(db, component) {
        let body = crate::dynamic::expressions::body(&text).to_string();
        let names = crate::inspector::trace_picker::element_names_for_component(db, component);
        return SharedString::from(match names.get(element) {
            Some(name) if names.len() > 1 => format!("{body}.{name}"),
            _ => body,
        });
    }
    let name = crate::inspector::trace_picker::list_components(db)
        .into_iter()
        .find(|(id, _)| *id == component)
        .map(|(_, name)| name)
        .unwrap_or_else(|| format!("{component:?}"));
    let names = crate::inspector::trace_picker::element_names_for_component(db, component);
    SharedString::from(match names.get(element) {
        Some(elem) if names.len() > 1 => format!("{name}.{elem}"),
        _ => name,
    })
}

impl InspectorRegistry {
    pub(super) fn register_defaults(&mut self, db: Arc<DB>) {
        self.register_hsla();
        self.register_shared_string();
        self.register_component_id(db.clone());
        self.register_override_f64();
        self.register_override_shared_string();
        self.register_override_hsla();
        self.register_time_range_behavior();
        self.register_override_time_range_behavior();
        self.register_inspectable::<crate::views::Monitor>();
        self.register_inspectable::<crate::views::AlarmView>();
        self.register_inspectable::<crate::views::SequenceView>();
        self.register_inspectable::<crate::views::TrafficLight>();
        self.register_inspectable::<crate::views::Annunciator>();
        self.register_inspectable::<crate::views::Meter>();
        self.register_inspectable::<crate::views::Gauge>();
        self.register_inspectable::<crate::views::SequenceControl>();
        self.register_entity_list::<crate::views::AttitudeIndicator, crate::views::VectorMarker>(
            db.clone(),
            |a| &a.vectors,
            |a| &mut a.vectors,
            AddBehavior::Default(Arc::new({
                // A marker owns a stream, so it needs the db a freshly-added
                // one will bind against once a component is picked.
                let db = db.clone();
                move |_cx| crate::views::VectorMarker::empty(db.clone())
            })),
        );
        self.register_entity_list::<crate::views::StateChip, crate::views::StateEntry>(
            db.clone(),
            |c| &c.states,
            |c| &mut c.states,
            AddBehavior::Default(Arc::new(|_cx| crate::views::StateEntry::empty())),
        );
        self.register_entity_list::<LinePlot, Trace>(
            db.clone(),
            |lp| &lp.traces,
            |lp| &mut lp.traces,
            AddBehavior::Wizard(Arc::new(|parent, db, cx| {
                builders::build_trace_add_wizard(parent, db, cx)
            })),
        );
        self.register_entity_list::<LinePlot, YAxis>(
            db.clone(),
            |lp| &lp.axes,
            |lp| &mut lp.axes,
            AddBehavior::Default(Arc::new(|_cx| YAxis::new("Y"))),
        );
        self.register_entity_list::<LinePlot, EventOverlay>(
            db.clone(),
            |lp| &lp.event_overlays,
            |lp| &mut lp.event_overlays,
            AddBehavior::Wizard(Arc::new(|parent, db, cx| {
                builders::build_event_overlay_wizard(parent, db, cx)
            })),
        );
        self.register_entity_list::<Viewer3d, crate::views::viewer_3d::ModelEntry>(
            db.clone(),
            |v| &v.models,
            |v| &mut v.models,
            AddBehavior::Default(Arc::new(|_cx| crate::views::viewer_3d::ModelEntry::empty())),
        );
        self.register_viewer3d_builder(db.clone());
        self.register_measurement_cursor_builder();
        self.register_dashboard_builder(db.clone());
        self.register_pane_builder();
        self.register_component_browser_builder();
        self.register_data_table_builder();
        self.register_outline_builder();
        self.register_trace_builder(db.clone());
        self.register_xy_trace_builder();
        self.register_list_trace_builder();
        self.register_entity_list::<XyLinePlot, XyTrace>(
            db.clone(),
            |lp| &lp.traces,
            |lp| &mut lp.traces,
            AddBehavior::Wizard(Arc::new(|parent, db, cx| {
                builders::build_xy_trace_add_wizard(parent, db, cx)
            })),
        );
        self.register_entity_list::<ListLinePlot, ListTrace>(
            db.clone(),
            |lp| &lp.traces,
            |lp| &mut lp.traces,
            AddBehavior::Wizard(Arc::new(|parent, db, cx| {
                builders::build_list_trace_add_wizard(parent, db, cx)
            })),
        );
    }

    fn register_hsla(&mut self) {
        self.register_field_widget::<Hsla>(Arc::new(|ctx, peek, any_entity, idx| {
            let color = *peek.get::<Hsla>().unwrap();
            let label = ctx.label.clone();
            let read_entity = any_entity.clone();
            Box::new(ColorRow {
                label,
                color,
                read_color: Arc::new(move |cx| {
                    crate::inspector::reflect::get_field::<Hsla>(&read_entity, idx, cx)
                        .unwrap_or(color)
                }),
                on_change: Arc::new(move |c, _w, cx| {
                    crate::inspector::reflect::set_field::<Hsla>(&any_entity, idx, c, cx);
                }),
            })
        }));
    }

    fn register_shared_string(&mut self) {
        self.register_field_widget::<SharedString>(Arc::new(|ctx, peek, any_entity, idx| {
            let value = peek.get::<SharedString>().unwrap().clone();
            let label = ctx.label.clone();
            Box::new(TextRow::new(
                label,
                value,
                Arc::new(move |s, _w, cx| {
                    crate::inspector::reflect::set_field::<SharedString>(
                        &any_entity,
                        idx,
                        SharedString::from(s),
                        cx,
                    );
                }),
            ))
        }));
    }

    fn register_component_id(&mut self, db: Arc<DB>) {
        let db = db.clone();
        self.register_field_widget::<ComponentId>(Arc::new(move |ctx, peek, any_entity, idx| {
            let current = *peek.get::<ComponentId>().unwrap();
            let current_name = db.with_state(|s| {
                s.get_component_metadata(current)
                    .map(|m| SharedString::from(m.name.clone()))
            });
            let label = ctx.label.clone();
            let db = ctx.db.clone();
            Box::new(NavRow::new(
                label,
                current_name.unwrap_or_else(|| SharedString::from(format!("{}", current))),
                Box::new(move |cx| {
                    builders::build_component_picker(&db, any_entity.clone(), idx, cx)
                }),
            ))
        }));
    }

    /// Row for `Override<f64>`: numeric editor with an "Auto" escape hatch.
    fn register_override_f64(&mut self) {
        self.register_field_widget::<Override<f64>>(Arc::new(|ctx, peek, any_entity, idx| {
            let current = peek.get::<Override<f64>>().unwrap().clone();
            builders::build_override_row(
                ctx.label.clone(),
                current,
                any_entity,
                idx,
                |v| SharedString::from(format!("{}", v)),
                |label, initial, any_entity, idx| {
                    let write = any_entity.clone();
                    Box::new(ScalarRow::new(
                        label,
                        initial,
                        Arc::new(move |v, _w, cx| {
                            crate::inspector::reflect::set_field::<Override<f64>>(
                                &write,
                                idx,
                                Override::Custom(v),
                                cx,
                            );
                        }),
                    ))
                },
            )
        }));
    }

    /// Row for `Override<SharedString>`: text editor paired with an "Auto" reset.
    fn register_override_shared_string(&mut self) {
        self.register_field_widget::<Override<SharedString>>(Arc::new(
            |ctx, peek, any_entity, idx| {
                let current = peek.get::<Override<SharedString>>().unwrap().clone();
                builders::build_override_row(
                    ctx.label.clone(),
                    current,
                    any_entity,
                    idx,
                    |s| s.clone(),
                    |label, initial, any_entity, idx| {
                        let write = any_entity.clone();
                        Box::new(TextRow::new(
                            label,
                            initial,
                            Arc::new(move |s, _w, cx| {
                                crate::inspector::reflect::set_field::<Override<SharedString>>(
                                    &write,
                                    idx,
                                    Override::Custom(SharedString::from(s)),
                                    cx,
                                );
                            }),
                        ))
                    },
                )
            },
        ));
    }

    /// Row for `Override<Hsla>`: color picker paired with an "Auto" reset.
    fn register_override_hsla(&mut self) {
        self.register_field_widget::<Override<Hsla>>(Arc::new(|ctx, peek, any_entity, idx| {
            let current = peek.get::<Override<Hsla>>().unwrap().clone();
            builders::build_override_row(
                ctx.label.clone(),
                current,
                any_entity,
                idx,
                |_| SharedString::new_static("Custom"),
                |label, initial, any_entity, idx| {
                    let read = any_entity.clone();
                    let write = any_entity.clone();
                    Box::new(ColorRow {
                        label,
                        color: initial,
                        read_color: Arc::new(
                            move |cx| match crate::inspector::reflect::get_field::<Override<Hsla>>(
                                &read, idx, cx,
                            ) {
                                Some(Override::Custom(c)) => c,
                                _ => initial,
                            },
                        ),
                        on_change: Arc::new(move |c, _w, cx| {
                            crate::inspector::reflect::set_field::<Override<Hsla>>(
                                &write,
                                idx,
                                Override::Custom(c),
                                cx,
                            );
                        }),
                    })
                },
            )
        }));
    }

    /// Row for `TimeRangeBehavior`: preset picker plus a custom text field.
    ///
    /// The summary shows the matching preset name or, failing that, the
    /// formatted range so off-preset values remain legible at a glance.
    fn register_time_range_behavior(&mut self) {
        self.register_field_widget::<TimeRangeBehavior>(Arc::new(|ctx, peek, any_entity, idx| {
            let current = *peek.get::<TimeRangeBehavior>().unwrap();
            let summary = builders::preset_label(&current)
                .map(SharedString::from)
                .unwrap_or_else(|| SharedString::from(format!("{}", current)));
            let label = ctx.label.clone();
            Box::new(NavRow::new(
                label,
                summary,
                Box::new(move |cx| {
                    let current = crate::inspector::reflect::get_field::<TimeRangeBehavior>(
                        &any_entity,
                        idx,
                        cx,
                    )
                    .unwrap_or_default();
                    let entity = any_entity.clone();
                    crate::views::time_series::time_range::picker_rows(
                        SharedString::from(format!("{}", current)),
                        Arc::new(move |value, _w, cx| {
                            crate::inspector::reflect::set_field::<TimeRangeBehavior>(
                                &entity, idx, value, cx,
                            );
                        }),
                    )
                }),
            ))
        }));
    }

    /// Row for `Override<TimeRangeBehavior>`: like the plain range picker,
    /// with an extra "Auto" choice that resumes following the app-wide
    /// global range. Presets and custom text pin the plot to its own
    /// window.
    fn register_override_time_range_behavior(&mut self) {
        self.register_field_widget::<Override<TimeRangeBehavior>>(Arc::new(
            |ctx, peek, any_entity, idx| {
                let current = peek.get::<Override<TimeRangeBehavior>>().unwrap().clone();
                let summary = match current.as_custom() {
                    None => SharedString::new_static("Auto"),
                    Some(behavior) => builders::preset_label(behavior)
                        .map(SharedString::from)
                        .unwrap_or_else(|| SharedString::from(format!("{behavior}"))),
                };
                let label = ctx.label.clone();
                Box::new(NavRow::new(
                    label,
                    summary,
                    Box::new(move |cx| {
                        let current = crate::inspector::reflect::get_field::<
                            Override<TimeRangeBehavior>,
                        >(&any_entity, idx, cx)
                        .unwrap_or(Override::Auto);
                        let set_override = {
                            let entity = any_entity.clone();
                            move |value, cx: &mut gpui::App| {
                                crate::inspector::reflect::set_field::<Override<TimeRangeBehavior>>(
                                    &entity, idx, value, cx,
                                );
                            }
                        };
                        let auto_set = set_override.clone();
                        let mut rows: Vec<Box<dyn InspectorRow>> = vec![Box::new(CommandRow::new(
                            "Auto (follow global)",
                            Arc::new(move |_w, cx| auto_set(Override::Auto, cx)),
                        ))];
                        let current_text = current
                            .as_custom()
                            .map(|b| SharedString::from(format!("{b}")))
                            .unwrap_or_default();
                        rows.extend(crate::views::time_series::time_range::picker_rows(
                            current_text,
                            Arc::new(move |value, _w, cx| {
                                set_override(Override::Custom(value), cx)
                            }),
                        ));
                        rows
                    }),
                ))
            },
        ));
    }

    fn register_measurement_cursor_builder(&mut self) {
        use crate::views::time_series::MeasurementCursor;
        self.register_type_builder::<MeasurementCursor>(Arc::new(|any_entity, _db, cx| {
            let cursor: Entity<MeasurementCursor> = any_entity
                .downcast()
                .expect("MeasurementCursor type mismatch");
            crate::views::time_series::build_cursor_rows(cursor, cx)
        }));
    }

    /// Trace inspector: the default facet rows, a source picker, and an axis
    /// picker. The source picker rebinds the trace in place and asks the
    /// owning plot (via the trace's back-ref) to follow the new channel; the
    /// axis picker reads the plot's `axes` and writes `axis_index`, and is
    /// hidden when the plot has a single axis.
    fn register_trace_builder(&mut self, _db: Arc<DB>) {
        self.register_type_builder::<Trace>(Arc::new(|any_entity, db, cx| {
            let trace: Entity<Trace> = any_entity.clone().downcast().expect("Trace type mismatch");
            let mut rows =
                crate::inspector::reflect::default_rows_for_any_entity(&any_entity, db, cx);

            let (component, element) = {
                let t = trace.read(cx);
                (t.component_id, t.element_index)
            };
            let source = {
                let db = db.clone();
                let trace = trace.clone();
                let query = binding_query(&db, component);
                source_row(
                    channel_summary(&db, component, element),
                    query,
                    Box::new(move |_cx| {
                        let trace = trace.clone();
                        let color_basis: crate::inspector::trace_picker::ColorBasis = {
                            let trace = trace.clone();
                            Arc::new(move |cx| {
                                trace
                                    .read(cx)
                                    .line_plot
                                    .clone()
                                    .and_then(|w| w.upgrade())
                                    .map(|lp| lp.read(cx).traces().len())
                                    .unwrap_or(0)
                            })
                        };
                        crate::inspector::trace_picker::select_traces_wizard_rows(
                            db.clone(),
                            color_basis,
                            Arc::new(move |picked, _window, cx| replace_trace(&trace, picked, cx)),
                        )
                    }),
                )
            };
            rows.push(source);

            let Some(lp) = trace.read(cx).line_plot.clone().and_then(|w| w.upgrade()) else {
                return rows;
            };
            let axes = lp.read(cx).axes.clone();
            if axes.len() <= 1 {
                return rows;
            }

            let axis_name = |i: usize, label: &SharedString| -> SharedString {
                if label.is_empty() {
                    SharedString::from(format!("Axis {i}"))
                } else {
                    label.clone()
                }
            };
            let current = trace.read(cx).axis_index.min(axes.len() - 1);
            let summary = axis_name(current, &axes[current].read(cx).label);

            let trace_for_children = trace.clone();
            rows.push(Box::new(NavRow::new(
                SharedString::new_static("Axis"),
                summary,
                Box::new(move |cx| {
                    let axes = match trace_for_children
                        .read(cx)
                        .line_plot
                        .clone()
                        .and_then(|w| w.upgrade())
                    {
                        Some(lp) => lp.read(cx).axes.clone(),
                        None => return vec![],
                    };
                    axes.iter()
                        .enumerate()
                        .map(|(i, axis)| {
                            let name = axis_name(i, &axis.read(cx).label);
                            let t = trace_for_children.clone();
                            Box::new(CommandRow::new(
                                name,
                                Arc::new(move |_w, cx| {
                                    t.update(cx, |tr, cx| {
                                        tr.axis_index = i;
                                        cx.notify();
                                    });
                                    // Repaint the plot so the trace jumps axis.
                                    if let Some(lp) =
                                        t.read(cx).line_plot.clone().and_then(|w| w.upgrade())
                                    {
                                        lp.update(cx, |_, cx| cx.notify());
                                    }
                                }),
                            )) as Box<dyn InspectorRow>
                        })
                        .collect()
                }),
            )));
            rows
        }));
    }

    /// XY trace inspector: the default facet rows plus a source picker per
    /// axis, each rebinding its own end of the trace.
    fn register_xy_trace_builder(&mut self) {
        self.register_type_builder::<XyTrace>(Arc::new(|any_entity, db, cx| {
            let trace: Entity<XyTrace> = any_entity
                .clone()
                .downcast()
                .expect("XyTrace type mismatch");
            let mut rows =
                crate::inspector::reflect::default_rows_for_any_entity(&any_entity, db, cx);
            let (x, y) = {
                let t = trace.read(cx);
                (
                    (t.x_component_id, t.x_element_index),
                    (t.y_component_id, t.y_element_index),
                )
            };
            let summary = SharedString::from(format!(
                "{} vs {}",
                channel_summary(db, x.0, x.1),
                channel_summary(db, y.0, y.1)
            ));
            let query = binding_query(db, x.0);
            let db = db.clone();
            rows.push(source_row(
                summary,
                query,
                Box::new(move |_cx| {
                    let trace = trace.clone();
                    crate::views::xy_plot::trace_picker::select_xy_trace_wizard_rows(
                        db.clone(),
                        Arc::new(|_cx| 0),
                        Arc::new(move |picked, _window, cx| {
                            trace.update(cx, |t, cx| {
                                t.x_component_id = picked.x_component_id;
                                t.x_element_index = picked.x_element_index;
                                t.y_component_id = picked.y_component_id;
                                t.y_element_index = picked.y_element_index;
                                t.label = picked.label;
                                t.x_expression = picked.x_expression;
                                t.y_expression = picked.y_expression;
                                cx.notify();
                            });
                            if let Some(lp) =
                                trace.read(cx).line_plot.clone().and_then(|w| w.upgrade())
                            {
                                lp.update(cx, |lp, cx| lp.rebind_trace(&trace, cx));
                            }
                        }),
                    )
                }),
            ));
            rows
        }));
    }

    /// List trace inspector: the default facet rows plus a source picker,
    /// offering the same vector-only page the add wizard does.
    fn register_list_trace_builder(&mut self) {
        self.register_type_builder::<ListTrace>(Arc::new(|any_entity, db, cx| {
            let trace: Entity<ListTrace> = any_entity
                .clone()
                .downcast()
                .expect("ListTrace type mismatch");
            let mut rows =
                crate::inspector::reflect::default_rows_for_any_entity(&any_entity, db, cx);
            let component = trace.read(cx).component_id;
            let summary = channel_summary(db, component, 0);
            let db_for_children = db.clone();
            let query = binding_query(db, component);
            rows.push(source_row(
                summary,
                query,
                Box::new(move |_cx| {
                    let trace = trace.clone();
                    let on_select: crate::views::list_plot::trace_picker::OnListTraceSelected =
                        Arc::new(move |picked, _window, cx| {
                            trace.update(cx, |t, cx| {
                                t.component_id = picked.component_id;
                                t.len = picked.len;
                                t.label = picked.label;
                                t.expression = picked.expression;
                                cx.notify();
                            });
                            if let Some(lp) =
                                trace.read(cx).line_plot.clone().and_then(|w| w.upgrade())
                            {
                                lp.update(cx, |lp, cx| lp.rebind_trace(&trace, cx));
                            }
                        });
                    crate::views::list_plot::trace_picker::select_list_trace_wizard_rows(
                        db_for_children.clone(),
                        Arc::new(|_cx| 0),
                        on_select,
                    )
                }),
            ));
            rows
        }));
    }

    fn register_viewer3d_builder(&mut self, _db: Arc<DB>) {
        self.register_type_builder::<Viewer3d>(Arc::new(|any_entity, db, cx| {
            let viewer: Entity<Viewer3d> = any_entity
                .clone()
                .downcast()
                .expect("Viewer3d type mismatch");
            let mut rows =
                crate::inspector::reflect::default_rows_for_any_entity(&any_entity, db, cx);
            let add_viewer = viewer.clone();
            rows.push(Box::new(CommandRow::new(
                "Add Model",
                Arc::new(move |_w, cx| {
                    add_viewer.update(cx, |v, cx| v.add_model("", "", cx));
                }),
            )));
            let reset_viewer = viewer.clone();
            rows.push(Box::new(CommandRow::new(
                "Reset Camera",
                Arc::new(move |_w, cx| {
                    reset_viewer.update(cx, |v, cx| v.reset_camera(cx));
                }),
            )));
            rows
        }));
    }

    fn register_pane_builder(&mut self) {
        use crate::tiles::{Pane, TabOrientation};
        self.register_type_builder::<Pane>(Arc::new(|any_entity, _db, _cx| {
            let pane: Entity<Pane> = any_entity.downcast().expect("Pane type mismatch");

            let orientation_read = pane.clone();
            let orientation_write = pane.clone();
            let orientation_row = BoolRow::dynamic(
                "Vertical Tabs",
                Arc::new(move |cx| {
                    matches!(
                        orientation_read.read(cx).tab_orientation(),
                        TabOrientation::Vertical
                    )
                }),
                Arc::new(move |checked, _w, cx| {
                    orientation_write.update(cx, |p, cx| {
                        let next = if checked {
                            TabOrientation::Vertical
                        } else {
                            TabOrientation::Horizontal
                        };
                        p.set_tab_orientation(next, cx);
                    });
                }),
            );

            let hide_read = pane.clone();
            let hide_write = pane.clone();
            let hide_row = BoolRow::dynamic(
                "Hide Tab Bar",
                Arc::new(move |cx| hide_read.read(cx).hide_tab_bar()),
                Arc::new(move |checked, _w, cx| {
                    hide_write.update(cx, |p, cx| p.set_hide_tab_bar(checked, cx));
                }),
            );

            let lock_read = pane.clone();
            let lock_write = pane;
            let lock_row = BoolRow::dynamic(
                "Locked Size",
                Arc::new(move |cx| lock_read.read(cx).locked_size().is_some()),
                Arc::new(move |checked, _w, cx| {
                    lock_write.update(cx, |p, cx| {
                        let next = checked.then(|| p.current_outer_size());
                        p.set_locked_size(next, cx);
                    });
                }),
            );

            vec![
                Box::new(orientation_row),
                Box::new(hide_row),
                Box::new(lock_row),
            ]
        }));
    }

    fn register_component_browser_builder(&mut self) {
        use crate::views::ComponentBrowser;
        self.register_type_builder::<ComponentBrowser>(Arc::new(|any_entity, _db, cx| {
            let browser: Entity<ComponentBrowser> = any_entity
                .downcast()
                .expect("ComponentBrowser type mismatch");
            let summary = match browser.read(cx).delegate().custom_title() {
                Override::Auto => SharedString::new_static("Auto"),
                Override::Custom(s) => s.clone(),
            };
            let label = SharedString::new_static("Title");
            let nav_browser = browser.clone();
            let label_for_nav = label.clone();
            let filter_row = filter_bar_row(browser.clone());
            let title_row = NavRow::new(
                label.clone(),
                summary,
                Box::new(move |cx| {
                    let initial = match nav_browser.read(cx).delegate().custom_title() {
                        Override::Custom(s) => s.clone(),
                        Override::Auto => SharedString::new_static(""),
                    };
                    let write = nav_browser.clone();
                    let auto = nav_browser.clone();
                    vec![
                        Box::new(TextRow::new(
                            label_for_nav.clone(),
                            initial,
                            Arc::new(move |s, _w, cx| {
                                write.update(cx, |b, cx| {
                                    b.delegate_mut().set_custom_title(
                                        Override::Custom(SharedString::from(s)),
                                        cx,
                                    );
                                });
                            }),
                        )) as Box<dyn InspectorRow>,
                        Box::new(CommandRow::new(
                            "Auto",
                            Arc::new(move |_w, cx| {
                                auto.update(cx, |b, cx| {
                                    b.delegate_mut().set_custom_title(Override::Auto, cx);
                                });
                            }),
                        )) as Box<dyn InspectorRow>,
                    ]
                }),
            );
            vec![Box::new(title_row), Box::new(filter_row)]
        }));
    }

    fn register_data_table_builder(&mut self) {
        use crate::views::DataTable;
        self.register_type_builder::<DataTable>(Arc::new(|any_entity, _db, _cx| {
            let table: Entity<DataTable> = any_entity.downcast().expect("DataTable type mismatch");
            vec![Box::new(filter_bar_row(table))]
        }));
    }

    fn register_outline_builder(&mut self) {
        use crate::views::ComponentOutline;
        use crate::views::outline;
        self.register_type_builder::<ComponentOutline>(Arc::new(|any_entity, _db, _cx| {
            let outline: Entity<ComponentOutline> = any_entity
                .downcast()
                .expect("ComponentOutline type mismatch");
            vec![
                Box::new(outline::sparklines_row(outline.clone())),
                Box::new(outline::filter_bar_row(outline)),
            ]
        }));
    }

    fn register_dashboard_builder(&mut self, _db: Arc<DB>) {
        self.register_type_builder::<crate::views::dashboard::DashboardPanel>(Arc::new(
            |any_entity, db, cx| {
                let entity: Entity<crate::views::dashboard::DashboardPanel> =
                    any_entity.downcast().expect("DashboardPanel type mismatch");
                crate::views::dashboard::dashboard_rows(entity, db.clone(), cx)
            },
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use metor_db::ComponentSchema;
    use metor_proto::types::PrimType;

    /// Editing a source runs the add wizard, and what it yields stands in
    /// for the trace: the first keeps how the old one was drawn, the rest
    /// follow it in the list.
    #[gpui::test]
    fn the_wizards_traces_replace_the_edited_one_in_place(cx: &mut gpui::TestAppContext) {
        let temp = tempfile::tempdir().unwrap();
        let db = Arc::new(DB::create(temp.path().join("db")).unwrap());
        for name in ["a", "b", "c"] {
            db.with_state_mut(|s| {
                s.insert_component(
                    ComponentId::new(name),
                    ComponentSchema::new(PrimType::F64, &[]),
                    &db.path,
                )
            })
            .unwrap();
        }
        let palette = crate::theme::DARK.line_colors;
        let plot = cx.update(|cx| {
            crate::theme::set_theme(cx, Arc::new(crate::theme::DARK.clone()));
            cx.new(|cx| {
                let mut lp = LinePlot::new(db.clone(), cx);
                let mut edited = Trace::new(ComponentId::new("a"), 0, palette[3]);
                edited.stroke_width = 4.0;
                let after = Trace::new(ComponentId::new("c"), 0, palette[1]);
                lp.bind_traces(vec![edited, after], cx);
                lp
            })
        });
        cx.run_until_parked();
        let edited = cx.update(|cx| plot.read(cx).traces()[0].clone());

        cx.update(|cx| {
            let picked = vec![
                Trace::new(ComponentId::new("b"), 0, palette[0]),
                Trace::new(ComponentId::new("b"), 1, palette[1]),
            ];
            replace_trace(&edited, picked, cx);
        });
        cx.update(|cx| {
            let lp = plot.read(cx);
            let ids: Vec<(ComponentId, usize)> = lp
                .traces()
                .iter()
                .map(|t| (t.read(cx).component_id, t.read(cx).element_index))
                .collect();
            assert_eq!(
                ids,
                vec![
                    (ComponentId::new("b"), 0),
                    (ComponentId::new("b"), 1),
                    (ComponentId::new("c"), 0),
                ]
            );
            let first = lp.traces()[0].read(cx);
            assert_eq!(first.color, palette[3], "the edited trace keeps its colour");
            assert_eq!(first.stroke_width, 4.0);
            assert_eq!(
                lp.traces()[1].read(cx).color,
                palette[1],
                "extras keep the wizard's"
            );
        });
    }
}
