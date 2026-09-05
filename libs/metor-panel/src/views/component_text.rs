use std::sync::Arc;

use gpui::{Context, IntoElement, SharedString, Window, div, prelude::*};
use metor_db::DB;
use serde::{Deserialize, Serialize};

use super::binding::{StreamUpdate, spawn_seeded_stream};
use super::format::ValueFormatter;
use crate::ComponentStreamBuilder;
use crate::theme::theme;

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
    pub source: crate::data_binding::Binding,
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
    pub fn binding_text(&self) -> String {
        self.source.text(&self.db)
    }

    pub fn new(
        db: Arc<DB>,
        source: impl ComponentStreamBuilder + Send + 'static,
        cx: &mut Context<Self>,
    ) -> Self {
        let binding =
            crate::data_binding::Binding::from_legacy(source.component_id(), None, &db, cx);
        let task = spawn_seeded_stream(
            db.clone(),
            source,
            cx,
            |db, component_id| {
                let formatter = ValueFormatter::resolve(db, component_id);
                ((), move |view| Some(formatter.format(view)))
            },
            |this, update, cx| {
                if let StreamUpdate::Value(value) = update {
                    this.value = Some(SharedString::from(value));
                    cx.notify();
                }
            },
        );
        Self {
            bound: binding.id(),
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
            let source = self.source.clone();
            *self = Self::new(self.db.clone(), source.id(), cx);
            self.source = source;
        }
        let theme = theme(cx);
        div()
            .text_color(theme.text_primary)
            .text_size(gpui::px(24.0))
            .size_full()
            .child(
                self.value
                    .clone()
                    .unwrap_or_else(|| SharedString::new_static("")),
            )
    }
}
