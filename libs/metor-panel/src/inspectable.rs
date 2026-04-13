use std::sync::Arc;

use gpui::{App, Hsla, Pixels, Point, SharedString};
use metor_db::DB;
use metor_proto::types::ComponentId;

use crate::command_palette::PalettePage;

/// List all components from the DB, sorted by name.
pub fn list_components(db: &DB) -> Vec<(ComponentId, String)> {
    let mut components: Vec<_> = db.with_state(|state| {
        state
            .component_metadata_iter()
            .map(|(id, meta)| (*id, meta.name.clone()))
            .collect()
    });
    components.sort_by(|a, b| a.1.cmp(&b.1));
    components
}

/// Unique identifier for an inspection field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FieldId(pub u32);

/// A single inspectable field on an element.
#[derive(Clone, Debug)]
pub struct InspectionField {
    pub label: SharedString,
    pub field_id: FieldId,
    pub value: InspectionValue,
    /// For `F64` fields, an optional `(min, max)` range that enables a slider widget.
    pub range: Option<(f64, f64)>,
}

impl InspectionField {
    pub fn new(
        label: impl Into<SharedString>,
        field_id: FieldId,
        value: InspectionValue,
    ) -> Self {
        Self {
            label: label.into(),
            field_id,
            value,
            range: None,
        }
    }

    pub fn with_range(mut self, min: f64, max: f64) -> Self {
        self.range = Some((min, max));
        self
    }
}

/// A sub-item in a [`InspectionValue::List`], with its own inspectable fields.
#[derive(Clone, Debug)]
pub struct ListItem {
    pub label: SharedString,
    pub fields: Vec<InspectionField>,
    /// Whether this child can be removed from its parent list.
    pub can_remove: bool,
}

/// Type-erased callback that returns a palette page for adding a new child.
pub type AddCallback = Arc<dyn Fn() -> PalettePage + Send + Sync>;

/// One-shot action callback for command fields.
pub type CommandCallback = Arc<dyn Fn(&mut gpui::Window, &mut App) + Send + Sync>;

/// What kind of value an [`InspectionValue::ElementPicker`] component is
/// expected to produce. The picker UI uses this to label its prompt; the
/// downstream consumer uses it to choose the correct fixed-frame
/// conversion (see `viewer_3d::picker`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickerArity {
    /// A 3-element position component (`[x, y, z]` in metor-panel frame).
    Vec3,
    /// A 4-element quaternion (`[i, j, k, w]` in metor-panel frame).
    Quat,
}

/// The current value of an inspectable field.
#[derive(Clone)]
pub enum InspectionValue {
    Color(Hsla),
    Component { name: String },
    F64(f64),
    String(String),
    Bool(bool),
    /// A fixed set of named choices (rendered as a selectable list).
    Enum {
        selected: String,
        options: Vec<String>,
    },
    /// A trace selection: list of (ComponentId, element_index) pairs.
    /// Edited via the guided component/element picker.
    Traces(Vec<(ComponentId, usize)>),
    /// A list of named sub-items, each with its own fields.
    /// Field IDs for sub-fields encode `item_index * 100 + sub_field_id`.
    /// When `on_add` is present the UI shows an "Add" action that opens the
    /// returned palette page.
    List {
        items: Vec<ListItem>,
        on_add: Option<AddCallback>,
    },
    /// Bind to a component whose layout matches a fixed
    /// metor-panel-frame `Vec3` (position) or `Quat` (attitude). `None`
    /// means the binding has not yet been configured. The frame swizzle
    /// from metor-panel to Bevy is applied by the downstream consumer.
    ElementPicker {
        component: Option<ComponentId>,
        arity: PickerArity,
    },
    /// A one-shot action. Clicking executes the callback and dismisses.
    Command(CommandCallback),
    /// Sub-fields of a list item. Clicking cascades to show these fields.
    ListItemFields(Vec<InspectionField>),
}

impl std::fmt::Debug for InspectionValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Color(c) => f.debug_tuple("Color").field(c).finish(),
            Self::Component { name } => f.debug_struct("Component").field("name", name).finish(),
            Self::F64(v) => f.debug_tuple("F64").field(v).finish(),
            Self::String(s) => f.debug_tuple("String").field(s).finish(),
            Self::Bool(b) => f.debug_tuple("Bool").field(b).finish(),
            Self::Enum { selected, options } => f
                .debug_struct("Enum")
                .field("selected", selected)
                .field("options", options)
                .finish(),
            Self::Traces(t) => f.debug_tuple("Traces").field(t).finish(),
            Self::List { items, on_add } => f
                .debug_struct("List")
                .field("items", items)
                .field("on_add", &on_add.as_ref().map(|_| ".."))
                .finish(),
            Self::ElementPicker { component, arity } => f
                .debug_struct("ElementPicker")
                .field("component", component)
                .field("arity", arity)
                .finish(),
            Self::Command(_) => f.debug_tuple("Command").field(&"..").finish(),
            Self::ListItemFields(fields) => f.debug_tuple("ListItemFields").field(fields).finish(),
        }
    }
}

impl InspectionValue {
    /// Try to parse a string into the same variant as `self`.
    pub fn parse_like(&self, input: &str) -> Option<InspectionValue> {
        match self {
            InspectionValue::F64(_) => input.parse::<f64>().ok().map(InspectionValue::F64),
            InspectionValue::String(_) => Some(InspectionValue::String(input.to_string())),
            InspectionValue::Bool(_) => input.parse::<bool>().ok().map(InspectionValue::Bool),
            InspectionValue::Color(_) => parse_color(input).map(InspectionValue::Color),
            InspectionValue::Component { .. } => Some(InspectionValue::Component {
                name: input.to_string(),
            }),
            InspectionValue::Enum { options, .. } => {
                options.contains(&input.to_string()).then(|| InspectionValue::Enum {
                    selected: input.to_string(),
                    options: options.clone(),
                })
            }
            InspectionValue::Traces(_) => None,     // traces are edited via the picker, not parsed
            InspectionValue::List { .. } => None,
            InspectionValue::ElementPicker { .. } => None,
            InspectionValue::Command(_) => None,
            InspectionValue::ListItemFields(_) => None,
        }
    }
}

fn parse_color(input: &str) -> Option<Hsla> {
    let parts: Vec<f32> = input
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect();
    match parts.len() {
        3 => Some(Hsla {
            h: parts[0],
            s: parts[1],
            l: parts[2],
            a: 1.0,
        }),
        4 => Some(Hsla {
            h: parts[0],
            s: parts[1],
            l: parts[2],
            a: parts[3],
        }),
        _ => None,
    }
}

impl std::fmt::Display for InspectionValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InspectionValue::Color(c) => {
                write!(f, "{:.2} {:.2} {:.2} {:.2}", c.h, c.s, c.l, c.a)
            }
            InspectionValue::Component { name } => write!(f, "{}", name),
            InspectionValue::F64(v) => write!(f, "{}", v),
            InspectionValue::String(s) => write!(f, "{}", s),
            InspectionValue::Bool(b) => write!(f, "{}", b),
            InspectionValue::Enum { selected, .. } => write!(f, "{}", selected),
            InspectionValue::Traces(traces) => {
                write!(f, "{} selected", traces.len())
            }
            InspectionValue::List { items, .. } => {
                write!(f, "{} items", items.len())
            }
            InspectionValue::ElementPicker { component, .. } => match component {
                Some(_) => write!(f, "(bound)"),
                None => write!(f, "(none)"),
            },
            InspectionValue::Command(_) => Ok(()),
            InspectionValue::ListItemFields(fields) => {
                write!(f, "{} fields", fields.len())
            }
        }
    }
}

/// Bundles everything needed to open a rows-based inspector from any entry point.
pub struct InspectorRowsRequest {
    pub rows: Vec<Box<dyn crate::widgets::InspectorRow>>,
    pub position: Point<Pixels>,
}

/// Callback that opens a rows-based inspector at a given position.
pub type OpenInspectorCallback =
    Arc<dyn Fn(InspectorRowsRequest, &mut gpui::Window, &mut App) + 'static>;

