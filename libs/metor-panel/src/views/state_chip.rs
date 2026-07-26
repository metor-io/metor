//! Discrete-state indicator: a number rendered as the state it means.
//!
//! Flight software encodes modes, laws, and health as small integers, and a
//! panel that shows `2` where the operator thinks in `POINTING` is making
//! them do the lookup. A chip carries that table itself — value, name, and
//! colour per state — so the mapping lives with the display instead of in
//! the reader's head.
//!
//! The chip is deliberately not a traffic light: it distinguishes *which*
//! state, not merely on from off, and an unlisted value is shown as unknown
//! rather than silently folded into "off".

use std::sync::Arc;

use gpui::{Context, Entity, Hsla, IntoElement, SharedString, Window, div, prelude::*, px};
use metor_db::DB;
use metor_proto::types::ComponentId;
use serde::{Deserialize, Serialize};

use super::binding::{ElementRef, component_meta, spawn_meta_resolver, spawn_scalar_stream};
use super::format_number;
use crate::theme::theme;

/// Values are integer codes on the wire but arrive as `f64`, so matching
/// tolerates the conversion rather than comparing exactly.
const MATCH_TOLERANCE: f64 = 0.5;

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
/// surfaces.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct StateChipConfig {
    pub component: String,
    pub element: usize,
    pub label: Option<String>,
    pub states: Vec<StateEntryConfig>,
    /// Shown when the live value matches no entry. Empty falls back to
    /// rendering the raw number, which is more useful than a blank chip when
    /// the table is out of date with the flight software.
    pub unknown_label: String,
}

/// Single-element discrete-state indicator.
#[derive(facet::Facet)]
pub struct StateChip {
    pub label: SharedString,
    pub unknown_label: SharedString,
    pub states: Vec<Entity<StateEntry>>,
    #[facet(skip)]
    component: SharedString,
    #[facet(opaque)]
    at: ElementRef,
    #[facet(skip)]
    value: Option<f64>,
    #[facet(opaque)]
    _task: gpui::Task<()>,
    #[facet(opaque)]
    _resolver_task: gpui::Task<()>,
}

impl StateChip {
    pub fn from_config(cfg: &StateChipConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let component_id = ComponentId::new(&cfg.component);
        let element = cfg.element;
        let meta = component_meta(&db, component_id);

        let task = spawn_scalar_stream(db.clone(), component_id, element, cx, |chip, value, cx| {
            chip.value = Some(value);
            cx.notify();
        });

        let keep_label = cfg.label.is_some();
        let resolver_task = spawn_meta_resolver(db, component_id, cx, move |chip, meta, cx| {
            if !keep_label {
                chip.label = super::meter::default_label(&meta.element_names, element, &meta.name);
            }
            cx.notify();
        });

        let states = cfg
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
            .collect();

        Self {
            label: cfg
                .label
                .clone()
                .map(SharedString::from)
                .unwrap_or_else(|| {
                    super::meter::default_label(&meta.element_names, element, &meta.name)
                }),
            unknown_label: SharedString::from(cfg.unknown_label.clone()),
            states,
            component: SharedString::from(cfg.component.clone()),
            at: ElementRef::new(component_id, element),
            value: None,
            _task: task,
            _resolver_task: resolver_task,
        }
    }

    pub fn to_config(&self, cx: &gpui::App) -> StateChipConfig {
        StateChipConfig {
            component: self.component.to_string(),
            element: self.at.element,
            label: Some(self.label.to_string()),
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

    pub fn component(&self) -> &SharedString {
        &self.component
    }
}

/// The entry whose code matches `value`, if the table lists one.
fn match_state(
    states: &[(f64, SharedString, Option<Hsla>)],
    value: f64,
) -> Option<&(f64, SharedString, Option<Hsla>)> {
    states
        .iter()
        .find(|(code, _, _)| (code - value).abs() < MATCH_TOLERANCE)
}

impl Render for StateChip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let states: Vec<(f64, SharedString, Option<Hsla>)> = self
            .states
            .iter()
            .map(|e| {
                let e = e.read(cx);
                (e.value, e.label.clone(), e.color)
            })
            .collect();

        let (text, accent) = match self.value {
            None => (SharedString::new_static("—"), None),
            Some(v) => match match_state(&states, v) {
                Some((_, label, color)) => {
                    (label.clone(), Some(color.unwrap_or(theme.control_active)))
                }
                // An unmatched code is not an error state, but it must not
                // masquerade as a known one either.
                None if self.unknown_label.is_empty() => {
                    (SharedString::from(format_number(v)), None)
                }
                None => (self.unknown_label.clone(), None),
            },
        };
        let accent = accent.unwrap_or(theme.text_tertiary);

        let mut tile = div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(3.0))
            .bg(theme.bg_primary)
            .p(px(6.0));

        if !self.label.is_empty() {
            tile = tile.child(
                div()
                    .text_size(px(10.0))
                    .text_color(theme.text_secondary)
                    .truncate()
                    .child(self.label.clone()),
            );
        }

        tile.child(
            div()
                .px(px(10.0))
                .py(px(3.0))
                .rounded(px(4.0))
                .bg(crate::theme::Theme::dim(accent, 0.18))
                .border_1()
                .border_color(accent)
                .text_size(px(13.0))
                .text_color(theme.text_primary)
                .truncate()
                .child(text),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> Vec<(f64, SharedString, Option<Hsla>)> {
        vec![
            (0.0, "IDLE".into(), None),
            (1.0, "SETTLING".into(), None),
            (2.0, "POINTING".into(), None),
            (3.0, "SAFE".into(), None),
        ]
    }

    #[test]
    fn integer_codes_match_their_state() {
        let t = table();
        assert_eq!(match_state(&t, 0.0).unwrap().1, "IDLE");
        assert_eq!(match_state(&t, 2.0).unwrap().1, "POINTING");
        assert_eq!(match_state(&t, 3.0).unwrap().1, "SAFE");
    }

    #[test]
    fn float_representation_error_still_matches() {
        let t = table();
        assert_eq!(match_state(&t, 2.0000001).unwrap().1, "POINTING");
        assert_eq!(match_state(&t, 1.9999999).unwrap().1, "POINTING");
    }

    #[test]
    fn codes_outside_the_table_do_not_match() {
        let t = table();
        assert!(match_state(&t, 7.0).is_none());
        assert!(match_state(&t, -1.0).is_none());
        // Exactly between two codes is not either of them.
        assert!(match_state(&t, 2.5).is_none());
    }

    #[test]
    fn an_empty_table_matches_nothing() {
        assert!(match_state(&[], 0.0).is_none());
    }
}
