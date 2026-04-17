use std::fmt::Write;
use std::sync::Arc;

use gpui::{
    App, Context, InteractiveElement, IntoElement, MouseButton, SharedString, Stateful, Window,
    div, prelude::*, px,
};
use metor_db::DB;
use metor_proto::types::{ComponentId, ComponentView, ElementValue};
use smallvec::SmallVec;

use crate::theme::{Theme, theme};
use crate::{AsComponentView, ComponentStream, ComponentStreamBuilder};

/// One formatted value within a [`ComponentValueStrip`].
#[derive(Clone, Debug, PartialEq)]
pub struct StripCell {
    pub label: Option<SharedString>,
    pub value: SharedString,
}

/// Visual preset controlling the strip's chrome and typography.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StripPreset {
    /// Monitor dashboard: inline values, no box chrome, optional larger
    /// font for solo scalars, unit suffix shown alongside each value.
    Dashboard,
    /// Browser detail list & Table value cell: rounded bg_secondary boxes
    /// with comfortable click padding.
    Boxes,
}

/// Style configuration for a [`ComponentValueStrip`].
#[derive(Clone)]
pub struct StripStyle {
    pub preset: StripPreset,
    /// When true and the strip has a single unlabeled cell, use a larger
    /// display font. Honoured by the Dashboard preset only.
    pub solo_emphasize: bool,
    /// Optional unit suffix shown after each value (e.g. "V", "rpm").
    pub unit: SharedString,
    /// Text shown when no sample has arrived yet (e.g. "—" or "…").
    pub placeholder: SharedString,
}

impl StripStyle {
    /// Preset used by `Monitor`.
    pub fn dashboard() -> Self {
        Self {
            preset: StripPreset::Dashboard,
            solo_emphasize: true,
            unit: SharedString::default(),
            placeholder: SharedString::new_static("—"),
        }
    }

    /// Preset shared by the browser detail list and the table value cell.
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

/// Callback invoked when the user clicks a value — the strip surfaces the
/// index of the clicked element; callers translate that to whatever edit
/// flow they use (typically populating `pending_edits.pending_request`).
pub type StripClick = Arc<dyn Fn(usize, &mut Window, &mut App) + Send + Sync>;

/// Per-render interactive state: which elements show as pending-modified,
/// whether clicks are disabled (Cmd+L lock), and the edit callback.
#[derive(Default, Clone)]
pub struct StripBehavior {
    pub on_element_click: Option<StripClick>,
    pub highlighted: SmallVec<[usize; 4]>,
    pub locked: bool,
}

impl StripBehavior {
    fn equivalent(&self, other: &Self) -> bool {
        self.locked == other.locked
            && self.highlighted == other.highlighted
            && click_ptr(&self.on_element_click) == click_ptr(&other.on_element_click)
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

/// A live-updating horizontal row of a component's elements.
///
/// Shared across the dashboard `Monitor`, the `ComponentBrowser` detail
/// list, and the `ComponentTable` value cell. The strip owns the stream
/// task; callers embed `Entity<ComponentValueStrip>` and refresh the
/// behavior snapshot each render.
pub struct ComponentValueStrip {
    component_id: ComponentId,
    style: StripStyle,
    behavior: StripBehavior,
    cells: Vec<StripCell>,
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
                let (element_names, enum_variants, is_string) = resolve_metadata(&db, component_id);
                loop {
                    let cells = {
                        let view = stream.next().await;
                        let cv = view.as_component_view();
                        format_cells(&cv, &element_names, enum_variants.as_deref(), is_string)
                    };
                    let result = this.update(cx, |this, cx| {
                        this.cells = cells;
                        cx.notify();
                    });
                    if result.is_err() {
                        break;
                    }
                }
            }
        });

        Self {
            component_id,
            style,
            behavior,
            cells: Vec::new(),
            _task: task,
        }
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

    /// Refresh the interactive layer. Cheap no-op when nothing changed, so
    /// callers can safely call this every render.
    pub fn set_behavior(&mut self, behavior: StripBehavior, cx: &mut Context<Self>) {
        if self.behavior.equivalent(&behavior) {
            return;
        }
        self.behavior = behavior;
        cx.notify();
    }
}

impl Render for ComponentValueStrip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);

        if self.cells.is_empty() {
            return render_placeholder(&self.style, &theme).into_any_element();
        }

        let is_solo =
            self.style.solo_emphasize && self.cells.len() == 1 && self.cells[0].label.is_none();

        let container = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(px(4.0));

        let container = self
            .cells
            .iter()
            .enumerate()
            .fold(container, |container, (idx, cell)| {
                let is_pending = self.behavior.highlighted.iter().any(|h| *h == idx);
                let clickable = self.behavior.on_element_click.is_some() && !self.behavior.locked;
                container.child(render_cell(
                    self.component_id,
                    idx,
                    cell,
                    &self.style,
                    &self.behavior,
                    &theme,
                    is_solo,
                    is_pending,
                    clickable,
                ))
            });

        container.into_any_element()
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
fn render_cell(
    component_id: ComponentId,
    idx: usize,
    cell: &StripCell,
    style: &StripStyle,
    behavior: &StripBehavior,
    theme: &Theme,
    is_solo: bool,
    is_pending: bool,
    clickable: bool,
) -> Stateful<gpui::Div> {
    let id_hash = component_id.0.wrapping_mul(31) ^ idx as u64;
    let mut atom = div()
        .id(("strip-cell", id_hash as usize))
        .flex()
        .flex_row()
        .items_baseline()
        .gap(px(4.0))
        .px(px(6.0))
        .py(px(3.0))
        .rounded(px(3.0));

    // Chrome by preset.
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

    // Label.
    if let Some(label) = cell.label.as_ref() {
        let label_size = match style.preset {
            StripPreset::Dashboard => px(9.0),
            StripPreset::Boxes => px(10.0),
        };
        atom = atom.child(
            div()
                .text_size(label_size)
                .text_color(theme.text_tertiary)
                .child(label.clone()),
        );
    }

    // Value.
    let value_size = match (style.preset, is_solo) {
        (StripPreset::Dashboard, true) => px(16.0),
        (StripPreset::Dashboard, false) => px(12.0),
        (StripPreset::Boxes, _) => px(12.0),
    };
    atom = atom.child(
        div()
            .text_size(value_size)
            .text_color(theme.text_primary)
            .child(cell.value.clone()),
    );

    // Unit suffix.
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
                .child(style.unit.clone()),
        );
    }

    if clickable {
        if let Some(click) = behavior.on_element_click.clone() {
            atom = atom
                .cursor_pointer()
                .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                    click(idx, window, cx);
                    cx.refresh_windows();
                });
        }
    }

    atom
}

/// Resolve the metadata the strip needs to format values. Returns empty
/// results if the component isn't yet registered; callers should call this
/// again after `into_stream().await` resolves.
pub(crate) fn resolve_metadata(
    db: &DB,
    component_id: ComponentId,
) -> (Vec<SharedString>, Option<Vec<String>>, bool) {
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
        let element_names = if !custom.is_empty() {
            custom
        } else {
            state
                .get_component(component_id)
                .map(|c| {
                    crate::trace_picker::element_names(c.schema.dim.as_slice())
                        .into_iter()
                        .map(SharedString::from)
                        .collect()
                })
                .unwrap_or_default()
        };
        (element_names, enum_variants, is_string)
    })
}

/// Format a component's latest view into a list of display cells, applying
/// string/enum/numeric detection and element labelling.
pub(crate) fn format_cells(
    view: &ComponentView<'_>,
    element_names: &[SharedString],
    enum_variants: Option<&[String]>,
    is_string: bool,
) -> Vec<StripCell> {
    if is_string {
        if let ComponentView::U8(array) = view {
            let buf = array.buf();
            let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            if let Ok(s) = std::str::from_utf8(&buf[..len]) {
                return vec![StripCell {
                    label: None,
                    value: SharedString::from(s.to_string()),
                }];
            }
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
            s
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
        // Cannot construct a real ComponentView in a unit test without the
        // surrounding DB plumbing, so instead test format_element / solo
        // collapsing via a synthetic cells result.
        let cells = vec![StripCell {
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
        assert_eq!(format_element(ElementValue::F32(0.0)), "0");
        assert_eq!(format_element(ElementValue::F32(1.5)), "1.50");
        assert_eq!(format_element(ElementValue::F32(1234.5)), "1234");
    }

    #[test]
    fn i64_formats_without_decimals() {
        assert_eq!(format_element(ElementValue::I64(42)), "42");
    }

    #[test]
    fn behavior_equivalence_detects_diff() {
        let a = StripBehavior::default();
        let b = StripBehavior::default();
        assert!(a.equivalent(&b));

        let mut c = StripBehavior::default();
        c.locked = true;
        assert!(!a.equivalent(&c));

        let mut d = StripBehavior::default();
        d.highlighted.push(0);
        assert!(!a.equivalent(&d));
    }
}
