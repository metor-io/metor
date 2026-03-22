use std::sync::Arc;

use gpui::{App, Context, Entity, Hsla, SharedString};
use metor_db::DB;

use crate::command_palette::{PaletteAction, PaletteItem, PalettePage};

/// Unique identifier for an inspection field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FieldId(pub u32);

/// A single inspectable field on an element.
pub struct InspectionField {
    pub label: SharedString,
    pub field_id: FieldId,
    pub value: InspectionValue,
}

/// The current value of an inspectable field.
#[derive(Clone, Debug)]
pub enum InspectionValue {
    Color(Hsla),
    Component { name: String },
    F64(f64),
    String(String),
    Bool(bool),
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
        }
    }
}

fn parse_color(input: &str) -> Option<Hsla> {
    // Accept "h s l a" or "h s l" (space-separated floats)
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
        }
    }
}

/// Trait for elements that expose their configuration at runtime.
pub trait Inspectable: Sized + 'static {
    fn fields(&self) -> Vec<InspectionField>;
    fn set_field(&mut self, field_id: FieldId, value: InspectionValue, cx: &mut Context<Self>);
}

/// Create a [`PalettePage`] showing the inspectable fields of an element.
///
/// Selecting a field opens an editor sub-page. Typing a value and pressing
/// Enter on the "Apply" item writes the value back to the element.
///
/// For `Component` fields, an optional `db` can be provided to list available
/// components as selectable items.
pub fn palette_page_for_inspectable<T: Inspectable>(
    entity: Entity<T>,
    db: Option<Arc<DB>>,
    cx: &App,
) -> PalettePage {
    let items = entity
        .read(cx)
        .fields()
        .into_iter()
        .map(|field| {
            let label: SharedString = format!("{}: {}", field.label, field.value).into();
            let field_id = field.field_id;
            let field_label = field.label.clone();
            let current_value = field.value.clone();
            let entity = entity.clone();
            let db = db.clone();
            PaletteItem {
                label,
                action: PaletteAction::NextPage(Box::new(move || {
                    editor_page_for_field(entity, field_id, field_label, current_value, db)
                })),
            }
        })
        .collect();
    PalettePage::new(items).label("Inspect")
}

fn editor_page_for_field<T: Inspectable>(
    entity: Entity<T>,
    field_id: FieldId,
    field_label: SharedString,
    current_value: InspectionValue,
    db: Option<Arc<DB>>,
) -> PalettePage {
    let mut items = Vec::new();

    // For Component fields, list available components from the DB as preset items.
    if matches!(current_value, InspectionValue::Component { .. }) {
        if let Some(db) = &db {
            let mut components: Vec<_> = db.with_state(|state| {
                state
                    .component_metadata_iter()
                    .map(|(id, meta)| (*id, meta.name.clone()))
                    .collect()
            });
            components.sort_by(|a, b| a.1.cmp(&b.1));
            for (id, name) in components {
                let entity = entity.clone();
                let component_name = name.clone();
                items.push(PaletteItem {
                    label: SharedString::from(name),
                    action: PaletteAction::Execute(Box::new(move |_input, _window, cx| {
                        let value = InspectionValue::Component {
                            name: component_name,
                        };
                        entity.update(cx, |this, cx| {
                            this.set_field(field_id, value, cx);
                        });
                    })),
                });
            }
        }
    }

    let apply_entity = entity.clone();
    let apply_value = current_value.clone();
    let apply_item = PaletteItem {
        label: "Apply".into(),
        action: PaletteAction::Execute(Box::new(move |input, _window, cx| {
            if let Some(new_value) = apply_value.parse_like(input) {
                apply_entity.update(cx, |this, cx| {
                    this.set_field(field_id, new_value, cx);
                });
            }
        })),
    };
    PalettePage::new(items)
        .label(field_label)
        .prompt("Enter value...")
        .default_action(apply_item)
}
