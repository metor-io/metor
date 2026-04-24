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
use crate::views::value_strip::{ComponentValueStrip, StripBehavior, StripClick, StripStyle};

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
    for (ix, field) in fields.iter().enumerate() {
        let Some(Some(component_id)) = instance.field_ids.get(ix).copied() else {
            strips.push(None);
            clicks.push(None);
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
    }
    RowState {
        instance: instance.clone(),
        strips,
        clicks,
    }
}

impl TableDelegate for DataTableGrid {
    fn columns(&self) -> Vec<Column> {
        let mut cols = vec![Column::new("Instance", 180.0)];
        if let Some(group) = &self.group {
            for f in &group.fields {
                cols.push(Column::new(f.clone(), 200.0));
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
