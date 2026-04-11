use std::sync::Arc;

use gpui::{App, Context, Entity, Hsla, SharedString};
use metor_db::DB;
use metor_proto::types::ComponentId;

use crate::command_palette::{PaletteAction, PaletteItem, PalettePage};

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
}

/// A sub-item in a [`InspectionValue::List`], with its own inspectable fields.
#[derive(Clone, Debug)]
pub struct ListItem {
    pub label: SharedString,
    pub fields: Vec<InspectionField>,
}

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
#[derive(Clone, Debug)]
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
    List(Vec<ListItem>),
    /// Bind to a component whose layout matches a fixed
    /// metor-panel-frame `Vec3` (position) or `Quat` (attitude). `None`
    /// means the binding has not yet been configured. The frame swizzle
    /// from metor-panel to Bevy is applied by the downstream consumer.
    ElementPicker {
        component: Option<ComponentId>,
        arity: PickerArity,
    },
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
            InspectionValue::Traces(_) => None, // traces are edited via the picker, not parsed
            InspectionValue::List(_) => None,   // lists are navigated, not parsed
            InspectionValue::ElementPicker { .. } => None, // same — always picker-driven
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
            InspectionValue::List(items) => {
                write!(f, "{} items", items.len())
            }
            InspectionValue::ElementPicker { component, .. } => match component {
                Some(_) => write!(f, "(bound)"),
                None => write!(f, "(none)"),
            },
        }
    }
}

/// Trait for elements that expose their configuration at runtime.
pub trait Inspectable: Sized + 'static {
    fn fields(&self) -> Vec<InspectionField>;
    fn set_field(&mut self, field_id: FieldId, value: InspectionValue, cx: &mut Context<Self>);
}

/// Create a [`PalettePage`] showing the inspectable fields of an element.
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
            let field_id = field.field_id;
            let field_label = field.label.clone();
            let current_value = field.value.clone();
            let entity = entity.clone();
            let db_clone = db.clone();

            // For Traces, show pills of current trace names
            let (label, pills) = match &current_value {
                InspectionValue::Traces(traces) if !traces.is_empty() => {
                    let pill_labels: Vec<SharedString> = if let Some(db) = &db {
                        use crate::trace_picker::element_names_for_component;
                        db.with_state(|state| {
                            traces
                                .iter()
                                .filter_map(|(id, idx)| {
                                    let meta = state.get_component_metadata(*id)?;
                                    let elem_names = element_names_for_component(db, *id);
                                    let label = elem_names
                                        .get(*idx)
                                        .map(|n| format!("{}.{}", meta.name, n))
                                        .unwrap_or_else(|| format!("{}[{}]", meta.name, idx));
                                    Some(SharedString::from(label))
                                })
                                .collect()
                        })
                    } else {
                        vec![]
                    };
                    (
                        SharedString::from(format!(
                            "{}: {} traces",
                            field.label,
                            traces.len()
                        )),
                        pill_labels,
                    )
                }
                _ => (
                    SharedString::from(format!("{}: {}", field.label, field.value)),
                    vec![],
                ),
            };

            // Bool fields toggle immediately on select
            let action = if matches!(current_value, InspectionValue::Bool(_)) {
                let entity = entity.clone();
                let toggled = matches!(current_value, InspectionValue::Bool(false));
                PaletteAction::Execute(Box::new(move |_input, _window, cx| {
                    entity.update(cx, |this, cx| {
                        this.set_field(field_id, InspectionValue::Bool(toggled), cx);
                    });
                }))
            } else {
                PaletteAction::NextPage {
                    label: None,
                    page: Box::new(move || {
                        palette_page_for_field(entity, field_id, field_label, current_value, db_clone)
                    }),
                }
            };

            PaletteItem::new(label, action).with_pills(pills)
        })
        .collect();
    PalettePage::new(items).label("Inspect")
}

/// Create a [`PalettePage`] that edits a specific field on an inspectable entity.
/// For `Traces` fields this opens the guided component/element picker.
pub fn palette_page_for_field<T: Inspectable>(
    entity: Entity<T>,
    field_id: FieldId,
    field_label: SharedString,
    current_value: InspectionValue,
    db: Option<Arc<DB>>,
) -> PalettePage {
    // For Traces fields, use the guided component/element picker.
    if let InspectionValue::Traces(ref existing) = current_value {
        if let Some(db) = db {
            return crate::trace_picker::trace_picker_page(entity, field_id, db, existing);
        }
    }

    // For ElementPicker fields, use the fixed-arity element picker.
    if let InspectionValue::ElementPicker {
        component: _,
        arity,
    } = current_value
    {
        if let Some(db) = db {
            return crate::trace_picker::element_picker_page(entity, field_id, db, arity);
        }
    }

    // For Enum fields, show each option as a selectable item.
    if let InspectionValue::Enum { ref options, .. } = current_value {
        let items: Vec<PaletteItem> = options
            .iter()
            .map(|option| {
                let entity = entity.clone();
                let option_val = option.clone();
                let opts = options.clone();
                PaletteItem::new(
                    option.clone(),
                    PaletteAction::Execute(Box::new(move |_input, _window, cx| {
                        entity.update(cx, |this, cx| {
                            this.set_field(
                                field_id,
                                InspectionValue::Enum {
                                    selected: option_val,
                                    options: opts,
                                },
                                cx,
                            );
                        });
                    })),
                )
            })
            .collect();
        return PalettePage::new(items).label(field_label);
    }

    // For List fields, show each item as a navigable row.
    if let InspectionValue::List(ref items) = current_value {
        let list_items: Vec<PaletteItem> = items
            .iter()
            .map(|item| {
                let entity = entity.clone();
                let db_clone = db.clone();
                let sub_fields = item.fields.clone();
                let item_label = item.label.clone();
                PaletteItem::new(
                    item.label.clone(),
                    PaletteAction::NextPage {
                        label: None,
                        page: Box::new(move || {
                            palette_page_for_list_item(entity, &sub_fields, item_label, db_clone)
                        }),
                    },
                )
            })
            .collect();
        return PalettePage::new(list_items).label(field_label);
    }

    let mut items = Vec::new();

    // For Component fields, list available components from the DB as preset items.
    if matches!(current_value, InspectionValue::Component { .. }) {
        if let Some(db) = &db {
            for (_id, name) in list_components(db) {
                let entity = entity.clone();
                let component_name = name.clone();
                items.push(PaletteItem::new(
                    name,
                    PaletteAction::Execute(Box::new(move |_input, _window, cx| {
                        let value = InspectionValue::Component {
                            name: component_name,
                        };
                        entity.update(cx, |this, cx| {
                            this.set_field(field_id, value, cx);
                        });
                    })),
                ));
            }
        }
    }

    let apply_entity = entity.clone();
    let apply_value = current_value.clone();
    let apply_item = PaletteItem::new(
        "Apply",
        PaletteAction::Execute(Box::new(move |input, _window, cx| {
            if let Some(new_value) = apply_value.parse_like(input) {
                apply_entity.update(cx, |this, cx| {
                    this.set_field(field_id, new_value, cx);
                });
            }
        })),
    );
    PalettePage::new(items)
        .label(field_label)
        .prompt("Enter value...")
        .default_action(apply_item)
}

/// Render a palette page for a single item inside a `List`, showing its sub-fields.
pub fn palette_page_for_list_item<T: Inspectable>(
    entity: Entity<T>,
    fields: &[InspectionField],
    label: SharedString,
    db: Option<Arc<DB>>,
) -> PalettePage {
    let items: Vec<PaletteItem> = fields
        .iter()
        .map(|field| {
            let field_id = field.field_id;
            let field_label = field.label.clone();
            let current_value = field.value.clone();
            let entity = entity.clone();
            let db_clone = db.clone();

            let display = SharedString::from(format!("{}: {}", field.label, field.value));

            // Bool toggles immediately; everything else opens a sub-page
            let action = if matches!(current_value, InspectionValue::Bool(_)) {
                let toggled = matches!(current_value, InspectionValue::Bool(false));
                PaletteAction::Execute(Box::new(move |_input, _window, cx| {
                    entity.update(cx, |this, cx| {
                        this.set_field(field_id, InspectionValue::Bool(toggled), cx);
                    });
                }))
            } else {
                PaletteAction::NextPage {
                    label: None,
                    page: Box::new(move || {
                        palette_page_for_field(entity, field_id, field_label, current_value, db_clone)
                    }),
                }
            };

            PaletteItem::new(display, action)
        })
        .collect();
    PalettePage::new(items).label(label)
}

