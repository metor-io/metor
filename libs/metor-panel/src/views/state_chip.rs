//! Discrete-state indicator: a value strip whose cells render as the states
//! they mean.
//!
//! Flight software encodes modes, laws, and health as small integers, and a
//! panel that shows `2` where the operator thinks in `POINTING` is making
//! them do the lookup. The chip carries that table itself — value, name, and
//! colour per state — and hands it to a plain [`ComponentValueStrip`], so a
//! state reads exactly like every other value cell on the panel: same boxes,
//! same staleness tint, same click-to-edit. Nothing else is drawn — no
//! label, no chrome — which is what lets a chip sit inside a schematic.
//!
//! The same table drives a second face, [`StateChipStyle::Toggle`]: every
//! state as one segment of a switch, the live one lit, and a click commands
//! that state's code. Arming a payload or enabling a heater is a choice
//! between named states, so the control that makes the choice is the table
//! that names them.

use std::sync::Arc;

use gpui::{
    AnyElement, App, Context, Entity, FontWeight, Hsla, IntoElement, MouseButton, SharedString,
    Window, div, prelude::*,
};
use metor_db::DB;
use metor_proto::types::ComponentId;
use serde::{Deserialize, Serialize};

use super::binding::{self, CodeFeed};
use super::monitor::{behavior_snapshot, edit_click};
use super::value_strip::{ComponentValueStrip, StateTable, StripClick, StripStyle};

use crate::dynamic::tensor::TypedScalar;
use crate::inspect;
use crate::theme::{Theme, theme, ui_px};

/// How a chip draws its table.
#[derive(Clone, Copy, Debug, Default, PartialEq, facet::Facet, Serialize, Deserialize)]
#[repr(u8)]
pub enum StateChipStyle {
    /// The live value as the one state it means.
    #[default]
    Chip,
    /// Every state as a segment of a switch; clicking one commands it.
    Toggle,
}

/// One row of a chip's lookup table.
#[derive(facet::Facet)]
#[facet(pod)]
pub struct StateEntry {
    pub label: SharedString,
    pub value: f64,
    pub color: Option<Hsla>,
}

impl StateEntry {
    pub fn empty() -> Self {
        Self {
            label: SharedString::new_static(""),
            value: 0.0,
            color: None,
        }
    }
}

/// Serialized form of one [`StateEntry`].
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct StateEntryConfig {
    pub value: f64,
    pub label: String,
    pub color: Option<Hsla>,
}

/// Persisted shape of a [`StateChip`], shared by the tile and dashboard
/// surfaces, target-shipped presets, and the Python config API.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct StateChipConfig {
    pub component: String,
    pub element: usize,
    /// Names the chip in tab titles and widget lists; the chip itself draws
    /// no label.
    pub label: Option<String>,
    pub states: Vec<StateEntryConfig>,
    /// Shown when the live value matches no entry. Empty falls back to
    /// rendering the raw number, which is more useful than a blank chip when
    /// the table is out of date with the flight software.
    pub unknown_label: String,
    pub style: StateChipStyle,
    /// Text size in unscaled pixels; the switch's padding and corners follow
    /// it, so this one number sizes the whole control. 0 is the face's own
    /// default.
    pub text_size: f32,
}

/// A bare value strip carrying a state table.
#[derive(facet::Facet)]
pub struct StateChip {
    /// What this chip reads. Editable: picking another component in the
    /// inspector rebinds the strip on the next frame. Declared first so the
    /// binding heads the inspector page — fields are walked in order.
    pub source: crate::data_binding::Binding,
    pub style: StateChipStyle,
    #[facet(inspect::label = "Text size (0 = auto)")]
    #[facet(inspect::range(min = "0.0", max = "96.0"))]
    pub text_size: f32,
    pub states: Vec<Entity<StateEntry>>,
    pub unknown_label: SharedString,
    /// Carried for the tab title and the config round trip; never drawn.
    #[facet(skip)]
    label: Option<SharedString>,
    /// Persistence fallback when the bound id resolves to nothing at save
    /// time: the text the layout was built from. Never a debug-formatted
    /// id — a saved layout re-hashes this to recover the id, so a
    /// `"ComponentId(…)"` would silently rebind the chip on the next load.
    #[facet(skip)]
    saved_component: SharedString,
    /// What the strip is actually reading, compared against the editable
    /// `component_id` each frame.
    #[facet(opaque)]
    bound: Option<ComponentId>,
    #[facet(opaque)]
    db: Arc<DB>,
    #[facet(opaque)]
    _binding_changes: gpui::Task<()>,
    #[facet(opaque)]
    strip: Entity<ComponentValueStrip>,
    #[facet(opaque)]
    click: StripClick,
    /// The toggle's own read of the bound value; only streams while the
    /// chip is drawn as a toggle.
    #[facet(opaque)]
    feed: CodeFeed,
}

impl StateChip {
    /// A non-zero `element` folds into the `=` tier: one element of a
    /// component is an expression over it, so the chip's element selection
    /// rides the standard binding path and serializes back as the expression
    /// text with `element` 0.
    pub fn from_config(cfg: &StateChipConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let text = binding_text(cfg);
        let source = crate::data_binding::Binding::from_text(&text, &db, cx);
        let component_id = source.id();

        let name = binding::component_name(&db, component_id)
            .unwrap_or_else(|| SharedString::from(text.clone()));
        let click = edit_click(db.clone(), component_id, name);
        let strip = new_strip(db.clone(), component_id, click.clone(), cx);

        Self {
            source,
            style: cfg.style,
            text_size: cfg.text_size,
            states: cfg
                .states
                .iter()
                .map(|s| {
                    let s = s.clone();
                    cx.new(|_| StateEntry {
                        label: SharedString::from(s.label),
                        value: s.value,
                        color: s.color,
                    })
                })
                .collect(),
            unknown_label: SharedString::from(cfg.unknown_label.clone()),
            label: cfg.label.clone().map(SharedString::from),
            saved_component: SharedString::from(text),
            bound: Some(component_id),
            _binding_changes: crate::data_binding::watch_registrations(db.clone(), cx),
            db,
            strip,
            click,
            feed: CodeFeed::default(),
        }
    }

    pub fn to_config(&self, cx: &App) -> StateChipConfig {
        StateChipConfig {
            // Resolve the text for whatever is bound *now*, so a rebind is
            // what gets saved. An expression's component is named by a
            // content hash and labelled with the text that made it, so what
            // round-trips is the text — a name would rehydrate onto nothing.
            component: self.source.text(&self.db),
            element: 0,
            label: self.label.as_ref().map(|l| l.to_string()),
            states: self
                .states
                .iter()
                .map(|e| {
                    let e = e.read(cx);
                    StateEntryConfig {
                        value: e.value,
                        label: e.label.to_string(),
                        color: e.color,
                    }
                })
                .collect(),
            unknown_label: self.unknown_label.to_string(),
            style: self.style,
            text_size: self.text_size,
        }
    }

    /// Restart the strip when the inspector has re-pointed the chip.
    ///
    /// The state table is *kept*: it is the operator's translation of a code
    /// space, not something derived from the component, and rebinding
    /// between two components that share an encoding (a commanded mode and
    /// the reported one) is the reason to edit the binding at all.
    pub(crate) fn rebind(&mut self, cx: &mut Context<Self>) {
        self.source.resolve(&self.db, cx);
        if self.bound == Some(self.source.id()) {
            return;
        }
        self.bound = Some(self.source.id());
        let component_id = self.source.id();

        if let Some(name) = binding::component_name(&self.db, component_id) {
            self.saved_component = name;
        }
        self.click = edit_click(self.db.clone(), component_id, self.saved_component.clone());
        self.strip = new_strip(self.db.clone(), component_id, self.click.clone(), cx);
        self.feed = CodeFeed::default();
    }

    /// Stream the bound value for the toggle face, or stop when the chip is
    /// a chip again.
    fn refresh_feed(&mut self, cx: &mut Context<Self>) {
        let want = (self.style == StateChipStyle::Toggle).then(|| self.source.id());
        let db = self.db.clone();
        self.feed.follow(want, &db, cx, |chip| &mut chip.feed);
    }

    /// Whether a click may command the bound component right now: the
    /// operator unlocked edits, the panel is showing live data (a scrubbed
    /// past can't be commanded), and the binding names a real component —
    /// an expression's output is computed here, not commandable there.
    fn writable(&self, cx: &App) -> bool {
        !crate::inspector::edits::pending_edits(cx).locked
            && crate::temporal::is_live(cx)
            && !self.source.is_unbound()
            && self.source.expression_text().is_none()
    }

    /// Command the bound component to `code`, written as its own type so a
    /// bool flag receives a bool.
    fn command(&mut self, code: f64, cx: &mut Context<Self>) {
        if !self.writable(cx) {
            return;
        }
        let id = self.source.id();
        let Some(prim) = self
            .db
            .with_state(|s| s.get_component(id).map(|c| c.schema.prim_type))
        else {
            return;
        };
        let value = TypedScalar::from_f64(code, prim).into();
        if crate::inspector::edits::command_element(&self.db, id, 0, value) {
            cx.notify();
        }
    }

    fn render_toggle(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx);
        let entries = self
            .states
            .iter()
            .map(|e| {
                let e = e.read(cx);
                (e.value, e.label.clone(), e.color)
            })
            .collect();
        let table = StateTable {
            entries: toggle_segments(entries, self.feed.is_bool),
            unknown_label: self.unknown_label.clone(),
        };
        let enabled = self.writable(cx);
        let active = self.feed.value.and_then(|v| table.position(v));
        let stale = self.feed.stale;
        let size = toggle_text_size(self.text_size);
        let em = |n: f32| ui_px(n * size);

        // Sized to its labels rather than the tile, so a wide widget doesn't
        // stretch a two-position switch into a bar.
        let mut track = div()
            .flex()
            .flex_row()
            .flex_none()
            .max_w_full()
            .items_center()
            .gap(em(0.15))
            .p(em(0.15))
            .rounded(em(0.35))
            .bg(theme.bg_primary)
            .border_1()
            .border_color(theme.line_color)
            .text_size(ui_px(size))
            .when(!enabled, |track| track.opacity(0.55));

        if table.entries.is_empty() {
            return track
                .child(
                    div()
                        .px(em(0.6))
                        .py(em(0.2))
                        .text_center()
                        .text_color(theme.text_tertiary)
                        .child("No states"),
                )
                .into_any_element();
        }

        for (i, (code, label, color)) in table.entries.iter().cloned().enumerate() {
            let is_active = active == Some(i);
            let accent = color.unwrap_or(theme.control_active);
            let segment = div()
                .id(("toggle-segment", i))
                .flex_shrink_1()
                .min_w_0()
                .px(em(0.6))
                .py(em(0.2))
                .rounded(em(0.25))
                .border_1()
                .text_center()
                .whitespace_nowrap()
                .overflow_hidden()
                .child(label);
            let segment = if is_active {
                segment
                    .bg(if stale {
                        theme.stale_bg()
                    } else {
                        Theme::dim(accent, 0.25)
                    })
                    .border_color(if stale { theme.stale_text() } else { accent })
                    .text_color(if stale {
                        theme.stale_text()
                    } else {
                        theme.text_primary
                    })
                    .font_weight(FontWeight::SEMIBOLD)
            } else {
                segment
                    .border_color(gpui::transparent_black())
                    .text_color(theme.text_tertiary)
            };
            let segment = if enabled && !is_active {
                let hover = theme.bg_secondary;
                segment
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |chip, _, _window, cx| {
                            chip.command(code, cx);
                            cx.stop_propagation();
                        }),
                    )
            } else {
                segment
            };
            track = track.child(segment);
        }

        // A code the table doesn't list lights nothing; the unknown label,
        // when set, says so in a segment of its own that can't be clicked.
        if self.feed.value.is_some() && active.is_none() && !table.unknown_label.is_empty() {
            track = track.child(
                div()
                    .flex_none()
                    .px(em(0.6))
                    .py(em(0.2))
                    .rounded(em(0.25))
                    .border_1()
                    .border_color(theme.error_accent)
                    .bg(Theme::dim(theme.error_accent, 0.18))
                    .text_color(theme.text_primary)
                    .whitespace_nowrap()
                    .child(table.unknown_label.clone()),
            );
        }

        track.into_any_element()
    }

    fn style(&self, cx: &App) -> StripStyle {
        let entries = self
            .states
            .iter()
            .map(|e| {
                let e = e.read(cx);
                (e.value, e.label.clone(), e.color)
            })
            .collect();
        StripStyle::boxes()
            .with_states(state_table(entries, &self.unknown_label))
            .with_value_size((self.text_size > 0.0).then_some(self.text_size))
    }
}

/// The table a chip hands its strip. An unknown label alone is still a
/// table: it is what the chip says for every value it can't name.
fn state_table(
    entries: Vec<(f64, SharedString, Option<Hsla>)>,
    unknown_label: &SharedString,
) -> Option<StateTable> {
    (!entries.is_empty() || !unknown_label.is_empty()).then(|| StateTable {
        entries,
        unknown_label: unknown_label.clone(),
    })
}

/// A switch's text size: the configured one, or a compact default that
/// leaves room in a small widget.
fn toggle_text_size(configured: f32) -> f32 {
    if configured > 0.0 {
        configured.min(96.0)
    } else {
        10.0
    }
}

/// A toggle's segments: the table as configured, or OFF/ON for a bool
/// flag nobody has named yet — a switch with no positions isn't one.
fn toggle_segments(
    entries: Vec<(f64, SharedString, Option<Hsla>)>,
    is_bool: bool,
) -> Vec<(f64, SharedString, Option<Hsla>)> {
    if entries.is_empty() && is_bool {
        default_toggle_states()
            .into_iter()
            .map(|s| (s.value, SharedString::from(s.label), s.color))
            .collect()
    } else {
        entries
    }
}

/// The two positions a fresh toggle starts with.
pub fn default_toggle_states() -> Vec<StateEntryConfig> {
    vec![
        StateEntryConfig {
            value: 0.0,
            label: "OFF".into(),
            color: None,
        },
        StateEntryConfig {
            value: 1.0,
            label: "ON".into(),
            color: None,
        },
    ]
}

/// The chip's element selection as binding text: element 0 is the component
/// itself; any other element is that expression over it.
fn binding_text(cfg: &StateChipConfig) -> String {
    element_binding_text(&cfg.component, cfg.element)
}

/// `component` narrowed to one element, as binding text. Shared by the
/// views whose config names an element but whose binding is one component.
pub(crate) fn element_binding_text(component: &str, element: usize) -> String {
    use crate::dynamic::expressions::{body, is_expression};
    if element == 0 {
        component.to_string()
    } else if is_expression(component) {
        format!("=({})[{}]", body(component), element)
    } else {
        format!("={component}[{element}]")
    }
}

fn new_strip(
    db: Arc<DB>,
    component_id: ComponentId,
    click: StripClick,
    cx: &mut Context<StateChip>,
) -> Entity<ComponentValueStrip> {
    let behavior = behavior_snapshot(cx, db.clone(), component_id, click);
    cx.new(|cx| {
        ComponentValueStrip::new(db.clone(), component_id, StripStyle::boxes(), behavior, cx)
    })
}

impl Render for StateChip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.rebind(cx);
        self.refresh_feed(cx);
        let body = match self.style {
            StateChipStyle::Toggle => self.render_toggle(cx),
            StateChipStyle::Chip => self.render_chip(cx),
        };
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p(ui_px(4.0))
            .overflow_hidden()
            .child(body)
    }
}

impl StateChip {
    fn render_chip(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let style = self.style(cx);
        let behavior = behavior_snapshot(cx, self.db.clone(), self.source.id(), self.click.clone());
        self.strip.update(cx, |strip, cx| {
            strip.set_style(style, cx);
            strip.set_behavior(behavior, cx);
        });
        self.strip.clone().into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_label_without_states_still_makes_a_table() {
        let unknown = SharedString::from("UNKNOWN");
        let table = state_table(Vec::new(), &unknown).expect("label alone is a table");
        assert_eq!(table.unknown_label, unknown);
        assert!(state_table(Vec::new(), &SharedString::default()).is_none());
        let named = vec![(0.0, SharedString::from("IDLE"), None)];
        assert!(state_table(named, &SharedString::default()).is_some());
    }

    #[test]
    fn an_unnamed_bool_toggle_gets_off_and_on() {
        let segments = toggle_segments(Vec::new(), true);
        let labels: Vec<_> = segments.iter().map(|s| s.1.to_string()).collect();
        assert_eq!(labels, ["OFF", "ON"]);
        assert_eq!((segments[0].0, segments[1].0), (0.0, 1.0));

        // A numeric component has no implied positions, and a configured
        // table always wins.
        assert!(toggle_segments(Vec::new(), false).is_empty());
        let armed = vec![(1.0, SharedString::from("ARMED"), None)];
        assert_eq!(toggle_segments(armed.clone(), true), armed);
    }

    #[test]
    fn a_chip_saved_before_toggles_loads_as_a_chip() {
        let cfg: StateChipConfig =
            serde_json::from_str(r#"{"component":"sat.mode","states":[]}"#).unwrap();
        assert_eq!(cfg.style, StateChipStyle::Chip);
        let toggle: StateChipConfig = serde_json::from_str(r#"{"style":"Toggle"}"#).unwrap();
        assert_eq!(toggle.style, StateChipStyle::Toggle);
    }
}
