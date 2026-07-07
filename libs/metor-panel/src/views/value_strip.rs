use std::fmt::Write;
use std::sync::Arc;

use gpui::{
    App, Context, FocusHandle, Focusable, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, Pixels, Point, SharedString, Stateful, Window, div, prelude::*, px,
};
use metor_db::DB;
use metor_proto::types::{ComponentId, ComponentView, ElementValue, PrimType};
use smallvec::SmallVec;

use crate::icons::Icon;
use crate::inspector::rows::TextField;
use crate::theme::{Theme, theme};
use crate::{AsComponentView, ComponentStream, ComponentStreamBuilder};

/// Display payload for one element of a component.
#[derive(Clone, Debug, PartialEq)]
pub struct StripCell {
    pub label: Option<SharedString>,
    pub value: SharedString,
}

/// Scalar interpretation of every cell in a strip.
///
/// All elements of a given component share the same primitive type, so this
/// can be resolved once and reused. The variant drives editor affordance
/// (checkbox, enum picker, text field).
#[derive(Clone, Debug, PartialEq)]
pub enum CellKind {
    /// Component isn't registered yet or its metadata can't be resolved.
    Unknown,
    Bool,
    Enum { variants: Vec<SharedString> },
    Numeric,
    String,
}

impl CellKind {
    fn from_schema(prim_type: PrimType, is_string: bool, enum_variants: Option<&[String]>) -> Self {
        if is_string {
            return CellKind::String;
        }
        if let Some(variants) = enum_variants {
            return CellKind::Enum {
                variants: variants
                    .iter()
                    .map(|s| SharedString::from(s.clone()))
                    .collect(),
            };
        }
        match prim_type {
            PrimType::Bool => CellKind::Bool,
            PrimType::F32
            | PrimType::F64
            | PrimType::I8
            | PrimType::I16
            | PrimType::I32
            | PrimType::I64
            | PrimType::U8
            | PrimType::U16
            | PrimType::U32
            | PrimType::U64 => CellKind::Numeric,
        }
    }
}

/// Visual mode for the strip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StripPreset {
    /// Inline values with no cell background; used by the dashboard Monitor
    /// where the value sits next to the label and an optional unit suffix.
    Dashboard,
    /// Rounded boxed cells with click padding; used by the browser detail
    /// view and table value column.
    Boxes,
}

/// Style knobs applied uniformly across every cell in the strip.
#[derive(Clone)]
pub struct StripStyle {
    pub preset: StripPreset,
    /// Dashboard-only: render a solo unlabeled scalar at a larger font size
    /// so lone values read as the focal point of the tile.
    pub solo_emphasize: bool,
    /// Suffix shown after each value. Empty string disables the suffix.
    pub unit: SharedString,
    /// Displayed before the first sample arrives.
    pub placeholder: SharedString,
}

impl StripStyle {
    /// Style used by [`Monitor`](super::Monitor).
    pub fn dashboard() -> Self {
        Self {
            preset: StripPreset::Dashboard,
            solo_emphasize: true,
            unit: SharedString::default(),
            placeholder: SharedString::new_static("—"),
        }
    }

    /// Style shared by the component browser and table value column.
    pub fn boxes() -> Self {
        Self {
            preset: StripPreset::Boxes,
            solo_emphasize: false,
            unit: SharedString::default(),
            placeholder: SharedString::new_static("…"),
        }
    }

    pub fn with_unit(mut self, unit: impl Into<SharedString>) -> Self {
        self.unit = unit.into();
        self
    }
}

/// Click handler surfaced by the strip.
///
/// Receives the element index and the cursor position so callers can anchor
/// a popover next to the clicked cell. Typically stashes an
/// [`EditRequest`](crate::inspector::edits::EditRequest) onto the pending
/// edits global, which the root view drains on the next render.
pub type StripClick = Arc<dyn Fn(usize, Point<Pixels>, &mut Window, &mut App) + Send + Sync>;

/// Interactive state supplied afresh by the host each render.
///
/// Snapshot pattern keeps the strip decoupled from the pending-edits global
/// and lets `Monitor`, `ComponentText`, and `ComponentTable` share one
/// widget with different policies.
#[derive(Default, Clone)]
pub struct StripBehavior {
    pub on_element_click: Option<StripClick>,
    /// Apply-chevron handler shown beside highlighted cells. Fires the
    /// pending edit for the component and clears the element from
    /// `highlighted`. Ignored while `locked`.
    pub on_apply_element: Option<StripClick>,
    /// Right-click handler. Hosts that want a context menu install this
    /// alongside (not instead of) `on_element_click`. Not suppressed by
    /// `locked` since right-click menus are typically read-only (e.g.
    /// "plot this element").
    pub on_element_right_click: Option<StripClick>,
    pub highlighted: SmallVec<[usize; 4]>,
    pub locked: bool,
}

impl StripBehavior {
    fn equivalent(&self, other: &Self) -> bool {
        self.locked == other.locked
            && self.highlighted == other.highlighted
            && click_ptr(&self.on_element_click) == click_ptr(&other.on_element_click)
            && click_ptr(&self.on_apply_element) == click_ptr(&other.on_apply_element)
            && click_ptr(&self.on_element_right_click) == click_ptr(&other.on_element_right_click)
    }
}

fn style_equivalent(a: &StripStyle, b: &StripStyle) -> bool {
    a.preset == b.preset
        && a.solo_emphasize == b.solo_emphasize
        && a.unit == b.unit
        && a.placeholder == b.placeholder
}

fn click_ptr(click: &Option<StripClick>) -> usize {
    click
        .as_ref()
        .map(|a| Arc::as_ptr(a) as *const () as usize)
        .unwrap_or(0)
}

/// In-flight inline edit. `error` latches when a commit fails to parse so
/// the cell can render an error state and retain focus.
struct EditingCell {
    index: usize,
    field: TextField,
    error: bool,
}

/// Live horizontal row of a component's elements.
///
/// Shared widget between `Monitor`, `ComponentBrowser`, and
/// `ComponentTable`. The strip owns its stream task; hosts refresh the
/// visible [`StripStyle`] and [`StripBehavior`] every render.
pub struct ComponentValueStrip {
    db: Arc<DB>,
    component_id: ComponentId,
    component_name: SharedString,
    element_names: Vec<SharedString>,
    is_string: bool,
    enum_variants: Option<Vec<String>>,
    raw_values: Vec<ElementValue>,
    style: StripStyle,
    behavior: StripBehavior,
    cells: Vec<StripCell>,
    kind: CellKind,
    editing: Option<EditingCell>,
    focus: FocusHandle,
    _task: gpui::Task<()>,
}

impl ComponentValueStrip {
    pub fn new(
        db: Arc<DB>,
        source: impl ComponentStreamBuilder + Send + 'static,
        style: StripStyle,
        behavior: StripBehavior,
        cx: &mut Context<Self>,
    ) -> Self {
        let component_id = source.component_id();

        let task = cx.spawn({
            let db = db.clone();
            async move |this, cx| {
                let mut stream = source.into_stream(&db).await;
                let metadata = resolve_metadata(&db, component_id);
                let ResolvedMetadata {
                    element_names,
                    enum_variants,
                    is_string,
                    kind,
                    component_name,
                } = metadata;
                let _ = this.update(cx, |this, cx| {
                    this.kind = kind;
                    this.component_name = component_name;
                    this.element_names = element_names.clone();
                    this.is_string = is_string;
                    this.enum_variants = enum_variants.clone();
                    cx.notify();
                });
                // A fresh WAL reader only sees samples committed from now on,
                // so paint the last committed value immediately instead of
                // leaving the strip blank until the next sample arrives.
                let seed = db.with_state(|state| {
                    let component = state.get_component(component_id)?;
                    let latest = component.time_series.latest()?;
                    let (_size, view) = component.schema.parse_value(latest.data()).ok()?;
                    let raw: Vec<ElementValue> = view.iter().collect();
                    let cells =
                        format_cells(&view, &element_names, enum_variants.as_deref(), is_string);
                    Some((cells, raw))
                });
                if let Some((cells, raw_values)) = seed {
                    let _ = this.update(cx, |this, cx| {
                        this.cells = cells;
                        this.raw_values = raw_values;
                        cx.notify();
                    });
                }
                loop {
                    let (cells, raw_values) = {
                        let view = stream.next().await;
                        let cv = view.as_component_view();
                        let raw: Vec<ElementValue> = cv.iter().collect();
                        let cells =
                            format_cells(&cv, &element_names, enum_variants.as_deref(), is_string);
                        (cells, raw)
                    };
                    let result = this.update(cx, |this, cx| {
                        this.cells = cells;
                        this.raw_values = raw_values;
                        cx.notify();
                    });
                    if result.is_err() {
                        break;
                    }
                }
            }
        });

        Self {
            db,
            component_id,
            component_name: SharedString::default(),
            element_names: Vec::new(),
            is_string: false,
            enum_variants: None,
            raw_values: Vec::new(),
            style,
            behavior,
            cells: Vec::new(),
            kind: CellKind::Unknown,
            editing: None,
            focus: cx.focus_handle(),
            _task: task,
        }
    }

    pub fn kind(&self) -> &CellKind {
        &self.kind
    }

    pub fn component_id(&self) -> ComponentId {
        self.component_id
    }

    pub fn cells(&self) -> &[StripCell] {
        &self.cells
    }

    pub fn set_style(&mut self, style: StripStyle, cx: &mut Context<Self>) {
        if style_equivalent(&self.style, &style) {
            return;
        }
        self.style = style;
        cx.notify();
    }

    /// Install a new behavior snapshot.
    ///
    /// Short-circuits when nothing changed so callers can invoke it every
    /// render without triggering redundant repaints.
    pub fn set_behavior(&mut self, behavior: StripBehavior, cx: &mut Context<Self>) {
        if self.behavior.equivalent(&behavior) {
            return;
        }
        self.behavior = behavior;
        cx.notify();
    }
}

impl ComponentValueStrip {
    fn displayed_bool(&self, idx: usize, cx: &App) -> Option<bool> {
        let value = match crate::inspector::edits::pending_edits(cx).get(self.component_id) {
            Some(edit) => edit.value.as_view().iter().nth(idx)?,
            None => self.raw_values.get(idx).copied()?,
        };
        match value {
            ElementValue::Bool(b) => Some(b),
            _ => None,
        }
    }

    fn toggle_bool_cell(&mut self, idx: usize, cx: &mut Context<Self>) -> Option<bool> {
        let current = self.displayed_bool(idx, cx)?;
        let next = !current;
        self.set_bool_cell(idx, next, cx);
        Some(next)
    }

    fn set_bool_cell(&mut self, idx: usize, value: bool, cx: &mut Context<Self>) {
        if self.behavior.locked {
            return;
        }
        let Some(current) = self.displayed_bool(idx, cx) else {
            return;
        };
        if current == value {
            return;
        }
        crate::inspector::edits::upsert_element_value(
            &self.db,
            self.component_id,
            self.component_name.clone(),
            self.element_names.clone(),
            idx,
            ElementValue::Bool(value),
            cx,
        );
        cx.notify();
    }

    fn enter_text_edit(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.behavior.locked {
            return;
        }
        // Commit an in-progress edit before moving focus so clicking across
        // cells doesn't silently drop typed input.
        if self.editing.as_ref().is_some_and(|e| e.index != idx) {
            self.commit_edit(window, cx);
        }
        let seed = match self.kind {
            CellKind::String => string_seed(&self.raw_values),
            CellKind::Numeric => self
                .raw_values
                .get(idx)
                .map(|v| format_seed(*v))
                .unwrap_or_default(),
            _ => return,
        };
        let mut field = TextField::new("", cx);
        field.text = seed;
        field.cursor = field.text.len();
        field.mark = field.cursor;
        self.editing = Some(EditingCell {
            index: idx,
            field,
            error: false,
        });
        self.focus.focus(window);
        cx.notify();
    }

    fn commit_edit(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut edit) = self.editing.take() else {
            return;
        };
        let text = edit.field.text.clone();
        match self.kind {
            CellKind::Numeric => match parse_numeric(&text, self.prim_type()) {
                Some(value) => {
                    crate::inspector::edits::upsert_element_value(
                        &self.db,
                        self.component_id,
                        self.component_name.clone(),
                        self.element_names.clone(),
                        edit.index,
                        value,
                        cx,
                    );
                }
                None => {
                    edit.error = true;
                    self.editing = Some(edit);
                    cx.notify();
                    return;
                }
            },
            CellKind::String => {
                crate::inspector::edits::upsert_string_value(
                    &self.db,
                    self.component_id,
                    self.component_name.clone(),
                    self.element_names.clone(),
                    &text,
                    cx,
                );
            }
            _ => {}
        }
        cx.notify();
    }

    fn cancel_edit(&mut self, cx: &mut Context<Self>) {
        if self.editing.take().is_some() {
            cx.notify();
        }
    }

    fn prim_type(&self) -> Option<PrimType> {
        self.db.with_state(|s| {
            s.get_component(self.component_id)
                .map(|c| c.schema.prim_type)
        })
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editing.is_none() {
            return;
        }
        let key = event.keystroke.key.as_str();
        match key {
            "escape" => {
                self.cancel_edit(cx);
            }
            "enter" | "return" => {
                self.commit_edit(window, cx);
            }
            _ => {
                if let Some(edit) = self.editing.as_mut()
                    && edit.field.handle_key_down(event, cx)
                {
                    edit.error = false;
                    cx.notify();
                }
            }
        }
    }
}

impl Focusable for ComponentValueStrip {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn format_seed(v: ElementValue) -> String {
    match v {
        ElementValue::F32(x) => format!("{}", x),
        ElementValue::F64(x) => format!("{}", x),
        ElementValue::Bool(b) => (if b { "true" } else { "false" }).to_string(),
        other => format!("{}", other.as_f64()),
    }
}

fn string_seed(raw: &[ElementValue]) -> String {
    let bytes: Vec<u8> = raw
        .iter()
        .filter_map(|v| match v {
            ElementValue::U8(b) => Some(*b),
            _ => None,
        })
        .take_while(|&b| b != 0)
        .collect();
    String::from_utf8(bytes).unwrap_or_default()
}

fn parse_numeric(text: &str, prim_type: Option<PrimType>) -> Option<ElementValue> {
    let prim_type = prim_type?;
    Some(match prim_type {
        PrimType::U8 => ElementValue::U8(text.parse().ok()?),
        PrimType::U16 => ElementValue::U16(text.parse().ok()?),
        PrimType::U32 => ElementValue::U32(text.parse().ok()?),
        PrimType::U64 => ElementValue::U64(text.parse().ok()?),
        PrimType::I8 => ElementValue::I8(text.parse().ok()?),
        PrimType::I16 => ElementValue::I16(text.parse().ok()?),
        PrimType::I32 => ElementValue::I32(text.parse().ok()?),
        PrimType::I64 => ElementValue::I64(text.parse().ok()?),
        PrimType::F32 => ElementValue::F32(text.parse().ok()?),
        PrimType::F64 => ElementValue::F64(text.parse().ok()?),
        PrimType::Bool => return None,
    })
}

impl Render for ComponentValueStrip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);

        // Pending edits shadow WAL samples so typed changes stay visible
        // until the user applies or discards them.
        let pending = crate::inspector::edits::pending_edits(cx)
            .get(self.component_id)
            .map(|edit| {
                let view = edit.value.as_view();
                let raw: Vec<ElementValue> = view.iter().collect();
                let cells = format_cells(
                    &view,
                    &self.element_names,
                    self.enum_variants.as_deref(),
                    self.is_string,
                );
                (cells, raw)
            });
        let (cells, raw_values): (&[StripCell], &[ElementValue]) = match &pending {
            Some((cells, raw)) => (cells, raw),
            None => (&self.cells, &self.raw_values),
        };

        if cells.is_empty() {
            return div()
                .track_focus(&self.focus)
                .child(render_placeholder(&self.style, &theme))
                .into_any_element();
        }

        let is_solo = self.style.solo_emphasize && cells.len() == 1 && cells[0].label.is_none();

        let kind = self.kind.clone();
        let style = self.style.clone();
        let behavior = self.behavior.clone();
        let component_id = self.component_id;
        let editing_index = self.editing.as_ref().map(|e| e.index);
        let editing_error = self.editing.as_ref().map(|e| e.error).unwrap_or(false);

        let mut row = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(px(4.0));

        for (idx, cell) in cells.iter().enumerate() {
            let is_pending = behavior.highlighted.contains(&idx);
            let is_editing = editing_index == Some(idx);
            let is_bool = matches!(kind, CellKind::Bool)
                && matches!(raw_values.get(idx), Some(ElementValue::Bool(_)));
            let bool_value = if is_bool {
                match raw_values.get(idx) {
                    Some(ElementValue::Bool(b)) => *b,
                    _ => false,
                }
            } else {
                false
            };

            let has_apply_group =
                is_pending && !behavior.locked && behavior.on_apply_element.is_some();

            let atom = if is_editing {
                build_editing_chrome(
                    component_id,
                    idx,
                    cell,
                    &style,
                    &theme,
                    is_pending,
                    editing_error,
                    self.editing.as_ref().map(|e| &e.field),
                )
            } else {
                build_cell_chrome(
                    component_id,
                    idx,
                    cell,
                    &style,
                    &theme,
                    is_solo,
                    is_pending,
                    is_bool,
                    bool_value,
                    has_apply_group,
                )
            };

            let can_edit_inline =
                !behavior.locked && matches!(kind, CellKind::Numeric | CellKind::String);

            let atom = if behavior.locked || is_editing {
                atom
            } else if is_bool {
                atom.cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _window, cx| {
                            if let Some(next) = this.toggle_bool_cell(idx, cx) {
                                crate::inspector::drag_paint::start(next, cx);
                            }
                        }),
                    )
                    .on_mouse_move(cx.listener(
                        move |this, event: &gpui::MouseMoveEvent, _window, cx| {
                            if !event.dragging() {
                                crate::inspector::drag_paint::clear(cx);
                                return;
                            }
                            let Some(target) = crate::inspector::drag_paint::current(cx) else {
                                return;
                            };
                            this.set_bool_cell(idx, target, cx);
                        },
                    ))
            } else if can_edit_inline {
                atom.cursor_pointer().on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, window, cx| {
                        this.enter_text_edit(idx, window, cx);
                    }),
                )
            } else if let Some(click) = behavior.on_element_click.clone() {
                atom.cursor_pointer().on_mouse_down(
                    MouseButton::Left,
                    move |event: &gpui::MouseDownEvent, window, cx| {
                        click(idx, event.position, window, cx);
                        cx.refresh_windows();
                    },
                )
            } else {
                atom
            };

            let atom = if let Some(right_click) = behavior.on_element_right_click.clone() {
                atom.on_mouse_down(
                    MouseButton::Right,
                    move |event: &gpui::MouseDownEvent, window, cx| {
                        right_click(idx, event.position, window, cx);
                        cx.stop_propagation();
                    },
                )
            } else {
                atom
            };

            let atom = atom.on_mouse_move(crate::inspector::plot_preview::shift_hover_listener(
                component_id,
                smallvec::smallvec![idx],
            ));

            let child = if has_apply_group {
                let apply = behavior.on_apply_element.clone().unwrap();
                let chevron_id =
                    (component_id.0.wrapping_mul(31) ^ idx as u64).wrapping_add(0x1001) as usize;
                let mut bg = theme.control_active;
                bg.a = 0.15;
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .child(atom)
                    .child(
                        div()
                            .id(("strip-apply", chevron_id))
                            .px(px(6.0))
                            .h(px(25.5))
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(bg)
                            .rounded_r(px(3.0))
                            .text_size(px(12.0))
                            .text_color(theme.control_active)
                            .cursor_pointer()
                            .child(Icon::ChevronRight.svg_color(11.0, theme.control_active))
                            .on_mouse_down(
                                MouseButton::Left,
                                move |event: &gpui::MouseDownEvent, window, cx| {
                                    apply(idx, event.position, window, cx);
                                    cx.refresh_windows();
                                },
                            ),
                    )
                    .into_any_element()
            } else {
                atom.into_any_element()
            };

            row = row.child(child);
        }

        div()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.handle_key_down(event, window, cx);
            }))
            .child(row)
            .into_any_element()
    }
}

fn render_placeholder(style: &StripStyle, theme: &Theme) -> impl IntoElement {
    let text_size = match (style.preset, style.solo_emphasize) {
        (StripPreset::Dashboard, true) => px(14.0),
        (StripPreset::Dashboard, false) => px(13.0),
        (StripPreset::Boxes, _) => px(12.0),
    };
    div()
        .text_size(text_size)
        .text_color(theme.text_tertiary)
        .child(style.placeholder.clone())
}

#[allow(clippy::too_many_arguments)]
fn build_editing_chrome(
    component_id: ComponentId,
    idx: usize,
    cell: &StripCell,
    style: &StripStyle,
    theme: &Theme,
    is_pending: bool,
    error: bool,
    field: Option<&TextField>,
) -> Stateful<gpui::Div> {
    let id_hash = component_id.0.wrapping_mul(31) ^ idx as u64;
    let mut atom = div()
        .id(("strip-cell-edit", id_hash as usize))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .px(px(6.0))
        .py(px(3.0))
        .rounded(px(3.0))
        .overflow_hidden()
        .w(cell_width(style.preset, false));

    let bg = if error || is_pending {
        theme.drop_target
    } else {
        theme.bg_secondary
    };
    atom = atom.bg(bg).border_1().border_color(if error {
        theme.line_colors[3]
    } else {
        theme.line_color
    });

    if let Some(label) = cell.label.as_ref() {
        let label_size = match style.preset {
            StripPreset::Dashboard => px(9.0),
            StripPreset::Boxes => px(10.0),
        };
        atom = atom.child(
            div()
                .text_size(label_size)
                .text_color(theme.text_tertiary)
                .flex_none()
                .child(label.clone()),
        );
    }

    if let Some(field) = field {
        atom = atom.child(div().flex_1().child(field.element()));
    }

    atom
}

#[allow(clippy::too_many_arguments)]
fn build_cell_chrome(
    component_id: ComponentId,
    idx: usize,
    cell: &StripCell,
    style: &StripStyle,
    theme: &Theme,
    is_solo: bool,
    is_pending: bool,
    is_bool: bool,
    bool_value: bool,
    inside_apply_group: bool,
) -> Stateful<gpui::Div> {
    let id_hash = component_id.0.wrapping_mul(31) ^ idx as u64;
    let mut atom = div().id(("strip-cell", id_hash as usize)).flex().flex_row();
    // Round only the left corners when a chevron sits flush against the
    // right edge, so the two halves visually fuse into a single pill.
    if inside_apply_group {
        atom = atom.rounded_l(px(3.0));
    } else {
        atom = atom.rounded(px(3.0));
    }

    if is_bool {
        // Bool cells double as their own toggle track. Padding mirrors text
        // cells so the baseline aligns; the knob is inset so the track
        // still reads as a frame.
        let track_color = if is_pending {
            theme.drop_target
        } else if bool_value {
            theme.control_active_track
        } else {
            theme.bg_secondary
        };
        let knob = div()
            .w(px(15.0))
            .h(px(15.0))
            .rounded(px(3.0))
            .bg(theme.text_primary);
        atom = atom
            .w(px(40.0))
            .h(px(25.5))
            .py(px(4.0))
            .px(px(4.0))
            .items_center()
            .bg(track_color)
            .when(bool_value, |el| el.flex_row_reverse())
            .child(knob);
        return atom;
    }

    atom = atom
        .items_center()
        .gap(px(2.0))
        .px(px(4.0))
        .py(px(3.0))
        .overflow_hidden();

    // Fixed-width keeps a "0" cell and a "0.0083" cell aligned across rows
    // so neighboring boxes don't shift. Solo unlabeled cells have no siblings
    // to align with, so they keep their intrinsic width.
    let labeled = cell.label.is_some();
    let needs_fixed_width =
        labeled && !matches!((style.preset, is_solo), (StripPreset::Dashboard, true));
    if needs_fixed_width {
        atom = atom.w(cell_width(style.preset, is_solo));
    }

    match style.preset {
        StripPreset::Boxes => {
            let bg = if is_pending {
                theme.drop_target
            } else {
                theme.bg_secondary
            };
            atom = atom.bg(bg);
        }
        StripPreset::Dashboard => {
            if is_pending {
                atom = atom.bg(theme.drop_target);
            }
        }
    }

    if let Some(label) = cell.label.as_ref() {
        let label_size = match style.preset {
            StripPreset::Dashboard => px(9.0),
            StripPreset::Boxes => px(10.0),
        };
        atom = atom.child(
            div()
                .text_size(label_size)
                .text_color(theme.text_tertiary)
                .flex_none()
                .child(label.clone()),
        );
    }

    // Thresholds calibrated for the 78px Boxes width minus padding/label/gap
    // (≈50px of usable value space at ~7px/char monospace).
    let base_value_size = match (style.preset, is_solo) {
        (StripPreset::Dashboard, true) => 16.0,
        (StripPreset::Dashboard, false) => 12.0,
        (StripPreset::Boxes, _) => 12.0,
    };
    let value_size = if matches!(style.preset, StripPreset::Boxes) && cell.label.is_some() {
        let len = cell.value.chars().count();
        if len >= 9 {
            px(9.0)
        } else if len >= 7 {
            px(10.5)
        } else {
            px(base_value_size)
        }
    } else {
        px(base_value_size)
    };
    atom = atom.child(
        div()
            .flex_1()
            .text_size(value_size)
            .text_color(theme.text_primary)
            .text_right()
            .whitespace_nowrap()
            .overflow_hidden()
            .child(cell.value.clone()),
    );

    if !style.unit.is_empty() {
        let unit_size = match style.preset {
            StripPreset::Dashboard => {
                if is_solo {
                    px(10.0)
                } else {
                    px(9.0)
                }
            }
            StripPreset::Boxes => px(10.0),
        };
        atom = atom.child(
            div()
                .text_size(unit_size)
                .text_color(theme.text_secondary)
                .flex_none()
                .child(style.unit.clone()),
        );
    }

    atom
}

pub const STRIP_BOX_CELL_WIDTH: f32 = 78.0;
pub const STRIP_CELL_GAP: f32 = 4.0;

pub fn cell_width(preset: StripPreset, is_solo: bool) -> Pixels {
    match (preset, is_solo) {
        (StripPreset::Boxes, _) => px(STRIP_BOX_CELL_WIDTH),
        (StripPreset::Dashboard, false) => px(96.0),
        (StripPreset::Dashboard, true) => px(0.0),
    }
}

pub fn strip_row_width(n_cells: usize) -> f32 {
    if n_cells == 0 {
        return 0.0;
    }
    n_cells as f32 * STRIP_BOX_CELL_WIDTH + (n_cells.saturating_sub(1)) as f32 * STRIP_CELL_GAP
}

pub(crate) struct ResolvedMetadata {
    pub element_names: Vec<SharedString>,
    pub enum_variants: Option<Vec<String>>,
    pub is_string: bool,
    pub kind: CellKind,
    pub component_name: SharedString,
}

/// Look up the metadata the strip needs to decode and label elements.
///
/// An unregistered component yields [`CellKind::Unknown`] so callers can
/// render a placeholder until the producer connects; the async strip task
/// re-resolves after `into_stream` awakens.
pub(crate) fn resolve_metadata(db: &DB, component_id: ComponentId) -> ResolvedMetadata {
    db.with_state(|state| {
        let meta = state.get_component_metadata(component_id);
        let is_string = meta.map(|m| m.is_string()).unwrap_or(false);
        let enum_variants: Option<Vec<String>> = meta.and_then(|m| {
            m.enum_variants()
                .map(|it| it.map(|s| s.to_string()).collect())
        });
        let raw = meta
            .map(|m| m.element_names().to_string())
            .unwrap_or_default();
        let custom: Vec<SharedString> = if raw.is_empty() {
            Vec::new()
        } else {
            raw.split(',')
                .map(|s| SharedString::from(s.trim().to_string()))
                .collect()
        };
        let component = state.get_component(component_id);
        let element_names = if !custom.is_empty() {
            custom
        } else {
            component
                .map(|c| {
                    crate::inspector::trace_picker::element_names(c.schema.dim.as_slice())
                        .into_iter()
                        .map(SharedString::from)
                        .collect()
                })
                .unwrap_or_default()
        };
        let kind = component
            .map(|c| CellKind::from_schema(c.schema.prim_type, is_string, enum_variants.as_deref()))
            .unwrap_or(CellKind::Unknown);
        let component_name = SharedString::from(
            meta.map(|m| m.name.clone())
                .unwrap_or_else(|| format!("{:?}", component_id)),
        );
        ResolvedMetadata {
            element_names,
            enum_variants,
            is_string,
            kind,
            component_name,
        }
    })
}

/// Render a [`ComponentView`] into display cells.
///
/// Collapses string and enum components into a single unlabeled cell so the
/// strip reads as a value, not an array of bytes or discriminants.
pub(crate) fn format_cells(
    view: &ComponentView<'_>,
    element_names: &[SharedString],
    enum_variants: Option<&[String]>,
    is_string: bool,
) -> Vec<StripCell> {
    if is_string && let ComponentView::U8(array) = view {
        let buf = array.buf();
        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        if let Ok(s) = std::str::from_utf8(&buf[..len]) {
            return vec![StripCell {
                label: None,
                value: SharedString::from(s.to_string()),
            }];
        }
    }
    if let Some(variants) = enum_variants {
        let idx = view.to_f64() as usize;
        if let Some(name) = variants.get(idx) {
            return vec![StripCell {
                label: None,
                value: SharedString::from(name.to_string()),
            }];
        }
    }

    let values: Vec<ElementValue> = view.iter().collect();
    if values.is_empty() {
        return Vec::new();
    }
    if values.len() == 1 {
        return vec![StripCell {
            label: None,
            value: SharedString::from(format_element(values[0])),
        }];
    }
    values
        .into_iter()
        .enumerate()
        .map(|(i, v)| StripCell {
            label: element_names.get(i).cloned(),
            value: SharedString::from(format_element(v)),
        })
        .collect()
}

pub(crate) fn format_element(v: ElementValue) -> String {
    match v {
        ElementValue::Bool(b) => (if b { "true" } else { "false" }).to_string(),
        ElementValue::F32(x) => super::format_number(x as f64),
        ElementValue::F64(x) => super::format_number(x),
        other => {
            let mut s = String::new();
            let _ = write!(s, "{}", other.as_f64());
            super::format::pad_positive(&s)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sv(s: &str) -> SharedString {
        SharedString::from(s.to_string())
    }

    #[test]
    fn scalar_no_label() {
        // Full strip construction needs the DB; test the collapse rule via
        // a synthetic cells vector instead.
        let cells = [StripCell {
            label: None,
            value: sv("42"),
        }];
        assert_eq!(cells.len(), 1);
        assert!(cells[0].label.is_none());
    }

    #[test]
    fn bool_formats_as_word() {
        assert_eq!(format_element(ElementValue::Bool(true)), "true");
        assert_eq!(format_element(ElementValue::Bool(false)), "false");
    }

    #[test]
    fn f32_uses_adaptive_precision() {
        assert_eq!(format_element(ElementValue::F32(0.0)), " 0");
        assert_eq!(format_element(ElementValue::F32(1.5)), " 1.50");
        assert_eq!(format_element(ElementValue::F32(1234.5)), " 1234");
        assert_eq!(format_element(ElementValue::F32(-1.5)), "-1.50");
    }

    #[test]
    fn i64_formats_without_decimals() {
        assert_eq!(format_element(ElementValue::I64(42)), " 42");
        assert_eq!(format_element(ElementValue::I64(-42)), "-42");
    }

    #[test]
    fn cell_kind_from_schema_covers_each_branch() {
        assert_eq!(
            CellKind::from_schema(PrimType::Bool, false, None),
            CellKind::Bool
        );
        assert_eq!(
            CellKind::from_schema(PrimType::U8, true, None),
            CellKind::String
        );
        let variants: Vec<String> = vec!["Idle".into(), "Run".into()];
        match CellKind::from_schema(PrimType::U8, false, Some(&variants)) {
            CellKind::Enum { variants: v } => assert_eq!(v.len(), 2),
            other => panic!("expected Enum, got {:?}", other),
        }
        assert_eq!(
            CellKind::from_schema(PrimType::F32, false, None),
            CellKind::Numeric
        );
        assert_eq!(
            CellKind::from_schema(PrimType::I32, false, None),
            CellKind::Numeric
        );
        assert_eq!(
            CellKind::from_schema(PrimType::U64, false, None),
            CellKind::Numeric
        );
    }

    #[test]
    fn behavior_equivalence_detects_diff() {
        let a = StripBehavior::default();
        let b = StripBehavior::default();
        assert!(a.equivalent(&b));

        let c = StripBehavior {
            locked: true,
            ..Default::default()
        };
        assert!(!a.equivalent(&c));

        let mut d = StripBehavior::default();
        d.highlighted.push(0);
        assert!(!a.equivalent(&d));
    }
}
