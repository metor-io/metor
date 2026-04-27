//! Built-in widgets and overrides registered at startup.
//!
//! Populates the registry with coverage for primitive types (`Hsla`,
//! `SharedString`, `ComponentId`), generic shapes (`Override<T>`,
//! `TimeRangeBehavior`), list-of-entity fields, and the views whose
//! inspector pages need custom rows.

use std::sync::Arc;

use gpui::{Entity, Hsla, SharedString};
use metor_db::DB;
use metor_proto::types::ComponentId;

use crate::inspector::rows::{
    BoolRow, ColorRow, CommandRow, InspectorRow, NavRow, ScalarRow, TextRow,
};
use crate::views::time_series::time_range::TimeRangeBehavior;
use crate::views::time_series::{LinePlot, Override, Trace};
use crate::views::viewer_3d::Viewer3d;

use super::{AddBehavior, FieldOverride, InspectorRegistry, builders};

impl InspectorRegistry {
    pub(super) fn register_defaults(&mut self, db: Arc<DB>) {
        self.register_hsla();
        self.register_shared_string();
        self.register_component_id(db.clone());
        self.register_override_f64();
        self.register_override_shared_string();
        self.register_time_range_behavior();
        self.register_inspectable::<crate::views::Monitor>();
        self.register_inspectable::<crate::views::TrafficLight>();
        self.register_inspectable::<crate::views::TrafficLightGrid>();
        self.register_entity_list::<LinePlot, Trace>(
            db.clone(),
            |lp| &lp.traces,
            |lp| &mut lp.traces,
            AddBehavior::Wizard(Arc::new(|parent, db, cx| {
                builders::build_trace_add_wizard(parent, db, cx)
            })),
        );
        self.register_entity_list::<Viewer3d, crate::views::viewer_3d::ModelEntry>(
            db.clone(),
            |v| &v.models,
            |v| &mut v.models,
            AddBehavior::Default(Arc::new(|_cx| crate::views::viewer_3d::ModelEntry::empty())),
        );
        self.register_viewer3d_builder(db.clone());
        self.register_dashboard_builder(db);
        self.register_pane_builder();
        self.register_field_override::<crate::views::time_series::Trace>(
            "stroke_width",
            FieldOverride {
                range: Some((0.5, 10.0)),
            },
        );
        self.register_field_override::<crate::views::viewer_3d::Viewer3d>(
            "camera_fov",
            FieldOverride {
                range: Some((0.1, std::f64::consts::PI)),
            },
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
                    Box::new(ScalarRow {
                        label,
                        value: initial,
                        on_change: Arc::new(move |v, _w, cx| {
                            crate::inspector::reflect::set_field::<Override<f64>>(
                                &write,
                                idx,
                                Override::Custom(v),
                                cx,
                            );
                        }),
                    })
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
                    let mut rows: Vec<Box<dyn InspectorRow>> = TimeRangeBehavior::PRESETS
                        .iter()
                        .map(|(name, value)| {
                            let entity = any_entity.clone();
                            let value = *value;
                            Box::new(CommandRow::new(
                                *name,
                                Arc::new(move |_w, cx| {
                                    crate::inspector::reflect::set_field::<TimeRangeBehavior>(
                                        &entity, idx, value, cx,
                                    );
                                }),
                            )) as Box<dyn InspectorRow>
                        })
                        .collect();
                    let entity = any_entity.clone();
                    rows.push(Box::new(TextRow::new(
                        SharedString::new_static("Custom"),
                        SharedString::from(format!("{}", current)),
                        Arc::new(move |s, _w, cx| {
                            if let Ok(value) = s.parse::<TimeRangeBehavior>() {
                                crate::inspector::reflect::set_field::<TimeRangeBehavior>(
                                    &entity, idx, value, cx,
                                );
                            }
                        }),
                    )));
                    rows
                }),
            ))
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
