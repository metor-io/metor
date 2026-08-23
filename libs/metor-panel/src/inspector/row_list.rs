//! Lightweight row-list widget for embedding inspector rows inline.
//!
//! Reuses the [`InspectorRow`](crate::inspector::rows::InspectorRow) trait
//! and the inline-edit pattern from [`Inspector`], minus the chrome (search
//! bar, breadcrumbs, anchored overlay, dismiss-on-outside-click) that's
//! specific to the standalone palette / right-click inspector.
//!
//! Used by `NodeEditor` to render a node's editable args inside the node
//! card itself, so the user edits values without opening a separate panel.
//! The list emits a [`RowListEvent::Action`] for every non-inline
//! [`RowAction`] (Cascade / Pop / Dismiss) so the host decides what to do
//! — typically open a transient `Inspector` overlay for `Cascade`.

use gpui::{
    AnyElement, App, Context, FocusHandle, Focusable, IntoElement, KeyDownEvent, Render,
    SharedString, Window, div, prelude::*, px,
};

use crate::inspector::rows::text_field::TextAlign;
use crate::inspector::rows::{InspectorRow, RowAction, TextField, row_base};
use crate::inspector::{InspectorMode, InspectorRequest, open_inspector};
use crate::theme::theme;

/// Matches `row_base`'s 28px so editing and non-editing rows stack to the
/// same height inside a host. Hosts that compute container heights (e.g.
/// the node card) use the same constant.
pub const ROW_HEIGHT: f32 = 28.0;

struct EditState {
    row_index: usize,
    field: TextField,
    on_commit: Option<Box<dyn FnOnce(String, &mut Window, &mut App)>>,
}

/// Vertical list of [`InspectorRow`]s with click→activate and inline edit.
pub struct RowList {
    rows: Vec<Box<dyn InspectorRow>>,
    editing: Option<EditState>,
    focus_handle: FocusHandle,
}

impl RowList {
    pub fn new(rows: Vec<Box<dyn InspectorRow>>, cx: &mut Context<Self>) -> Self {
        Self {
            rows,
            editing: None,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn set_rows(&mut self, rows: Vec<Box<dyn InspectorRow>>, cx: &mut Context<Self>) {
        self.rows = rows;
        self.editing = None;
        cx.notify();
    }

    pub fn is_editing(&self) -> bool {
        self.editing.is_some()
    }

    fn activate_row(&mut self, row_idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let action = self.rows[row_idx].activate(window, cx);
        match action {
            RowAction::Handled => {
                cx.notify();
            }
            RowAction::StartEdit {
                current_text,
                on_commit,
            } => {
                let mut field = TextField::new("", cx);
                field.text = current_text;
                field.cursor = field.text.len();
                field.mark = field.cursor;
                field.set_align(TextAlign::Right);
                self.editing = Some(EditState {
                    row_index: row_idx,
                    field,
                    on_commit: Some(on_commit),
                });
                self.focus_handle.focus(window);
                cx.notify();
            }
            // Cascade-out: pop the standard `Inspector` overlay with the
            // child rows. This keeps heavy pickers (FromDb's component
            // list, enum variant pickers, etc.) consistent with the
            // palette / right-click UX, while inline rows handle the
            // cheap Scalar/Text edits themselves.
            RowAction::Cascade(child_rows) => {
                if let Some(open) = open_inspector(cx) {
                    open(
                        InspectorRequest {
                            rows: child_rows,
                            mode: InspectorMode::Centered,
                        },
                        window,
                        cx,
                    );
                }
            }
            // Embedded RowList has no page stack and no overlay to dismiss.
            RowAction::Pop | RowAction::Dismiss | RowAction::CascadeView { .. } => {}
        }
    }

    fn commit_edit(&mut self, window: &mut Window, cx: &mut App) {
        if let Some(mut edit) = self.editing.take()
            && let Some(on_commit) = edit.on_commit.take()
        {
            on_commit(edit.field.text.clone(), window, cx);
        }
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        if self.editing.is_none() {
            return;
        }
        match key {
            "escape" => {
                self.editing = None;
                cx.notify();
            }
            "enter" | "return" => {
                self.commit_edit(window, cx);
                cx.notify();
            }
            _ => {
                if let Some(edit) = &mut self.editing {
                    edit.field.handle_key_down(event, cx);
                }
                cx.notify();
            }
        }
    }
}

impl Focusable for RowList {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for RowList {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let editing_idx = self.editing.as_ref().map(|e| e.row_index);

        let mut col = div()
            .id("row-list")
            // Sets a `RowList` key context so hosts can exclude their own
            // single-key bindings (e.g. NodeEditor's Delete keymap) while
            // a row is being edited — otherwise typing Backspace into a
            // text field would also delete the surrounding node.
            .key_context("RowList TextInput")
            .flex()
            .flex_col()
            .py(px(1.0))
            .w_full()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.handle_key_down(event, window, cx);
            }));

        let row_count = self.rows.len();
        for row_ix in 0..row_count {
            let element: AnyElement = if editing_idx == Some(row_ix) {
                // Render edit field in the value slot, label on the left.
                let label = SharedString::from(self.rows[row_ix].label().to_string());
                let field_element = self
                    .editing
                    .as_ref()
                    .map(|e| e.field.element())
                    .map(|e| e.into_any_element())
                    .unwrap_or_else(|| div().into_any_element());
                row_base(row_ix, false, cx)
                    .h(px(ROW_HEIGHT))
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.text_primary)
                            .child(label),
                    )
                    .child(div().flex_1().min_w_0().child(field_element))
                    .into_any_element()
            } else {
                let row_element = self.rows[row_ix].render_row(row_ix, false, _window, cx);
                let row_idx_for_click = row_ix;
                div()
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.activate_row(row_idx_for_click, window, cx);
                            // Don't let the host's mouse_down (e.g. the
                            // node card's drag-to-move) also fire — clicking
                            // a row is for editing, not dragging.
                            cx.stop_propagation();
                        }),
                    )
                    .child(row_element)
                    .into_any_element()
            };
            col = col.child(element);
        }

        col
    }
}
