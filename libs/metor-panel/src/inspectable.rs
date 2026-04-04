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
                        palette_page_for_field(entity, field_id, field_label, current_value, db_clone)
                    }),
                },
            )
            .with_pills(pills)
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

            let ctx = TracePickerCtx { entity, field_id, db };
            return ctx.prepopulated(existing_selections);
        }
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

// ── Trace picker ────────────────────────────────────────────────────

/// A pending trace selection: display label, component id, element index.
type TraceSelection = (String, ComponentId, usize);

/// Bundles the immutable context for trace picker page construction,
/// avoiding repetitive parameter threading across recursive calls.
struct TracePickerCtx<T: Inspectable> {
    entity: Entity<T>,
    field_id: FieldId,
    db: Arc<DB>,
}

impl<T: Inspectable> TracePickerCtx<T> {
    /// Build a trace picker pre-populated with existing selections as pills.
    fn prepopulated(&self, existing: Vec<TraceSelection>) -> PalettePage {
        if existing.is_empty() {
            return self.component_page(vec![]);
        }

        let mut prepopulated = Vec::new();
        for i in 0..existing.len() {
            let mut page = self.component_page(existing[..i].to_vec());
            page.label = Some(SharedString::from(existing[i].0.clone()));
            prepopulated.push(page);
        }

        let mut active = self.component_page(existing);
        active.prepopulated_pills_pages = prepopulated;
        active
    }

    /// Build the component selection page.
    fn component_page(&self, selections: Vec<TraceSelection>) -> PalettePage {
        let apply_item = if selections.is_empty() {
            None
        } else {
            let sels = selections.clone();
            let entity = self.entity.clone();
            let field_id = self.field_id;
            Some(PaletteItem::new(
                format!("Apply ({} selected)", sels.len()),
                PaletteAction::Execute(Box::new(move |_input, _window, cx| {
                    let traces: Vec<(ComponentId, usize)> =
                        sels.iter().map(|(_name, id, idx)| (*id, *idx)).collect();
                    entity.update(cx, |this, cx| {
                        this.set_field(field_id, InspectionValue::Traces(traces), cx);
                    });
                })),
            ))
        };

        let items: Vec<PaletteItem> = list_components(&self.db)
            .into_iter()
            .map(|(id, name)| {
                let entity = self.entity.clone();
                let db = self.db.clone();
                let field_id = self.field_id;
                let selections = selections.clone();
                let comp_name = name.clone();
                PaletteItem::new(
                    name,
                    PaletteAction::NextPage {
                        label: None,
                        page: Box::new(move || {
                            let ctx = TracePickerCtx { entity, field_id, db };
                            ctx.element_page(id, comp_name, selections)
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
    fn element_page(
        &self,
        component_id: ComponentId,
        component_name: String,
        selections: Vec<TraceSelection>,
    ) -> PalettePage {
        let element_names = element_names_for_component(&self.db, component_id);

        // For scalars, synthesize a single entry so the loop handles everything
        let element_names = if element_names.is_empty() {
            vec![component_name.clone()]
        } else {
            element_names
        };

        let mut items = Vec::new();

        // "All" — adds one selection per element
        if element_names.len() > 1 {
            let entity = self.entity.clone();
            let db = self.db.clone();
            let field_id = self.field_id;
            let mut sels = selections.clone();
            for i in 0..element_names.len() {
                sels.push((component_name.clone(), component_id, i));
            }
            items.push(PaletteItem::new(
                "All",
                PaletteAction::NextPage {
                    label: Some(SharedString::from(component_name.clone())),
                    page: Box::new(move || {
                        let ctx = TracePickerCtx { entity, field_id, db };
                        ctx.component_page(sels)
                    }),
                },
            ));
        }

        // Individual elements
        for (i, elem_name) in element_names.iter().enumerate() {
            let entity = self.entity.clone();
            let db = self.db.clone();
            let field_id = self.field_id;
            let mut sels = selections.clone();
            sels.push((component_name.clone(), component_id, i));

            let display = if elem_name.is_empty() {
                format!("[{}]", i)
            } else {
                elem_name.clone()
            };
            let pill_label = SharedString::from(format!("{}.{}", component_name, display));

            items.push(PaletteItem::new(
                display,
                PaletteAction::NextPage {
                    label: Some(pill_label),
                    page: Box::new(move || {
                        let ctx = TracePickerCtx { entity, field_id, db };
                        ctx.component_page(sels)
                    }),
                },
            ));
        }

        PalettePage::new(items)
            .label(component_name)
            .prompt("Select element...")
    }
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
