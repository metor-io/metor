use std::sync::Arc;

use gpui::{Entity, SharedString};
use metor_db::DB;
use metor_proto::types::ComponentId;

use crate::command_palette::{PaletteAction, PaletteItem, PalettePage};
use crate::inspectable::{list_components, FieldId, Inspectable, InspectionValue};

/// A pending trace selection: display label, component id, element index.
type TraceSelection = (String, ComponentId, usize);

/// Build a trace picker page for an inspectable entity's Traces field.
///
/// If `existing` contains prior selections, they appear as pills in the
/// palette breadcrumb bar so the user can see what's already chosen and
/// continue adding more.
pub(crate) fn trace_picker_page<T: Inspectable>(
    entity: Entity<T>,
    field_id: FieldId,
    db: Arc<DB>,
    existing: &[(ComponentId, usize)],
) -> PalettePage {
    let existing_selections: Vec<TraceSelection> = db.with_state(|state| {
        existing
            .iter()
            .filter_map(|(id, idx)| {
                let meta = state.get_component_metadata(*id)?;
                let elem_names = element_names(state.get_component(*id)?.schema.dim.as_slice());
                let elem_label = elem_names
                    .get(*idx)
                    .map(|n| format!("{}.{}", meta.name, n))
                    .unwrap_or_else(|| format!("{}[{}]", meta.name, idx));
                Some((elem_label, *id, *idx))
            })
            .collect()
    });

    let ctx = TracePickerCtx {
        entity,
        field_id,
        db,
    };
    ctx.prepopulated(existing_selections)
}

/// Return element names for a component from the DB, or an empty vec if not found.
pub fn element_names_for_component(db: &DB, component_id: ComponentId) -> Vec<String> {
    db.with_state(|state| {
        state
            .get_component(component_id)
            .map(|c| element_names(c.schema.dim.as_slice()))
            .unwrap_or_default()
    })
}

/// Generate default element names from a shape (e.g. [3] -> ["x", "y", "z"]).
pub(crate) fn element_names(shape: &[usize]) -> Vec<String> {
    fn walk(shape: &[usize], prefix: &str, out: &mut Vec<String>) {
        if shape.is_empty() {
            out.push(prefix.to_string());
            return;
        }
        const NAMES: [char; 8] = ['x', 'y', 'z', 'w', 'u', 'v', 's', 't'];
        for x in 0..shape[0] {
            let mut elem = prefix.to_string();
            if let Some(c) = NAMES.get(x) {
                elem.push(*c);
            } else {
                elem.push_str(&x.to_string());
            }
            walk(&shape[1..], &elem, out);
        }
    }
    let mut out = Vec::new();
    walk(shape, "", &mut out);
    out
}

/// Bundles the immutable context for trace picker page construction,
/// avoiding repetitive parameter threading across recursive calls.
struct TracePickerCtx<T: Inspectable> {
    entity: Entity<T>,
    field_id: FieldId,
    db: Arc<DB>,
}

impl<T: Inspectable> TracePickerCtx<T> {
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

    fn element_page(
        &self,
        component_id: ComponentId,
        component_name: String,
        selections: Vec<TraceSelection>,
    ) -> PalettePage {
        let names = element_names_for_component(&self.db, component_id);

        let names = if names.is_empty() {
            vec![component_name.clone()]
        } else {
            names
        };

        let mut items = Vec::new();

        if names.len() > 1 {
            let entity = self.entity.clone();
            let db = self.db.clone();
            let field_id = self.field_id;
            let mut sels = selections.clone();
            for i in 0..names.len() {
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

        for (i, elem_name) in names.iter().enumerate() {
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
