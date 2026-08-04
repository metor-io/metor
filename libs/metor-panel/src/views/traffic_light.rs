use std::sync::Arc;

use gpui::{
    App, Context, FocusHandle, Focusable, Hsla, IntoElement, MouseButton, Pixels, SharedString,
    Window, div, prelude::*, px,
};
use metor_db::DB;
use metor_proto::types::{ComponentId, ElementValue};
use serde::{Deserialize, Serialize};

use super::binding::{component_meta, spawn_meta_resolver, spawn_on_stream};
use crate::ComponentStreamBuilder;
use crate::theme::{Theme, theme};

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct TrafficLightConfig {
    pub component: String,
    pub color: Option<Hsla>,
}

/// Single-component on/off indicator.
///
/// The constructor spawns one task that consumes the WAL stream and stores
/// the latest sample; rendering is a single coloured swatch. Streaming,
/// metadata, and late binding all come from the `binding` module, whose
/// `any_on` decides how a non-bool component reads as on or off.
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

        let resolver_task = spawn_meta_resolver(db.clone(), component_id, cx, |light, meta, cx| {
            light.name = meta.name;
            light.is_bool = meta.is_bool;
            light.element_names = meta.element_names;
            cx.notify();
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
