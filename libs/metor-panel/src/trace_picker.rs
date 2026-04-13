use std::sync::Arc;

use gpui::SharedString;
use metor_db::DB;
use metor_proto::types::ComponentId;

use crate::command_palette::{PaletteAction, PaletteItem, PalettePage};

/// List all components from the DB, sorted by name.
pub(crate) fn list_components(db: &DB) -> Vec<(ComponentId, String)> {
    let mut components: Vec<_> = db.with_state(|state| {
        state
            .component_metadata_iter()
            .map(|(id, meta)| (*id, meta.name.clone()))
            .collect()
    });
    components.sort_by(|a, b| a.1.cmp(&b.1));
    components
}

/// What kind of value an element picker produces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickerArity {
    Vec3,
    Quat,
}

/// A pending trace selection: display label, component id, element index.
type TraceSelection = (String, ComponentId, usize);

/// Callback that applies selected traces.
type ApplyTraces = Arc<dyn Fn(Vec<(ComponentId, usize)>, &mut gpui::Window, &mut gpui::App) + Send + Sync>;

/// Callback that applies an element picker selection.
type ApplyElement = Arc<dyn Fn(ComponentId, &mut gpui::Window, &mut gpui::App) + Send + Sync>;

/// Build a trace picker page.
///
/// If `existing` contains prior selections, they appear as pills in the
/// palette breadcrumb bar so the user can see what's already chosen and
/// continue adding more.
pub(crate) fn trace_picker_page(
    on_apply: ApplyTraces,
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

    let ctx = TracePickerCtx { on_apply, db };
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

/// Bundles the immutable context for trace picker page construction.
struct TracePickerCtx {
    on_apply: ApplyTraces,
    db: Arc<DB>,
}

impl TracePickerCtx {
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
            let on_apply = self.on_apply.clone();
            Some(PaletteItem::new(
                format!("Apply ({} selected)", sels.len()),
                PaletteAction::Execute(Box::new(move |_input, window, cx| {
                    let traces: Vec<(ComponentId, usize)> =
                        sels.iter().map(|(_name, id, idx)| (*id, *idx)).collect();
                    on_apply(traces, window, cx);
                })),
            ))
        };

        let items: Vec<PaletteItem> = list_components(&self.db)
            .into_iter()
            .map(|(id, name)| {
                let on_apply = self.on_apply.clone();
                let db = self.db.clone();
                let selections = selections.clone();
                let comp_name = name.clone();
                PaletteItem::new(
                    name,
                    PaletteAction::NextPage {
                        label: None,
                        page: Box::new(move || {
                            let ctx = TracePickerCtx { on_apply, db };
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
            let on_apply = self.on_apply.clone();
            let db = self.db.clone();
            let mut sels = selections.clone();
            for i in 0..names.len() {
                sels.push((component_name.clone(), component_id, i));
            }
            items.push(PaletteItem::new(
                "All",
                PaletteAction::NextPage {
                    label: Some(SharedString::from(component_name.clone())),
                    page: Box::new(move || {
                        let ctx = TracePickerCtx { on_apply, db };
                        ctx.component_page(sels)
                    }),
                },
            ));
        }

        for (i, elem_name) in names.iter().enumerate() {
            let on_apply = self.on_apply.clone();
            let db = self.db.clone();
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
                        let ctx = TracePickerCtx { on_apply, db };
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

/// Build a picker for an element binding field.
pub(crate) fn element_picker_page(
    on_apply: ApplyElement,
    db: Arc<DB>,
    arity: PickerArity,
) -> PalettePage {
    let items: Vec<PaletteItem> = list_components(&db)
        .into_iter()
        .map(|(id, name)| {
            let on_apply = on_apply.clone();
            PaletteItem::new(
                name,
                PaletteAction::Execute(Box::new(move |_input, window, cx| {
                    on_apply(id, window, cx);
                })),
            )
        })
        .collect();

    PalettePage::new(items).prompt(match arity {
        PickerArity::Vec3 => "Select a position component...",
        PickerArity::Quat => "Select an attitude component...",
    })
}
