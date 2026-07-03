//! The schema-guided KDL→postcard params encoder for dl systems (the
//! one-postcard-encoding path), split out of `wiring/mod.rs` (C6).

use std::collections::HashMap;

use kdl::KdlDocument;
use miette::SourceSpan;
use postcard_schema::schema::owned::{OwnedDataModelType, OwnedNamedType};
use serde_json::{Map, Value};

use super::{LoadError, de};

/// Encode a dl system's KDL node config into the canonical postcard `Params` bytes,
/// **guided by the `.so`'s exported `Params` schema** (the one-postcard-encoding
/// decision). The host never links the system's `Params` type: the shared KDL
/// deserializer ([`de`]) reads the node's params surface into a dynamic
/// [`serde_json::Value`] (plus a key → span table), the value is conformed to the
/// schema ([`conform_to_schema`]: unknown/missing/type-mismatched fields become
/// named, spanned errors; absent `Option` fields become explicit nulls), and
/// [`postcard_dyn::to_stdvec_dyn`] emits the **same bytes** the typed Rust builder's
/// [`WiringBuilder::params`](crate::WiringBuilder) postcard-encodes (the byte
/// equality is the headline equivalence gate, asserted in `tests_abi`).
///
/// `node_text` is the carried node source ([`ParamSource::Kdl`]); `schema` is
/// [`DlSystem::params_schema`]; `system` names the instance for diagnostics;
/// `reserved`/`skip_args` name the node's wiring surface (`type=`/`artifact=` + the
/// instance name on `system` nodes, `occupant=` on `allow` nodes). Errors are
/// span-aware [`LoadError`]s: [`UnknownParam`](LoadError::UnknownParam),
/// [`MissingParam`](LoadError::MissingParam), [`InvalidParam`](LoadError::InvalidParam),
/// or [`DlParamEncode`](LoadError::DlParamEncode) for an un-encodable schema shape.
pub fn encode_kdl_params(
    node_text: &str,
    schema: &OwnedNamedType,
    system: &str,
    reserved: &'static [&'static str],
    skip_args: usize,
) -> Result<Vec<u8>, LoadError> {
    let span: SourceSpan = (0, node_text.len()).into();
    let encode_err = |reason: String| LoadError::DlParamEncode {
        system: system.to_string(),
        reason,
        src: node_text.to_string(),
        span,
    };

    let doc = node_text.parse::<KdlDocument>().map_err(|e| encode_err(e.to_string()))?;
    let node = doc
        .nodes()
        .first()
        .ok_or_else(|| encode_err("the carried params text has no node".into()))?;

    let (value, spans) = de::params_value(node, node_text, system, reserved, skip_args)?;
    let value = conform_to_schema(&schema.ty, value, &spans, system, node_text, node.span())
        .map_err(|e| match e {
            Conform::Load(err) => *err,
            Conform::Shape(reason) => encode_err(reason),
        })?;

    postcard_dyn::to_stdvec_dyn(schema, &value)
        .map_err(|e| encode_err(format!("dynamic postcard encode failed: {e:?}")))
}

/// A [`conform_to_schema`] failure: a named, spanned params diagnostic, or a schema
/// shape the walk cannot model (surfaced as [`DlParamEncode`](LoadError::DlParamEncode)).
enum Conform {
    Load(Box<LoadError>),
    Shape(String),
}

impl From<LoadError> for Conform {
    fn from(e: LoadError) -> Self {
        Conform::Load(Box::new(e))
    }
}

/// Conform a deserialized params [`Value`] to a `Params` schema **struct** (or unit):
/// every JSON key must be a schema field ([`UnknownParam`](LoadError::UnknownParam)),
/// every non-`Option` schema field must be present ([`MissingParam`](LoadError::MissingParam),
/// absent `Option`s get an explicit `Null` — postcard-dyn requires it), and each field
/// value is checked/recursed by [`conform_value`]. The output object holds schema
/// field order (order is irrelevant to postcard-dyn, which iterates the schema).
fn conform_to_schema(
    ty: &OwnedDataModelType,
    value: Value,
    spans: &HashMap<String, SourceSpan>,
    system: &str,
    src: &str,
    node_span: SourceSpan,
) -> Result<Value, Conform> {
    let fields = match ty {
        OwnedDataModelType::Struct(fields) => fields.as_slice(),
        OwnedDataModelType::Unit | OwnedDataModelType::UnitStruct => &[],
        other => {
            return Err(Conform::Shape(format!(
                "the `Params` schema is `{other:?}`, which a KDL params surface cannot \
                 express (only a struct, or a unit)"
            )));
        }
    };
    let mut obj = match value {
        Value::Object(map) => map,
        other => {
            return Err(Conform::Shape(format!(
                "expected a params object for a struct schema, got `{other}`"
            )));
        }
    };

    // Every key must name a schema field (the typo guard, with the entry's span).
    for key in obj.keys() {
        if !fields.iter().any(|f| f.name == *key) {
            return Err(LoadError::UnknownParam {
                property: key.clone(),
                system: system.to_string(),
                src: src.to_string(),
                span: spans.get(key).copied().unwrap_or(node_span),
            }
            .into());
        }
    }

    let mut out = Map::new();
    for field in fields {
        let span = spans.get(&field.name).copied().unwrap_or(node_span);
        match obj.remove(&field.name) {
            Some(v) => {
                let v = conform_value(&field.ty.ty, v, &field.name, span, system, src)?;
                out.insert(field.name.clone(), v);
            }
            // An `Option` field with no property is `None` (postcard-dyn needs the
            // explicit null); any other absent field is a hard miss (no defaults).
            None if matches!(field.ty.ty, OwnedDataModelType::Option(_)) => {
                out.insert(field.name.clone(), Value::Null);
            }
            None => {
                return Err(LoadError::MissingParam {
                    property: field.name.clone(),
                    system: system.to_string(),
                    src: src.to_string(),
                    span: node_span,
                }
                .into());
            }
        }
    }
    Ok(Value::Object(out))
}

/// Check/recurse one field value against its schema type. Leaves are type- and
/// range-checked with [`leaf_expected`] wording; nested structs/sequences/options
/// recurse (nested errors carry the **top-level** property's span — the side table
/// is top-level only, v1). Any schema shape the walk does not model passes through
/// to postcard-dyn, whose failure surfaces as
/// [`DlParamEncode`](LoadError::DlParamEncode) — never silently wrong bytes.
fn conform_value(
    ty: &OwnedDataModelType,
    value: Value,
    property: &str,
    span: SourceSpan,
    system: &str,
    src: &str,
) -> Result<Value, Conform> {
    use OwnedDataModelType as T;
    let mismatch = || -> Conform {
        LoadError::InvalidParam {
            property: property.to_string(),
            system: system.to_string(),
            expected: leaf_expected(ty),
            src: src.to_string(),
            span,
        }
        .into()
    };
    // The signed/unsigned width an integer leaf must fit.
    let int_ok = |min: i128, max: i128| -> bool {
        match &value {
            Value::Number(n) => n
                .as_i64()
                .map(|i| (i as i128) >= min && (i as i128) <= max)
                .or_else(|| n.as_u64().map(|u| (u as i128) <= max))
                .unwrap_or(false),
            _ => false,
        }
    };
    match ty {
        T::Bool => value.is_boolean().then_some(value).ok_or_else(mismatch),
        T::I8 => int_ok(i8::MIN as i128, i8::MAX as i128).then_some(value).ok_or_else(mismatch),
        T::I16 => int_ok(i16::MIN as i128, i16::MAX as i128).then_some(value).ok_or_else(mismatch),
        T::I32 => int_ok(i32::MIN as i128, i32::MAX as i128).then_some(value).ok_or_else(mismatch),
        // postcard-dyn reads i64/isize/i128 via `as_i64` — i64 range on the wire.
        T::I64 | T::Isize | T::I128 => {
            int_ok(i64::MIN as i128, i64::MAX as i128).then_some(value).ok_or_else(mismatch)
        }
        T::U8 => int_ok(0, u8::MAX as i128).then_some(value).ok_or_else(mismatch),
        T::U16 => int_ok(0, u16::MAX as i128).then_some(value).ok_or_else(mismatch),
        T::U32 => int_ok(0, u32::MAX as i128).then_some(value).ok_or_else(mismatch),
        // postcard-dyn reads u64/usize/u128 via `as_u64` — u64 range on the wire.
        T::U64 | T::Usize | T::U128 => {
            int_ok(0, u64::MAX as i128).then_some(value).ok_or_else(mismatch)
        }
        // postcard-dyn's `as_f64` accepts integer literals (`rate=200` ⇒ 200.0).
        T::F32 | T::F64 => value.is_number().then_some(value).ok_or_else(mismatch),
        T::String => value.is_string().then_some(value).ok_or_else(mismatch),
        T::Char => match &value {
            Value::String(s) if s.chars().count() == 1 => Ok(value),
            _ => Err(mismatch()),
        },
        // A present `Option<T>` is the `Some(T)` inner value; `#null` is `None`.
        T::Option(inner) => match value {
            Value::Null => Ok(Value::Null),
            v => conform_value(&inner.ty, v, property, span, system, src),
        },
        // Nested structs recurse with the same rules; the top-level property's span
        // is the fallback for everything inside.
        T::Struct(_) | T::Unit | T::UnitStruct => {
            conform_to_schema(ty, value, &HashMap::new(), system, src, span)
        }
        T::Seq(inner) => match value {
            Value::Array(items) => {
                let items = items
                    .into_iter()
                    .map(|v| conform_value(&inner.ty, v, property, span, system, src))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(Value::Array(items))
            }
            _ => Err(mismatch()),
        },
        T::NewtypeStruct(inner) => conform_value(&inner.ty, value, property, span, system, src),
        // Tuples/maps/enums/etc.: pass through — postcard-dyn models them, and its
        // failure surfaces as `DlParamEncode` (never silently wrong bytes).
        _ => Ok(value),
    }
}

/// A human name of a schema leaf type, for [`InvalidParam`](LoadError::InvalidParam)
/// on the dl (schema-guided) path.
fn leaf_expected(ty: &OwnedDataModelType) -> String {
    use OwnedDataModelType as T;
    match ty {
        T::Bool => "a boolean (#true/#false)".into(),
        T::I8 | T::I16 | T::I32 | T::I64 | T::Isize | T::I128 => "a signed integer".into(),
        T::U8 | T::U16 | T::U32 | T::U64 | T::Usize | T::U128 => "a non-negative integer".into(),
        T::F32 | T::F64 => "a number".into(),
        T::String | T::Char => "a string".into(),
        T::Option(inner) => format!("{} (or omit the property)", leaf_expected(&inner.ty)),
        T::Seq(inner) => format!("a sequence of {}", leaf_expected(&inner.ty)),
        T::Struct(_) => "a nested node of params".into(),
        other => format!("an unsupported type ({other:?})"),
    }
}

