use std::sync::Arc;

use gpui::{
    AnyElement, App, Context, Entity, IntoElement, Pixels, SharedString, Window, div, prelude::*,
    px,
};
use metor_db::DB;

use super::grouping::{Group, GroupInstance};
use crate::theme::theme;
use crate::views::monitor::{behavior_snapshot, edit_click};
use crate::views::table::{Column, ColumnSort, Table, TableDelegate};
use crate::views::value_strip::{
    ComponentValueStrip, StripBehavior, StripClick, StripStyle, resolve_metadata, strip_row_width,
};

struct RowState {
    instance: GroupInstance,
    strips: Vec<Option<Entity<ComponentValueStrip>>>,
    clicks: Vec<Option<StripClick>>,
    element_counts: Vec<usize>,
}

pub struct DataTableGrid {
    db: Arc<DB>,
    group: Option<Group>,
    filter: Option<SharedString>,
    rows: Vec<RowState>,
}

impl DataTableGrid {
    pub fn new(db: Arc<DB>) -> Self {
        Self {
            db,
            group: None,
            filter: None,
            rows: Vec::new(),
        }
    }

    pub fn set_group(
        &mut self,
        group: Option<Group>,
        filter: Option<SharedString>,
        cx: &mut Context<Table<Self>>,
    ) {
        self.filter = filter;

        let same_shape = match (&self.group, &group) {
            (None, None) => true,
            (Some(a), Some(b)) => {
                a.name == b.name
                    && a.fields == b.fields
                    && a.instances.len() == b.instances.len()
                    && a.instances
                        .iter()
                        .zip(b.instances.iter())
                        .all(|(x, y)| x.name == y.name && x.field_ids == y.field_ids)
            }
            _ => false,
        };

        if same_shape {
            return;
        }

        self.rows.clear();
        self.group = group;
        if let Some(group) = self.group.clone() {
            let db = self.db.clone();
            for inst in &group.instances {
                let row = build_row(&db, inst, &group.fields, cx);
                self.rows.push(row);
            }
        }
        cx.notify();
    }

    fn visible_row_indices(&self) -> Vec<usize> {
        match (&self.group, &self.filter) {
            (None, _) => Vec::new(),
            (Some(g), Some(name)) => g
                .instances
                .iter()
                .enumerate()
                .filter(|(_, inst)| &inst.name == name)
                .map(|(ix, _)| ix)
                .collect(),
            (Some(g), None) => (0..g.instances.len()).collect(),
        }
    }
}

fn build_row(
    db: &Arc<DB>,
    instance: &GroupInstance,
    fields: &[SharedString],
    cx: &mut Context<Table<DataTableGrid>>,
) -> RowState {
    let mut strips = Vec::with_capacity(fields.len());
    let mut clicks = Vec::with_capacity(fields.len());
    let mut element_counts = Vec::with_capacity(fields.len());
    for (ix, field) in fields.iter().enumerate() {
        let Some(Some(component_id)) = instance.field_ids.get(ix).copied() else {
            strips.push(None);
            clicks.push(None);
            element_counts.push(0);
            continue;
        };
        let full_name = SharedString::from(format!("{}.{}", instance.name, field));
        let click = edit_click(db.clone(), component_id, full_name);
        let click_for_strip = click.clone();
        let db_for_strip = db.clone();
        let strip = cx.new(|cx| {
            ComponentValueStrip::new(
                db_for_strip,
                component_id,
                StripStyle::boxes(),
                StripBehavior {
                    on_element_click: Some(click_for_strip),
                    ..Default::default()
                },
                cx,
            )
        });
        strips.push(Some(strip));
        clicks.push(Some(click));
        // Falls back to 1 when the vtable hasn't arrived yet; group
        // rebuilds on the next vtable_gen tick.
        let meta = resolve_metadata(db, component_id);
        let n = meta.element_names.len().max(1);
        element_counts.push(n);
    }
    RowState {
        instance: instance.clone(),
        strips,
        clicks,
        element_counts,
    }
}

impl TableDelegate for DataTableGrid {
    fn columns(&self) -> Vec<Column> {
        const CELL_H_PAD: f32 = 8.0;
        let mut cols = vec![Column::new("Instance", 180.0).min_width(120.0)];
        if let Some(group) = &self.group {
            for (field_ix, field) in group.fields.iter().enumerate() {
                let n_cells = self
                    .rows
                    .iter()
                    .map(|r| r.element_counts.get(field_ix).copied().unwrap_or(0))
                    .max()
                    .unwrap_or(0)
                    .max(1);
                let width = if n_cells == 1 {
                    120.0 + CELL_H_PAD
                } else {
                    strip_row_width(n_cells) + CELL_H_PAD
                };
                cols.push(Column::new(field.clone(), width).min_width(width));
            }
        }
        cols
    }

    fn rows_count(&self) -> usize {
        self.visible_row_indices().len()
    }

    fn row_height(&self) -> Pixels {
        px(40.0)
    }

    fn render_cell(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<Table<Self>>,
    ) -> AnyElement {
        let theme = theme(cx);
        let visible = self.visible_row_indices();
        let Some(&real_ix) = visible.get(row_ix) else {
            return div().into_any_element();
        };
        let row = &self.rows[real_ix];

        if col_ix == 0 {
            return div()
                .px(px(12.0))
                .text_size(px(13.0))
                .text_color(theme.text_primary)
                .child(row.instance.short_name.clone())
                .into_any_element();
        }

        let field_ix = col_ix - 1;
        let (Some(Some(strip)), Some(Some(click)), Some(Some(component_id))) = (
            row.strips.get(field_ix).cloned(),
            row.clicks.get(field_ix).cloned(),
            row.instance.field_ids.get(field_ix).copied(),
        ) else {
            return div().into_any_element();
        };

        let db = self.db.clone();
        let behavior = behavior_snapshot(cx, db, component_id, click);
        strip.update(cx, |s, cx| s.set_behavior(behavior, cx));

        div()
            .flex()
            .flex_row()
            .items_center()
            .px(px(4.0))
            .child(strip)
            .into_any_element()
    }

    fn sort_column(&mut self, _col_ix: usize, _sort: ColumnSort, _cx: &App) {}
}
