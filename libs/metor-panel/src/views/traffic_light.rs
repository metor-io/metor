use std::sync::Arc;

use gpui::{
    AnyView, App, Context, FocusHandle, Focusable, Hsla, IntoElement, MouseButton, Pixels,
    SharedString, Window, div, prelude::*, px,
};
use metor_db::DB;
use metor_proto::types::{ComponentId, ElementValue, PrimType};

use crate::theme::{Theme, theme};
use crate::{AsComponentView, ComponentStream, ComponentStreamBuilder};

/// Single-component on/off indicator.
///
/// Mirrors [`Monitor`](super::Monitor)'s streaming pattern: the constructor
/// spawns one task that consumes the WAL stream and stores the latest sample;
/// rendering is a single coloured swatch. See [`coerce_on`] for how non-bool
/// components are interpreted as on/off.
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
}

impl TrafficLight {
    pub fn new(
        db: Arc<DB>,
        source: impl ComponentStreamBuilder + Send + 'static,
        cx: &mut Context<Self>,
    ) -> Self {
        let component_id = source.component_id();
        let default_color = theme(cx).line_colors[2];

        let (name, is_bool, element_names) = db.with_state(|state| {
            let meta = state.get_component_metadata(component_id);
            let name = meta
                .map(|m| m.name.clone())
                .unwrap_or_else(|| format!("{:?}", component_id));
            let is_bool = state
                .get_component(component_id)
                .map(|c| c.schema.prim_type == PrimType::Bool)
                .unwrap_or(false);
            let element_names: Vec<SharedString> = state
                .get_component(component_id)
                .map(|c| {
                    crate::inspector::trace_picker::element_names(c.schema.dim.as_slice())
                        .into_iter()
                        .map(SharedString::from)
                        .collect()
                })
                .unwrap_or_default();
            (name, is_bool, element_names)
        });

        let task = cx.spawn({
            let db = db.clone();
            async move |this, cx| {
                let mut stream = source.into_stream(&db).await;
                loop {
                    let on = {
                        let view = stream.next().await;
                        let cv = view.as_component_view();
                        coerce_on(cv.iter())
                    };
                    let result = this.update(cx, |this, cx| {
                        this.latest_on = Some(on);
                        cx.notify();
                    });
                    if result.is_err() {
                        break;
                    }
                }
            }
        });

        Self {
            name: SharedString::from(name),
            color: default_color,
            db,
            component_id,
            element_names,
            is_bool,
            latest_on: None,
            focus: cx.focus_handle(),
            _task: task,
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

/// Render a coloured square reflecting the on/off state.
///
/// `Some(true)` → solid `color`. `Some(false)` → same hue at low alpha so the
/// off state still reads as the same indicator, just dimmed. `None` (no sample
/// yet) → neutral background.
pub(crate) fn traffic_light_swatch(
    value: Option<bool>,
    color: Hsla,
    size: Pixels,
    theme: &Theme,
) -> impl IntoElement {
    let bg = match value {
        Some(true) => color,
        Some(false) => Hsla { a: 0.15, ..color },
        None => theme.bg_secondary,
    };
    div().w(size).h(size).rounded(px(3.0)).bg(bg)
}

/// Returns true iff any element of the iterator is non-zero.
///
/// Lets a `TrafficLight` light up on numeric alarm/status components without
/// requiring callers to pre-classify the schema. `Bool` short-circuits to the
/// element value directly.
pub(crate) fn coerce_on(values: impl Iterator<Item = ElementValue>) -> bool {
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

/// Tiny renderable used as a tooltip body.
///
/// gpui's `tooltip` builder must return an `AnyView`, but the project doesn't
/// depend on zed's `ui::Tooltip`. This 30-line helper fills that gap.
pub(crate) struct TooltipText {
    text: SharedString,
}

impl TooltipText {
    pub fn build(text: SharedString, cx: &mut App) -> AnyView {
        cx.new(|_cx| Self { text }).into()
    }
}

impl Render for TooltipText {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        div()
            .px(px(8.0))
            .py(px(4.0))
            .rounded(px(4.0))
            .bg(theme.bg_elevated)
            .border_1()
            .border_color(theme.border_primary)
            .text_size(px(11.0))
            .text_color(theme.text_primary)
            .child(self.text.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coerce_on_bool() {
        assert!(coerce_on([ElementValue::Bool(true)].into_iter()));
        assert!(!coerce_on([ElementValue::Bool(false)].into_iter()));
    }

    #[test]
    fn coerce_on_numeric() {
        assert!(coerce_on([ElementValue::I32(5)].into_iter()));
        assert!(!coerce_on([ElementValue::I32(0)].into_iter()));
        assert!(coerce_on(
            [ElementValue::F32(0.0), ElementValue::F32(1.5)].into_iter()
        ));
        assert!(!coerce_on([ElementValue::F32(0.0)].into_iter()));
    }

    #[test]
    fn coerce_on_empty() {
        assert!(!coerce_on(Vec::<ElementValue>::new().into_iter()));
    }
}
