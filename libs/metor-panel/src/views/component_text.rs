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
pub struct ComponentText {
    value: Option<SharedString>,
    _task: gpui::Task<()>,
}

impl ComponentText {
    pub fn new(
        db: Arc<DB>,
        source: impl ComponentStreamBuilder + Send + 'static,
        cx: &mut Context<Self>,
    ) -> Self {
        let task = spawn_seeded_stream(
            db,
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
            value: None,
            _task: task,
        }
    }
}

impl Render for ComponentText {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        div()
            .text_color(theme.text_primary)
            .bg(theme.bg_primary)
            .text_size(gpui::px(24.0))
            .size_full()
            .child(
                self.value
                    .clone()
                    .unwrap_or_else(|| SharedString::new_static("")),
            )
    }
}
