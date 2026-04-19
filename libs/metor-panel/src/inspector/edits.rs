use std::sync::Arc;

use gpui::{App, Global, Pixels, Point, SharedString};
use metor_db::DB;
use metor_proto::types::{ComponentId, ElementValue, Timestamp};
use metor_proto_wkt::{ComponentValue, UpdateComponent};
use metor_proto::types::Msg;

use crate::inspector::rows::{CommandRow, DefaultActionRow, InspectorRow, NavRow};

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
    /// Window-space position for anchoring the inspector near the clicked
    /// cell. `None` falls back to centered.
    pub anchor: Option<Point<Pixels>>,
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

/// Commit the pending edit for `component_id` and drop `element_index` from
/// its modified list. The wire protocol is component-scoped, so the full
/// `ComponentValue` (including any other modified elements) is sent; only
/// `element_index` is removed from the pending UI state.
pub fn apply_element(db: &DB, component_id: ComponentId, element_index: usize, cx: &mut App) {
    let Some(edit) = pending_edits(cx).get(component_id).cloned() else {
        return;
    };
    apply_edit(db, &edit);
    let pending = pending_edits_mut(cx);
    if let Some(existing) = pending
        .edits
        .iter_mut()
        .find(|e| e.component_id == component_id)
    {
        existing.modified_elements.retain(|&i| i != element_index);
        if existing.modified_elements.is_empty() {
            pending.remove(component_id);
        }
    }
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
                .map(|v| crate::views::format_element_value(v, None))
                .unwrap_or_default();
            format!("{}={}", label, value)
        })
        .collect();
    SharedString::from(format!("{}: {}", edit.component_name, parts.join(", ")))
}

/// Build inspector rows listing all components. Selecting one cascades into an
/// element picker, which then opens the value editor.
pub fn update_component_rows(db: Arc<DB>) -> Vec<Box<dyn InspectorRow>> {
    let components = crate::inspector::trace_picker::list_components(&db);
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
        crate::inspector::trace_picker::element_names_for_component(&db, component_id)
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
                anchor: None,
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
                            anchor: None,
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
        anchor: _,
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

/// Upsert a pending edit that replaces the full U8 buffer of a string
/// component. UTF-8 bytes are written and any remainder is zero-padded so
/// the receiver sees a properly null-terminated string. `modified_elements`
/// is `[0]` as a sentinel — strings have no element-level granularity.
pub fn upsert_string_value(
    db: &DB,
    component_id: ComponentId,
    component_name: SharedString,
    element_names: Vec<SharedString>,
    text: &str,
    cx: &mut App,
) {
    let Some(component) = db.with_state(|s| s.get_component(component_id).cloned()) else {
        return;
    };
    let cap: usize = component.schema.dim.iter().product();
    let bytes = text.as_bytes();
    if bytes.len() > cap.saturating_sub(1) {
        return;
    }
    let Some(latest) = component.time_series.latest() else {
        return;
    };
    let buf = latest.data();
    let Ok((_size, view)) = component.schema.parse_value(buf) else {
        return;
    };
    let mut value = ComponentValue::from_view(view);
    if let ComponentValue::U8(arr) = &mut value {
        use nox::ArrayBuf;
        let slot = arr.buf.as_mut_buf();
        for (i, cell) in slot.iter_mut().enumerate() {
            *cell = *bytes.get(i).unwrap_or(&0);
        }
    } else {
        return;
    }
    merge_pending(cx, component_id, component_name, element_names, 0, value);
}

/// Upsert a pending edit with an already-typed element value. Used by the
/// inline editors (checkbox toggle, text field commit) to skip the
/// string-parse round trip.
pub fn upsert_element_value(
    db: &DB,
    component_id: ComponentId,
    component_name: SharedString,
    element_names: Vec<SharedString>,
    element_index: usize,
    value: ElementValue,
    cx: &mut App,
) {
    let Some(component_value) = build_updated_value_typed(db, component_id, element_index, value)
    else {
        return;
    };
    merge_pending(
        cx,
        component_id,
        component_name,
        element_names,
        element_index,
        component_value,
    );
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
    merge_pending(
        cx,
        component_id,
        component_name,
        element_names,
        element_index,
        value,
    );
}

fn merge_pending(
    cx: &mut App,
    component_id: ComponentId,
    component_name: SharedString,
    element_names: Vec<SharedString>,
    element_index: usize,
    value: ComponentValue,
) {
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

fn build_updated_value_typed(
    db: &DB,
    component_id: ComponentId,
    element_index: usize,
    value: ElementValue,
) -> Option<ComponentValue> {
    let component = db.with_state(|s| s.get_component(component_id).cloned())?;
    let latest = component.time_series.latest()?;
    let buf = latest.data();
    let (_size, view) = component.schema.parse_value(buf).ok()?;
    let mut out = ComponentValue::from_view(view);
    set_element_typed(&mut out, element_index, value)?;
    Some(out)
}

fn set_element_typed(value: &mut ComponentValue, idx: usize, element: ElementValue) -> Option<()> {
    use nox::ArrayBuf;
    match (value, element) {
        (ComponentValue::U8(a), ElementValue::U8(v)) => *a.buf.as_mut_buf().get_mut(idx)? = v,
        (ComponentValue::U16(a), ElementValue::U16(v)) => *a.buf.as_mut_buf().get_mut(idx)? = v,
        (ComponentValue::U32(a), ElementValue::U32(v)) => *a.buf.as_mut_buf().get_mut(idx)? = v,
        (ComponentValue::U64(a), ElementValue::U64(v)) => *a.buf.as_mut_buf().get_mut(idx)? = v,
        (ComponentValue::I8(a), ElementValue::I8(v)) => *a.buf.as_mut_buf().get_mut(idx)? = v,
        (ComponentValue::I16(a), ElementValue::I16(v)) => *a.buf.as_mut_buf().get_mut(idx)? = v,
        (ComponentValue::I32(a), ElementValue::I32(v)) => *a.buf.as_mut_buf().get_mut(idx)? = v,
        (ComponentValue::I64(a), ElementValue::I64(v)) => *a.buf.as_mut_buf().get_mut(idx)? = v,
        (ComponentValue::Bool(a), ElementValue::Bool(v)) => *a.buf.as_mut_buf().get_mut(idx)? = v,
        (ComponentValue::F32(a), ElementValue::F32(v)) => *a.buf.as_mut_buf().get_mut(idx)? = v,
        (ComponentValue::F64(a), ElementValue::F64(v)) => *a.buf.as_mut_buf().get_mut(idx)? = v,
        _ => return None,
    }
    Some(())
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
