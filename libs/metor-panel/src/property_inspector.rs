use std::sync::Arc;

use gpui::{
    anchored, deferred, div, prelude::*, px, App, Context, Corner, FocusHandle, Focusable, Hsla,
    IntoElement, KeyDownEvent, MouseDownEvent, Pixels, Point, Render, SharedString, Window,
};

use crate::command_palette::{PalettePage, TextField};
use crate::inspectable::{FieldId, InspectionField, InspectionValue};
use crate::theme::theme;
use crate::tiles::item::FieldSetter;

/// Right-click property inspector that renders inspectable fields with inline
/// widgets (toggles, text inputs, enum selectors) anchored at the mouse position.
pub struct PropertyInspector {
    fields: Vec<InspectionField>,
    position: Point<Pixels>,
    focus_handle: FocusHandle,
    parent_focus: Option<FocusHandle>,
    pub dismissed: bool,
    editing_field: Option<FieldId>,
    text_field: TextField,
    on_set_field: FieldSetter,
    on_open_palette: Option<Arc<dyn Fn(PalettePage, &mut Window, &mut App)>>,
}

impl PropertyInspector {
    pub fn new(
        fields: Vec<InspectionField>,
        position: Point<Pixels>,
        on_set_field: FieldSetter,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            fields,
            position,
            focus_handle: cx.focus_handle(),
            parent_focus: None,
            dismissed: false,
            editing_field: None,
            text_field: TextField::new("", cx),
            on_set_field,
            on_open_palette: None,
        }
    }

    pub fn set_parent_focus(&mut self, handle: FocusHandle) {
        self.parent_focus = Some(handle);
    }

    pub fn set_on_open_palette(
        &mut self,
        cb: impl Fn(PalettePage, &mut Window, &mut App) + 'static,
    ) {
        self.on_open_palette = Some(Arc::new(cb));
    }

    fn dismiss(&mut self, window: &mut Window) {
        self.dismissed = true;
        if let Some(parent) = &self.parent_focus {
            parent.focus(window);
        } else {
            window.blur();
        }
    }

    fn start_editing(&mut self, field: &InspectionField) {
        self.editing_field = Some(field.field_id);
        self.text_field.clear();
        self.text_field.text = field.value.to_string();
        self.text_field.cursor = self.text_field.text.len();
        self.text_field.mark = self.text_field.cursor;
    }

    fn commit_edit(&mut self, window: &mut Window, cx: &mut App) {
        let Some(field_id) = self.editing_field.take() else {
            return;
        };
        let field = self.fields.iter().find(|f| f.field_id == field_id);
        if let Some(field) = field {
            if let Some(new_value) = field.value.parse_like(&self.text_field.text) {
                (self.on_set_field)(field_id, new_value, window, cx);
            }
        }
    }

    fn cancel_edit(&mut self) {
        self.editing_field = None;
    }

    fn handle_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut App) {
        let key = event.keystroke.key.as_str();

        match key {
            "escape" => {
                if self.editing_field.is_some() {
                    self.cancel_edit();
                } else {
                    self.dismiss(window);
                }
                return;
            }
            "enter" | "return" => {
                if self.editing_field.is_some() {
                    self.commit_edit(window, cx);
                }
                return;
            }
            _ => {}
        }

        if self.editing_field.is_some() {
            self.text_field.handle_key_down(event, cx);
        }
    }

    fn render_field_row(
        &self,
        field: &InspectionField,
        row_ix: usize,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match &field.value {
            InspectionValue::Bool(val) => {
                self.render_bool_row(field, *val, row_ix, cx).into_any_element()
            }
            InspectionValue::F64(_)
            | InspectionValue::String(_) => {
                self.render_text_row(field, row_ix, cx).into_any_element()
            }
            InspectionValue::Enum { selected, options } => {
                self.render_enum_row(field, selected, options, row_ix, cx).into_any_element()
            }
            InspectionValue::Color(color) => {
                self.render_color_row(field, *color, row_ix, cx).into_any_element()
            }
            _ => {
                self.render_fallback_row(field, row_ix, cx).into_any_element()
            }
        }
    }

    fn render_bool_row(
        &self,
        field: &InspectionField,
        val: bool,
        row_ix: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = theme(cx);
        let field_id = field.field_id;
        let toggled = !val;
        let toggle_color = if val {
            theme.line_color
        } else {
            theme.text_tertiary
        };

        div()
            .id(("inspector-row", row_ix))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .w_full()
            .px(px(12.0))
            .py(px(4.0))
            .cursor_pointer()
            .hover(|s| s.bg(theme.selection_bg))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    (this.on_set_field)(
                        field_id,
                        InspectionValue::Bool(toggled),
                        window,
                        cx,
                    );
                    // Re-read the field
                    if let Some(f) = this.fields.iter_mut().find(|f| f.field_id == field_id) {
                        f.value = InspectionValue::Bool(toggled);
                    }
                    cx.notify();
                }),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_primary)
                    .child(field.label.clone()),
            )
            .child(
                div()
                    .w(px(14.0))
                    .h(px(14.0))
                    .rounded(px(7.0))
                    .bg(toggle_color),
            )
    }

    fn render_text_row(
        &self,
        field: &InspectionField,
        row_ix: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = theme(cx);
        let field_id = field.field_id;
        let is_editing = self.editing_field == Some(field_id);
        let value_text = SharedString::from(field.value.to_string());

        let value_child: gpui::AnyElement = if is_editing {
            self.text_field.element().into_any_element()
        } else {
            div()
                .text_size(px(12.0))
                .text_color(theme.text_secondary)
                .child(value_text)
                .into_any_element()
        };

        let field_clone = field.clone();
        div()
            .id(("inspector-row", row_ix))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .w_full()
            .px(px(12.0))
            .py(px(4.0))
            .cursor_pointer()
            .hover(|s| s.bg(theme.selection_bg))
            .when(!is_editing, |el| {
                el.on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        this.start_editing(&field_clone);
                        cx.notify();
                    }),
                )
            })
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_primary)
                    .child(field.label.clone()),
            )
            .child(
                div()
                    .min_w(px(60.0))
                    .max_w(px(140.0))
                    .child(value_child),
            )
    }

    fn render_enum_row(
        &self,
        field: &InspectionField,
        selected: &str,
        options: &[String],
        row_ix: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = theme(cx);
        let field_id = field.field_id;
        // Cycle to next option on click
        let next_option = {
            let idx = options.iter().position(|o| o == selected).unwrap_or(0);
            let next_idx = (idx + 1) % options.len();
            options[next_idx].clone()
        };
        let all_opts = options.to_vec();

        div()
            .id(("inspector-row", row_ix))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .w_full()
            .px(px(12.0))
            .py(px(4.0))
            .cursor_pointer()
            .hover(|s| s.bg(theme.selection_bg))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    (this.on_set_field)(
                        field_id,
                        InspectionValue::Enum {
                            selected: next_option.clone(),
                            options: all_opts.clone(),
                        },
                        window,
                        cx,
                    );
                    if let Some(f) = this.fields.iter_mut().find(|f| f.field_id == field_id) {
                        f.value = InspectionValue::Enum {
                            selected: next_option.clone(),
                            options: all_opts.clone(),
                        };
                    }
                    cx.notify();
                }),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_primary)
                    .child(field.label.clone()),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_secondary)
                    .child(SharedString::from(selected.to_string())),
            )
    }

    fn render_color_row(
        &self,
        field: &InspectionField,
        color: Hsla,
        row_ix: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        self.render_fallback_row_with_swatch(field, Some(color), row_ix, cx)
    }

    fn render_fallback_row_with_swatch(
        &self,
        field: &InspectionField,
        swatch: Option<Hsla>,
        row_ix: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = theme(cx);
        let summary = SharedString::from(field.value.to_string());

        let label = div()
            .text_size(px(12.0))
            .text_color(theme.text_primary)
            .child(field.label.clone());

        let mut right = div().flex().flex_row().items_center().gap(px(6.0));

        if let Some(color) = swatch {
            right = right.child(
                div()
                    .w(px(14.0))
                    .h(px(14.0))
                    .rounded(px(3.0))
                    .bg(color),
            );
        }

        right = right.child(
            div()
                .text_size(px(12.0))
                .text_color(theme.text_tertiary)
                .child(summary),
        );

        div()
            .id(("inspector-row", row_ix))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .w_full()
            .px(px(12.0))
            .py(px(4.0))
            .hover(|s| s.bg(theme.selection_bg))
            .child(label)
            .child(right)
    }

    fn render_fallback_row(
        &self,
        field: &InspectionField,
        row_ix: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        self.render_fallback_row_with_swatch(field, None, row_ix, cx)
    }
}

impl Focusable for PropertyInspector {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PropertyInspector {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.dismissed {
            return div().into_any_element();
        }

        let theme = theme(cx);
        let position = self.position;

        let mut panel = div()
            .flex()
            .flex_col()
            .id("property-inspector")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.handle_key_down(event, window, cx);
                cx.notify();
            }))
            .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, window, _cx| {
                this.dismiss(window);
            }))
            .w(px(280.0))
            .max_h(px(400.0))
            .bg(theme.bg_elevated)
            .border_1()
            .border_color(theme.border_primary)
            .rounded(px(6.0))
            .overflow_y_scroll()
            .py(px(4.0));

        let fields = self.fields.clone();
        for (i, field) in fields.iter().enumerate() {
            panel = panel.child(self.render_field_row(field, i, cx));
        }

        if self.fields.is_empty() {
            panel = panel.child(
                div()
                    .px(px(12.0))
                    .py(px(6.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_tertiary)
                    .child(SharedString::new_static("No properties")),
            );
        }

        deferred(
            anchored()
                .position(position)
                .anchor(Corner::TopLeft)
                .snap_to_window_with_margin(px(8.0))
                .child(panel),
        )
        .with_priority(1)
        .into_any_element()
    }
}
