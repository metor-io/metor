use std::sync::Arc;

use gpui::{Context, IntoElement, SharedString, Window, div, prelude::*, rgb};
use metor_db::DB;
use std::fmt::Write;

use crate::{AsComponentView, ComponentStream, ComponentStreamBuilder};

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
            let mut stream = source.into_stream(&db).await;
            let mut s = String::new();
            loop {
                {
                    let view = stream.next().await;
                    s.clear();
                    let _ = write!(s, "{}", view.as_component_view());
                }
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
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .text_color(rgb(0xFFFFFF))
            .text_size(gpui::px(24.0))
            .size_full()
            .child(
                self.value
                    .take()
                    .unwrap_or_else(|| SharedString::new_static("")),
            )
    }
}
