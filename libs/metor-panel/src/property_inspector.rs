use std::sync::Arc;

use gpui::{
    anchored, canvas, deferred, div, fill, point, prelude::*, px, size, App, Bounds, Context,
    Corner, DragMoveEvent, Empty, FocusHandle, Focusable, Hsla, IntoElement, KeyDownEvent,
    MouseDownEvent, Pixels, Point, Render, SharedString, Window,
};

use crate::command_palette::{PalettePage, TextField};
use crate::inspectable::{FieldId, InspectionField, InspectionValue};
use crate::theme::theme;
use crate::tiles::item::FieldSetter;

const SLIDER_HEIGHT: f32 = 14.0;
const SLIDER_TRACK_HEIGHT: f32 = 4.0;
const SLIDER_HANDLE_SIZE: f32 = 10.0;

/// Drag payload for slider scrubbing.
struct SliderDrag {
    field_id: FieldId,
    min: f64,
    max: f64,
}

impl Render for SliderDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// Drag payload for color HSL slider scrubbing.
struct ColorSliderDrag {
    field_id: FieldId,
    channel: ColorChannel,
}

impl Render for ColorSliderDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

#[derive(Clone, Copy)]
enum ColorChannel {
    Hue,
    Saturation,
    Lightness,
}

/// Right-click property inspector that renders inspectable fields with inline
/// widgets (toggles, sliders, color pickers, text inputs) anchored at the
/// mouse position.
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
    /// Which color field is expanded for HSL editing.
    expanded_color: Option<FieldId>,
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
            expanded_color: None,
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

    fn apply_field(&mut self, field_id: FieldId, value: InspectionValue, window: &mut Window, cx: &mut App) {
        (self.on_set_field)(field_id, value.clone(), window, cx);
        if let Some(f) = self.fields.iter_mut().find(|f| f.field_id == field_id) {
            f.value = value;
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
            InspectionValue::F64(val) if field.range.is_some() => {
                let (min, max) = field.range.unwrap();
                self.render_slider_row(field, *val, min, max, row_ix, cx)
                    .into_any_element()
            }
            InspectionValue::F64(_) | InspectionValue::String(_) => {
                self.render_text_row(field, row_ix, cx).into_any_element()
            }
            InspectionValue::Enum { selected, options } => self
                .render_enum_row(field, selected, options, row_ix, cx)
                .into_any_element(),
            InspectionValue::Color(color) => self
                .render_color_row(field, *color, row_ix, cx)
                .into_any_element(),
            _ => self.render_fallback_row(field, row_ix, cx).into_any_element(),
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
                    this.apply_field(field_id, InspectionValue::Bool(toggled), window, cx);
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

    fn render_slider_row(
        &self,
        field: &InspectionField,
        val: f64,
        min: f64,
        max: f64,
        row_ix: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = theme(cx);
        let field_id = field.field_id;
        let fraction = if max > min {
            ((val - min) / (max - min)).clamp(0.0, 1.0) as f32
        } else {
            0.0
        };

        let track_color = theme.border_primary;
        let fill_color = theme.line_color;
        let handle_color = theme.text_primary;

        let slider = div()
            .id(("slider", row_ix))
            .w(px(100.0))
            .h(px(SLIDER_HEIGHT))
            .cursor(gpui::CursorStyle::PointingHand)
            .on_drag(
                SliderDrag { field_id, min, max },
                |drag, _, _, cx| {
                    cx.new(|_| SliderDrag {
                        field_id: drag.field_id,
                        min: drag.min,
                        max: drag.max,
                    })
                },
            )
            .on_drag_move(cx.listener(
                move |this, event: &DragMoveEvent<SliderDrag>, window, cx| {
                    let drag = event.drag(cx);
                    let bounds = event.bounds;
                    let rel_x = f32::from(event.event.position.x - bounds.origin.x);
                    let width = f32::from(bounds.size.width);
                    let frac = (rel_x / width).clamp(0.0, 1.0) as f64;
                    let new_val = drag.min + frac * (drag.max - drag.min);
                    let rounded = (new_val * 100.0).round() / 100.0;
                    this.apply_field(
                        drag.field_id,
                        InspectionValue::F64(rounded),
                        window,
                        cx,
                    );
                    cx.notify();
                },
            ))
            .child(
                canvas(
                    move |bounds, _window, _cx| (bounds, fraction),
                    move |_, (bounds, fraction), window, _cx| {
                        let track_y =
                            bounds.origin.y + px((SLIDER_HEIGHT - SLIDER_TRACK_HEIGHT) / 2.0);
                        let track_bounds = Bounds::new(
                            point(bounds.origin.x, track_y),
                            size(bounds.size.width, px(SLIDER_TRACK_HEIGHT)),
                        );
                        window.paint_quad(fill(track_bounds, track_color));

                        let fill_w = bounds.size.width * fraction;
                        if fill_w > px(0.0) {
                            let fill_bounds = Bounds::new(
                                point(bounds.origin.x, track_y),
                                size(fill_w, px(SLIDER_TRACK_HEIGHT)),
                            );
                            window.paint_quad(fill(fill_bounds, fill_color));
                        }

                        let handle_x =
                            bounds.origin.x + fill_w - px(SLIDER_HANDLE_SIZE / 2.0);
                        let handle_y =
                            bounds.origin.y + px((SLIDER_HEIGHT - SLIDER_HANDLE_SIZE) / 2.0);
                        let handle_bounds = Bounds::new(
                            point(handle_x, handle_y),
                            size(px(SLIDER_HANDLE_SIZE), px(SLIDER_HANDLE_SIZE)),
                        );
                        let mut handle_quad = fill(handle_bounds, handle_color);
                        handle_quad.corner_radii =
                            gpui::Corners::all(px(SLIDER_HANDLE_SIZE / 2.0));
                        window.paint_quad(handle_quad);
                    },
                )
                .w_full()
                .h(px(SLIDER_HEIGHT)),
            );

        let value_text = SharedString::from(format!("{:.2}", val));

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
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_primary)
                    .child(field.label.clone()),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .child(slider)
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.text_secondary)
                            .min_w(px(36.0))
                            .child(value_text),
                    ),
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
                    this.apply_field(
                        field_id,
                        InspectionValue::Enum {
                            selected: next_option.clone(),
                            options: all_opts.clone(),
                        },
                        window,
                        cx,
                    );
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
        let theme = theme(cx);
        let field_id = field.field_id;
        let is_expanded = self.expanded_color == Some(field_id);

        let swatch = div()
            .w(px(14.0))
            .h(px(14.0))
            .rounded(px(3.0))
            .bg(color);

        let header = div()
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
                cx.listener(move |this, _, _, cx| {
                    if this.expanded_color == Some(field_id) {
                        this.expanded_color = None;
                    } else {
                        this.expanded_color = Some(field_id);
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
            .child(swatch);

        let mut col = div().flex().flex_col().w_full().child(header);

        if is_expanded {
            col = col.child(self.render_hsl_sliders(field_id, color, row_ix, cx));
        }

        col
    }

    fn render_hsl_sliders(
        &self,
        field_id: FieldId,
        color: Hsla,
        base_ix: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = theme(cx);

        let channels = [
            (ColorChannel::Hue, "H", color.h),
            (ColorChannel::Saturation, "S", color.s),
            (ColorChannel::Lightness, "L", color.l),
        ];

        let mut col = div()
            .flex()
            .flex_col()
            .w_full()
            .px(px(12.0))
            .pb(px(4.0))
            .gap(px(2.0));

        for (i, &(channel, label, value)) in channels.iter().enumerate() {
            let slider_ix = base_ix * 10 + i + 1000;

            let gradient_start: Hsla;
            let gradient_end: Hsla;
            match channel {
                ColorChannel::Hue => {
                    gradient_start = Hsla { h: 0.0, s: color.s, l: color.l, a: 1.0 };
                    gradient_end = Hsla { h: 1.0, s: color.s, l: color.l, a: 1.0 };
                }
                ColorChannel::Saturation => {
                    gradient_start = Hsla { h: color.h, s: 0.0, l: color.l, a: 1.0 };
                    gradient_end = Hsla { h: color.h, s: 1.0, l: color.l, a: 1.0 };
                }
                ColorChannel::Lightness => {
                    gradient_start = Hsla { h: color.h, s: color.s, l: 0.0, a: 1.0 };
                    gradient_end = Hsla { h: color.h, s: color.s, l: 1.0, a: 1.0 };
                }
            }

            let handle_color = theme.text_primary;

            let slider = div()
                .id(("color-slider", slider_ix))
                .flex_1()
                .h(px(SLIDER_HEIGHT))
                .cursor(gpui::CursorStyle::PointingHand)
                .on_drag(
                    ColorSliderDrag { field_id, channel },
                    |drag, _, _, cx| {
                        cx.new(|_| ColorSliderDrag {
                            field_id: drag.field_id,
                            channel: drag.channel,
                        })
                    },
                )
                .on_drag_move(cx.listener(
                    move |this, event: &DragMoveEvent<ColorSliderDrag>, window, cx| {
                        let drag = event.drag(cx);
                        let bounds = event.bounds;
                        let rel_x = f32::from(event.event.position.x - bounds.origin.x);
                        let width = f32::from(bounds.size.width);
                        let frac = (rel_x / width).clamp(0.0, 1.0);

                        let field = this.fields.iter().find(|f| f.field_id == drag.field_id);
                        let cur = match field.map(|f| &f.value) {
                            Some(InspectionValue::Color(c)) => *c,
                            _ => return,
                        };

                        let new_color = match drag.channel {
                            ColorChannel::Hue => Hsla { h: frac, ..cur },
                            ColorChannel::Saturation => Hsla { s: frac, ..cur },
                            ColorChannel::Lightness => Hsla { l: frac, ..cur },
                        };

                        this.apply_field(
                            drag.field_id,
                            InspectionValue::Color(new_color),
                            window,
                            cx,
                        );
                        cx.notify();
                    },
                ))
                .child(
                    canvas(
                        move |bounds, _window, _cx| (bounds, value, gradient_start, gradient_end),
                        move |_, (bounds, value, g_start, g_end), window, _cx| {
                            let track_y =
                                bounds.origin.y + px((SLIDER_HEIGHT - SLIDER_TRACK_HEIGHT) / 2.0);

                            // Paint gradient track by stepping across in 1px increments
                            let w = f32::from(bounds.size.width);
                            let steps = (w as usize).max(1);
                            for step in 0..steps {
                                let t = step as f32 / w;
                                let c = Hsla {
                                    h: g_start.h + (g_end.h - g_start.h) * t,
                                    s: g_start.s + (g_end.s - g_start.s) * t,
                                    l: g_start.l + (g_end.l - g_start.l) * t,
                                    a: 1.0,
                                };
                                let seg = Bounds::new(
                                    point(bounds.origin.x + px(step as f32), track_y),
                                    size(px(1.0), px(SLIDER_TRACK_HEIGHT)),
                                );
                                window.paint_quad(fill(seg, c));
                            }

                            // Handle
                            let handle_x = bounds.origin.x
                                + px(value * w)
                                - px(SLIDER_HANDLE_SIZE / 2.0);
                            let handle_y = bounds.origin.y
                                + px((SLIDER_HEIGHT - SLIDER_HANDLE_SIZE) / 2.0);
                            let handle_bounds = Bounds::new(
                                point(handle_x, handle_y),
                                size(px(SLIDER_HANDLE_SIZE), px(SLIDER_HANDLE_SIZE)),
                            );
                            let mut handle_quad = fill(handle_bounds, handle_color);
                            handle_quad.corner_radii =
                                gpui::Corners::all(px(SLIDER_HANDLE_SIZE / 2.0));
                            window.paint_quad(handle_quad);
                        },
                    )
                    .w_full()
                    .h(px(SLIDER_HEIGHT)),
                );

            let value_text = SharedString::from(format!("{:.2}", value));
            col = col.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(theme.text_tertiary)
                            .w(px(10.0))
                            .child(SharedString::from(label)),
                    )
                    .child(slider)
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(theme.text_tertiary)
                            .min_w(px(28.0))
                            .child(value_text),
                    ),
            );
        }

        col
    }

    fn render_fallback_row(
        &self,
        field: &InspectionField,
        row_ix: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = theme(cx);
        let summary = SharedString::from(field.value.to_string());

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
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_primary)
                    .child(field.label.clone()),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_tertiary)
                    .child(summary),
            )
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
