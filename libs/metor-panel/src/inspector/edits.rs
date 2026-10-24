use std::sync::Arc;

use gpui::{App, Global, Pixels, Point, SharedString};
use metor_db::DB;
use metor_proto::types::Msg;
use metor_proto::types::{ComponentId, ElementValue, Timestamp};
use metor_proto_wkt::{ComponentValue, UpdateComponent};
use nox::ArrayBuf;

use crate::dynamic::tensor::TypedScalar;
use crate::inspector::rows::{CommandRow, DefaultActionRow, InspectorRow, NavRow};

/// A staged edit to a single component, held until the user applies it.
///
/// The full [`ComponentValue`] is stored because the wire protocol updates
/// components atomically; `modified_elements` tracks which indices the user
/// actually touched so the review UI can show a concise summary.
#[derive(Clone)]
pub struct PendingEdit {
    pub component_id: ComponentId,
    pub component_name: SharedString,
    pub value: ComponentValue,
    pub modified_elements: Vec<usize>,
    pub element_names: Vec<SharedString>,
}

/// A request to open the editor palette, raised by a value-cell click.
///
/// The handler stashes this on the global [`PendingEdits`]; `AppRoot` drains
/// it during the next render, which is where a new inspector entity can be
/// created with access to [`Window`].
#[derive(Clone)]
pub struct EditRequest {
    pub component_id: ComponentId,
    pub component_name: SharedString,
    pub element_names: Vec<SharedString>,
    pub element_index: usize,
    /// Click position used to anchor the editor. `None` falls back to a
    /// centered palette.
    pub anchor: Option<Point<Pixels>>,
}

/// Global holding the edit queue plus the cross-view signalling flags that
/// let click handlers defer UI work to the next render.
#[derive(Default)]
pub struct PendingEdits {
    pub edits: Vec<PendingEdit>,
    /// When true, writable cells render as read-only; Cmd+L toggles.
    pub locked: bool,
    pub pending_request: Option<EditRequest>,
    pub open_review_requested: bool,
}

impl Global for PendingEdits {}

impl PendingEdits {
    pub fn get(&self, id: ComponentId) -> Option<&PendingEdit> {
        self.edits.iter().find(|e| e.component_id == id)
    }

    fn get_mut(&mut self, id: ComponentId) -> Option<&mut PendingEdit> {
        self.edits.iter_mut().find(|e| e.component_id == id)
    }

    /// Replace the pending edit for a component, or insert if absent.
    pub fn upsert(&mut self, edit: PendingEdit) {
        if let Some(existing) = self.get_mut(edit.component_id) {
            *existing = edit;
        } else {
            self.edits.push(edit);
        }
    }

    pub fn remove(&mut self, id: ComponentId) {
        self.edits.retain(|e| e.component_id != id);
    }
}

/// Install the global. Call once at app startup before any edit-UI runs.
pub fn init(cx: &mut App) {
    cx.set_global(PendingEdits {
        edits: Vec::new(),
        locked: true,
        pending_request: None,
        open_review_requested: false,
    });
}

pub fn pending_edits(cx: &App) -> &PendingEdits {
    cx.global::<PendingEdits>()
}

pub fn pending_edits_mut(cx: &mut App) -> &mut PendingEdits {
    cx.global_mut::<PendingEdits>()
}

/// Encode `edit` as an [`UpdateComponent`] message and push it to the DB log.
pub fn apply_edit(db: &DB, edit: &PendingEdit) {
    let update = UpdateComponent {
        id: edit.component_id,
        value: edit.value.clone(),
    };
    let bytes = postcard::to_allocvec(&update).expect("postcard serialize");
    let _ = db.push_msg(Timestamp::now(), UpdateComponent::ID, &bytes);
}

/// Apply a pending component edit and remove `element_index` from its list
/// of modified elements.
///
/// Protocol updates are component-wide, so the full [`ComponentValue`]
/// (including other modified indices) is still sent; only the UI's
/// "modified" tracking is narrowed. Once the list empties the whole pending
/// edit is dropped.
pub fn apply_element(db: &DB, component_id: ComponentId, element_index: usize, cx: &mut App) {
    let Some(edit) = pending_edits(cx).get(component_id).cloned() else {
        return;
    };
    apply_edit(db, &edit);
    let pending = pending_edits_mut(cx);
    if let Some(existing) = pending.get_mut(component_id) {
        existing.modified_elements.retain(|&i| i != element_index);
        if existing.modified_elements.is_empty() {
            pending.remove(component_id);
        }
    }
}

/// Rows for the review palette: one per pending edit plus bulk apply/discard.
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

/// Top-level component picker for the "update component" palette action.
///
/// Selecting a row drills into an element picker and finally the editor.
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

/// Element picker. Scalar and single-element components skip straight to
/// the value editor since there's nothing to choose.
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

/// Value-editor rows for a single element.
///
/// Enum components list one row per variant; other types fall back to a
/// free-text [`DefaultActionRow`] that parses input into the element's type.
pub fn edit_value_rows(db: Arc<DB>, request: EditRequest) -> Vec<Box<dyn InspectorRow>> {
    let EditRequest {
        component_id,
        component_name,
        element_names,
        element_index,
        anchor: _,
    } = request;

    let enum_variants: Option<Vec<String>> = db.with_state(|s| {
        s.get_component_metadata(component_id).and_then(|m| {
            m.enum_variants()
                .map(|it| it.map(|s| s.to_string()).collect())
        })
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

    vec![Box::new(DefaultActionRow::new(
        prompt,
        Arc::new(move |input, _window, cx| {
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
    ))]
}

/// Stage a string-component edit by overwriting its `U8` buffer with UTF-8
/// bytes, zero-padding the remainder so the receiver sees a null-terminated
/// string.
///
/// Strings have no element-level granularity, so the pending edit records
/// `modified_elements = [0]` as a sentinel.
pub fn upsert_string_value(
    db: &DB,
    component_id: ComponentId,
    component_name: SharedString,
    element_names: Vec<SharedString>,
    text: &str,
    cx: &mut App,
) {
    let Some(mut value) = latest_value(db, component_id) else {
        return;
    };
    let bytes = text.as_bytes();
    if let ComponentValue::U8(arr) = &mut value
        && bytes.len() <= arr.buf.as_buf().len().saturating_sub(1)
    {
        let slot = arr.buf.as_mut_buf();
        for (i, cell) in slot.iter_mut().enumerate() {
            *cell = *bytes.get(i).unwrap_or(&0);
        }
    } else {
        return;
    }
    merge_pending(cx, component_id, component_name, element_names, 0, value);
}

/// Stage an element edit when the caller already has a typed [`ElementValue`].
///
/// Used by inline editors (checkboxes, committed text fields) so simple
/// toggles avoid the string-parse round trip.
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
    if let Some(existing) = pending.get_mut(component_id) {
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

/// The component's most recent value, decoded into an owned buffer that an
/// edit can modify and send back whole.
fn latest_value(db: &DB, component_id: ComponentId) -> Option<ComponentValue> {
    let component = db.with_state(|s| s.get_component(component_id).cloned())?;
    let latest = component.time_series.latest()?;
    let (_size, view) = component.schema.parse_value(latest.data()).ok()?;
    Some(ComponentValue::from_view(view))
}

/// The component's latest value with `element` written at `element_index`.
fn build_updated_value_typed(
    db: &DB,
    component_id: ComponentId,
    element_index: usize,
    element: ElementValue,
) -> Option<ComponentValue> {
    let mut value = latest_value(db, component_id)?;
    set_element_typed(&mut value, element_index, element)?;
    Some(value)
}

fn set_element_typed(value: &mut ComponentValue, idx: usize, element: ElementValue) -> Option<()> {
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

/// Read the component's current value, apply the parsed `input` to
/// `element_index`, and return the combined [`ComponentValue`].
fn build_updated_value(
    db: &DB,
    component_id: ComponentId,
    element_index: usize,
    input: &str,
) -> Option<ComponentValue> {
    let mut value = latest_value(db, component_id)?;
    let element = TypedScalar::parse(input, value.as_view().prim_type())?;
    set_element_typed(&mut value, element_index, element.into())?;
    Some(value)
}
