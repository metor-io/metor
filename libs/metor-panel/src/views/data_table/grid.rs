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

/// Live state backing one instance row of the data-table detail grid.
///
/// Owns one [`ComponentValueStrip`] per group field so each cell streams
/// its own WAL updates and handles click-to-edit independently. A `None`
/// slot represents a field the instance doesn't have — the cell renders
/// blank.
struct RowState {
    instance: GroupInstance,
    strips: Vec<Option<Entity<ComponentValueStrip>>>,
    clicks: Vec<Option<StripClick>>,
    /// Element count per field. Drives column widths so a 3-box strip gets
    /// a wider column than a scalar. `0` for fields the instance lacks.
    element_counts: Vec<usize>,
}

/// [`TableDelegate`] for the data-table detail grid.
///
/// Columns are a fixed `Instance` column plus one column per field of
/// the currently-selected group. Rebuilt in-place via [`Self::set_group`]
/// whenever the browser's group selection changes.
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

        let same_group = match (&self.group, &group) {
            (Some(a), Some(b)) => a.name == b.name,
            (None, None) => true,
            _ => false,
        };
        let same_shape = same_group
            && match (&self.group, &group) {
                (Some(a), Some(b)) => {
                    a.fields == b.fields
                        && a.instances.len() == b.instances.len()
                        && a.instances
                            .iter()
                            .zip(b.instances.iter())
                            .all(|(x, y)| {
                                x.name == y.name && x.field_ids == y.field_ids
                            })
                }
                (None, None) => true,
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
        // Element count comes from the registered schema/metadata. Falls
        // back to 1 when the vtable hasn't arrived yet — the group
        // rebuilds on the next `vtable_gen` tick and picks up the real
        // dim.
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
        const CELL_H_PAD: f32 = 8.0; // matches `.px(px(4.0))` cell wrapper
        let mut cols = vec![Column::new("Instance", 180.0).min_width(120.0)];
        if let Some(group) = &self.group {
            for (field_ix, field) in group.fields.iter().enumerate() {
                // Take the widest strip across instances — ragged groups
                // still get a column that fits the fullest representation.
                let n_cells = self
                    .rows
                    .iter()
                    .map(|r| r.element_counts.get(field_ix).copied().unwrap_or(0))
                    .max()
                    .unwrap_or(0)
                    .max(1);
                // Scalar (unlabeled, intrinsic-width) cells don't get the
                // fixed 78px box treatment, so the 1-cell strip can grow
                // or shrink with the value. Give those columns a little
                // extra breathing room; multi-cell strips already know
                // their size exactly via `strip_row_width`.
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
        let strip_opt = row.strips.get(field_ix).cloned().flatten();
        let click_opt = row.clicks.get(field_ix).cloned().flatten();
        let component_id_opt = row
            .instance
            .field_ids
            .get(field_ix)
            .copied()
            .flatten();

        let (Some(strip), Some(click), Some(component_id)) = (strip_opt, click_opt, component_id_opt)
        else {
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

    fn sort_column(&mut self, _col_ix: usize, _sort: ColumnSort, _cx: &App) {
        // v1: detail grid is unsorted. Values stream live, so a useful
        // sort would need to re-key on every sample — deferred.
    }
}
