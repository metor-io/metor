use std::sync::Arc;

use gpui::{App, Global, SharedString};
use metor_db::DB;
use metor_proto::types::{ComponentId, Timestamp};
use metor_proto_wkt::{ComponentValue, UpdateComponent};
use metor_proto::types::Msg;

use crate::widgets::{CommandRow, DefaultActionRow, InspectorRow, NavRow};

/// A pending edit to a component's value, accumulated until the user commits it.
#[derive(Clone)]
pub struct PendingEdit {
    pub component_id: ComponentId,
    pub component_name: SharedString,
    pub value: ComponentValue,
    /// Element indices that were modified, for display in the review UI.
    pub modified_elements: Vec<usize>,
    /// Element labels for display.
    pub element_names: Vec<SharedString>,
}

/// A request from a value box click to open an edit palette. The AppRoot
/// drains this on render and opens the palette.
#[derive(Clone)]
pub struct EditRequest {
    pub component_id: ComponentId,
    pub component_name: SharedString,
    pub element_names: Vec<SharedString>,
    pub element_index: usize,
}

/// Global store for the editor lock state and accumulated pending edits.
#[derive(Default)]
pub struct PendingEdits {
    pub edits: Vec<PendingEdit>,
    /// True when editing is locked (default). Cmd+L toggles.
    pub locked: bool,
    /// Set by click handlers; AppRoot drains this each render.
    pub pending_request: Option<EditRequest>,
    /// Set when a click wants to open the review palette.
    pub open_review_requested: bool,
}

impl Global for PendingEdits {}

impl PendingEdits {
    /// Returns the existing pending edit for a component, if any.
    pub fn get(&self, id: ComponentId) -> Option<&PendingEdit> {
        self.edits.iter().find(|e| e.component_id == id)
    }

    /// Inserts or updates the pending edit for a component.
    pub fn upsert(&mut self, edit: PendingEdit) {
        if let Some(existing) = self.edits.iter_mut().find(|e| e.component_id == edit.component_id) {
            *existing = edit;
        } else {
            self.edits.push(edit);
        }
    }

    /// Removes the pending edit for a component.
    pub fn remove(&mut self, id: ComponentId) {
        self.edits.retain(|e| e.component_id != id);
    }
}

/// Initialize the global state. Call once at app startup.
pub fn init(cx: &mut App) {
    cx.set_global(PendingEdits {
        edits: Vec::new(),
        locked: true,
        pending_request: None,
        open_review_requested: false,
    });
}

/// Convenience read accessor.
pub fn pending_edits(cx: &App) -> &PendingEdits {
    cx.global::<PendingEdits>()
}

/// Convenience write accessor.
pub fn pending_edits_mut(cx: &mut App) -> &mut PendingEdits {
    cx.global_mut::<PendingEdits>()
}

/// Send a single pending edit to the FSW via the DB message log.
pub fn apply_edit(db: &DB, edit: &PendingEdit) {
    let update = UpdateComponent {
        id: edit.component_id,
        value: edit.value.clone(),
    };
    let bytes = postcard::to_allocvec(&update).expect("postcard serialize");
    let _ = db.push_msg(Timestamp::now(), UpdateComponent::ID, &bytes);
}

/// Build the inspector rows that list pending edits with apply/discard actions.
pub fn review_rows(db: Arc<DB>, cx: &App) -> Vec<Box<dyn InspectorRow>> {
    let edits = pending_edits(cx).edits.clone();

    let mut rows: Vec<Box<dyn InspectorRow>> = edits
        .iter()
        .map(|edit| {
            let summary = summarize_edit(edit);
            let id = edit.component_id;
            let db = db.clone();
            Box::new(CommandRow::new(
                summary,
                Arc::new(move |_window, cx| {
                    let pending = pending_edits(cx);
                    if let Some(e) = pending.get(id).cloned() {
                        apply_edit(&db, &e);
                        pending_edits_mut(cx).remove(id);
                    }
                }),
            )) as Box<dyn InspectorRow>
        })
        .collect();

    if !edits.is_empty() {
        let apply_db = db.clone();
        rows.push(Box::new(CommandRow::new(
            "Apply All",
            Arc::new(move |_window, cx| {
                let edits = pending_edits(cx).edits.clone();
                for edit in &edits {
                    apply_edit(&apply_db, edit);
                }
                pending_edits_mut(cx).edits.clear();
            }),
        )));
        rows.push(Box::new(CommandRow::new(
            "Discard All",
            Arc::new(move |_window, cx| {
                pending_edits_mut(cx).edits.clear();
            }),
        )));
    }

    rows
}

fn summarize_edit(edit: &PendingEdit) -> SharedString {
    let parts: Vec<String> = edit
        .modified_elements
        .iter()
        .map(|&i| {
            let label = edit
                .element_names
                .get(i)
                .map(|s| s.to_string())
                .unwrap_or_else(|| i.to_string());
            let value = edit
                .value
                .get(i)
                .map(|v| crate::elements::format_element_value(v, None))
                .unwrap_or_default();
            format!("{}={}", label, value)
        })
        .collect();
    SharedString::from(format!("{}: {}", edit.component_name, parts.join(", ")))
}

/// Build inspector rows listing all components. Selecting one cascades into an
/// element picker, which then opens the value editor.
pub fn update_component_rows(db: Arc<DB>) -> Vec<Box<dyn InspectorRow>> {
    let components = crate::trace_picker::list_components(&db);
    components
        .into_iter()
        .map(|(id, name)| {
            let db = db.clone();
            let label = SharedString::from(name);
            let label_for_children = label.clone();
            Box::new(NavRow::new(
                label,
                SharedString::new_static(""),
                Box::new(move |_cx| {
                    select_element_rows(db.clone(), id, label_for_children.clone())
                }),
            )) as Box<dyn InspectorRow>
        })
        .collect()
}

/// Build inspector rows listing each element of a component, or skip directly
/// to the editor for scalar / single-element components.
fn select_element_rows(
    db: Arc<DB>,
    component_id: ComponentId,
    component_name: SharedString,
) -> Vec<Box<dyn InspectorRow>> {
    let element_names: Vec<SharedString> =
        crate::trace_picker::element_names_for_component(&db, component_id)
            .into_iter()
            .map(SharedString::from)
            .collect();

    if element_names.len() <= 1 {
        return edit_value_rows(
            db,
            EditRequest {
                component_id,
                component_name,
                element_names,
                element_index: 0,
            },
        );
    }

    element_names
        .iter()
        .enumerate()
        .map(|(idx, name)| {
            let db = db.clone();
            let component_name = component_name.clone();
            let element_names = element_names.clone();
            let name = name.clone();
            Box::new(NavRow::new(
                name,
                SharedString::new_static(""),
                Box::new(move |_cx| {
                    edit_value_rows(
                        db.clone(),
                        EditRequest {
                            component_id,
                            component_name: component_name.clone(),
                            element_names: element_names.clone(),
                            element_index: idx,
                        },
                    )
                }),
            )) as Box<dyn InspectorRow>
        })
        .collect()
}

/// Build inspector rows that prompt the user for a new value for a single
/// element. For enum components, lists each variant; otherwise emits a
/// [`DefaultActionRow`] that prompts for free-text input.
pub fn edit_value_rows(db: Arc<DB>, request: EditRequest) -> Vec<Box<dyn InspectorRow>> {
    let EditRequest {
        component_id,
        component_name,
        element_names,
        element_index,
    } = request;

    let enum_variants: Option<Vec<String>> = db.with_state(|s| {
        s.get_component_metadata(component_id)
            .and_then(|m| m.enum_variants().map(|it| it.map(|s| s.to_string()).collect()))
    });

    if let Some(variants) = enum_variants {
        return variants
            .into_iter()
            .enumerate()
            .map(|(idx, name)| {
                let db = db.clone();
                let component_name = component_name.clone();
                let element_names = element_names.clone();
                Box::new(CommandRow::new(
                    SharedString::from(name),
                    Arc::new(move |_window, cx| {
                        upsert_element_edit(
                            &db,
                            component_id,
                            component_name.clone(),
                            element_names.clone(),
                            element_index,
                            &idx.to_string(),
                            cx,
                        );
                    }),
                )) as Box<dyn InspectorRow>
            })
            .collect();
    }

    let label = element_names
        .get(element_index)
        .cloned()
        .unwrap_or_else(|| SharedString::from(format!("[{}]", element_index)));
    let prompt = SharedString::from(format!("New value for {}.{}", component_name, label));

    vec![Box::new(DefaultActionRow {
        label: prompt,
        callback: Arc::new(move |input, _window, cx| {
            upsert_element_edit(
                &db,
                component_id,
                component_name.clone(),
                element_names.clone(),
                element_index,
                &input,
                cx,
            );
        }),
    })]
}

fn upsert_element_edit(
    db: &DB,
    component_id: ComponentId,
    component_name: SharedString,
    element_names: Vec<SharedString>,
    element_index: usize,
    input: &str,
    cx: &mut App,
) {
    let Some(value) = build_updated_value(db, component_id, element_index, input) else {
        return;
    };

    let pending = pending_edits_mut(cx);
    if let Some(existing) = pending
        .edits
        .iter_mut()
        .find(|e| e.component_id == component_id)
    {
        let _ = existing.value.copy_from_view(value.as_view());
        if !existing.modified_elements.contains(&element_index) {
            existing.modified_elements.push(element_index);
        }
    } else {
        pending.upsert(PendingEdit {
            component_id,
            component_name,
            value,
            modified_elements: vec![element_index],
            element_names,
        });
    }
}

/// Build an updated [`ComponentValue`] by reading the current value, then setting
/// `element_index` to the parsed input.
fn build_updated_value(
    db: &DB,
    component_id: ComponentId,
    element_index: usize,
    input: &str,
) -> Option<ComponentValue> {
    let pending = db.with_state(|s| s.get_component(component_id).cloned())?;
    let latest = pending.time_series.latest()?;
    let buf = latest.data();
    let (_size, view) = pending.schema.parse_value(buf).ok()?;
    let mut value = ComponentValue::from_view(view);
    set_element(&mut value, element_index, input)?;
    Some(value)
}

fn set_element(value: &mut ComponentValue, idx: usize, input: &str) -> Option<()> {
    use nox::ArrayBuf;
    match value {
        ComponentValue::U8(a) => *a.buf.as_mut_buf().get_mut(idx)? = input.parse().ok()?,
        ComponentValue::U16(a) => *a.buf.as_mut_buf().get_mut(idx)? = input.parse().ok()?,
        ComponentValue::U32(a) => *a.buf.as_mut_buf().get_mut(idx)? = input.parse().ok()?,
        ComponentValue::U64(a) => *a.buf.as_mut_buf().get_mut(idx)? = input.parse().ok()?,
        ComponentValue::I8(a) => *a.buf.as_mut_buf().get_mut(idx)? = input.parse().ok()?,
        ComponentValue::I16(a) => *a.buf.as_mut_buf().get_mut(idx)? = input.parse().ok()?,
        ComponentValue::I32(a) => *a.buf.as_mut_buf().get_mut(idx)? = input.parse().ok()?,
        ComponentValue::I64(a) => *a.buf.as_mut_buf().get_mut(idx)? = input.parse().ok()?,
        ComponentValue::Bool(a) => *a.buf.as_mut_buf().get_mut(idx)? = input.parse().ok()?,
        ComponentValue::F32(a) => *a.buf.as_mut_buf().get_mut(idx)? = input.parse().ok()?,
        ComponentValue::F64(a) => *a.buf.as_mut_buf().get_mut(idx)? = input.parse().ok()?,
    }
    Some(())
}
