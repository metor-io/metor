//! A command's form: inspector rows built from its schema, each bound to a
//! place in the command's draft.
//!
//! Unlike the facet registry, which reflects a Rust type the panel compiled,
//! these rows come from a schema the remote announced at runtime. Shapes
//! with a natural row get one: numbers, bools, text, unit-only enums,
//! structs as a drill-in page, and enums with payloads as a page of their
//! variants. Everything else (options, sequences, maps, tuples) is edited
//! as JSON text for that subtree, checked against its schema as it is
//! entered.

use std::sync::Arc;

use gpui::{App, Entity, SharedString};
use metor_proto::types::{PacketId, PrimType};
use postcard_schema::schema::owned::{
    OwnedDataModelType, OwnedDataModelVariant, OwnedNamedType, OwnedNamedValue, OwnedNamedVariant,
};
use serde_json::Value;

use super::encode::{self, Form, prim_type};
use super::frame::{FrameField, FrameLayout};
use super::{CommandCatalog, Outcome};
use crate::dynamic::tensor::TypedScalar;
use crate::inspector::rows::{
    BoolRow, CommandRow, EnumRow, InspectorRow, NavRow, RowAction, ScalarRow, TextRow,
};
use crate::theme::theme;

type Rows = Vec<Box<dyn InspectorRow>>;

/// A place in one command's draft, addressed by JSON pointer.
#[derive(Clone)]
struct Slot {
    catalog: Entity<CommandCatalog>,
    id: PacketId,
    pointer: String,
}

impl Slot {
    fn child(&self, key: &str) -> Self {
        let key = key.replace('~', "~0").replace('/', "~1");
        Self {
            pointer: format!("{}/{key}", self.pointer),
            ..self.clone()
        }
    }

    fn read(&self, cx: &App) -> Option<Value> {
        self.catalog.read(cx).field(self.id, &self.pointer).cloned()
    }

    fn write(&self, value: Value, cx: &mut App) {
        self.catalog.update(cx, |catalog, cx| {
            catalog.set_field(self.id, &self.pointer, value, cx)
        });
    }

    fn refuse(&self, reason: String, cx: &mut App) {
        self.catalog
            .update(cx, |catalog, cx| catalog.refuse(self.id, reason.into(), cx));
    }

    /// The value as one line of JSON, the summary of a page or JSON row.
    fn compact(&self, cx: &App) -> SharedString {
        self.read(cx)
            .map(|value| value.to_string())
            .unwrap_or_default()
            .into()
    }
}

/// The Send page for command `id`: its fields, then Send and the outcome of
/// the last attempt.
pub(crate) fn command_rows(catalog: &Entity<CommandCatalog>, id: PacketId, cx: &App) -> Rows {
    let Some(command) = catalog.read(cx).command(id) else {
        return vec![Box::new(TextRow::readonly(
            "Unavailable".into(),
            "The command is no longer offered".into(),
        ))];
    };
    let root = Slot {
        catalog: catalog.clone(),
        id,
        pointer: String::new(),
    };
    let mut rows = match &command.form {
        Form::Postcard(schema) => schema_rows(schema, &root, cx),
        Form::Json => vec![json_row("JSON".into(), None, root.clone(), cx)],
        Form::Frame(layout) => frame_rows(layout, &root, cx),
        Form::Unsupported(reason) => {
            return vec![Box::new(TextRow::readonly(
                "Unsupported".into(),
                reason.clone(),
            ))];
        }
    };
    rows.push(send_row(root.clone()));
    rows.push(outcome_row(root));
    rows
}

fn send_row(slot: Slot) -> Box<dyn InspectorRow> {
    Box::new(
        CommandRow::action(
            "Send",
            Arc::new(move |_, cx| {
                slot.catalog.update(cx, |catalog, cx| {
                    let _ = catalog.send(slot.id, cx);
                });
                RowAction::Handled
            }),
        )
        .pinned(),
    )
}

fn outcome_row(slot: Slot) -> Box<dyn InspectorRow> {
    let tint_slot = slot.clone();
    Box::new(
        TextRow::readonly("Status".into(), SharedString::default())
            .live(Arc::new(move |cx| {
                Some(match slot.catalog.read(cx).outcome(slot.id) {
                    Some(Outcome::Sent(at)) => {
                        format!("Sent {}", crate::views::format::format_time(at.0)).into()
                    }
                    Some(Outcome::Refused(reason)) => reason.clone(),
                    None => "Not sent yet".into(),
                })
            }))
            .tinted(Arc::new(move |cx| {
                let theme = theme(cx);
                match tint_slot.catalog.read(cx).outcome(tint_slot.id) {
                    Some(Outcome::Sent(_)) => Some(theme.control_active),
                    Some(Outcome::Refused(_)) => Some(theme.error_accent),
                    None => None,
                }
            })),
    )
}

/// The rows for a whole message: a struct's fields directly, or one row
/// for any other root.
fn schema_rows(schema: &OwnedNamedType, slot: &Slot, cx: &App) -> Rows {
    match &schema.ty {
        OwnedDataModelType::Struct(fields) => fields_rows(fields, slot, cx),
        OwnedDataModelType::NewtypeStruct(inner) => schema_rows(inner, slot, cx),
        _ => field_row(schema, schema.name.clone().into(), slot.clone(), cx)
            .into_iter()
            .collect(),
    }
}

/// The row editing `slot` as `schema`; `None` for a unit, which has nothing
/// to edit.
fn field_row(
    schema: &OwnedNamedType,
    label: SharedString,
    slot: Slot,
    cx: &App,
) -> Option<Box<dyn InspectorRow>> {
    use OwnedDataModelType as T;
    Some(match &schema.ty {
        T::Unit | T::UnitStruct => return None,
        T::Bool => bool_row(label, slot),
        T::String | T::Char => {
            let source = slot.clone();
            Box::new(
                TextRow::new(
                    label,
                    text_of(&slot, cx),
                    Arc::new(move |text, _, cx| slot.write(Value::String(text), cx)),
                )
                .live(Arc::new(move |cx| Some(text_of(&source, cx)))),
            )
        }
        T::Enum(variants)
            if variants
                .iter()
                .all(|variant| variant.ty == OwnedDataModelVariant::UnitVariant) =>
        {
            let options = variants
                .iter()
                .map(|variant| SharedString::from(variant.name.clone()))
                .collect();
            let source = slot.clone();
            Box::new(
                EnumRow::new(
                    label,
                    text_of(&slot, cx),
                    options,
                    Arc::new(move |name, _, cx| slot.write(Value::String(name), cx)),
                )
                .live(Arc::new(move |cx| Some(text_of(&source, cx)))),
            )
        }
        T::Struct(fields) => {
            let fields = fields.clone();
            let summary = slot.clone();
            Box::new(NavRow::with_dynamic_summary(
                label,
                Box::new(move |cx| summary.compact(cx)),
                Box::new(move |cx| fields_rows(&fields, &slot, cx)),
            ))
        }
        T::Enum(variants) => variants_row(label, variants.clone(), slot),
        T::NewtypeStruct(inner) => return field_row(inner, label, slot, cx),
        T::Tuple(items) | T::TupleStruct(items) if items.len() == 1 => {
            return field_row(&items[0], label, slot, cx);
        }
        ty => match prim_type(ty) {
            Some(prim) => scalar_row(label, prim, slot, cx),
            None => json_row(label, Some(schema.clone()), slot, cx),
        },
    })
}

fn fields_rows(fields: &[OwnedNamedValue], slot: &Slot, cx: &App) -> Rows {
    fields
        .iter()
        .filter_map(|field| {
            field_row(
                &field.ty,
                field.name.clone().into(),
                slot.child(&field.name),
                cx,
            )
        })
        .collect()
}

/// An enum with payloads, as a page of its variants: the current one is
/// tagged, choosing a unit variant sets it and returns, and entering one
/// with a payload switches to it and edits that payload with the usual
/// rows. Picking a variant and filling it in are then one gesture, and no
/// page has to rebuild when the variant changes.
fn variants_row(
    label: SharedString,
    variants: Vec<OwnedNamedVariant>,
    slot: Slot,
) -> Box<dyn InspectorRow> {
    let summary = slot.clone();
    Box::new(NavRow::with_dynamic_summary(
        label,
        Box::new(move |cx| describe(summary.read(cx).as_ref())),
        Box::new(move |_| {
            variants
                .iter()
                .map(|variant| variant_row(variant.clone(), slot.clone()))
                .collect()
        }),
    ))
}

fn variant_row(variant: OwnedNamedVariant, slot: Slot) -> Box<dyn InspectorRow> {
    let name = SharedString::from(variant.name.clone());
    let current = {
        let (slot, name) = (slot.clone(), name.clone());
        move |cx: &App| {
            (variant_of(slot.read(cx).as_ref()) == Some(name.as_ref()))
                .then(|| SharedString::new_static("current"))
        }
    };
    if variant.ty == OwnedDataModelVariant::UnitVariant {
        return Box::new(
            CommandRow::action(
                name,
                Arc::new(move |_, cx| {
                    slot.write(Value::String(variant.name.clone()), cx);
                    RowAction::Pop
                }),
            )
            .with_live_tag(Arc::new(current)),
        );
    }
    let payload = slot.child(&variant.name);
    let summary = {
        let (slot, name) = (slot.clone(), name.clone());
        let payload = payload.clone();
        move |cx: &App| match variant_of(slot.read(cx).as_ref()) == Some(name.as_ref()) {
            true => payload.compact(cx),
            false => SharedString::default(),
        }
    };
    let ty = variant.ty.clone();
    Box::new(
        NavRow::with_dynamic_summary(
            name.clone(),
            Box::new(summary),
            Box::new(move |cx| match &ty {
                OwnedDataModelVariant::UnitVariant => Vec::new(),
                OwnedDataModelVariant::NewtypeVariant(inner) => schema_rows(inner, &payload, cx),
                OwnedDataModelVariant::StructVariant(fields) => fields_rows(fields, &payload, cx),
                OwnedDataModelVariant::TupleVariant(items) => items
                    .iter()
                    .enumerate()
                    .filter_map(|(ix, item)| {
                        let ix = ix.to_string();
                        field_row(item, ix.clone().into(), payload.child(&ix), cx)
                    })
                    .collect(),
            }),
        )
        .with_live_tag(Box::new(current))
        .with_on_enter(Box::new(move |cx| {
            // Entering the current variant keeps what was typed into it.
            if variant_of(slot.read(cx).as_ref()) != Some(name.as_ref()) {
                slot.write(encode::variant_default(&variant), cx);
            }
        })),
    )
}

/// The variant an enum's value holds: a bare name, or the one key of
/// `{name: payload}`.
fn variant_of(value: Option<&Value>) -> Option<&str> {
    match value? {
        Value::String(name) => Some(name),
        Value::Object(map) if map.len() == 1 => map.keys().next().map(String::as_str),
        _ => None,
    }
}

/// An enum's value as a summary: the variant's name, then its payload.
fn describe(value: Option<&Value>) -> SharedString {
    match value {
        Some(Value::Object(map)) if map.len() == 1 => {
            let (name, payload) = map.iter().next().expect("one entry");
            format!("{name} {payload}").into()
        }
        Some(Value::String(name)) => name.clone().into(),
        Some(value) => value.to_string().into(),
        None => SharedString::default(),
    }
}

fn text_of(slot: &Slot, cx: &App) -> SharedString {
    slot.read(cx)
        .and_then(|value| value.as_str().map(|text| text.to_string().into()))
        .unwrap_or_default()
}

fn bool_row(label: SharedString, slot: Slot) -> Box<dyn InspectorRow> {
    let source = slot.clone();
    Box::new(BoolRow::dynamic(
        label,
        Arc::new(move |cx| {
            source
                .read(cx)
                .and_then(|value| value.as_bool())
                .unwrap_or(false)
        }),
        Arc::new(move |value, _, cx| slot.write(Value::Bool(value), cx)),
    ))
}

fn scalar_row(label: SharedString, prim: PrimType, slot: Slot, cx: &App) -> Box<dyn InspectorRow> {
    let read = {
        let slot = slot.clone();
        move |cx: &App| slot.read(cx).and_then(|value| scalar_of(&value, prim))
    };
    let current = read(cx).unwrap_or(TypedScalar::from_f64(0.0, prim));
    Box::new(
        ScalarRow::typed(
            label,
            current,
            Arc::new(move |value, _, cx| slot.write(json_of(value), cx)),
        )
        .live(Arc::new(read)),
    )
}

/// A subtree edited as JSON text. With a schema, text that does not fit it
/// is refused as it is entered, so a bad edit is reported on the field
/// instead of at Send.
fn json_row(
    label: SharedString,
    schema: Option<OwnedNamedType>,
    slot: Slot,
    cx: &App,
) -> Box<dyn InspectorRow> {
    let source = slot.clone();
    let name = label.clone();
    Box::new(
        TextRow::new(
            label,
            slot.compact(cx),
            Arc::new(move |text, _, cx| {
                let value = match serde_json::from_str::<Value>(&text) {
                    Ok(value) => value,
                    Err(err) => return slot.refuse(format!("{name}: not JSON ({err})"), cx),
                };
                if let Some(schema) = &schema
                    && let Err(err) = encode::postcard(schema, &value, &name)
                {
                    return slot.refuse(err.to_string(), cx);
                }
                slot.write(value, cx);
            }),
        )
        .live(Arc::new(move |cx| Some(source.compact(cx)))),
    )
}

fn frame_rows(layout: &FrameLayout, root: &Slot, cx: &App) -> Rows {
    layout
        .fields()
        .iter()
        .map(|field| {
            let slot = root.child(&field.label);
            match field.shape.is_empty() {
                true => element_row(field.label.clone(), field.ty, slot, cx),
                false => tensor_row(field.clone(), slot),
            }
        })
        .collect()
}

fn element_row(label: SharedString, ty: PrimType, slot: Slot, cx: &App) -> Box<dyn InspectorRow> {
    match ty {
        PrimType::Bool => bool_row(label, slot),
        ty => scalar_row(label, ty, slot, cx),
    }
}

/// A tensor field as a page of its elements, summarized by its values.
fn tensor_row(field: FrameField, slot: Slot) -> Box<dyn InspectorRow> {
    let summary = slot.clone();
    Box::new(NavRow::with_dynamic_summary(
        field.label.clone(),
        Box::new(move |cx| summary.compact(cx)),
        Box::new(move |cx| {
            (0..field.len())
                .map(|ix| {
                    element_row(
                        field.element_label(ix),
                        field.ty,
                        slot.child(&ix.to_string()),
                        cx,
                    )
                })
                .collect()
        }),
    ))
}

/// A draft number as the row's typed scalar.
fn scalar_of(value: &Value, prim: PrimType) -> Option<TypedScalar> {
    match prim {
        PrimType::F32 | PrimType::F64 => Some(TypedScalar::from_f64(value.as_f64()?, prim)),
        PrimType::Bool => value.as_bool().map(TypedScalar::Bool),
        _ => {
            let text = match (value.as_u64(), value.as_i64()) {
                (Some(n), _) => n.to_string(),
                (_, Some(n)) => n.to_string(),
                _ => return None,
            };
            TypedScalar::parse(&text, prim)
        }
    }
}

fn json_of(value: TypedScalar) -> Value {
    match value {
        TypedScalar::U8(v) => v.into(),
        TypedScalar::U16(v) => v.into(),
        TypedScalar::U32(v) => v.into(),
        TypedScalar::U64(v) => v.into(),
        TypedScalar::I8(v) => v.into(),
        TypedScalar::I16(v) => v.into(),
        TypedScalar::I32(v) => v.into(),
        TypedScalar::I64(v) => v.into(),
        TypedScalar::F32(v) => f64::from(v).into(),
        TypedScalar::F64(v) => v.into(),
        TypedScalar::Bool(v) => v.into(),
    }
}
