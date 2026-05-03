//! Row builders shared across default field-widget registrations.
//!
//! Keeps reusable shapes — `Override<T>`, `Option<T>`, component pickers,
//! trace wizards — in one place so `defaults.rs` stays a flat list of
//! registrations rather than a dumping ground of closures.

use std::sync::Arc;

use facet::Facet;
use gpui::{AnyEntity, App, AppContext, Entity, SharedString};
use metor_db::DB;
use metor_proto::types::ComponentId;

use crate::inspector::rows::{CommandRow, InspectorRow, NavRow};
use crate::views::time_series::LinePlot;
use crate::views::time_series::Override;
use crate::views::time_series::time_range::TimeRangeBehavior;

use super::FieldBuildCtx;

/// First non-empty `SharedString` field of `value`, used as a list-item label
/// when no explicit label attribute is present.
pub(super) fn find_label_field<T: Facet<'static>>(value: &T) -> Option<SharedString> {
    let peek = facet::Peek::new(value);
    let peek_struct = peek.into_struct().ok()?;
    for (i, field_def) in peek_struct.ty().fields.iter().enumerate() {
        if field_def.shape().id == <SharedString as Facet>::SHAPE.id
            && let Ok(field_peek) = peek_struct.field(i)
            && let Ok(s) = field_peek.get::<SharedString>()
            && !s.is_empty()
        {
            return Some(s.clone());
        }
    }
    None
}

pub(super) fn build_component_picker(
    db: &Arc<DB>,
    any_entity: AnyEntity,
    idx: usize,
    _cx: &App,
) -> Vec<Box<dyn InspectorRow>> {
    let components = crate::inspector::trace_picker::list_components(db);
    components
        .into_iter()
        .map(|(id, name)| {
            let any_entity = any_entity.clone();
            Box::new(CommandRow::new(
                SharedString::from(name),
                Arc::new(move |_w, cx| {
                    crate::inspector::reflect::set_field::<ComponentId>(&any_entity, idx, id, cx);
                }),
            )) as Box<dyn InspectorRow>
        })
        .collect()
}

/// Name of the matching preset, or `None` when `value` is a custom range.
pub(super) fn preset_label(value: &TimeRangeBehavior) -> Option<&'static str> {
    TimeRangeBehavior::PRESETS
        .iter()
        .find(|(_, preset)| preset == value)
        .map(|(name, _)| *name)
}

/// Nav row for an [`Override<T>`] field.
///
/// Summary shows `Auto` or the formatted custom value; cascading into the
/// row exposes the typed value editor plus an "Auto" shortcut that restores
/// the automatic behavior.
pub(super) fn build_override_row<T>(
    label: SharedString,
    current: Override<T>,
    any_entity: AnyEntity,
    idx: usize,
    fmt_custom: impl Fn(&T) -> SharedString + 'static,
    value_row: impl Fn(SharedString, T, AnyEntity, usize) -> Box<dyn InspectorRow> + 'static,
) -> Box<dyn InspectorRow>
where
    T: Facet<'static> + Clone + Default + 'static,
{
    let summary = match &current {
        Override::Auto => SharedString::new_static("Auto"),
        Override::Custom(v) => fmt_custom(v),
    };
    let label_for_value = label.clone();
    Box::new(NavRow::new(
        label,
        summary,
        Box::new(move |cx| {
            let initial = crate::inspector::reflect::get_field::<Override<T>>(&any_entity, idx, cx)
                .unwrap_or(Override::Auto);
            let value_initial = match &initial {
                Override::Custom(v) => v.clone(),
                Override::Auto => T::default(),
            };
            let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();
            rows.push(value_row(
                label_for_value.clone(),
                value_initial,
                any_entity.clone(),
                idx,
            ));
            let auto_entity = any_entity.clone();
            rows.push(Box::new(CommandRow::new(
                "Auto",
                Arc::new(move |_w, cx| {
                    crate::inspector::reflect::set_field::<Override<T>>(
                        &auto_entity,
                        idx,
                        Override::Auto,
                        cx,
                    );
                }),
            )));
            rows
        }),
    ))
}

/// Nav row for an `Option<T>` field.
///
/// Only `Option<ComponentId>` has a populated picker today; other inner
/// types render as a passive `Set`/`None` summary so the walker doesn't
/// silently drop the field.
pub(super) fn build_option_row(
    ctx: &FieldBuildCtx,
    peek_option: facet::PeekOption<'_, '_>,
    any_entity: AnyEntity,
    field_idx: usize,
) -> Box<dyn InspectorRow> {
    let label = ctx.label.clone();
    let is_some = peek_option.is_some();
    let inner_shape = peek_option.def().t;
    let db = ctx.db.clone();

    let summary = if is_some {
        SharedString::from("Set")
    } else {
        SharedString::from("None")
    };

    Box::new(NavRow::new(
        label,
        summary,
        Box::new(move |_cx| {
            let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();

            if inner_shape.id != <ComponentId as Facet>::SHAPE.id {
                return rows;
            }

            if is_some {
                let any_entity = any_entity.clone();
                rows.push(Box::new(CommandRow::new(
                    "Clear",
                    Arc::new(move |_w, cx| {
                        crate::inspector::reflect::set_field::<Option<ComponentId>>(
                            &any_entity,
                            field_idx,
                            None,
                            cx,
                        );
                    }),
                )));
            } else {
                for (id, name) in crate::inspector::trace_picker::list_components(&db) {
                    let any_entity = any_entity.clone();
                    rows.push(Box::new(CommandRow::new(
                        SharedString::from(name),
                        Arc::new(move |_w, cx| {
                            crate::inspector::reflect::set_field::<Option<ComponentId>>(
                                &any_entity,
                                field_idx,
                                Some(id),
                                cx,
                            );
                        }),
                    )));
                }
            }

            rows
        }),
    ))
}

/// Trace-creation wizard for the "Add" button on a `LinePlot`'s trace list.
///
/// Drills through component → element selection and appends the resulting
/// traces to the parent plot. The color basis starts at the parent's
/// existing trace count so palette colors continue contiguously.
pub(super) fn build_trace_add_wizard(
    parent: gpui::AnyEntity,
    db: &Arc<DB>,
    _cx: &App,
) -> Vec<Box<dyn InspectorRow>> {
    let parent_for_basis = parent.clone();
    let color_basis: crate::inspector::trace_picker::ColorBasis = Arc::new(move |cx: &App| {
        parent_for_basis
            .clone()
            .downcast::<LinePlot>()
            .map(|p| p.read(cx).traces.len())
            .unwrap_or(0)
    });

    let on_select: crate::inspector::trace_picker::OnTracesSelected =
        Arc::new(move |traces, _w, cx| {
            let parent: Entity<LinePlot> = parent.clone().downcast().expect("parent type mismatch");
            let new_entities: Vec<_> = traces.into_iter().map(|t| cx.new(|_| t)).collect();
            parent.update(cx, |lp, cx| {
                lp.traces.extend(new_entities);
                cx.notify();
            });
        });

    crate::inspector::trace_picker::select_traces_wizard_rows(db.clone(), color_basis, on_select)
}

/// XY counterpart to [`build_trace_add_wizard`]. Drills through the
/// two-step picker and appends the resulting trace to the parent
/// `XyLinePlot`.
pub(super) fn build_xy_trace_add_wizard(
    parent: gpui::AnyEntity,
    db: &Arc<DB>,
    _cx: &App,
) -> Vec<Box<dyn InspectorRow>> {
    use crate::views::xy_plot::XyLinePlot;
    let parent_for_basis = parent.clone();
    let color_basis: crate::inspector::trace_picker::ColorBasis = Arc::new(move |cx: &App| {
        parent_for_basis
            .clone()
            .downcast::<XyLinePlot>()
            .map(|p| p.read(cx).traces.len())
            .unwrap_or(0)
    });

    let on_select: crate::views::xy_plot::trace_picker::OnXyTraceSelected =
        Arc::new(move |trace, _w, cx| {
            let parent: Entity<XyLinePlot> =
                parent.clone().downcast().expect("parent type mismatch");
            let new_entity = cx.new(|_| trace);
            parent.update(cx, |lp, cx| {
                lp.traces.push(new_entity);
                cx.notify();
            });
        });

    crate::views::xy_plot::trace_picker::select_xy_trace_wizard_rows(
        db.clone(),
        color_basis,
        on_select,
    )
}

/// List-plot counterpart to [`build_trace_add_wizard`]. Drills through
/// the single-page picker and appends the resulting trace to the parent
/// `ListLinePlot`.
pub(super) fn build_list_trace_add_wizard(
    parent: gpui::AnyEntity,
    db: &Arc<DB>,
    _cx: &App,
) -> Vec<Box<dyn InspectorRow>> {
    use crate::views::list_plot::ListLinePlot;
    let parent_for_basis = parent.clone();
    let color_basis: crate::inspector::trace_picker::ColorBasis = Arc::new(move |cx: &App| {
        parent_for_basis
            .clone()
            .downcast::<ListLinePlot>()
            .map(|p| p.read(cx).traces.len())
            .unwrap_or(0)
    });

    let on_select: crate::views::list_plot::trace_picker::OnListTraceSelected =
        Arc::new(move |trace, _w, cx| {
            let parent: Entity<ListLinePlot> =
                parent.clone().downcast().expect("parent type mismatch");
            let new_entity = cx.new(|_| trace);
            parent.update(cx, |lp, cx| {
                lp.traces.push(new_entity);
                cx.notify();
            });
        });

    crate::views::list_plot::trace_picker::select_list_trace_wizard_rows(
        db.clone(),
        color_basis,
        on_select,
    )
}
