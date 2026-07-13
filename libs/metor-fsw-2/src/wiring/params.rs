//! Schema-guided encoding of a dl system's params into postcard bytes.
//!
//! A dynamically loaded system's `Params` type is never linked into the host.
//! What the host has instead is the postcard schema the shared library exports
//! ([`DlSystem::params_schema`](crate::dl::DlSystem::params_schema)). This
//! module walks that schema to turn a params value tree into the exact bytes
//! the `Params` struct itself would postcard-encode to, so the loaded side can
//! decode them as its own type.
//!
//! [`encode_value_params`] starts from the
//! [`ParamSource::Value`](super::ParamSource::Value) tree the Python front-end
//! emits and runs the [`conform_and_encode`] tail: overlay the entry's
//! declared defaults, check the value against the schema — [`conform_to_schema`]
//! turns unknown keys, missing fields, and type mismatches into [`LoadError`]s
//! and inserts an explicit `Null` for every absent `Option` field, since
//! postcard-dyn requires the key to be present — and emit the bytes with
//! [`postcard_dyn::to_stdvec_dyn`].
//!
//! One property of the conformance walk is worth knowing: any schema shape the
//! walk does not model (tuples, maps, enums) passes through unchecked; if
//! postcard-dyn cannot encode it, the failure surfaces as
//! [`LoadErrorKind::DlParamEncode`] rather than as silently wrong bytes. Errors
//! anchor to the whole value-tree surface, which carries no document spans.

use std::collections::HashMap;

use miette::SourceSpan;
use postcard_schema::schema::owned::{OwnedDataModelType, OwnedNamedType};
use serde_json::{Map, Value};

use super::{LoadError, LoadErrorKind};

/// Encodes a params value tree into the postcard bytes described by `schema`,
/// over the [`conform_and_encode`] tail.
///
/// A value tree carries no document spans, so the rendered JSON stands in as
/// the diagnostic source and every error anchors to the whole surface.
pub fn encode_value_params(
    value: &serde_json::Value,
    schema: &OwnedNamedType,
    system: &str,
    defaults: Option<&[u8]>,
) -> Result<Vec<u8>, LoadError> {
    let src = value.to_string();
    let span: SourceSpan = (0, src.len()).into();
    conform_and_encode(
        value.clone(),
        schema,
        &HashMap::new(),
        system,
        defaults,
        &src,
        span,
    )
}

/// The shared encode tail: overlay the entry's declared defaults, conform
/// the value to the schema, and emit the postcard bytes. `spans` maps
/// top-level keys to their spans in `src` (empty for a value tree), and
/// `node_span` is the fallback anchor for everything the table misses.
fn conform_and_encode(
    value: Value,
    schema: &OwnedNamedType,
    spans: &HashMap<String, SourceSpan>,
    system: &str,
    defaults: Option<&[u8]>,
    src: &str,
    node_span: SourceSpan,
) -> Result<Vec<u8>, LoadError> {
    let encode_err = |reason: String| {
        LoadErrorKind::DlParamEncode {
            system: system.to_string(),
            reason,
        }
        .whole(src)
    };

    let value = match defaults {
        Some(bytes) if !bytes.is_empty() => {
            let base = postcard_dyn::from_slice_dyn(schema, bytes)
                .map_err(|e| encode_err(format!("default params do not decode: {e:?}")))?;
            merge_onto_defaults(base, value).map_err(encode_err)?
        }
        _ => value,
    };
    let value =
        conform_to_schema(&schema.ty, value, spans, system, src, node_span).map_err(|e| match e {
            Conform::Load(err) => *err,
            Conform::Shape(reason) => encode_err(reason),
        })?;

    postcard_dyn::to_stdvec_dyn(schema, &value)
        .map_err(|e| encode_err(format!("dynamic postcard encode failed: {e:?}")))
}

/// Overlay the config object's top-level keys onto the decoded default base.
fn merge_onto_defaults(base: Value, config: Value) -> Result<Value, String> {
    let Value::Object(mut base) = base else {
        return Err(format!("default params are not an object: `{base}`"));
    };
    let Value::Object(config) = config else {
        return Err(format!("expected a params object, got `{config}`"));
    };
    for (key, value) in config {
        base.insert(key, value);
    }
    Ok(Value::Object(base))
}

/// What the schema walk reports when a value does not fit, either a spanned
/// params diagnostic ready to surface as-is or a description of a schema shape
/// the walk cannot express (reported as [`LoadErrorKind::DlParamEncode`]).
enum Conform {
    Load(Box<LoadError>),
    Shape(String),
}

impl From<LoadError> for Conform {
    fn from(e: LoadError) -> Self {
        Conform::Load(Box::new(e))
    }
}

/// Checks a params object against a struct (or unit) schema, producing the
/// object postcard-dyn expects for that schema.
///
/// Every key must name a schema field and every non-`Option` field must be
/// present; there are no defaults. Absent `Option` fields become explicit
/// `Null`s. Field values are checked and recursed by [`conform_value`].
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
                "the `Params` schema is `{other:?}`, which a params value tree cannot \
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

    // The typo guard, with the offending entry's own span when we have one.
    for key in obj.keys() {
        if !fields.iter().any(|f| f.name == *key) {
            return Err(LoadErrorKind::UnknownParam {
                property: key.clone(),
                system: system.to_string(),
            }
            .at(src, spans.get(key).copied().unwrap_or(node_span))
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
            // postcard-dyn requires the key even for `None`, hence the explicit null.
            None if matches!(field.ty.ty, OwnedDataModelType::Option(_)) => {
                out.insert(field.name.clone(), Value::Null);
            }
            None => {
                return Err(LoadErrorKind::MissingParam {
                    property: field.name.clone(),
                    system: system.to_string(),
                }
                .at(src, node_span)
                .into());
            }
        }
    }
    Ok(Value::Object(out))
}

/// Checks one field value against its schema type, recursing into options,
/// sequences, newtypes, and nested structs.
///
/// Unmodeled schema shapes pass through to postcard-dyn, and errors inside
/// nested values reuse the top-level property's span; see the module docs.
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
        LoadErrorKind::InvalidParam {
            property: property.to_string(),
            system: system.to_string(),
            expected: leaf_expected(ty),
        }
        .at(src, span)
        .into()
    };
    // Whether the value is an integer within the leaf's width.
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
        T::I8 => int_ok(i8::MIN as i128, i8::MAX as i128)
            .then_some(value)
            .ok_or_else(mismatch),
        T::I16 => int_ok(i16::MIN as i128, i16::MAX as i128)
            .then_some(value)
            .ok_or_else(mismatch),
        T::I32 => int_ok(i32::MIN as i128, i32::MAX as i128)
            .then_some(value)
            .ok_or_else(mismatch),
        // postcard-dyn reads i64, isize, and i128 through `as_i64`, so the
        // whole family is i64-ranged on the wire.
        T::I64 | T::Isize | T::I128 => int_ok(i64::MIN as i128, i64::MAX as i128)
            .then_some(value)
            .ok_or_else(mismatch),
        T::U8 => int_ok(0, u8::MAX as i128)
            .then_some(value)
            .ok_or_else(mismatch),
        T::U16 => int_ok(0, u16::MAX as i128)
            .then_some(value)
            .ok_or_else(mismatch),
        T::U32 => int_ok(0, u32::MAX as i128)
            .then_some(value)
            .ok_or_else(mismatch),
        // Likewise u64, usize, and u128 all go through `as_u64`.
        T::U64 | T::Usize | T::U128 => int_ok(0, u64::MAX as i128)
            .then_some(value)
            .ok_or_else(mismatch),
        // Any number is fine here; postcard-dyn's `as_f64` accepts integer
        // literals, so `rate=200` encodes as `200.0`.
        T::F32 | T::F64 => value.is_number().then_some(value).ok_or_else(mismatch),
        T::String => value.is_string().then_some(value).ok_or_else(mismatch),
        T::Char => match &value {
            Value::String(s) if s.chars().count() == 1 => Ok(value),
            _ => Err(mismatch()),
        },
        // A present value is the `Some` payload; `#null` stays `None`.
        T::Option(inner) => match value {
            Value::Null => Ok(Value::Null),
            v => conform_value(&inner.ty, v, property, span, system, src),
        },
        // Nested structs get an empty span table, so everything inside falls
        // back to the enclosing property's span.
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
        // Unmodeled shapes pass through to postcard-dyn; see the module docs.
        _ => Ok(value),
    }
}

/// The wording for the `expected` field of an
/// [`InvalidParam`](LoadErrorKind::InvalidParam) diagnostic.
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
