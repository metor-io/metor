use std::sync::Arc;

use gpui::{Context, IntoElement, SharedString, Window, canvas, div, prelude::*, px};
use metor_db::{Component, DB};
use metor_proto::types::ComponentView;
use smallvec::SmallVec;

use super::time_series::{PlotBounds, expand_y_bounds, paint_data_line};
use super::ElementIndexes;
use crate::inspectable::{FieldId, Inspectable, InspectionField, InspectionValue};
use crate::theme::theme;
use crate::{AsComponentView, ComponentStream, ComponentStreamBuilder};

/// A single element's formatted value and label.
#[derive(Clone)]
struct ElementDisplay {
    label: Option<SharedString>,
    value: SharedString,
}

/// A monitor displays a component's current value with its name and an optional sparkline.
///
/// Designed for dashboard use where a clean, at-a-glance readout is needed — similar
/// to the value cards in Revel's turbine dashboards.
pub struct Monitor {
    name: SharedString,
    unit: SharedString,
    elements: Vec<ElementDisplay>,
    show_sparkline: bool,
    component: Option<Component>,
    indexes: ElementIndexes,
    y_bounds: Option<(f64, f64)>,
    last_scan_ts: Option<metor_proto::types::Timestamp>,
    _task: gpui::Task<()>,
}

impl Monitor {
    pub fn new(
        db: Arc<DB>,
        source: impl ComponentStreamBuilder + Send + 'static,
        cx: &mut Context<Self>,
    ) -> Self {
        let component_id = source.component_id();

        let (name, unit, element_names) = db.with_state(|state| {
            let meta = state.get_component_metadata(component_id);
            let name = meta
                .map(|m| m.name.clone())
                .unwrap_or_else(|| format!("{:?}", component_id));
            let unit = meta
                .and_then(|m| m.metadata.get("unit").cloned())
                .unwrap_or_default();
            let element_names: Vec<String> = meta
                .map(|m| {
                    let raw = m.element_names();
                    if raw.is_empty() {
                        vec![]
                    } else {
                        raw.split(',').map(|s| s.trim().to_string()).collect()
                    }
                })
                .unwrap_or_default();
            (name, unit, element_names)
        });

        let component = db.with_state(|state| state.get_component(component_id).cloned());
        let indexes: ElementIndexes = component
            .as_ref()
            .map(|c| {
                let n: usize = c.schema.dim.iter().product();
                (0..n.max(1)).collect()
            })
            .unwrap_or_else(|| SmallVec::from_elem(0, 1));

        let elem_names = element_names.clone();
        let task = cx.spawn(async move |this, cx| {
            let mut stream = source.into_stream(&db).await;

            let comp = db.with_state(|state| state.get_component(component_id).cloned());
            let idx: ElementIndexes = comp
                .as_ref()
                .map(|c| {
                    let n: usize = c.schema.dim.iter().product();
                    (0..n.max(1)).collect()
                })
                .unwrap_or_else(|| SmallVec::from_elem(0, 1));

            let _ = this.update(cx, |this, _cx| {
                this.component = comp;
                this.indexes = idx;
            });

            let enum_variants: Option<Vec<String>> = db.with_state(|state| {
                state
                    .get_component_metadata(component_id)
                    .and_then(|m| {
                        m.enum_variants().map(|v| v.map(|s| s.to_string()).collect())
                    })
            });

            loop {
                let elements = {
                    let view = stream.next().await;
                    let cv = view.as_component_view();
                    format_elements(&cv, &elem_names, enum_variants.as_deref())
                };
                let result = this.update(cx, |this, cx| {
                    this.elements = elements;
                    if let Some(ref comp) = this.component {
                        this.y_bounds = expand_y_bounds(
                            comp,
                            &this.indexes,
                            this.y_bounds,
                            this.last_scan_ts,
                        );
                        this.last_scan_ts = comp.time_series.latest().map(|n| n.timestamp());
                    }
                    cx.notify();
                });
                if result.is_err() {
                    break;
                }
            }
        });

        Self {
            name: SharedString::from(name),
            unit: SharedString::from(unit),
            elements: Vec::new(),
            show_sparkline: true,
            component,
            indexes,
            y_bounds: None,
            last_scan_ts: None,
            _task: task,
        }
    }

    fn sparkline_bounds(&self) -> Option<PlotBounds> {
        let comp = self.component.as_ref()?;
        let ts = &comp.time_series;
        let start = ts.start_timestamp()?.0 as f64;
        let end = ts.latest()?.timestamp().0 as f64;
        if start == end {
            return None;
        }
        let (min_y, max_y) = self.y_bounds?;
        Some(PlotBounds::new(start, min_y, end, max_y).normalize())
    }
}

/// Format component elements into display-friendly strings.
fn format_elements(
    view: &ComponentView<'_>,
    element_names: &[String],
    enum_variants: Option<&[String]>,
) -> Vec<ElementDisplay> {
    // Check if this is a string component
    if let ComponentView::U8(array) = view {
        let buf = array.buf();
        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        if let Ok(s) = std::str::from_utf8(&buf[..len]) {
            return vec![ElementDisplay {
                label: None,
                value: SharedString::from(s.to_string()),
            }];
        }
    }

    // Check if this is an enum
    if let Some(variants) = enum_variants {
        let idx = view.to_f64() as usize;
        if let Some(name) = variants.get(idx) {
            return vec![ElementDisplay {
                label: None,
                value: SharedString::from(name.to_string()),
            }];
        }
    }

    let values: Vec<_> = view.iter().collect();

    // Single scalar — no label needed
    if values.len() == 1 {
        return vec![ElementDisplay {
            label: None,
            value: SharedString::from(format_number(values[0].as_f64())),
        }];
    }

    // Multi-element — show each with its label
    values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let label = element_names
                .get(i)
                .map(|n| SharedString::from(n.clone()));
            ElementDisplay {
                label,
                value: SharedString::from(format_number(v.as_f64())),
            }
        })
        .collect()
}

/// Format a number concisely: drop unnecessary trailing zeros.
fn format_number(v: f64) -> String {
    if v.abs() >= 1000.0 {
        format!("{:.0}", v)
    } else if v.abs() >= 100.0 {
        format!("{:.1}", v)
    } else if v.abs() >= 1.0 {
        format!("{:.2}", v)
    } else if v == 0.0 {
        "0".to_string()
    } else {
        format!("{:.4}", v)
    }
}

impl Render for Monitor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);

        let mut root = div()
            .size_full()
            .flex()
            .flex_col()
            .justify_end()
            .bg(theme.bg_primary)
            .overflow_hidden();

        // Sparkline — fills available space above the text content
        if self.show_sparkline {
            if let Some(ref comp) = self.component {
                let component = comp.clone();
                let plot_bounds = self.sparkline_bounds();
                let indexes = self.indexes.clone();
                let line_colors = theme.line_colors;

                root = root.child(
                    canvas(
                        move |bounds, _window, _cx| (bounds, component, plot_bounds, indexes),
                        move |_, (bounds, component, plot_bounds, indexes), window, _cx| {
                            if let Some(view) = plot_bounds {
                                for (i, &idx) in indexes.iter().enumerate() {
                                    let color = line_colors[i % line_colors.len()];
                                    paint_data_line(
                                        bounds, &component, &view, color, px(1.5), idx, window,
                                    );
                                }
                            }
                        },
                    )
                    .w_full()
                    .pt(px(4.0))
                    .flex_1()
                    .min_h(px(16.0)),
                );
            }
        }

        // Values
        let is_multi = self.elements.len() > 1;
        if is_multi {
            // Multi-element: compact row of labeled values
            let mut row = div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap(px(6.0))
                .px(px(6.0))
                .pt(px(3.0));

            for elem in &self.elements {
                let mut item = div().flex().flex_row().items_baseline().gap(px(2.0));

                if let Some(ref label) = elem.label {
                    item = item.child(
                        div()
                            .text_size(px(9.0))
                            .text_color(theme.text_tertiary)
                            .child(label.clone()),
                    );
                }

                item = item.child(
                    div()
                        .text_size(px(13.0))
                        .text_color(theme.text_primary)
                        .child(elem.value.clone()),
                );

                if !self.unit.is_empty() {
                    item = item.child(
                        div()
                            .text_size(px(9.0))
                            .text_color(theme.text_secondary)
                            .child(self.unit.clone()),
                    );
                }

                row = row.child(item);
            }

            root = root.child(row);
        } else if let Some(elem) = self.elements.first() {
            // Single value: larger display
            let mut value_row = div()
                .flex()
                .flex_row()
                .items_baseline()
                .gap(px(3.0))
                .px(px(6.0))
                .pt(px(3.0));

            value_row = value_row.child(
                div()
                    .text_size(px(16.0))
                    .text_color(theme.text_primary)
                    .child(elem.value.clone()),
            );

            if !self.unit.is_empty() {
                value_row = value_row.child(
                    div()
                        .text_size(px(10.0))
                        .text_color(theme.text_secondary)
                        .child(self.unit.clone()),
                );
            }

            root = root.child(value_row);
        } else {
            root = root.child(
                div()
                    .px(px(6.0))
                    .pt(px(3.0))
                    .text_size(px(14.0))
                    .text_color(theme.text_tertiary)
                    .child("—"),
            );
        }

        // Component name
        root = root.child(
            div()
                .px(px(6.0))
                .pb(px(4.0))
                .text_size(px(11.0))
                .text_color(theme.text_primary)
                .child(self.name.clone()),
        );

        root
    }
}

impl Inspectable for Monitor {
    fn fields(&self) -> Vec<InspectionField> {
        vec![
            InspectionField {
                label: "Unit".into(),
                field_id: FieldId(0),
                value: InspectionValue::String(self.unit.to_string()),
            },
            InspectionField {
                label: "Sparkline".into(),
                field_id: FieldId(1),
                value: InspectionValue::Bool(self.show_sparkline),
            },
        ]
    }

    fn set_field(&mut self, field_id: FieldId, value: InspectionValue, cx: &mut Context<Self>) {
        match field_id {
            FieldId(0) => {
                if let InspectionValue::String(s) = value {
                    self.unit = SharedString::from(s);
                    cx.notify();
                }
            }
            FieldId(1) => {
                if let InspectionValue::Bool(b) = value {
                    self.show_sparkline = b;
                    cx.notify();
                }
            }
            _ => {}
        }
    }
}
