//! How a command's draft becomes the bytes its link carries.
//!
//! A draft is a `serde_json::Value` in the shape `postcard_dyn` reads: a
//! struct is an object keyed by field name, a unit variant its name, a
//! variant with a payload a one-key object, an `Option` `null` or its value,
//! a tuple an array. Frames reuse the same value type keyed by field label,
//! so every command kind edits and stores its draft the same way.
//!
//! Encoding checks the draft against the schema before handing it to
//! `postcard_dyn`, which would otherwise narrow an `f32` silently and report
//! an out-of-range integer only as a schema mismatch with no field named.

use gpui::SharedString;
use metor_proto::types::{PrimType, Timestamp};
use metor_proto_wkt::{CommandInfo, CommandKind, ComponentMetadata};
use postcard_schema::schema::owned::{OwnedDataModelType, OwnedDataModelVariant, OwnedNamedType};
use serde_json::{Map, Value};

use super::frame::FrameLayout;

/// What the encoder knows about a command: enough to build its form, seed
/// its draft and turn the draft into bytes.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Form {
    /// A postcard message, encoded against its announced schema.
    Postcard(OwnedNamedType),
    /// A message whose payload is JSON text; the panel knows no shape for it.
    Json,
    /// A fixed `#[repr(C)]` record laid out by its vtable.
    Frame(FrameLayout),
    /// Listed so the operator sees it exists, but never sent.
    Unsupported(SharedString),
}

/// Why a draft could not be encoded. `field` is the dotted path into the
/// draft, so the operator can find the row to fix.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub(crate) enum EncodeError {
    #[error("{field}: {value} is out of range for {ty}")]
    OutOfRange {
        field: String,
        value: String,
        ty: &'static str,
    },
    #[error("{field}: expected {expected}")]
    Mismatch {
        field: String,
        expected: SharedString,
    },
    #[error("{0}")]
    Unsupported(SharedString),
}

impl Form {
    /// The form for `info`. `metadata` resolves a frame field's component to
    /// its announced name.
    pub(crate) fn of(
        info: &CommandInfo,
        metadata: &dyn Fn(metor_proto::types::ComponentId) -> Option<ComponentMetadata>,
    ) -> Self {
        match &info.kind {
            CommandKind::Msg(meta) => match meta.metadata.get("codec").map(String::as_str) {
                None => Self::Postcard(meta.schema.clone()),
                Some("json") => Self::Json,
                Some(codec) => {
                    Self::Unsupported(format!("The {codec} codec is not supported yet").into())
                }
            },
            CommandKind::Frame { vtable, .. } => match FrameLayout::new(vtable, metadata) {
                Ok(layout) => Self::Frame(layout),
                Err(reason) => Self::Unsupported(reason),
            },
        }
    }

    /// A draft every field of which holds a valid value: zero, `false`, the
    /// empty string, the first variant.
    pub(crate) fn default_draft(&self) -> Value {
        match self {
            Self::Postcard(schema) => default_value(&schema.ty),
            Self::Json => Value::Null,
            Self::Frame(layout) => layout.default_draft(),
            Self::Unsupported(_) => Value::Null,
        }
    }

    /// The bytes `draft` sends as. `now` fills a frame's timestamp, and is
    /// the time the caller records the command at.
    pub(crate) fn encode(&self, draft: &Value, now: Timestamp) -> Result<Vec<u8>, EncodeError> {
        match self {
            Self::Postcard(schema) => postcard(schema, draft, ""),
            Self::Json => serde_json::to_vec(draft).map_err(|err| EncodeError::Mismatch {
                field: "value".into(),
                expected: format!("JSON ({err})").into(),
            }),
            Self::Frame(layout) => layout.encode(draft, now),
            Self::Unsupported(reason) => Err(EncodeError::Unsupported(reason.clone())),
        }
    }
}

/// Encode `value` as `schema`, naming fields relative to `field` in errors.
pub(crate) fn postcard(
    schema: &OwnedNamedType,
    value: &Value,
    field: &str,
) -> Result<Vec<u8>, EncodeError> {
    check(&schema.ty, value, field)?;
    postcard_dyn::to_stdvec_dyn(schema, value).map_err(|_| EncodeError::Mismatch {
        field: display_field(field),
        expected: format!("a value of {}", schema.name).into(),
    })
}

fn default_value(ty: &OwnedDataModelType) -> Value {
    use OwnedDataModelType as T;
    match ty {
        T::Bool => Value::Bool(false),
        T::I8 | T::I16 | T::I32 | T::I64 | T::I128 | T::Isize => Value::from(0i64),
        T::U8 | T::U16 | T::U32 | T::U64 | T::U128 | T::Usize => Value::from(0u64),
        T::F32 | T::F64 => Value::from(0.0),
        T::Char => Value::String(" ".into()),
        T::String => Value::String(String::new()),
        T::ByteArray | T::Seq(_) => Value::Array(Vec::new()),
        T::Option(_) | T::Unit | T::UnitStruct | T::Schema => Value::Null,
        T::NewtypeStruct(inner) => default_value(&inner.ty),
        T::Tuple(items) | T::TupleStruct(items) => match items.as_slice() {
            [only] => default_value(&only.ty),
            items => Value::Array(items.iter().map(|item| default_value(&item.ty)).collect()),
        },
        T::Map { .. } => Value::Object(Map::new()),
        T::Struct(fields) => Value::Object(
            fields
                .iter()
                .map(|field| (field.name.clone(), default_value(&field.ty.ty)))
                .collect(),
        ),
        T::Enum(variants) => match variants.first() {
            None => Value::Null,
            Some(variant) => match &variant.ty {
                OwnedDataModelVariant::UnitVariant => Value::String(variant.name.clone()),
                payload => {
                    let payload = match payload {
                        OwnedDataModelVariant::UnitVariant => Value::Null,
                        OwnedDataModelVariant::NewtypeVariant(inner) => default_value(&inner.ty),
                        OwnedDataModelVariant::TupleVariant(items) => {
                            default_value(&OwnedDataModelType::Tuple(items.clone()))
                        }
                        OwnedDataModelVariant::StructVariant(fields) => {
                            default_value(&OwnedDataModelType::Struct(fields.clone()))
                        }
                    };
                    Value::Object(Map::from_iter([(variant.name.clone(), payload)]))
                }
            },
        },
    }
}

fn display_field(field: &str) -> String {
    match field {
        "" => "value".into(),
        field => field.into(),
    }
}

fn child(field: &str, name: &str) -> String {
    match field {
        "" => name.into(),
        field => format!("{field}.{name}"),
    }
}

fn mismatch(field: &str, expected: &str) -> EncodeError {
    EncodeError::Mismatch {
        field: display_field(field),
        expected: SharedString::from(expected.to_string()),
    }
}

/// Walk `value` against `ty`, refusing what the wire type cannot hold.
fn check(ty: &OwnedDataModelType, value: &Value, field: &str) -> Result<(), EncodeError> {
    use OwnedDataModelType as T;
    match ty {
        T::Bool => value
            .as_bool()
            .map(drop)
            .ok_or_else(|| mismatch(field, "true or false")),
        T::I8 => integer(value, field, "i8", i8::MIN.into(), i8::MAX.into()).map(drop),
        T::I16 => integer(value, field, "i16", i16::MIN.into(), i16::MAX.into()).map(drop),
        T::I32 => integer(value, field, "i32", i32::MIN.into(), i32::MAX.into()).map(drop),
        T::I64 | T::I128 | T::Isize => {
            integer(value, field, "i64", i64::MIN.into(), i64::MAX.into()).map(drop)
        }
        T::U8 => integer(value, field, "u8", 0, u8::MAX.into()).map(drop),
        T::U16 => integer(value, field, "u16", 0, u16::MAX.into()).map(drop),
        T::U32 => integer(value, field, "u32", 0, u32::MAX.into()).map(drop),
        T::U64 | T::U128 | T::Usize => integer(value, field, "u64", 0, u64::MAX.into()).map(drop),
        T::F32 => float(value, field, f32::MAX.into(), "f32").map(drop),
        T::F64 => float(value, field, f64::MAX, "f64").map(drop),
        T::String => value
            .as_str()
            .map(drop)
            .ok_or_else(|| mismatch(field, "text")),
        T::Char => match value.as_str().map(|text| text.chars().count()) {
            Some(1) => Ok(()),
            _ => Err(mismatch(field, "a single character")),
        },
        T::ByteArray => {
            let items = value
                .as_array()
                .ok_or_else(|| mismatch(field, "an array of bytes"))?;
            for (ix, item) in items.iter().enumerate() {
                integer(item, &child(field, &ix.to_string()), "u8", 0, 255)?;
            }
            Ok(())
        }
        T::Option(inner) => match value {
            Value::Null => Ok(()),
            value => check(&inner.ty, value, field),
        },
        T::Unit | T::UnitStruct => Ok(()),
        T::NewtypeStruct(inner) => check(&inner.ty, value, field),
        T::Seq(inner) => {
            let items = value
                .as_array()
                .ok_or_else(|| mismatch(field, "an array"))?;
            for (ix, item) in items.iter().enumerate() {
                check(&inner.ty, item, &child(field, &ix.to_string()))?;
            }
            Ok(())
        }
        T::Tuple(items) | T::TupleStruct(items) => match items.as_slice() {
            [only] => check(&only.ty, value, field),
            items => check_tuple(items, value, field),
        },
        T::Map { val, .. } => {
            let entries = value
                .as_object()
                .ok_or_else(|| mismatch(field, "an object"))?;
            for (key, item) in entries {
                check(&val.ty, item, &child(field, key))?;
            }
            Ok(())
        }
        T::Struct(fields) => check_struct(fields, value, field),
        T::Enum(variants) => {
            let (name, payload) = match value {
                Value::String(name) => (name.as_str(), None),
                Value::Object(entries) if entries.len() == 1 => {
                    let (name, payload) = entries.iter().next().expect("one entry");
                    (name.as_str(), Some(payload))
                }
                _ => return Err(mismatch(field, "a variant name")),
            };
            let variant = variants
                .iter()
                .find(|variant| variant.name == name)
                .ok_or_else(|| mismatch(field, "a declared variant"))?;
            let field = &child(field, name);
            match (&variant.ty, payload) {
                (OwnedDataModelVariant::UnitVariant, _) => Ok(()),
                (_, None) => Err(mismatch(field, "a payload")),
                (OwnedDataModelVariant::NewtypeVariant(inner), Some(payload)) => {
                    check(&inner.ty, payload, field)
                }
                (OwnedDataModelVariant::TupleVariant(items), Some(payload)) => {
                    match items.as_slice() {
                        [only] => check(&only.ty, payload, field),
                        items => check_tuple(items, payload, field),
                    }
                }
                (OwnedDataModelVariant::StructVariant(fields), Some(payload)) => {
                    check_struct(fields, payload, field)
                }
            }
        }
        T::Schema => Err(EncodeError::Unsupported(
            "Schema-typed fields are not supported".into(),
        )),
    }
}

fn check_tuple(items: &[OwnedNamedType], value: &Value, field: &str) -> Result<(), EncodeError> {
    let values = value
        .as_array()
        .filter(|values| values.len() == items.len())
        .ok_or_else(|| mismatch(field, &format!("an array of {}", items.len())))?;
    for (ix, (item, value)) in items.iter().zip(values).enumerate() {
        check(&item.ty, value, &child(field, &ix.to_string()))?;
    }
    Ok(())
}

fn check_struct(
    fields: &[postcard_schema::schema::owned::OwnedNamedValue],
    value: &Value,
    field: &str,
) -> Result<(), EncodeError> {
    let entries = value
        .as_object()
        .ok_or_else(|| mismatch(field, "an object"))?;
    for declared in fields {
        let name = child(field, &declared.name);
        let item = entries
            .get(&declared.name)
            .ok_or_else(|| mismatch(&name, "a value"))?;
        check(&declared.ty.ty, item, &name)?;
    }
    if let Some(extra) = entries
        .keys()
        .find(|key| !fields.iter().any(|declared| &declared.name == *key))
    {
        return Err(mismatch(&child(field, extra), "no such field"));
    }
    Ok(())
}

/// `value` as an integer within `min..=max`.
pub(crate) fn integer(
    value: &Value,
    field: &str,
    ty: &'static str,
    min: i128,
    max: i128,
) -> Result<i128, EncodeError> {
    let n = match (value.as_u64(), value.as_i64()) {
        (Some(n), _) => i128::from(n),
        (_, Some(n)) => i128::from(n),
        _ if value.is_number() => return Err(mismatch(field, "a whole number")),
        _ => return Err(mismatch(field, "a number")),
    };
    if n < min || n > max {
        return Err(EncodeError::OutOfRange {
            field: display_field(field),
            value: n.to_string(),
            ty,
        });
    }
    Ok(n)
}

/// `value` as a finite float no larger in magnitude than `max`.
pub(crate) fn float(
    value: &Value,
    field: &str,
    max: f64,
    ty: &'static str,
) -> Result<f64, EncodeError> {
    let n = value.as_f64().ok_or_else(|| mismatch(field, "a number"))?;
    if !n.is_finite() || n.abs() > max {
        return Err(EncodeError::OutOfRange {
            field: display_field(field),
            value: n.to_string(),
            ty,
        });
    }
    Ok(n)
}

/// The prim type a schema number edits as, if it is one. 128-bit and
/// pointer-sized integers edit as 64-bit, the widest `postcard_dyn` writes.
pub(crate) fn prim_type(ty: &OwnedDataModelType) -> Option<PrimType> {
    use OwnedDataModelType as T;
    Some(match ty {
        T::U8 => PrimType::U8,
        T::U16 => PrimType::U16,
        T::U32 => PrimType::U32,
        T::U64 | T::U128 | T::Usize => PrimType::U64,
        T::I8 => PrimType::I8,
        T::I16 => PrimType::I16,
        T::I32 => PrimType::I32,
        T::I64 | T::I128 | T::Isize => PrimType::I64,
        T::F32 => PrimType::F32,
        T::F64 => PrimType::F64,
        _ => return None,
    })
}
