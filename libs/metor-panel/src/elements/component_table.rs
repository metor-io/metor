use std::sync::Arc;

use super::table::{Column, ColumnSort, Table, TableDelegate};
use super::time_series::{LinePlot, Trace};
use crate::pending_edits::{EditRequest, pending_edits, pending_edits_mut};
use crate::theme::{Theme, theme};
use crate::{ComponentStream, WalComponentStream};
use gpui::{
    AnyElement, App, AppContext, AsyncApp, Context, Entity, InteractiveElement, IntoElement,
    Pixels, SharedString, Stateful, Window, div, prelude::*, px,
};
use metor_db::{Component, DB};
use metor_proto::types::ComponentView;

struct ComponentRow {
    db: Arc<DB>,
    name: SharedString,
    component: Component,
    element_names: Vec<SharedString>,
    sparkline: Entity<LinePlot>,
    _task: gpui::Task<()>,
}

impl ComponentRow {
    pub fn new(
        db: Arc<DB>,
        name: impl Into<SharedString>,
        component: Component,
        element_names: Vec<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        let line_colors = theme(cx).line_colors;
        let num_elements: usize = component.schema.dim.iter().product::<usize>().max(1);
        let traces: Vec<Trace> = (0..num_elements)
            .map(|i| {
                let mut t = Trace::new(component.component_id, i, line_colors[i % line_colors.len()]);
                t.stroke_width = 1.0;
                t
            })
            .collect();

        let sparkline = cx.new(|cx| LinePlot::new(db.clone(), cx));
        sparkline.update(cx, |sp, cx| sp.bind_traces(traces, cx));

        // Tiny notify task: wake the row's render on every new sample so
        // the value cell re-renders. Y-bounds tracking lives inside the
        // LinePlot's own per-trace tracker task.
        let mut stream = WalComponentStream::new(&component);
        let task = cx.spawn(async move |this, cx| {
            loop {
                stream.next().await;
                let result = this.update(cx, |_this, cx| cx.notify());
                if result.is_err() {
                    break;
                }
            }
        });

        Self {
            db,
            name: name.into(),
            component,
            element_names,
            sparkline,
            _task: task,
        }
    }

    fn current_value_string(&self) -> String {
        let Some(latest) = self.component.time_series.latest() else {
            return String::new();
        };
        let buf = latest.data();
        let Ok((_size, view)) = self.component.schema.parse_value(buf) else {
            return String::new();
        };
        super::format_value(view, &self.db, self.component.component_id)
    }
}

/// Table delegate that displays all components in the database with name, value, and sparkline columns.
pub struct ComponentTableDelegate {
    rows: Vec<Entity<ComponentRow>>,
    _task: gpui::Task<()>,
}

impl ComponentTableDelegate {
    fn spawn_watcher(db: Arc<DB>, cx: &mut Context<Table<Self>>) -> gpui::Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let rows = Self::build_rows(&db, cx);
                let result = this.update(cx, |this, cx| {
                    this.delegate_mut().rows = rows;
                    cx.notify();
                });
                if result.is_err() {
                    break;
                }
                db.vtable_gen.wait().await;
            }
        })
    }

    fn build_rows(db: &Arc<DB>, cx: &mut AsyncApp) -> Vec<Entity<ComponentRow>> {
        let prepared: Vec<(SharedString, Component, Vec<SharedString>)> = db.with_state(|state| {
            state
                .component_metadata_iter()
                .filter_map(|(id, meta)| {
                    let name = SharedString::from(meta.name.clone());
                    let component = state.get_component(*id)?.clone();
                    let element_names: Vec<SharedString> =
                        crate::trace_picker::element_names(component.schema.dim.as_slice())
                            .into_iter()
                            .map(SharedString::from)
                            .collect();
                    Some((name, component, element_names))
                })
                .collect()
        });

        prepared
            .into_iter()
            .filter_map(|(name, component, element_names)| {
                let db = db.clone();
                cx.new(|cx| ComponentRow::new(db, name, component, element_names, cx))
                    .ok()
            })
            .collect()
    }
}

impl TableDelegate for ComponentTableDelegate {
    fn columns(&self) -> Vec<Column> {
        vec![
            Column::new("Name", 280.0).sortable(),
            Column::new("Value", 320.0).sortable(),
            Column::new("Sparkline", 200.0).flex().resizable(false),
        ]
    }

    fn rows_count(&self) -> usize {
        self.rows.len()
    }

    fn row_height(&self) -> Pixels {
        px(50.0)
    }

    fn render_cell(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<Table<Self>>,
    ) -> AnyElement {
        let theme = theme(cx);
        let row = &self.rows[row_ix];
        let row_ref = row.read(cx);
        match col_ix {
            0 => div()
                .px(px(12.0))
                .text_size(px(13.0))
                .text_color(theme.text_primary)
                .child(row_ref.name.clone())
                .into_any_element(),
            1 => render_value_cell(row_ref, &theme, cx).into_any_element(),
            2 => {
                let row_height = self.row_height();
                div()
                    .w_full()
                    .h(row_height - px(8.0))
                    .child(row_ref.sparkline.clone())
                    .into_any_element()
            }
            _ => div().into_any_element(),
        }
    }

    fn sort_column(&mut self, col_ix: usize, sort: ColumnSort, cx: &App) {
        match (col_ix, sort) {
            (0, ColumnSort::Ascending) => {
                self.rows.sort_by(|a, b| a.read(cx).name.cmp(&b.read(cx).name));
            }
            (0, ColumnSort::Descending) => {
                self.rows.sort_by(|a, b| b.read(cx).name.cmp(&a.read(cx).name));
            }
            (1, ColumnSort::Ascending) => {
                self.rows.sort_by(|a, b| {
                    a.read(cx)
                        .current_value_string()
                        .cmp(&b.read(cx).current_value_string())
                });
            }
            (1, ColumnSort::Descending) => {
                self.rows.sort_by(|a, b| {
                    b.read(cx)
                        .current_value_string()
                        .cmp(&a.read(cx).current_value_string())
                });
            }
            _ => {}
        }
    }
}

/// A table showing all database components with sortable columns and sparklines.
pub type ComponentTable = Table<ComponentTableDelegate>;

pub fn new_component_table(db: Arc<DB>, cx: &mut Context<ComponentTable>) -> ComponentTable {
    let task = ComponentTableDelegate::spawn_watcher(db.clone(), cx);
    let delegate = ComponentTableDelegate {
        rows: Vec::new(),
        _task: task,
    };
    Table::new(delegate)
}

/// Render the value cell as a horizontal row of labeled element boxes.
fn render_value_cell(row: &ComponentRow, theme: &Theme, cx: &App) -> Stateful<gpui::Div> {
    let component_id = row.component.component_id;
    let element_names = row.element_names.clone();
    let component_name = row.name.clone();
    let pending = pending_edits(cx);
    let locked = pending.locked;
    let pending_value = pending
        .get(component_id)
        .map(|e| (e.value.clone(), e.modified_elements.clone()));

    let mut container = div()
        .id(("value-cell", component_id.0 as usize))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(2.0))
        .px(px(4.0))
        .text_size(px(13.0));

    let Some(latest) = row.component.time_series.latest() else {
        return container;
    };
    let buf = latest.data();
    let Ok((_size, view)) = row.component.schema.parse_value(buf) else {
        return container;
    };

    let meta = row
        .db
        .with_state(|s| s.get_component_metadata(component_id).cloned());
    let enum_variants: Option<Vec<String>> = meta
        .as_ref()
        .and_then(|m| m.enum_variants().map(|it| it.map(|s| s.to_string()).collect()));
    let is_string = meta.as_ref().map(|m| m.is_string()).unwrap_or(false);

    if is_string {
        if let ComponentView::U8(array) = &view {
            let buf = array.buf();
            let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            let s = std::str::from_utf8(&buf[..len]).unwrap_or("").to_string();
            return container.child(value_box(
                None,
                SharedString::from(s),
                false,
                locked,
                theme,
                EditRequest {
                    component_id,
                    component_name,
                    element_names,
                    element_index: 0,
                },
            ));
        }
    }

    let element_values: Vec<_> = view.iter().collect();
    let variant_refs: Option<Vec<&str>> = enum_variants
        .as_ref()
        .map(|v| v.iter().map(|s| s.as_str()).collect());

    for (idx, element) in element_values.into_iter().enumerate() {
        let label = row.element_names.get(idx).cloned();

        // If a pending edit exists for this element, show that value instead.
        let (display_value, is_pending) = if let Some((pv, modified)) = &pending_value {
            if modified.contains(&idx) {
                let pv_element = pv.get(idx);
                if let Some(pe) = pv_element {
                    (
                        super::format_element_value(pe, variant_refs.as_deref()),
                        true,
                    )
                } else {
                    (
                        super::format_element_value(element, variant_refs.as_deref()),
                        false,
                    )
                }
            } else {
                (
                    super::format_element_value(element, variant_refs.as_deref()),
                    false,
                )
            }
        } else {
            (
                super::format_element_value(element, variant_refs.as_deref()),
                false,
            )
        };

        container = container.child(value_box(
            label,
            SharedString::from(display_value),
            is_pending,
            locked,
            theme,
            EditRequest {
                component_id,
                component_name: component_name.clone(),
                element_names: element_names.clone(),
                element_index: idx,
            },
        ));
    }

    container
}

/// Render a single labeled value box. When unlocked, it becomes clickable to
/// open an edit palette for the element.
fn value_box(
    label: Option<SharedString>,
    value: SharedString,
    is_pending: bool,
    locked: bool,
    theme: &Theme,
    request: EditRequest,
) -> Stateful<gpui::Div> {
    let bg = if is_pending {
        theme.drop_target
    } else {
        theme.bg_secondary
    };

    let id_hash = request.component_id.0.wrapping_mul(31)
        + request.element_index as u64;

    let mut b = div()
        .id(("vbox", id_hash as usize))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .px(px(4.0))
        .py(px(1.0))
        .rounded(px(3.0))
        .bg(bg);

    if let Some(label) = label {
        b = b.child(
            div()
                .text_color(theme.text_tertiary)
                .child(label),
        );
    }

    b = b.child(
        div()
            .text_color(theme.text_primary)
            .child(value),
    );

    if !locked {
        b = b.cursor_pointer().on_mouse_down(
            gpui::MouseButton::Left,
            move |_, _window, cx| {
                pending_edits_mut(cx).pending_request = Some(request.clone());
                cx.refresh_windows();
            },
        );
    }

    b
}
