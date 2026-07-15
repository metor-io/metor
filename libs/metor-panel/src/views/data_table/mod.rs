use std::sync::Arc;

use gpui::{
    AnyElement, App, Context, Entity, IntoElement, SharedString, Window, div, prelude::*, px,
};
use metor_db::DB;
use smallvec::SmallVec;

pub mod grid;
pub mod grouping;

use grid::DataTableGrid;
use grouping::{Group, detect_groups};

use super::column_browser::{ColumnBrowser, ColumnBrowserDelegate};
use super::table::Table;
use crate::theme::theme;

pub type DataTable = ColumnBrowser<DataTableDelegate>;

#[derive(Clone)]
pub enum DataTableItem {
    Group {
        name: SharedString,
    },
    Instance {
        group: SharedString,
        name: SharedString,
        short_name: SharedString,
    },
}

pub struct DataTableDelegate {
    groups: Vec<Group>,
    selected_group: Option<SharedString>,
    selected_instance: Option<SharedString>,
    grid: Entity<Table<DataTableGrid>>,
    _watcher: gpui::Task<()>,
}

impl DataTableDelegate {
    fn spawn_watcher(db: Arc<DB>, cx: &mut Context<DataTable>) -> gpui::Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let groups = detect_groups(&db);
                let result = this.update(cx, |browser, cx| {
                    let delegate = browser.delegate_mut();
                    delegate.groups = groups;
                    delegate.push_group_to_grid(cx);
                    cx.notify();
                });
                if result.is_err() {
                    break;
                }
                db.vtable_gen.wait().await;
            }
        })
    }

    fn push_group_to_grid(&self, cx: &mut Context<DataTable>) {
        let group = self
            .selected_group
            .as_ref()
            .and_then(|name| self.groups.iter().find(|g| &g.name == name).cloned());
        let filter = self.selected_instance.clone();
        self.grid.update(cx, |grid, cx| {
            grid.delegate_mut().set_group(group, filter, cx);
        });
    }
}

impl ColumnBrowserDelegate for DataTableDelegate {
    type Item = DataTableItem;

    fn resolve_selection(&self, _cx: &App) -> SmallVec<[Self::Item; 8]> {
        let mut out = SmallVec::new();
        let Some(gname) = &self.selected_group else {
            return out;
        };
        let Some(group) = self.groups.iter().find(|g| &g.name == gname) else {
            return out;
        };
        out.push(DataTableItem::Group {
            name: gname.clone(),
        });
        let Some(iname) = &self.selected_instance else {
            return out;
        };
        if let Some(inst) = group.instances.iter().find(|i| &i.name == iname) {
            out.push(DataTableItem::Instance {
                group: gname.clone(),
                name: inst.name.clone(),
                short_name: inst.short_name.clone(),
            });
        }
        out
    }

    fn root_items(&self, _cx: &App) -> Vec<Self::Item> {
        self.groups
            .iter()
            .map(|g| DataTableItem::Group {
                name: g.name.clone(),
            })
            .collect()
    }

    fn children(&self, item: &Self::Item, _cx: &App) -> Vec<Self::Item> {
        match item {
            DataTableItem::Group { name } => {
                let Some(group) = self.groups.iter().find(|g| &g.name == name) else {
                    return Vec::new();
                };
                group
                    .instances
                    .iter()
                    .map(|inst| DataTableItem::Instance {
                        group: name.clone(),
                        name: inst.name.clone(),
                        short_name: inst.short_name.clone(),
                    })
                    .collect()
            }
            DataTableItem::Instance { .. } => Vec::new(),
        }
    }

    fn is_leaf(&self, item: &Self::Item) -> bool {
        matches!(item, DataTableItem::Instance { .. })
    }

    fn item_label(&self, item: &Self::Item) -> SharedString {
        match item {
            DataTableItem::Group { name } => name.clone(),
            DataTableItem::Instance { short_name, .. } => short_name.clone(),
        }
    }

    fn column_label(&self, parent: Option<&Self::Item>) -> SharedString {
        match parent {
            None => SharedString::new_static("Groups"),
            Some(DataTableItem::Group { name }) => name.clone(),
            Some(DataTableItem::Instance { name, .. }) => name.clone(),
        }
    }

    fn items_equal(&self, a: &Self::Item, b: &Self::Item) -> bool {
        match (a, b) {
            (DataTableItem::Group { name: x }, DataTableItem::Group { name: y }) => x == y,
            (DataTableItem::Instance { name: x, .. }, DataTableItem::Instance { name: y, .. }) => {
                x == y
            }
            _ => false,
        }
    }

    fn set_selection(&mut self, column_ix: usize, item: &Self::Item, cx: &mut Context<DataTable>) {
        match (column_ix, item) {
            (0, DataTableItem::Group { name }) => {
                self.selected_group = Some(name.clone());
                self.selected_instance = None;
            }
            (1, DataTableItem::Instance { group, name, .. }) => {
                self.selected_group = Some(group.clone());
                self.selected_instance = Some(name.clone());
            }
            _ => return,
        }
        self.push_group_to_grid(cx);
    }

    // The group/instance tree is fixed at two levels, so rerooting has
    // nothing to offer.
    fn supports_reroot(&self) -> bool {
        false
    }

    fn render_detail(
        &mut self,
        _tail: Option<&Self::Item>,
        _window: &mut Window,
        cx: &mut Context<DataTable>,
    ) -> Option<AnyElement> {
        if self.selected_group.is_none() {
            let theme = theme(cx);
            return Some(
                div()
                    .p(px(8.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_tertiary)
                    .child(SharedString::new_static("Pick a group"))
                    .into_any_element(),
            );
        }
        Some(
            div()
                .size_full()
                .child(self.grid.clone())
                .into_any_element(),
        )
    }

    fn detail_label(&self, tail: Option<&Self::Item>) -> SharedString {
        match tail {
            Some(DataTableItem::Instance { name, .. }) => name.clone(),
            Some(DataTableItem::Group { name }) => name.clone(),
            None => SharedString::default(),
        }
    }
}

pub fn new_data_table(db: Arc<DB>, cx: &mut Context<DataTable>) -> DataTable {
    let grid = cx.new(|_| Table::new(DataTableGrid::new(db.clone())));
    let watcher = DataTableDelegate::spawn_watcher(db, cx);
    let delegate = DataTableDelegate {
        groups: Vec::new(),
        selected_group: None,
        selected_instance: None,
        grid,
        _watcher: watcher,
    };
    ColumnBrowser::new(delegate, cx)
}
