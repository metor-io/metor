use std::sync::Arc;

use gpui::{Context, IntoElement, SharedString, Window, div, prelude::*};
use metor_db::DB;
use serde::{Deserialize, Serialize};

use super::binding::{StreamUpdate, spawn_seeded_stream};
use super::format::ValueFormatter;
use crate::data_binding::Binding;
use crate::theme::{theme, ui_px};

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ComponentTextConfig {
    pub component: String,
}

/// Large-text readout for a single component.
///
/// Subscribes to the component's WAL and replaces the displayed string on
/// each tick; the task ends when the entity is dropped.
#[derive(facet::Facet)]
pub struct ComponentText {
    pub source: Binding,
    #[facet(opaque)]
    db: Arc<DB>,
    #[facet(opaque)]
    _binding_changes: gpui::Task<()>,
    #[facet(skip)]
    bound: metor_proto::types::ComponentId,
    #[facet(skip)]
    value: Option<SharedString>,
    #[facet(opaque)]
    _task: gpui::Task<()>,
}

impl ComponentText {
    pub fn from_config(cfg: ComponentTextConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let binding = Binding::from_text(&cfg.component, &db, cx);
        Self::bind(db, binding, cx)
    }

    pub fn to_config(&self) -> ComponentTextConfig {
        ComponentTextConfig {
            component: self.source.text(&self.db),
        }
    }

    /// Stream whatever `binding` currently resolves to. A rebind replaces the
    /// whole view, which is what drops the previous stream task.
    fn bind(db: Arc<DB>, binding: Binding, cx: &mut Context<Self>) -> Self {
        let component_id = binding.id();
        let task = spawn_seeded_stream(
            db.clone(),
            component_id,
            cx,
            |db, component_id| {
                let formatter = ValueFormatter::resolve(db, component_id);
                ((), move |view| Some(formatter.format(view)))
            },
            |this, update, cx| {
                match update {
                    StreamUpdate::Value(value) => this.value = Some(SharedString::from(value)),
                    StreamUpdate::Unavailable => this.value = None,
                    _ => {}
                }
                cx.notify();
            },
        );
        Self {
            bound: component_id,
            source: binding,
            _binding_changes: crate::data_binding::watch_registrations(db.clone(), cx),
            db,
            value: None,
            _task: task,
        }
    }
}

impl Render for ComponentText {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.source.resolve(&self.db, cx);
        if self.bound != self.source.id() {
            *self = Self::bind(self.db.clone(), self.source.clone(), cx);
        }
        let theme = theme(cx);
        div()
            .text_color(theme.text_primary)
            .text_size(ui_px(24.0))
            .size_full()
            .child(
                self.value
                    .clone()
                    .unwrap_or_else(|| SharedString::new_static("—")),
            )
    }
}
