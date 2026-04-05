use std::sync::Arc;

use gpui::{Context, IntoElement, SharedString, Window, div, prelude::*};
use metor_db::DB;
use smallvec::SmallVec;
use std::fmt::Write;

use crate::inspectable::{FieldId, Inspectable, InspectionField, InspectionValue};
use crate::theme::DARK;
use crate::{AsComponentView, ComponentStream, ComponentStreamBuilder};

pub mod component_table;
pub mod table;
pub mod time_series;

/// Element indices within a component (e.g. x=0, y=1, z=2 for a Vec3).
pub type ElementIndexes = SmallVec<[usize; 8]>;

pub use component_table::{ComponentTable, new_component_table};
pub use table::{Column, ColumnSort, Table, TableDelegate};
pub use time_series::{TimeSeriesPlot, Trace};

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
                    let _ = write!(s, "{:5}", view.as_component_view());
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
            .text_color(DARK.text_primary)
            .bg(DARK.bg_primary)
            .text_size(gpui::px(24.0))
            .size_full()
            .child(
                self.value
                    .clone()
                    .unwrap_or_else(|| SharedString::new_static("")),
            )
    }
}

impl Inspectable for ComponentText {
    fn fields(&self) -> Vec<InspectionField> {
        vec![]
    }

    fn set_field(&mut self, _field_id: FieldId, _value: InspectionValue, _cx: &mut Context<Self>) {}
}
