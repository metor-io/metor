//! How instrument views attach to a component.
//!
//! Plots own their sampling; the small single-value views — traffic lights,
//! meters, gauges, state chips — all need the same three things instead, and
//! that overlap is what lives here. A view binds to *one element of one
//! component* ([`ElementRef`]), drains it with a task that outlives nothing
//! but the view itself, and reads back whatever the control system declared
//! about it.
//!
//! Three details are easy to get wrong alone and are therefore centralized:
//!
//! - **Seeding.** A fresh WAL reader only sees samples committed from now on.
//!   Without an explicit read of the last committed value, a meter placed on a
//!   slow component sits blank until the next sample — so the spawn helpers
//!   paint the seed before entering their loop.
//! - **Late binding.** A view can be placed, or restored from a layout, before
//!   its producer registers. [`spawn_meta_resolver`] waits on the DB's vtable
//!   generation and re-reads metadata once the schema appears.
//! - **Limits.** Warn/critical thresholds are already declared by the control
//!   system's alarm definitions, so an instrument should never be configured
//!   with them by hand. [`limit_marks`] and [`alarm_tint`] read the same store
//!   the plots read.

use std::sync::Arc;

use gpui::{App, Context, Hsla, SharedString};
use metor_db::DB;
use metor_proto::types::{ComponentId, ElementValue, PrimType};
use smallvec::SmallVec;

use crate::{AsComponentView, ComponentStream, ComponentStreamBuilder};

/// The single number an instrument view displays: one element of one
/// component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ElementRef {
    pub component: ComponentId,
    pub element: usize,
}

impl ElementRef {
    pub fn new(component: ComponentId, element: usize) -> Self {
        Self { component, element }
    }
}

/// Per-component metadata instrument views read at construction and again
/// once the component registers.
pub(crate) struct ComponentMeta {
    pub name: SharedString,
    pub unit: SharedString,
    pub is_bool: bool,
    pub element_names: Vec<SharedString>,
}

/// Look up everything a view needs about a component in one DB pass.
///
/// Falls back to a debug-formatted id when the component has no metadata
/// yet, matching what the user sees in the picker.
pub(crate) fn component_meta(db: &DB, component_id: ComponentId) -> ComponentMeta {
    db.with_state(|state| {
        let meta = state.get_component_metadata(component_id);
        let name = meta
            .map(|m| SharedString::from(m.name.clone()))
            .unwrap_or_else(|| SharedString::from(format!("{:?}", component_id)));
        let unit = meta
            .and_then(|m| m.metadata.get("unit").cloned())
            .map(SharedString::from)
            .unwrap_or_default();
        let comp = state.get_component(component_id);
        let is_bool = comp
            .map(|c| c.schema.prim_type == PrimType::Bool)
            .unwrap_or(false);
        let element_names = comp
            .map(|c| {
                crate::inspector::trace_picker::element_names(c.schema.dim.as_slice())
                    .into_iter()
                    .map(SharedString::from)
                    .collect()
            })
            .unwrap_or_default();
        ComponentMeta {
            name,
            unit,
            is_bool,
            element_names,
        }
    })
}

/// Spawn a task that waits for `component_id` to register, then hands its
/// metadata to `apply` once.
///
/// A view placed (or restored) before its producer registers would otherwise
/// show the debug id forever and never learn its element names or unit.
pub(crate) fn spawn_meta_resolver<E, F>(
    db: Arc<DB>,
    component_id: ComponentId,
    cx: &mut Context<E>,
    apply: F,
) -> gpui::Task<()>
where
    E: 'static,
    F: Fn(&mut E, ComponentMeta, &mut Context<E>) + Send + 'static,
{
    cx.spawn(async move |this, cx| {
        loop {
            if db.with_state(|state| state.get_component(component_id).is_some()) {
                let meta = component_meta(&db, component_id);
                let _ = this.update(cx, |view, cx| apply(view, meta, cx));
                break;
            }
            db.vtable_gen.wait().await;
        }
    })
}

/// The last committed value of one element, or `None` when the component has
/// never produced a sample.
pub(crate) fn latest_scalar(db: &DB, at: ElementRef) -> Option<f64> {
    db.with_state(|state| {
        let component = state.get_component(at.component)?;
        let latest = component.time_series.latest()?;
        let (_size, view) = component.schema.parse_value(latest.data()).ok()?;
        view.iter().nth(at.element).map(|v| v.as_f64())
    })
}

/// Spawn a task that drains a stream and forwards one element's value to
/// `apply` on the parent entity.
///
/// Seeds from the last committed sample before the first live one arrives.
/// The task exits when the entity is dropped (the `update` returns `Err`).
pub(crate) fn spawn_scalar_stream<E, F>(
    db: Arc<DB>,
    source: impl ComponentStreamBuilder + Send + 'static,
    element: usize,
    cx: &mut Context<E>,
    apply: F,
) -> gpui::Task<()>
where
    E: 'static,
    F: Fn(&mut E, f64, &mut Context<E>) + Send + 'static,
{
    let component_id = source.component_id();
    cx.spawn(async move |this, cx| {
        let mut stream = source.into_stream(&db).await;
        if let Some(seed) = latest_scalar(&db, ElementRef::new(component_id, element)) {
            let _ = this.update(cx, |view, cx| apply(view, seed, cx));
        }
        loop {
            let value = {
                let view = stream.next().await;
                let cv = view.as_component_view();
                cv.iter().nth(element).map(|v| v.as_f64())
            };
            // The update runs even when the element is out of range, so a
            // mis-bound view still notices its entity going away.
            let result = this.update(cx, |view, cx| {
                if let Some(value) = value {
                    apply(view, value, cx);
                }
            });
            if result.is_err() {
                break;
            }
        }
    })
}

/// Spawn a task that drains a stream and forwards the boolean
/// [`any_on`] to `apply` on the parent entity.
///
/// The task exits when the entity is dropped (the `update` returns `Err`).
pub(crate) fn spawn_on_stream<E, F>(
    db: Arc<DB>,
    source: impl ComponentStreamBuilder + Send + 'static,
    cx: &mut Context<E>,
    apply: F,
) -> gpui::Task<()>
where
    E: 'static,
    F: Fn(&mut E, bool, &mut Context<E>) + Send + 'static,
{
    cx.spawn(async move |this, cx| {
        let mut stream = source.into_stream(&db).await;
        loop {
            let on = {
                let view = stream.next().await;
                let cv = view.as_component_view();
                any_on(cv.iter())
            };
            let result = this.update(cx, |target, cx| apply(target, on, cx));
            if result.is_err() {
                break;
            }
        }
    })
}

/// Returns `true` if any element of the iterator is "on".
///
/// `Bool` short-circuits to the element value; numeric elements are treated
/// as "on" when non-zero. Lets on/off views light up on numeric alarm or
/// status components without requiring callers to pre-classify the schema.
pub(crate) fn any_on(values: impl Iterator<Item = ElementValue>) -> bool {
    for v in values {
        if let ElementValue::Bool(b) = v {
            if b {
                return true;
            }
        } else if v.as_f64() != 0.0 {
            return true;
        }
    }
    false
}

/// Limit values declared for `at` by the control system's alarm definitions,
/// paired with the color of their severity.
///
/// Empty when the alarm store was never initialized (tests) or the element
/// has no declared limits — an instrument then draws its scale unannotated.
pub(crate) fn limit_marks(at: ElementRef, cx: &App) -> SmallVec<[(f64, Hsla); 4]> {
    let Some(store) = crate::alarms::try_global(cx) else {
        return SmallVec::new();
    };
    let theme = crate::theme::theme(cx);
    store
        .read(cx)
        .state()
        .limits_for(at.component, at.element)
        .into_iter()
        .map(|limit| {
            let idx = crate::alarms::severity_index(limit.severity);
            (limit.value, theme.alarm_color(idx))
        })
        .collect()
}

/// Wash color for an instrument whose element currently has an active alarm.
pub(crate) fn alarm_tint(at: ElementRef, cx: &App) -> Option<Hsla> {
    let store = crate::alarms::try_global(cx)?;
    let severity = store
        .read(cx)
        .state()
        .active_severity_for(at.component, at.element)?;
    Some(crate::theme::theme(cx).alarm_tint(crate::alarms::severity_index(severity)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_on_bool() {
        assert!(any_on([ElementValue::Bool(true)].into_iter()));
        assert!(!any_on([ElementValue::Bool(false)].into_iter()));
    }

    #[test]
    fn any_on_numeric() {
        assert!(any_on([ElementValue::I32(5)].into_iter()));
        assert!(!any_on([ElementValue::I32(0)].into_iter()));
        assert!(any_on(
            [ElementValue::F32(0.0), ElementValue::F32(1.5)].into_iter()
        ));
        assert!(!any_on([ElementValue::F32(0.0)].into_iter()));
    }

    #[test]
    fn any_on_empty() {
        assert!(!any_on(Vec::<ElementValue>::new().into_iter()));
    }
}
