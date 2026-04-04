use std::sync::Arc;

use gpui::{App, Context, Entity, Hsla, SharedString};
use metor_db::DB;
use metor_proto::types::ComponentId;

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
    /// A trace selection: list of (ComponentId, element_index) pairs.
    /// Edited via the guided component/element picker.
    Traces(Vec<(ComponentId, usize)>),
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
            InspectionValue::Traces(_) => None, // traces are edited via the picker, not parsed
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
            InspectionValue::Traces(traces) => {
                write!(f, "{} selected", traces.len())
            }
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
                        db.with_state(|state| {
                            traces
                                .iter()
                                .filter_map(|(id, idx)| {
                                    let meta = state.get_component_metadata(*id)?;
                                    let schema =
                                        state.get_component(*id).map(|c| c.schema.clone());
                                    let elem_names: Vec<String> = schema
                                        .as_ref()
                                        .map(|s| {
                                            let dim: Vec<u64> =
                                                s.dim.iter().map(|d| *d as u64).collect();
                                            default_element_names(&dim)
                                        })
                                        .unwrap_or_default();
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

            PaletteItem::new(
                label,
                PaletteAction::NextPage {
                    label: None,
                    page: Box::new(move || {
                        editor_page_for_field(entity, field_id, field_label, current_value, db_clone)
                    }),
                },
            )
            .with_pills(pills)
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
    // For Traces fields, use the guided component/element picker.
    if let InspectionValue::Traces(ref existing) = current_value {
        if let Some(db) = db {
            // Convert existing (ComponentId, usize) pairs into TraceSelections with names
            let existing_selections: Vec<TraceSelection> = db.with_state(|state| {
                existing
                    .iter()
                    .filter_map(|(id, idx)| {
                        let meta = state.get_component_metadata(*id)?;
                        let schema = state.get_component(*id).map(|c| c.schema.clone());
                        let elem_names: Vec<String> = schema
                            .as_ref()
                            .map(|s| {
                                let dim: Vec<u64> = s.dim.iter().map(|d| *d as u64).collect();
                                default_element_names(&dim)
                            })
                            .unwrap_or_default();
                        let elem_label = elem_names
                            .get(*idx)
                            .map(|n| format!("{}.{}", meta.name, n))
                            .unwrap_or_else(|| format!("{}[{}]", meta.name, idx));
                        Some((elem_label, *id, *idx))
                    })
                    .collect()
            });

            return trace_picker_prepopulated(entity, field_id, db, existing_selections);
        }
    }

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
            for (_id, name) in components {
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

// ── Trace picker ────────────────────────────────────────────────────

/// A pending trace selection: display label, component id, element index.
type TraceSelection = (String, ComponentId, usize);

/// Build a pre-populated trace picker.
fn trace_picker_prepopulated<T: Inspectable>(
    entity: Entity<T>,
    field_id: FieldId,
    db: Arc<DB>,
    existing: Vec<TraceSelection>,
) -> PalettePage {
    if existing.is_empty() {
        return trace_picker_component_page(entity, field_id, db, vec![]);
    }

    let mut prepopulated = Vec::new();
    for i in 0..existing.len() {
        let sels_so_far: Vec<TraceSelection> = existing[..i].to_vec();
        let mut page = trace_picker_component_page(entity.clone(), field_id, db.clone(), sels_so_far);
        page.label = Some(SharedString::from(existing[i].0.clone()));
        prepopulated.push(page);
    }

    let mut active = trace_picker_component_page(entity, field_id, db, existing);
    active.prepopulated_pills_pages = prepopulated;
    active
}

/// Build the component selection page for the trace picker.
fn trace_picker_component_page<T: Inspectable>(
    entity: Entity<T>,
    field_id: FieldId,
    db: Arc<DB>,
    selections: Vec<TraceSelection>,
) -> PalettePage {
    let apply_selections = selections.clone();
    let apply_entity = entity.clone();
    let apply_item = if selections.is_empty() {
        None
    } else {
        Some(PaletteItem::new(
            format!("Apply ({} selected)", selections.len()),
            PaletteAction::Execute(Box::new(move |_input, _window, cx| {
                let traces: Vec<(ComponentId, usize)> = apply_selections
                    .iter()
                    .map(|(_name, id, idx)| (*id, *idx))
                    .collect();
                let value = InspectionValue::Traces(traces);
                apply_entity.update(cx, |this, cx| {
                    this.set_field(field_id, value, cx);
                });
            })),
        ))
    };

    let mut components: Vec<_> = db.with_state(|state| {
        state
            .component_metadata_iter()
            .map(|(id, meta)| (*id, meta.name.clone(), meta.clone()))
            .collect()
    });
    components.sort_by(|a, b| a.1.cmp(&b.1));

    let items: Vec<PaletteItem> = components
        .into_iter()
        .map(|(id, name, _meta)| {
            let entity = entity.clone();
            let db = db.clone();
            let selections = selections.clone();
            let comp_name = name.clone();
            let schema = db.with_state(|state| state.get_component(id).map(|c| c.schema.clone()));
            PaletteItem::new(
                name,
                PaletteAction::NextPage {
                    label: None,
                    page: Box::new(move || {
                        trace_picker_element_page(
                            entity, field_id, db, selections, id, comp_name, schema,
                        )
                    }),
                },
            )
        })
        .collect();

    let mut page = PalettePage::new(items).prompt("Select a component...");
    if let Some(apply) = apply_item {
        page = page.default_action(apply);
    }
    page
}

/// Build the element selection page for a specific component.
fn trace_picker_element_page<T: Inspectable>(
    entity: Entity<T>,
    field_id: FieldId,
    db: Arc<DB>,
    selections: Vec<TraceSelection>,
    component_id: ComponentId,
    component_name: String,
    schema: Option<metor_db::ComponentSchema>,
) -> PalettePage {
    let element_names: Vec<String> = schema
        .as_ref()
        .map(|s| {
            let dim: Vec<u64> = s.dim.iter().map(|d| *d as u64).collect();
            default_element_names(&dim)
        })
        .unwrap_or_default();

    let elem_count = element_names.len().max(1);
    let mut items = Vec::new();

    // "All" — adds one selection per element
    if elem_count > 1 {
        let entity_all = entity.clone();
        let db_all = db.clone();
        let name_all = component_name.clone();
        let mut sels_all = selections.clone();
        for i in 0..elem_count {
            sels_all.push((name_all.clone(), component_id, i));
        }
        let pill_label = SharedString::from(component_name.clone());
        items.push(PaletteItem::new(
            "All",
            PaletteAction::NextPage {
                label: Some(pill_label),
                page: Box::new(move || {
                    trace_picker_component_page(entity_all, field_id, db_all, sels_all)
                }),
            },
        ));
    }

    // Individual elements
    for (i, elem_name) in element_names.iter().enumerate() {
        let entity_elem = entity.clone();
        let db_elem = db.clone();
        let comp_name = component_name.clone();
        let mut sels_elem = selections.clone();
        sels_elem.push((comp_name.clone(), component_id, i));

        let display = if elem_name.is_empty() {
            format!("[{}]", i)
        } else {
            elem_name.clone()
        };
        let pill_label = SharedString::from(format!("{}.{}", comp_name, display));

        items.push(PaletteItem::new(
            display,
            PaletteAction::NextPage {
                label: Some(pill_label),
                page: Box::new(move || {
                    trace_picker_component_page(entity_elem, field_id, db_elem, sels_elem)
                }),
            },
        ));
    }

    // For scalars (single element), just select it directly
    if elem_count == 1 && element_names.is_empty() {
        let entity_scalar = entity.clone();
        let db_scalar = db.clone();
        let mut sels_scalar = selections.clone();
        sels_scalar.push((component_name.clone(), component_id, 0));
        let pill_label = SharedString::from(component_name.clone());
        items.push(PaletteItem::new(
            component_name.clone(),
            PaletteAction::NextPage {
                label: Some(pill_label),
                page: Box::new(move || {
                    trace_picker_component_page(entity_scalar, field_id, db_scalar, sels_scalar)
                }),
            },
        ));
    }

    PalettePage::new(items)
        .label(component_name)
        .prompt("Select element...")
}

/// Return element names for a component from the DB, or an empty vec if not found.
pub fn element_names_for_component(db: &DB, component_id: ComponentId) -> Vec<String> {
    db.with_state(|state| {
        state
            .get_component(component_id)
            .map(|c| {
                let dim: Vec<u64> = c.schema.dim.iter().map(|d| *d as u64).collect();
                default_element_names(&dim)
            })
            .unwrap_or_default()
    })
}

/// Generate default element names from a shape (e.g. [3] → ["x", "y", "z"]).
fn default_element_names(shape: &[u64]) -> Vec<String> {
    fn append_elements(shape: &[u64], parent_elem: &str, elems: &mut Vec<String>) {
        if shape.is_empty() {
            elems.push(parent_elem.to_string());
            return;
        }
        const NAMES: [char; 8] = ['x', 'y', 'z', 'w', 'u', 'v', 's', 't'];
        for x in 0..shape[0] {
            let mut elem = parent_elem.to_string();
            if let Some(c) = NAMES.get(x as usize) {
                elem.push(*c);
            } else {
                elem.push_str(&x.to_string());
            }
            append_elements(&shape[1..], &elem, elems);
        }
    }
    let mut elems = Vec::new();
    append_elements(shape, "", &mut elems);
    elems
}
