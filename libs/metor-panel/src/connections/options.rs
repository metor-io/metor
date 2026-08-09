//! Per-connection knobs: what a backend lets the operator decide, and the
//! values it reads when it stands itself up.
//!
//! A target declares typed options ([`ConnectionTarget::with_options`]) or
//! inherits the panel-wide set installed through
//! [`PanelApp::default_connection_options`](crate::PanelApp::default_connection_options)
//! — the latter is what reaches discovered and address-resolved targets,
//! which a wrapper author never constructs itself. The store folds spec
//! defaults together with the operator's saved overrides into a
//! [`ConnectionOptions`] and hands it to the backend through
//! [`ConnectContext`](super::ConnectContext), so a backend reads its knobs
//! the same way whoever produced its target chose to declare them.
//!
//! Values are a snapshot taken at connect time. Changing one on a live
//! connection restarts that backend rather than mutating it in flight —
//! cancel-and-reconnect is already the uniform teardown protocol, so no
//! backend has to grow live-update code to honour a knob.

use std::sync::Arc;

use gpui::{App, Entity, SharedString, Window};
use smallvec::SmallVec;

use super::{ConnectionTarget, ConnectionsStore};
use crate::inspector::rows::{BoolRow, EnumRow, InspectorRow, ScalarRow, SliderRow, TextRow};

/// Values for one target, sized for the handful of knobs a connection
/// realistically has.
pub(crate) type OptionValues = SmallVec<[(SharedString, OptionValue); 4]>;

/// A declared knob set. `Arc<[_]>` so it rides along with a cloned
/// [`ConnectionTarget`] for free.
pub type OptionSpec = Arc<[ConnectionOption]>;

/// One user-settable knob a backend reads when it connects.
#[derive(Clone, Debug)]
pub struct ConnectionOption {
    /// What the backend looks the value up by.
    pub key: SharedString,
    /// What the operator sees in the dialog.
    pub label: SharedString,
    pub kind: OptionKind,
}

/// The shape of a knob. Each variant carries its own default, so a kind and
/// a default can never disagree.
#[derive(Clone, Debug)]
pub enum OptionKind {
    Toggle {
        default: bool,
    },
    /// A closed set of named alternatives.
    Choice {
        choices: SmallVec<[SharedString; 4]>,
        default: usize,
    },
    Text {
        placeholder: SharedString,
        default: SharedString,
    },
    /// Bounded numbers render as a slider, unbounded ones as a typed field.
    Number {
        default: f64,
        min: f64,
        max: f64,
    },
}

impl ConnectionOption {
    pub fn toggle(
        key: impl Into<SharedString>,
        label: impl Into<SharedString>,
        default: bool,
    ) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            kind: OptionKind::Toggle { default },
        }
    }

    /// A closed set. `default` indexes `choices`; an out-of-range index
    /// falls back to the first entry.
    pub fn choice(
        key: impl Into<SharedString>,
        label: impl Into<SharedString>,
        choices: impl IntoIterator<Item = impl Into<SharedString>>,
        default: usize,
    ) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            kind: OptionKind::Choice {
                choices: choices.into_iter().map(Into::into).collect(),
                default,
            },
        }
    }

    pub fn text(
        key: impl Into<SharedString>,
        label: impl Into<SharedString>,
        placeholder: impl Into<SharedString>,
        default: impl Into<SharedString>,
    ) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            kind: OptionKind::Text {
                placeholder: placeholder.into(),
                default: default.into(),
            },
        }
    }

    /// A free number. Pass a non-finite `min`/`max` (e.g. `f64::NEG_INFINITY`)
    /// to get a typed field instead of a slider.
    pub fn number(
        key: impl Into<SharedString>,
        label: impl Into<SharedString>,
        default: f64,
        min: f64,
        max: f64,
    ) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            kind: OptionKind::Number { default, min, max },
        }
    }

    /// The value this knob takes when the operator hasn't touched it.
    pub fn default_value(&self) -> OptionValue {
        match &self.kind {
            OptionKind::Toggle { default } => OptionValue::Bool(*default),
            OptionKind::Choice { choices, default } => OptionValue::Choice(
                choices
                    .get(*default)
                    .or_else(|| choices.first())
                    .cloned()
                    .unwrap_or_default(),
            ),
            OptionKind::Text { default, .. } => OptionValue::Text(default.clone()),
            OptionKind::Number { default, .. } => OptionValue::Number(*default),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum OptionValue {
    Bool(bool),
    Choice(SharedString),
    Text(SharedString),
    Number(f64),
}

impl OptionValue {
    /// Round-trip form for the connections index. Deliberately lossy about
    /// which variant produced it: the value is re-typed against the live
    /// spec on the way back in, so a knob whose kind changed reads as its
    /// new default instead of poisoning the whole file.
    pub(crate) fn encode(&self) -> String {
        match self {
            OptionValue::Bool(v) => v.to_string(),
            OptionValue::Choice(v) | OptionValue::Text(v) => v.to_string(),
            OptionValue::Number(v) => v.to_string(),
        }
    }

    /// Re-type a stored string against `kind`. `None` when the text no
    /// longer fits — an unparsable number, a choice that was removed.
    pub(crate) fn decode(text: &str, kind: &OptionKind) -> Option<Self> {
        match kind {
            OptionKind::Toggle { .. } => match text {
                "true" => Some(OptionValue::Bool(true)),
                "false" => Some(OptionValue::Bool(false)),
                _ => None,
            },
            OptionKind::Choice { choices, .. } => choices
                .iter()
                .find(|c| c.as_ref() == text)
                .cloned()
                .map(OptionValue::Choice),
            OptionKind::Text { .. } => {
                Some(OptionValue::Text(SharedString::from(text.to_string())))
            }
            OptionKind::Number { .. } => text.parse().ok().map(OptionValue::Number),
        }
    }
}

/// The resolved knob values one backend sees.
///
/// Always complete against the target's spec — the store folds defaults in
/// before connecting — so the accessors are total. Asking for a key the
/// backend never declared is a call-site bug and trips a debug assertion
/// rather than being papered over.
#[derive(Clone, Debug, Default)]
pub struct ConnectionOptions {
    values: OptionValues,
}

impl ConnectionOptions {
    pub(crate) fn from_values(values: OptionValues) -> Self {
        Self { values }
    }

    pub fn get(&self, key: &str) -> Option<&OptionValue> {
        self.values.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn bool(&self, key: &str) -> bool {
        match self.get(key) {
            Some(OptionValue::Bool(v)) => *v,
            other => {
                debug_assert!(false, "option `{key}` is not a toggle: {other:?}");
                false
            }
        }
    }

    pub fn choice(&self, key: &str) -> &str {
        match self.get(key) {
            Some(OptionValue::Choice(v)) => v,
            other => {
                debug_assert!(false, "option `{key}` is not a choice: {other:?}");
                ""
            }
        }
    }

    pub fn text(&self, key: &str) -> &str {
        match self.get(key) {
            Some(OptionValue::Text(v)) => v,
            other => {
                debug_assert!(false, "option `{key}` is not text: {other:?}");
                ""
            }
        }
    }

    pub fn number(&self, key: &str) -> f64 {
        match self.get(key) {
            Some(OptionValue::Number(v)) => *v,
            other => {
                debug_assert!(false, "option `{key}` is not a number: {other:?}");
                0.0
            }
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &OptionValue)> {
        self.values.iter().map(|(k, v)| (k.as_ref(), v))
    }
}

/// Inspector rows for `target`'s knobs, writing straight back into the
/// store. The single source of option UI: the connection dialog embeds
/// these inline, and the command palette opens the same vector.
pub(crate) fn option_rows(
    store: Entity<ConnectionsStore>,
    target: ConnectionTarget,
    cx: &App,
) -> Vec<Box<dyn InspectorRow>> {
    let spec: OptionSpec = store.read(cx).state().spec_for(&target).clone();
    let current = store.read(cx).state().options_for(&target);

    spec.iter()
        .map(|option| {
            let key = option.key.clone();
            let label = option.label.clone();
            let value = current
                .get(&key)
                .cloned()
                .unwrap_or_else(|| option.default_value());
            // Every row writes back the same way; only the value's shape
            // differs, so the setter is built once here.
            let set = {
                let (store, target, key) = (store.clone(), target.clone(), key.clone());
                move |value: OptionValue, cx: &mut App| {
                    store.update(cx, |store, cx| store.set_option(&target, &key, value, cx));
                }
            };

            match (&option.kind, value) {
                (OptionKind::Toggle { .. }, _) => {
                    // Read live rather than caching: a reconnect repaints
                    // the dialog, and the row must agree with the store.
                    let (store, target, read_key) = (store.clone(), target.clone(), key.clone());
                    Box::new(BoolRow::dynamic(
                        label,
                        Arc::new(move |cx: &App| {
                            store.read(cx).state().options_for(&target).bool(&read_key)
                        }),
                        Arc::new(move |checked, _window: &mut Window, cx: &mut App| {
                            set(OptionValue::Bool(checked), cx)
                        }),
                    )) as Box<dyn InspectorRow>
                }
                (OptionKind::Choice { choices, .. }, value) => {
                    let selected = match value {
                        OptionValue::Choice(v) => v,
                        _ => choices.first().cloned().unwrap_or_default(),
                    };
                    Box::new(EnumRow {
                        label,
                        selected,
                        options: choices.to_vec(),
                        on_select: Arc::new(move |choice, _window: &mut Window, cx: &mut App| {
                            set(OptionValue::Choice(SharedString::from(choice)), cx)
                        }),
                    })
                }
                (OptionKind::Text { .. }, value) => {
                    let text = match value {
                        OptionValue::Text(v) => v,
                        _ => SharedString::default(),
                    };
                    Box::new(TextRow::new(
                        label,
                        text,
                        Arc::new(move |text, _window: &mut Window, cx: &mut App| {
                            set(OptionValue::Text(SharedString::from(text)), cx)
                        }),
                    ))
                }
                (OptionKind::Number { min, max, .. }, value) => {
                    let number = match value {
                        OptionValue::Number(v) => v,
                        _ => 0.0,
                    };
                    let on_change = Arc::new(move |v: f64, _window: &mut Window, cx: &mut App| {
                        set(OptionValue::Number(v), cx)
                    });
                    if min.is_finite() && max.is_finite() {
                        let (store, target, read_key) =
                            (store.clone(), target.clone(), key.clone());
                        Box::new(SliderRow {
                            label,
                            read_value: Arc::new(move |cx: &App| {
                                store
                                    .read(cx)
                                    .state()
                                    .options_for(&target)
                                    .number(&read_key)
                            }),
                            min: *min,
                            max: *max,
                            on_change,
                        }) as Box<dyn InspectorRow>
                    } else {
                        Box::new(ScalarRow::new(label, number, on_change))
                    }
                }
            }
        })
        .collect()
}
