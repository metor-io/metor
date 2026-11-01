//! The pieces of a grid every pivot surface paints the same way: a field's
//! value cell, its header label, the drags that rearrange a grid, and the
//! menu that hides or restores a field. How cells line up into rows —
//! embedded in an outline row, or as real table columns — is the host's.

use std::sync::Arc;

use gpui::{
    AnyElement, App, Context, Entity, IntoElement, Render, SharedString, Window, div, prelude::*,
    px,
};
use metor_db::DB;
use metor_proto::types::ComponentId;

use super::model::{Grid, Instance};
use crate::inspector::rows::{CommandRow, InspectorRow};
use crate::theme::{theme, ui_px};
use crate::views::component_browser::strip_menu;
use crate::views::image_view::collapsed_label;
use crate::views::lazy_pool::VisibleEntityCache;
use crate::views::monitor::{behavior_snapshot, edit_click};
use crate::views::table::drag_chip;
use crate::views::value_strip::{ComponentValueStrip, StripBehavior, StripStyle, strip_row_width};

/// Horizontal padding inside one cell.
pub(crate) const CELL_PAD: f32 = 8.0;

/// Approximate advance of one character of the 11px label face; the cell
/// only needs to be roomy enough that a label isn't clipped.
const LABEL_CHAR_WIDTH: f32 = 6.8;

/// Width of a cell: the wider of its `n`-element strip and its label, plus
/// padding and the divider — a pixel short and the strip wraps.
pub(crate) fn cell_width(n: usize, label: &str) -> f32 {
    let strip = strip_row_width(n.max(1));
    let text = label.chars().count() as f32 * LABEL_CHAR_WIDTH;
    strip.max(text) + CELL_PAD + 1.0
}

/// The live strip for `id`, kept in `strips` while it stays on screen.
pub(crate) fn live_strip<T: 'static>(
    db: &Arc<DB>,
    strips: &mut VisibleEntityCache<ComponentValueStrip>,
    id: ComponentId,
    cx: &mut Context<T>,
) -> Entity<ComponentValueStrip> {
    strips.get_or_create(id, || {
        let db = db.clone();
        cx.new(|cx| {
            ComponentValueStrip::new(db, id, StripStyle::boxes(), StripBehavior::default(), cx)
        })
    })
}

/// How a field cell takes its width.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CellFit {
    /// Its own width and divider, for a host that lays cells in a row.
    Sized,
    /// The host's cell, which already has a width and a divider.
    Fill,
}

/// One field of one instance: its strip, a muted stand-in for a value that
/// doesn't strip, or a dash where the instance lacks the field.
#[allow(clippy::too_many_arguments)]
pub(crate) fn field_cell<T: 'static>(
    db: &Arc<DB>,
    strips: &mut VisibleEntityCache<ComponentValueStrip>,
    instance: &Instance,
    field_ix: usize,
    field: &SharedString,
    n: usize,
    fit: CellFit,
    cx: &mut Context<T>,
) -> AnyElement {
    let theme = theme(cx);
    let cell = div()
        .h_full()
        .px(ui_px(CELL_PAD / 2.0))
        .flex()
        .items_center()
        .overflow_hidden();
    let cell = match fit {
        CellFit::Sized => cell
            .w(ui_px(cell_width(n, field)))
            .flex_shrink_0()
            .border_r_1()
            .border_color(theme.border_primary),
        CellFit::Fill => cell.w_full(),
    };
    let Some(id) = instance.ids[field_ix] else {
        return cell
            .text_size(ui_px(12.0))
            .text_color(theme.text_tertiary)
            .child(SharedString::new_static("—"))
            .into_any_element();
    };
    if let Some(label) = collapsed_label(db, id) {
        return cell
            .overflow_hidden()
            .whitespace_nowrap()
            .text_size(px(12.0))
            .text_color(theme.text_tertiary)
            .child(SharedString::from(label))
            .into_any_element();
    }
    let strip = live_strip(db, strips, id, cx);
    let name = SharedString::from(format!("{}.{}", instance.node.full_name, field));
    let click = edit_click(db.clone(), id, name);
    let mut behavior = behavior_snapshot(cx, db.clone(), id, click);
    behavior.on_element_right_click = Some(strip_menu(db.clone(), id));
    strip.update(cx, |s, cx| s.set_behavior(behavior, cx));
    // Hold the strip to one line: the cell is sized for it, and an element
    // that still doesn't fit should clip, not wrap.
    let mut line = div().min_w(ui_px(strip_row_width(n))).child(strip);
    line.style().flex_shrink = Some(0.0);
    cell.child(line).into_any_element()
}

/// A field's header label, sized to its column and draggable onto another
/// field of the same grid. The host attaches the drop and the menu.
pub(crate) fn field_label(
    grid: &Grid,
    field: &SharedString,
    n: usize,
    id: impl Into<gpui::ElementId>,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let theme = theme(cx);
    let drop_target = theme.drop_target;
    let drag = FieldDrag {
        key: grid.key.clone(),
        field: field.clone(),
    };
    div()
        .id(id)
        .w(ui_px(cell_width(n, field)))
        .h_full()
        .flex()
        .items_center()
        .flex_shrink_0()
        .border_r_1()
        .border_color(theme.border_primary)
        .px(ui_px(CELL_PAD / 2.0))
        .overflow_hidden()
        .whitespace_nowrap()
        .text_size(ui_px(11.0))
        .text_color(theme.text_tertiary)
        .child(field.clone())
        .on_drag(drag, |drag, _, _, cx| cx.new(|_| drag.clone()))
        .drag_over::<FieldDrag>(move |style, _, _, _| style.bg(drop_target))
}

/// Hide `field` when there is one, or bring back one the grid hides.
/// `set_hidden` applies the choice to whatever holds the grid's
/// arrangement.
pub(crate) fn field_menu_rows(
    grid: &Grid,
    field: Option<&SharedString>,
    set_hidden: Arc<dyn Fn(SharedString, bool, &mut App)>,
) -> Vec<Box<dyn InspectorRow>> {
    let mut rows: Vec<Box<dyn InspectorRow>> = Vec::with_capacity(grid.hidden.len() + 1);
    let entries = field
        .map(|f| (f.clone(), true))
        .into_iter()
        .chain(grid.hidden.iter().map(|f| (f.clone(), false)));
    for (field, hide) in entries {
        let verb = if hide { "Hide" } else { "Show" };
        let set_hidden = set_hidden.clone();
        rows.push(Box::new(CommandRow::new(
            format!("{verb} {field}"),
            Arc::new(move |_window, cx| set_hidden(field.clone(), hide, cx)),
        )));
    }
    rows
}

/// A field label in flight; it only lands on a field of the grid it came
/// from.
#[derive(Clone)]
pub(crate) struct FieldDrag {
    pub key: SharedString,
    pub field: SharedString,
}

impl Render for FieldDrag {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        drag_chip(self.field.clone(), cx)
    }
}

/// An instance in flight; it only lands on an instance of the grid it came
/// from.
#[derive(Clone)]
pub(crate) struct InstanceDrag {
    pub key: SharedString,
    pub label: SharedString,
}

impl Render for InstanceDrag {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        drag_chip(self.label.clone(), cx)
    }
}
