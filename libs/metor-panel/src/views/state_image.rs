//! Discrete-state indicator drawn as pictures: one image per state.
//!
//! A valve schematic reads faster as an open or a closed valve than as
//! `OPEN`; a deployment mechanism as the stowed or deployed drawing. The
//! table is the state chip's — code, name — with an image in place of a
//! colour, and the view draws whichever image the live code names.
//!
//! Three moments have no state to draw, and each gets its own image so an
//! operator never mistakes one for another: the binding is broken (no such
//! component, or its expression failed), the component exists but has sent
//! nothing, and it sent a code the table doesn't list. An unset image for
//! any of them falls back to themed text saying which it is.

use std::sync::Arc;

use gpui::{App, Context, Entity, Hsla, IntoElement, SharedString, Window, div, prelude::*, px};
use metor_db::DB;
use serde::{Deserialize, Serialize};

use super::binding::CodeFeed;
use super::image_source::{CachedImage, ImageRef, LoadedImage, image_canvas};
use super::state_chip::element_binding_text;
use super::value_strip::StateTable;

use crate::data_binding::Binding;
use crate::inspect;
use crate::theme::{theme, ui_px};

/// Serialized form of one [`StateImageEntry`].
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct StateImageEntryConfig {
    pub value: f64,
    /// Drawn in place of the image when it is unset or fails to load.
    pub label: String,
    pub path: String,
    /// Inline base64 bytes, which win over `path` (see [`ImageRef`]).
    pub data: String,
}

/// Persisted shape of a [`StateImage`], shared by the tile and dashboard
/// surfaces and the Python config API.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct StateImageConfig {
    pub component: String,
    pub element: usize,
    /// Names the view in tab titles and widget lists; never drawn.
    pub label: Option<String>,
    pub states: Vec<StateImageEntryConfig>,
    /// The live code matches no state.
    pub unknown: ImageRef,
    /// The component is known but has no sample at the selected instant.
    pub no_data: ImageRef,
    /// The binding is broken: unbound, no such component, or a failed
    /// expression.
    pub error: ImageRef,
}

/// One row of the table: a code and the picture it means.
#[derive(facet::Facet)]
pub struct StateImageEntry {
    pub label: SharedString,
    pub value: f64,
    /// Image file. Editing it replaces an image inlined by a preset.
    pub path: SharedString,
    #[facet(opaque)]
    image: CachedImage,
}

impl StateImageEntry {
    pub fn empty() -> Self {
        Self {
            label: SharedString::default(),
            value: 0.0,
            path: SharedString::default(),
            image: CachedImage::default(),
        }
    }

    fn from_config(cfg: &StateImageEntryConfig) -> Self {
        Self {
            label: cfg.label.clone().into(),
            value: cfg.value,
            path: cfg.path.clone().into(),
            image: CachedImage::new(&ImageRef {
                path: cfg.path.clone(),
                data: cfg.data.clone(),
            }),
        }
    }

    fn to_config(&self) -> StateImageEntryConfig {
        let image = self.image.image_ref(&self.path);
        StateImageEntryConfig {
            value: self.value,
            label: self.label.to_string(),
            path: image.path,
            data: image.data,
        }
    }
}

/// The images for the moments with no state to draw, decoded once each.
#[derive(Default)]
struct SpecialImages {
    unknown: CachedImage,
    no_data: CachedImage,
    error: CachedImage,
}

/// A state table whose states are images.
#[derive(facet::Facet)]
pub struct StateImage {
    /// Declared first so the binding heads the inspector page.
    pub source: Binding,
    pub states: Vec<Entity<StateImageEntry>>,
    #[facet(inspect::label = "Unknown value image")]
    pub unknown_image: SharedString,
    #[facet(inspect::label = "No data image")]
    pub no_data_image: SharedString,
    #[facet(inspect::label = "Error image")]
    pub error_image: SharedString,
    /// Carried for the tab title and the config round trip; never drawn.
    #[facet(skip)]
    label: Option<SharedString>,
    #[facet(opaque)]
    special: SpecialImages,
    #[facet(opaque)]
    db: Arc<DB>,
    #[facet(opaque)]
    _binding_changes: gpui::Task<()>,
    #[facet(opaque)]
    feed: CodeFeed,
}

/// What the view draws this frame.
#[derive(Debug, PartialEq)]
enum Shown {
    /// The binding can't produce data, and why.
    Error(SharedString),
    NoData,
    /// The index of the matching state.
    State(usize),
    /// A code the table doesn't list.
    Unknown(f64),
}

/// The precedence is the operator's: a broken binding outranks silence,
/// which outranks any value — an unknown code is at least a live one.
fn shown(error: Option<SharedString>, value: Option<f64>, table: &StateTable) -> Shown {
    if let Some(why) = error {
        return Shown::Error(why);
    }
    match value {
        None => Shown::NoData,
        Some(value) => match table.position(value) {
            Some(i) => Shown::State(i),
            None => Shown::Unknown(value),
        },
    }
}

impl StateImage {
    /// A non-zero `element` folds into the `=` tier, as on a state chip, and
    /// saves back as that expression with `element` 0.
    pub fn from_config(cfg: &StateImageConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let text = element_binding_text(&cfg.component, cfg.element);
        let source = Binding::from_text(&text, &db, cx);
        Self {
            source,
            states: cfg
                .states
                .iter()
                .map(|s| cx.new(|_| StateImageEntry::from_config(s)))
                .collect(),
            unknown_image: cfg.unknown.path.clone().into(),
            no_data_image: cfg.no_data.path.clone().into(),
            error_image: cfg.error.path.clone().into(),
            label: cfg.label.clone().map(SharedString::from),
            special: SpecialImages {
                unknown: CachedImage::new(&cfg.unknown),
                no_data: CachedImage::new(&cfg.no_data),
                error: CachedImage::new(&cfg.error),
            },
            _binding_changes: crate::data_binding::watch_registrations(db.clone(), cx),
            db,
            feed: CodeFeed::default(),
        }
    }

    pub fn to_config(&self, cx: &App) -> StateImageConfig {
        StateImageConfig {
            component: self.source.text(&self.db),
            element: 0,
            label: self.label.as_ref().map(|l| l.to_string()),
            states: self.states.iter().map(|e| e.read(cx).to_config()).collect(),
            unknown: self.special.unknown.image_ref(&self.unknown_image),
            no_data: self.special.no_data.image_ref(&self.no_data_image),
            error: self.special.error.image_ref(&self.error_image),
        }
    }

    /// Why the binding can't produce data, if it can't.
    fn binding_error(&self) -> Option<SharedString> {
        if self.source.is_unbound() {
            return Some("No component".into());
        }
        if let Some(error) = self.source.error() {
            return Some(error.into());
        }
        let id = self.source.id();
        let registered = self.db.with_state(|s| s.get_component(id).is_some());
        (!registered).then(|| format!("Unknown component: {}", self.source.text(&self.db)).into())
    }
}

impl Render for StateImage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.source.resolve(&self.db, cx);
        let db = self.db.clone();
        self.feed
            .follow(Some(self.source.id()), &db, cx, |view| &mut view.feed);

        let theme = theme(cx);
        let table = StateTable {
            entries: self
                .states
                .iter()
                .map(|e| {
                    let e = e.read(cx);
                    (e.value, e.label.clone(), None)
                })
                .collect(),
            unknown_label: SharedString::default(),
        };
        let now = shown(self.binding_error(), self.feed.value, &table);

        let (image, fallback, fallback_color): (Option<LoadedImage>, SharedString, Hsla) =
            match &now {
                Shown::State(i) => {
                    let entry = self.states[*i].clone();
                    entry.update(cx, |e, _| {
                        let path = e.path.clone();
                        let fallback = if e.label.is_empty() {
                            SharedString::from(format!("{}", e.value))
                        } else {
                            e.label.clone()
                        };
                        (e.image.resolve(&path), fallback, theme.text_primary)
                    })
                }
                Shown::Unknown(value) => (
                    self.special.unknown.resolve(&self.unknown_image),
                    format!("Unknown: {value}").into(),
                    theme.text_secondary,
                ),
                Shown::NoData => (
                    self.special.no_data.resolve(&self.no_data_image),
                    "No data".into(),
                    theme.text_tertiary,
                ),
                Shown::Error(why) => (
                    self.special.error.resolve(&self.error_image),
                    why.clone(),
                    theme.error_accent,
                ),
            };

        // A stale value still shows its picture, washed out like a stale
        // value cell, so a dead producer can't pass for a steady one.
        let stale = self.feed.stale && matches!(now, Shown::State(_) | Shown::Unknown(_));
        let tile = div().size_full().overflow_hidden();
        match image {
            Some(Ok(image)) => tile.child(image_canvas(image, stale)),
            Some(Err(failed)) => tile.child(placeholder(failed, theme.text_tertiary)),
            None => tile.child(placeholder(fallback, fallback_color)),
        }
    }
}

fn placeholder(text: SharedString, color: Hsla) -> impl IntoElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .p(ui_px(4.0))
        .rounded(px(3.0))
        .text_size(ui_px(12.0))
        .text_center()
        .text_color(color)
        .child(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> StateTable {
        StateTable {
            entries: vec![(0.0, "CLOSED".into(), None), (1.0, "OPEN".into(), None)],
            unknown_label: SharedString::default(),
        }
    }

    #[test]
    fn a_broken_binding_outranks_silence_which_outranks_any_value() {
        let t = table();
        assert_eq!(
            shown(Some("No component".into()), Some(1.0), &t),
            Shown::Error("No component".into())
        );
        assert_eq!(shown(None, None, &t), Shown::NoData);
        assert_eq!(shown(None, Some(1.0), &t), Shown::State(1));
        assert_eq!(shown(None, Some(7.0), &t), Shown::Unknown(7.0));
    }

    #[test]
    fn a_state_image_config_round_trips_and_defaults_when_partial() {
        let cfg = StateImageConfig {
            component: "sat.valve".into(),
            element: 0,
            label: Some("valve".into()),
            states: vec![StateImageEntryConfig {
                value: 1.0,
                label: "OPEN".into(),
                path: "open.png".into(),
                data: "aGk=".into(),
            }],
            unknown: ImageRef {
                path: "unknown.png".into(),
                data: String::new(),
            },
            no_data: ImageRef::default(),
            error: ImageRef::default(),
        };
        let s = serde_json::to_string(&cfg).unwrap();
        assert_eq!(serde_json::from_str::<StateImageConfig>(&s).unwrap(), cfg);

        let partial: StateImageConfig =
            serde_json::from_str(r#"{"component":"sat.valve"}"#).unwrap();
        assert!(partial.states.is_empty());
        assert!(partial.error.is_empty());
    }

    #[test]
    fn an_entry_keeps_its_inline_bytes_until_its_path_is_edited() {
        let mut entry = StateImageEntry::from_config(&StateImageEntryConfig {
            value: 1.0,
            label: "OPEN".into(),
            path: "open.png".into(),
            data: "aGk=".into(),
        });
        assert_eq!(entry.to_config().data, "aGk=");
        entry.path = "elsewhere.png".into();
        let saved = entry.to_config();
        assert_eq!(saved.path, "elsewhere.png");
        assert!(saved.data.is_empty());
    }
}
