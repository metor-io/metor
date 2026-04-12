use std::sync::Arc;

use gpui::{
    anchored, canvas, deferred, div, fill, point, prelude::*, px, size, App, Bounds, Context,
    Corner, DragMoveEvent, Empty, Entity, FocusHandle, Focusable, Hsla, IntoElement, KeyDownEvent,
    Pixels, Point, Render, SharedString, Window,
};

use crate::command_palette::{PalettePage, TextField};
use crate::icons::Icon;
use crate::inspectable::{FieldId, InspectionField, InspectionValue};
use crate::theme::theme;
use crate::tiles::item::{FieldSetter, FieldsProvider};

const SLIDER_HEIGHT: f32 = 14.0;
const SLIDER_TRACK_HEIGHT: f32 = 4.0;
const SLIDER_HANDLE_SIZE: f32 = 10.0;

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


/// When a child panel shows enum options instead of fields.
struct EnumPicker {
    field_id: FieldId,
    options: Vec<String>,
    selected: String,
}

/// Searchable right-click property panel with cascading submenus.
pub struct PropertyInspector {
    fields: Vec<InspectionField>,
    position: Point<Pixels>,
    focus_handle: FocusHandle,
    parent_focus: Option<FocusHandle>,
    pub dismissed: bool,
    text_field: TextField,
    selected_index: usize,
    editing_field: Option<FieldId>,
    edit_field: TextField,
    on_set_field: FieldSetter,
    fields_provider: Option<FieldsProvider>,
    on_open_palette: Option<Arc<dyn Fn(PalettePage, &mut Window, &mut App)>>,
    /// Optional "add" callback for list panels, rendered as a `+ New` row.
    on_add: Option<crate::inspectable::AddCallback>,
    /// If set, this panel renders enum options instead of fields.
    enum_picker: Option<EnumPicker>,
    /// Cascading child panel spawned to the right.
    child: Option<Entity<PropertyInspector>>,
    /// Tracked bounds of this panel for positioning children.
    panel_bounds: Option<Bounds<Pixels>>,
    /// Whether this is a root panel (renders its own occlude overlay) or a child.
    is_root: bool,
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
            text_field: TextField::new("Search...", cx),
            selected_index: 0,
            editing_field: None,
            edit_field: TextField::new("", cx),
            on_set_field,
            fields_provider: None,
            on_open_palette: None,
            on_add: None,
            enum_picker: None,
            child: None,
            panel_bounds: None,
            is_root: true,
        }
    }

    fn new_child(
        fields: Vec<InspectionField>,
        position: Point<Pixels>,
        on_set_field: FieldSetter,
        fields_provider: Option<FieldsProvider>,
        on_open_palette: Option<Arc<dyn Fn(PalettePage, &mut Window, &mut App)>>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            fields,
            position,
            focus_handle: cx.focus_handle(),
            parent_focus: None,
            dismissed: false,
            text_field: TextField::new("Search...", cx),
            selected_index: 0,
            editing_field: None,
            edit_field: TextField::new("", cx),
            on_set_field,
            fields_provider,
            on_open_palette,
            on_add: None,
            enum_picker: None,
            child: None,
            panel_bounds: None,
            is_root: false,
        }
    }

    pub fn set_parent_focus(&mut self, handle: FocusHandle) {
        self.parent_focus = Some(handle);
    }

    pub fn set_fields_provider(&mut self, provider: FieldsProvider) {
        self.fields_provider = Some(provider);
    }

    pub fn set_on_open_palette(
        &mut self,
        cb: impl Fn(PalettePage, &mut Window, &mut App) + 'static,
    ) {
        self.on_open_palette = Some(Arc::new(cb));
    }

    fn dismiss(&mut self, window: &mut Window) {
        self.child = None;
        self.dismissed = true;
        if let Some(parent) = &self.parent_focus {
            parent.focus(window);
        } else {
            window.blur();
        }
    }

    fn dismiss_child(&mut self) {
        self.child = None;
    }

    fn start_editing(&mut self, field: &InspectionField) {
        self.editing_field = Some(field.field_id);
        self.edit_field.clear();
        self.edit_field.text = field.value.to_string();
        self.edit_field.cursor = self.edit_field.text.len();
        self.edit_field.mark = self.edit_field.cursor;
    }

    fn commit_edit(&mut self, window: &mut Window, cx: &mut App) {
        let Some(field_id) = self.editing_field.take() else {
            return;
        };
        let field = self.fields.iter().find(|f| f.field_id == field_id);
        if let Some(field) = field {
            if let Some(new_value) = field.value.parse_like(&self.edit_field.text) {
                self.apply_field(field_id, new_value, window, cx);
            }
        }
    }

    fn apply_field(
        &mut self,
        field_id: FieldId,
        value: InspectionValue,
        window: &mut Window,
        cx: &mut App,
    ) {
        (self.on_set_field)(field_id, value.clone(), window, cx);
        if let Some(f) = self.fields.iter_mut().find(|f| f.field_id == field_id) {
            f.value = value;
        }
        if let Some(provider) = &self.fields_provider {
            self.fields = provider(cx);
        }
    }

    fn child_position(&self) -> Point<Pixels> {
        self.panel_bounds
            .map(|b| {
                let row_y = px(self.selected_index as f32 * 26.0);
                point(b.origin.x + b.size.width, b.origin.y + row_y)
            })
            .unwrap_or(self.position)
    }

    fn open_child_cascade_with_add(
        &mut self,
        child_fields: Vec<InspectionField>,
        on_add: Option<crate::inspectable::AddCallback>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let child_pos = self.child_position();
        let setter = self.on_set_field.clone();
        let on_open_palette = self.on_open_palette.clone();
        let parent_focus = self.focus_handle.clone();
        let child = cx.new(|cx| {
            let mut c = Self::new_child(
                child_fields,
                child_pos,
                setter,
                None,
                on_open_palette,
                cx,
            );
            c.set_parent_focus(parent_focus);
            c.on_add = on_add;
            c
        });
        child.focus_handle(cx).focus(window);
        self.child = Some(child);
    }

    fn filtered_indices(&self) -> Vec<usize> {
        let filter = &self.text_field.text;
        if filter.is_empty() {
            return (0..self.fields.len()).collect();
        }

        use nucleo_matcher::{
            Matcher,
            pattern::{CaseMatching, Normalization, Pattern},
        };

        let mut matcher = Matcher::new(nucleo_matcher::Config::DEFAULT);
        let pattern = Pattern::parse(filter, CaseMatching::Ignore, Normalization::Smart);

        let mut scored: Vec<(usize, u32)> = self
            .fields
            .iter()
            .enumerate()
            .filter_map(|(i, field)| {
                let mut buf = Vec::new();
                let haystack = nucleo_matcher::Utf32Str::new(&field.label, &mut buf);
                let score = pattern.score(haystack, &mut matcher)?;
                Some((i, score))
            })
            .collect();

        scored.sort_by(|a, b| b.1.cmp(&a.1));
        scored.into_iter().map(|(i, _)| i).collect()
    }

    fn visible_count(&self) -> usize {
        self.filtered_indices().len()
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let indices = self.filtered_indices();
        let Some(&field_idx) = indices.get(self.selected_index) else {
            return;
        };
        let field = self.fields[field_idx].clone();
        self.activate_field(&field, window, cx);
    }

    fn activate_field(
        &mut self,
        field: &InspectionField,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match &field.value {
            InspectionValue::Bool(val) => {
                let toggled = !val;
                self.apply_field(field.field_id, InspectionValue::Bool(toggled), window, cx);
                cx.notify();
            }
            InspectionValue::Enum { selected, options } => {
                let child_pos = self.child_position();
                let setter = self.on_set_field.clone();
                let on_open_palette = self.on_open_palette.clone();
                let parent_focus = self.focus_handle.clone();
                let picker = EnumPicker {
                    field_id: field.field_id,
                    options: options.clone(),
                    selected: selected.clone(),
                };
                let child = cx.new(|cx| {
                    let mut c = Self::new_child(
                        vec![],
                        child_pos,
                        setter,
                        None,
                        on_open_palette,
                        cx,
                    );
                    c.set_parent_focus(parent_focus);
                    c.enum_picker = Some(picker);
                    c
                });
                child.focus_handle(cx).focus(window);
                self.child = Some(child);
                cx.notify();
            }
            InspectionValue::List { items, on_add } => {
                let child_fields: Vec<InspectionField> = items
                    .iter()
                    .map(|item| {
                        // Each item becomes a nav row labeled with its name.
                        // The value is a String summary — clicking it cascades
                        // to show item.fields via the ListItemFields variant.
                        InspectionField::new(
                            item.label.clone(),
                            item.fields.first().map(|f| f.field_id).unwrap_or(FieldId(8000)),
                            InspectionValue::ListItemFields(item.fields.clone()),
                        )
                    })
                    .collect();
                let add_cb = on_add.clone();
                self.open_child_cascade_with_add(child_fields, add_cb, window, cx);
                cx.notify();
            }
            InspectionValue::Color(color) => {
                let field_id = field.field_id;
                let c = *color;
                let child_fields = vec![
                    InspectionField::new("Hue", FieldId(field_id.0 * 100 + 1), InspectionValue::F64(c.h as f64)).with_range(0.0, 1.0),
                    InspectionField::new("Saturation", FieldId(field_id.0 * 100 + 2), InspectionValue::F64(c.s as f64)).with_range(0.0, 1.0),
                    InspectionField::new("Lightness", FieldId(field_id.0 * 100 + 3), InspectionValue::F64(c.l as f64)).with_range(0.0, 1.0),
                ];
                let parent_setter = self.on_set_field.clone();
                let color_setter: FieldSetter = Arc::new(move |child_fid, value, window, cx| {
                    if let InspectionValue::F64(v) = value {
                        let channel = child_fid.0 % 100;
                        let mut new_color = c;
                        match channel {
                            1 => new_color.h = v as f32,
                            2 => new_color.s = v as f32,
                            3 => new_color.l = v as f32,
                            _ => {}
                        }
                        parent_setter(field_id, InspectionValue::Color(new_color), window, cx);
                    }
                });
                let on_open_palette = self.on_open_palette.clone();
                let parent_focus = self.focus_handle.clone();
                let child_pos = self
                    .panel_bounds
                    .map(|b| {
                        let row_y = px(self.selected_index as f32 * 26.0);
                        point(b.origin.x + b.size.width, b.origin.y + row_y)
                    })
                    .unwrap_or(self.position);
                let child = cx.new(|cx| {
                    let mut c = Self::new_child(
                        child_fields,
                        child_pos,
                        color_setter,
                        None,
                        on_open_palette,
                        cx,
                    );
                    c.set_parent_focus(parent_focus);
                    c
                });
                child.focus_handle(cx).focus(window);
                self.child = Some(child);
                cx.notify();
            }
            InspectionValue::ListItemFields(sub_fields) => {
                self.open_child_cascade_with_add(sub_fields.clone(), None, window, cx);
                cx.notify();
            }
            InspectionValue::Command(cb) => {
                cb(window, cx);
                self.dismiss(window);
            }
            _ => {}
        }
    }

    fn handle_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();

        if self.editing_field.is_some() {
            match key {
                "escape" => {
                    self.editing_field = None;
                    cx.notify();
                    return;
                }
                "enter" | "return" => {
                    self.commit_edit(window, cx);
                    cx.notify();
                    return;
                }
                _ => {
                    self.edit_field.handle_key_down(event, cx);
                    cx.notify();
                    return;
                }
            }
        }

        match key {
            "escape" => {
                if self.child.is_some() {
                    self.dismiss_child();
                } else {
                    self.dismiss(window);
                }
                cx.notify();
                return;
            }
            "up" => {
                if self.selected_index > 0 {
                    self.selected_index -= 1;
                }
                cx.notify();
                return;
            }
            "down" => {
                let total = self.visible_count();
                if total > 0 && self.selected_index < total - 1 {
                    self.selected_index += 1;
                }
                cx.notify();
                return;
            }
            "enter" | "return" => {
                self.confirm(window, cx);
                cx.notify();
                return;
            }
            _ => {}
        }

        if self.text_field.handle_key_down(event, cx) {
            self.selected_index = 0;
        }
    }

    fn render_search_bar(&self, cx: &App) -> impl IntoElement {
        let theme = theme(cx);
        div()
            .flex()
            .flex_row()
            .items_center()
            .px(px(8.0))
            .py(px(3.0))
            .border_b_1()
            .border_color(theme.border_primary)
            .text_size(px(12.0))
            .child(
                div()
                    .flex_1()
                    .min_w(px(60.0))
                    .child(self.text_field.element()),
            )
    }

    fn render_field_row(
        &self,
        field: &InspectionField,
        row_ix: usize,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match &field.value {
            InspectionValue::Bool(val) => {
                self.render_bool_row(field, *val, row_ix, selected, cx)
                    .into_any_element()
            }
            InspectionValue::F64(val) if field.range.is_some() => {
                let (min, max) = field.range.unwrap();
                self.render_slider_row(field, *val, min, max, row_ix, selected, cx)
                    .into_any_element()
            }
            InspectionValue::F64(_) | InspectionValue::String(_) => self
                .render_value_row(field, row_ix, selected, cx)
                .into_any_element(),
            InspectionValue::Enum { selected: sel, .. } => self
                .render_nav_row(field, sel, row_ix, selected, cx)
                .into_any_element(),
            InspectionValue::Color(color) => self
                .render_color_row(field, *color, row_ix, selected, cx)
                .into_any_element(),
            InspectionValue::List { items, .. } => {
                let summary = format!("{} items", items.len());
                self.render_nav_row(field, &summary, row_ix, selected, cx)
                    .into_any_element()
            }
            InspectionValue::ListItemFields(sub_fields) => {
                let summary = format!("{} fields", sub_fields.len());
                self.render_nav_row(field, &summary, row_ix, selected, cx)
                    .into_any_element()
            }
            InspectionValue::Command(_) => self
                .render_command_row(field, row_ix, selected, cx)
                .into_any_element(),
            _ => self
                .render_value_row(field, row_ix, selected, cx)
                .into_any_element(),
        }
    }

    fn row_base(
        &self,
        row_ix: usize,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let theme = theme(cx);
        let bg = if selected {
            theme.selection_bg
        } else {
            Hsla::transparent_black()
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
            .bg(bg)
            .cursor_pointer()
            .hover(|s| s.bg(theme.selection_bg))
    }

    fn render_bool_row(
        &self,
        field: &InspectionField,
        val: bool,
        row_ix: usize,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = theme(cx);
        let field_id = field.field_id;
        let toggled = !val;
        let track_color = if val {
            theme.line_color
        } else {
            theme.text_tertiary
        };

        let knob = div()
            .w(px(10.0))
            .h(px(10.0))
            .rounded(px(2.0))
            .bg(theme.text_primary);

        let track = div()
            .w(px(28.0))
            .h(px(14.0))
            .rounded(px(3.0))
            .bg(track_color)
            .flex()
            .items_center()
            .px(px(2.0))
            .when(val, |el| el.flex_row_reverse())
            .child(knob);

        self.row_base(row_ix, selected, cx)
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
            .child(track)
    }

    fn render_command_row(
        &self,
        field: &InspectionField,
        row_ix: usize,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = theme(cx);
        self.row_base(row_ix, selected, cx)
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    this.selected_index = row_ix;
                    this.confirm(window, cx);
                    cx.notify();
                }),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_secondary)
                    .child(field.label.clone()),
            )
    }

    fn render_value_row(
        &self,
        field: &InspectionField,
        row_ix: usize,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = theme(cx);
        let field_id = field.field_id;
        let is_editing = self.editing_field == Some(field_id);

        let value_child: gpui::AnyElement = if is_editing {
            self.edit_field.element().into_any_element()
        } else {
            let value_text = SharedString::from(field.value.to_string());
            div()
                .text_size(px(12.0))
                .text_color(theme.text_secondary)
                .child(value_text)
                .into_any_element()
        };

        let field_clone = field.clone();
        self.row_base(row_ix, selected, cx)
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
                    .max_w(px(120.0))
                    .child(value_child),
            )
    }

    fn render_nav_row(
        &self,
        field: &InspectionField,
        value_text: &str,
        row_ix: usize,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = theme(cx);

        self.row_base(row_ix, selected, cx)
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    this.selected_index = row_ix;
                    this.confirm(window, cx);
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
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.text_secondary)
                            .child(SharedString::from(value_text.to_string())),
                    )
                    .child(Icon::ChevronRight.svg(8.0)),
            )
    }

    fn render_color_row(
        &self,
        field: &InspectionField,
        color: Hsla,
        row_ix: usize,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = theme(cx);

        self.row_base(row_ix, selected, cx)
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    this.selected_index = row_ix;
                    this.confirm(window, cx);
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
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .w(px(14.0))
                            .h(px(14.0))
                            .rounded(px(3.0))
                            .bg(color),
                    )
                    .child(Icon::ChevronRight.svg(8.0)),
            )
    }

    fn render_slider_row(
        &self,
        field: &InspectionField,
        val: f64,
        min: f64,
        max: f64,
        row_ix: usize,
        selected: bool,
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

                        let handle_x = bounds.origin.x + fill_w - px(SLIDER_HANDLE_SIZE / 2.0);
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

        self.row_base(row_ix, selected, cx)
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

    fn render_panel(&mut self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let theme = theme(cx);
        let indices = self.filtered_indices();

        let view = cx.entity().clone();
        let bounds_tracker = canvas(
            move |bounds, _window, cx| {
                view.update(cx, |this, _| {
                    this.panel_bounds = Some(bounds);
                });
            },
            |_, _, _, _| {},
        )
        .size_full()
        .absolute();

        let mut items_col = div().flex().flex_col().py(px(4.0));

        if let Some(add_cb) = &self.on_add {
            let add_cb = add_cb.clone();
            items_col = items_col.child(
                div()
                    .id("add-new-row")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .w_full()
                    .px(px(12.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.selection_bg))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            let page = add_cb();
                            if let Some(cb) = &this.on_open_palette {
                                cb(page, window, cx);
                                this.dismiss(window);
                            }
                        }),
                    )
                    .child(Icon::Add.svg(10.0))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.text_secondary)
                            .child(SharedString::new_static("New")),
                    ),
            );
        }

        if let Some(picker) = &self.enum_picker {
            let field_id = picker.field_id;
            for (i, option) in picker.options.iter().enumerate() {
                let is_selected = *option == picker.selected;
                let option_val = option.clone();
                let all_opts = picker.options.clone();
                let text_color = if is_selected {
                    theme.text_primary
                } else {
                    theme.text_secondary
                };
                items_col = items_col.child(
                    div()
                        .id(("enum-option", i))
                        .flex()
                        .flex_row()
                        .items_center()
                        .w_full()
                        .px(px(12.0))
                        .py(px(4.0))
                        .bg(if i == self.selected_index {
                            theme.selection_bg
                        } else {
                            Hsla::transparent_black()
                        })
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.selection_bg))
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                (this.on_set_field)(
                                    field_id,
                                    InspectionValue::Enum {
                                        selected: option_val.clone(),
                                        options: all_opts.clone(),
                                    },
                                    window,
                                    cx,
                                );
                                this.dismiss(window);
                            }),
                        )
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(text_color)
                                .child(SharedString::from(option.clone())),
                        ),
                );
            }
        } else if indices.is_empty() {
            items_col = items_col.child(
                div()
                    .px(px(12.0))
                    .py(px(6.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_tertiary)
                    .child(SharedString::new_static("No results")),
            );
        } else {
            let fields = self.fields.clone();
            for (vis_ix, &field_idx) in indices.iter().enumerate() {
                let selected = vis_ix == self.selected_index;
                items_col = items_col
                    .child(self.render_field_row(&fields[field_idx], vis_ix, selected, cx));
            }
        }

        div()
            .relative()
            .flex()
            .flex_col()
            .id("property-inspector")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.handle_key_down(event, window, cx);
                cx.notify();
            }))
            .w(px(280.0))
            .max_h(px(400.0))
            .bg(theme.bg_elevated)
            .border_1()
            .border_color(theme.border_primary)
            .rounded(px(6.0))
            .overflow_y_scroll()
            .child(bounds_tracker)
            .child(self.render_search_bar(cx))
            .child(items_col)
    }
}

impl Focusable for PropertyInspector {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PropertyInspector {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.dismissed {
            return div().into_any_element();
        }

        if let Some(child) = &self.child {
            if child.read(cx).dismissed {
                self.child = None;
                self.focus_handle.focus(window);
            }
        }

        let position = self.position;
        let panel = self.render_panel(cx);

        let anchored_panel = anchored()
            .position(position)
            .anchor(Corner::TopLeft)
            .snap_to_window_with_margin(px(8.0))
            .child(
                panel.on_mouse_down_out(cx.listener(
                    |this, _: &gpui::MouseDownEvent, window, _cx| {
                        if this.child.is_none() {
                            this.dismiss(window);
                        }
                    },
                )),
            );

        if self.is_root {
            let mut overlay = div()
                .id("inspector-overlay")
                .occlude()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .child(anchored_panel);

            if let Some(child) = &self.child {
                overlay = overlay.child(child.clone());
            }

            deferred(overlay).with_priority(1).into_any_element()
        } else {
            let mut container = div().child(anchored_panel);
            if let Some(child) = &self.child {
                container = container.child(child.clone());
            }
            container.into_any_element()
        }
    }
}
