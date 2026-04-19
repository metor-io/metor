use std::sync::Arc;

use gpui::{Context, IntoElement, SharedString, Window, div, prelude::*};
use metor_db::DB;

use super::format::format_value;
use crate::theme::theme;
use crate::{AsComponentView, ComponentStream, ComponentStreamBuilder};

/// Displays a single component's latest value as text, updating reactively.
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
        let task = cx.spawn(async move |this, cx| {
            let component_id = source.component_id();
            let mut stream = source.into_stream(&db).await;
            loop {
                let s = {
                    let view = stream.next().await;
                    format_value(view.as_component_view(), &db, component_id)
                };
                let result = this.update(cx, |this, cx| {
                    this.value = Some(SharedString::from(&s));
                    cx.notify();
                });
                if result.is_err() {
                    break;
                }
            }
        });
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
