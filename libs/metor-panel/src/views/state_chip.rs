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

use std::sync::Arc;

use gpui::{App, Context, Entity, Hsla, IntoElement, SharedString, Window, div, prelude::*, px};
use metor_db::DB;
use metor_proto::types::ComponentId;
use serde::{Deserialize, Serialize};

use super::binding;
use super::monitor::{behavior_snapshot, edit_click};
use super::value_strip::{ComponentValueStrip, StateTable, StripClick, StripStyle};

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
}

/// A bare value strip carrying a state table.
#[derive(facet::Facet)]
pub struct StateChip {
    /// What this chip reads. Editable: picking another component in the
    /// inspector rebinds the strip on the next frame. Declared first so the
    /// binding heads the inspector page — fields are walked in order.
    pub component_id: ComponentId,
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
    strip: Entity<ComponentValueStrip>,
    #[facet(opaque)]
    click: StripClick,
    #[facet(opaque)]
    _expression: Option<crate::dynamic::expressions::Expression>,
}

impl StateChip {
    /// A non-zero `element` folds into the `=` tier: one element of a
    /// component is an expression over it, so the chip's element selection
    /// rides the standard binding path and serializes back as the expression
    /// text with `element` 0.
    pub fn from_config(cfg: &StateChipConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let text = binding_text(cfg);
        let (component_id, expression) = match crate::dynamic::expressions::bind(&text, &db, cx) {
            Ok(bound) => (bound.id, bound.expression),
            Err(_) => (ComponentId::new(&text), None),
        };

        let name = binding::component_name(&db, component_id)
            .unwrap_or_else(|| SharedString::from(text.clone()));
        let click = edit_click(db.clone(), component_id, name);
        let strip = new_strip(db.clone(), component_id, click.clone(), cx);

        Self {
            component_id,
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
            db,
            strip,
            click,
            _expression: expression,
        }
    }

    pub fn to_config(&self, cx: &App) -> StateChipConfig {
        StateChipConfig {
            // Resolve the text for whatever is bound *now*, so a rebind is
            // what gets saved. An expression's component is named by a
            // content hash and labelled with the text that made it, so what
            // round-trips is the text — a name would rehydrate onto nothing.
            component: crate::dynamic::expressions::binding_text(&self.db, self.component_id)
                .or_else(|| {
                    binding::component_name(&self.db, self.component_id).map(|n| n.to_string())
                })
                .unwrap_or_else(|| self.saved_component.to_string()),
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
        }
    }

    /// Restart the strip when the inspector has re-pointed the chip.
    ///
    /// The state table is *kept*: it is the operator's translation of a code
    /// space, not something derived from the component, and rebinding
    /// between two components that share an encoding (a commanded mode and
    /// the reported one) is the reason to edit the binding at all.
    pub(crate) fn rebind(&mut self, cx: &mut Context<Self>) {
        if self.bound == Some(self.component_id) {
            return;
        }
        self.bound = Some(self.component_id);
        let component_id = self.component_id;
        self._expression = crate::dynamic::expressions::running(component_id, cx);
        if let Some(name) = binding::component_name(&self.db, component_id) {
            self.saved_component = name;
        }
        self.click = edit_click(self.db.clone(), component_id, self.saved_component.clone());
        self.strip = new_strip(self.db.clone(), component_id, self.click.clone(), cx);
    }

    fn style(&self, cx: &App) -> StripStyle {
        let states = (!self.states.is_empty()).then(|| StateTable {
            entries: self
                .states
                .iter()
                .map(|e| {
                    let e = e.read(cx);
                    (e.value, e.label.clone(), e.color)
                })
                .collect(),
            unknown_label: self.unknown_label.clone(),
        });
        StripStyle::boxes().with_states(states)
    }
}

/// The chip's element selection as binding text: element 0 is the component
/// itself; any other element is that expression over it.
fn binding_text(cfg: &StateChipConfig) -> String {
    use crate::dynamic::expressions::{body, is_expression};
    if cfg.element == 0 {
        cfg.component.clone()
    } else if is_expression(&cfg.component) {
        format!("=({})[{}]", body(&cfg.component), cfg.element)
    } else {
        format!("={}[{}]", cfg.component, cfg.element)
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
        let style = self.style(cx);
        let behavior =
            behavior_snapshot(cx, self.db.clone(), self.component_id, self.click.clone());
        self.strip.update(cx, |strip, cx| {
            strip.set_style(style, cx);
            strip.set_behavior(behavior, cx);
        });

        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p(px(4.0))
            .overflow_hidden()
            .child(self.strip.clone())
    }
}
