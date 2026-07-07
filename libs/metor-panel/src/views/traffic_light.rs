use std::sync::Arc;

use gpui::{
    App, Context, FocusHandle, Focusable, Hsla, IntoElement, MouseButton, Pixels, SharedString,
    Window, div, prelude::*, px,
};
use metor_db::DB;
use metor_proto::types::{ComponentId, ElementValue, PrimType};

use crate::theme::{Theme, theme};
use crate::{AsComponentView, ComponentStream, ComponentStreamBuilder};

/// Single-component on/off indicator.
///
/// Mirrors [`Monitor`](super::Monitor)'s streaming pattern: the constructor
/// spawns one task that consumes the WAL stream and stores the latest sample;
/// rendering is a single coloured swatch. See [`any_on`] for how non-bool
/// components are interpreted as on/off. Metadata is bound lazily: producers
/// may register the component after the light is placed, so a resolver task
/// refreshes name/`is_bool`/element names once the schema shows up in the DB.
#[derive(facet::Facet)]
pub struct TrafficLight {
    #[facet(skip)]
    name: SharedString,
    pub color: Hsla,
    #[facet(opaque)]
    db: Arc<DB>,
    #[facet(opaque)]
    component_id: ComponentId,
    #[facet(opaque)]
    element_names: Vec<SharedString>,
    #[facet(skip)]
    is_bool: bool,
    #[facet(skip)]
    latest_on: Option<bool>,
    #[facet(opaque)]
    focus: FocusHandle,
    #[facet(opaque)]
    _task: gpui::Task<()>,
    #[facet(opaque)]
    _resolver_task: gpui::Task<()>,
}

impl TrafficLight {
    pub fn new(
        db: Arc<DB>,
        source: impl ComponentStreamBuilder + Send + 'static,
        cx: &mut Context<Self>,
    ) -> Self {
        let component_id = source.component_id();
        let default_color = theme(cx).control_active;

        let meta = component_meta(&db, component_id);
        let task = spawn_on_stream(db.clone(), source, cx, |this, on, cx| {
            this.latest_on = Some(on);
            cx.notify();
        });

        // Await component registration, then re-read metadata: a light
        // placed (or restored) before the producer registers would
        // otherwise show the debug id forever and never enable
        // click-to-toggle.
        let resolver_task = cx.spawn({
            let db = db.clone();
            async move |this, cx| {
                loop {
                    if db.with_state(|state| state.get_component(component_id).is_some()) {
                        let meta = component_meta(&db, component_id);
                        let _ = this.update(cx, |light, cx| {
                            light.name = meta.name;
                            light.is_bool = meta.is_bool;
                            light.element_names = meta.element_names;
                            cx.notify();
                        });
                        break;
                    }
                    db.vtable_gen.wait().await;
                }
            }
        });

        Self {
            name: meta.name,
            color: default_color,
            db,
            component_id,
            element_names: meta.element_names,
            is_bool: meta.is_bool,
            latest_on: None,
            focus: cx.focus_handle(),
            _task: task,
            _resolver_task: resolver_task,
        }
    }

    pub fn component_id(&self) -> ComponentId {
        self.component_id
    }

    pub fn name(&self) -> &SharedString {
        &self.name
    }

    pub fn color(&self) -> Hsla {
        self.color
    }

    pub fn set_color(&mut self, color: Hsla, cx: &mut Context<Self>) {
        if self.color == color {
            return;
        }
        self.color = color;
        cx.notify();
    }

    fn toggle(&mut self, cx: &mut Context<Self>) {
        if !self.is_bool {
            return;
        }
        let next = !self.latest_on.unwrap_or(false);
        crate::inspector::edits::upsert_element_value(
            &self.db,
            self.component_id,
            self.name.clone(),
            self.element_names.clone(),
            0,
            ElementValue::Bool(next),
            cx,
        );
        cx.notify();
    }
}

impl Focusable for TrafficLight {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for TrafficLight {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let value = self.latest_on;
        let color = self.color;
        let is_bool = self.is_bool;

        let mut tile = div()
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme.bg_primary)
            .p(px(8.0))
            .child(traffic_light_swatch(value, color, px(14.0), &theme));

        if is_bool {
            tile = tile.cursor_pointer().on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _window, cx| {
                    this.toggle(cx);
                }),
            );
        }

        tile
    }
}

/// Alpha factor applied to the swatch color when the indicator is "off". Same
/// hue as "on" so an off cell still reads as the same indicator, just dimmed.
const OFF_ALPHA: f32 = 0.15;

/// Render a coloured square reflecting the on/off state.
///
/// `Some(true)` → solid `color`. `Some(false)` → same hue at `OFF_ALPHA`.
/// `None` (no sample yet) → neutral background.
pub(crate) fn traffic_light_swatch(
    value: Option<bool>,
    color: Hsla,
    size: Pixels,
    theme: &Theme,
) -> impl IntoElement {
    let bg = match value {
        Some(true) => color,
        Some(false) => Hsla {
            a: OFF_ALPHA,
            ..color
        },
        None => theme.bg_secondary,
    };
    div().w(size).h(size).rounded(px(3.0)).bg(bg)
}

/// Returns `true` if any element of the iterator is "on".
///
/// `Bool` short-circuits to the element value; numeric elements are treated
/// as "on" when non-zero. Lets a `TrafficLight` light up on numeric
/// alarm/status components without requiring callers to pre-classify the
/// schema.
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

/// Bundle of per-component metadata that traffic-light views read out of the
/// DB at construction and again once the component registers.
pub(crate) struct ComponentMeta {
    pub name: SharedString,
    pub is_bool: bool,
    pub element_names: Vec<SharedString>,
}

/// Look up `(name, is_bool, element_names)` for a component in one DB pass.
///
/// Falls back to a debug-formatted id when the component has no metadata yet,
/// matching what the user sees in the picker.
pub(crate) fn component_meta(db: &DB, component_id: ComponentId) -> ComponentMeta {
    db.with_state(|state| {
        let name = state
            .get_component_metadata(component_id)
            .map(|m| SharedString::from(m.name.clone()))
            .unwrap_or_else(|| SharedString::from(format!("{:?}", component_id)));
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
            is_bool,
            element_names,
        }
    })
}

/// Spawn a task that drains a WAL stream and forwards the boolean
/// `any_on(...)` to `apply` on the parent entity.
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
