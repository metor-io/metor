//! Two ways to turn a params value tree into a system's typed `Params`, and
//! the diagnostics both raise.
//!
//! A statically linked system has its `Params` type in hand, so
//! [`decode_value_params`] is plain serde: `#[serde(default)]` is honoured and
//! `serde_ignored` supplies the typo guard serde_json lacks.
//!
//! A dynamically loaded one does not. Its `Params` type is never linked into
//! the host; what the host has instead is the postcard schema the shared
//! library exports. [`encode_value_params`] walks that schema to produce the
//! exact bytes the `Params` struct itself would postcard-encode to, so the
//! loaded side decodes them as its own type. It overlays the entry's declared
//! defaults, conforms the value to the schema — [`conform_to_schema`] turns
//! unknown keys, missing fields, and type mismatches into [`ParamError`]s and
//! inserts an explicit `Null` for every absent `Option` field, since
//! postcard-dyn requires the key to be present — and emits the bytes with
//! [`postcard_dyn::to_stdvec_dyn`].
//!
//! One property of the conformance walk is worth knowing: any schema shape the
//! walk does not model (tuples, maps, enums) passes through unchecked; if
//! postcard-dyn cannot encode it, the failure surfaces as
//! [`ParamErrorKind::DlParamEncode`] rather than as silently wrong bytes.
//! Errors anchor to the whole value-tree surface, which carries no document
//! spans.
//!
//! The errors stop here rather than at a `miette::Diagnostic`: the host owns
//! the wiring diagnostic (`LoadError`), which absorbs a [`ParamErrorKind`] as
//! one of its variants and reads [`code`](ParamErrorKind::code),
//! [`label`](ParamErrorKind::label), and the [`Anchor`] back off it. This
//! crate carries no reporter.

use std::collections::HashMap;

use miette::SourceSpan;
use postcard_schema::schema::owned::{OwnedDataModelType, OwnedNamedType};
use serde_json::{Map, Value};
use thiserror::Error;

/// The source snippet a spanned params error renders and the span of the
/// offending node within it. The span is a real field, not derived from the
/// snippet: params errors anchor at a value's own document span.
#[derive(Debug)]
pub struct Anchor {
    pub src: String,
    pub span: SourceSpan,
}

/// A params failure with the snippet and span it points at.
#[derive(Debug)]
pub struct ParamError {
    pub kind: ParamErrorKind,
    pub anchor: Option<Anchor>,
}

impl std::fmt::Display for ParamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.kind, f)
    }
}

impl std::error::Error for ParamError {}

/// What went wrong decoding or encoding a system's params.
#[derive(Error, Debug)]
pub enum ParamErrorKind {
    /// A required params field has no matching property or child on the node.
    #[error("missing required param `{property}` for system `{system}`")]
    MissingParam { property: String, system: String },

    /// A params value that does not decode as the field's type, or a malformed
    /// params surface such as a stray positional argument or a repeated
    /// property.
    #[error("invalid value for `{property}` on system `{system}`: expected {expected}")]
    InvalidParam {
        property: String,
        system: String,
        expected: String,
    },

    /// A property or child with no matching params field, usually a typo or a
    /// stale config. Both the typed deserializer and the dynamic schema
    /// validation raise this same variant.
    #[error("unknown param `{property}` for system `{system}`")]
    UnknownParam { property: String, system: String },

    /// A static system's value-tree params did not deserialize as its typed
    /// `Params`. The reason is serde's own message; a value tree carries no
    /// document spans, so the label covers the whole diagnostic snippet.
    #[error("system `{system}` value params did not deserialize: {reason}")]
    ValueParams { system: String, reason: String },

    /// The dl system's params could not be encoded against its `Params`
    /// schema, either because the schema has an unsupported shape or because
    /// the dynamic encoder rejected a value.
    #[error("dl system `{system}` params could not be schema-encoded: {reason}")]
    DlParamEncode { system: String, reason: String },
}

impl ParamErrorKind {
    /// Anchor this kind to a source snippet and the span of the offending node.
    pub fn at(self, src: impl Into<String>, span: SourceSpan) -> ParamError {
        ParamError {
            kind: self,
            anchor: Some(Anchor {
                src: src.into(),
                span,
            }),
        }
    }

    /// Anchor this kind to a snippet whose whole extent is the label span, the
    /// common case for resolve-time snippets that carry no interior spans.
    pub fn whole(self, src: impl Into<String>) -> ParamError {
        let src = src.into();
        let span = (0, src.len()).into();
        self.at(src, span)
    }

    /// This kind's stable diagnostic code.
    pub fn code(&self) -> &'static str {
        match self {
            ParamErrorKind::MissingParam { .. } => "fsw_wiring::missing_param",
            ParamErrorKind::InvalidParam { .. } => "fsw_wiring::invalid_param",
            ParamErrorKind::UnknownParam { .. } => "fsw_wiring::unknown_param",
            ParamErrorKind::ValueParams { .. } => "fsw_wiring::value_params",
            ParamErrorKind::DlParamEncode { .. } => "fsw_wiring::dl_param_encode",
        }
    }

    /// The label pointing at the offending span.
    pub fn label(&self) -> String {
        match self {
            ParamErrorKind::MissingParam { .. } => "this node is missing the param".into(),
            ParamErrorKind::InvalidParam { .. } => "invalid value here".into(),
            ParamErrorKind::UnknownParam { .. } => "no params field is named this".into(),
            ParamErrorKind::ValueParams { .. } => "these params".into(),
            ParamErrorKind::DlParamEncode { .. } => {
                "these params could not be encoded against the `Params` schema".into()
            }
        }
    }
}

/// A serde deserializer for a params surface that carries no fields: it yields
/// the unit value for `()` and an empty map for a struct (so `#[serde(default)]`
/// fields fill in and a required field is a clean missing-field error). A
/// `serde_json::Value` alone cannot serve both, since `()` needs `Null` and a
/// defaulted struct needs `{}`.
pub struct NoParams;

impl<'de> serde::Deserializer<'de> for NoParams {
    type Error = serde::de::value::Error;

    fn deserialize_any<V: serde::de::Visitor<'de>>(self, v: V) -> Result<V::Value, Self::Error> {
        // Default to an empty map, so a struct with defaulted fields decodes.
        v.visit_map(serde::de::value::MapDeserializer::new(std::iter::empty::<(
            &str,
            &str,
        )>()))
    }

    fn deserialize_unit<V: serde::de::Visitor<'de>>(self, v: V) -> Result<V::Value, Self::Error> {
        v.visit_unit()
    }

    fn deserialize_unit_struct<V: serde::de::Visitor<'de>>(
        self,
        _name: &'static str,
        v: V,
    ) -> Result<V::Value, Self::Error> {
        v.visit_unit()
    }

    fn deserialize_option<V: serde::de::Visitor<'de>>(self, v: V) -> Result<V::Value, Self::Error> {
        v.visit_none()
    }

    fn deserialize_newtype_struct<V: serde::de::Visitor<'de>>(
        self,
        _name: &'static str,
        v: V,
    ) -> Result<V::Value, Self::Error> {
        v.visit_newtype_struct(self)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf seq tuple tuple_struct map struct enum identifier ignored_any
    }
}

/// Deserialize a typed `Params` from a value tree.
///
/// Unlike the dl path's schema conformance, this is plain serde, so
/// `#[serde(default)]` field attributes are honored. `serde_ignored` supplies
/// the typo guard serde_json's deserializer lacks: a key no field consumed is
/// an [`UnknownParam`](ParamErrorKind::UnknownParam), and any other decode
/// failure is a [`ValueParams`](ParamErrorKind::ValueParams) carrying serde's
/// own message.
pub fn decode_value_params<P: serde::de::DeserializeOwned>(
    value: &serde_json::Value,
    system: &str,
    src: &str,
) -> Result<P, ParamError> {
    let mut unknown: Option<String> = None;
    let params = serde_ignored::deserialize(value, |path: serde_ignored::Path<'_>| {
        unknown.get_or_insert_with(|| path.to_string());
    })
    .map_err(|e| {
        ParamErrorKind::ValueParams {
            system: system.to_string(),
            reason: e.to_string(),
        }
        .whole(src)
    })?;
    if let Some(property) = unknown {
        return Err(ParamErrorKind::UnknownParam {
            property,
            system: system.to_string(),
        }
        .whole(src));
    }
    Ok(params)
}

/// Encodes a params value tree into the postcard bytes described by `schema`,
/// with the module's schema conformance rules.
///
/// A value tree carries no document spans, so the rendered JSON stands in as
/// the diagnostic source and every error anchors to the whole surface.
pub fn encode_value_params(
    value: &serde_json::Value,
    schema: &OwnedNamedType,
    system: &str,
    defaults: Option<&[u8]>,
) -> Result<Vec<u8>, ParamError> {
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
) -> Result<Vec<u8>, ParamError> {
    let encode_err = |reason: String| {
        ParamErrorKind::DlParamEncode {
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
    let value = conform_to_schema(&schema.ty, value, spans, system, src, node_span).map_err(
        |e| match e {
            Conform::Param(err) => *err,
            Conform::Shape(reason) => encode_err(reason),
        },
    )?;

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
/// the walk cannot express (reported as [`ParamErrorKind::DlParamEncode`]).
enum Conform {
    Param(Box<ParamError>),
    Shape(String),
}

impl From<ParamError> for Conform {
    fn from(e: ParamError) -> Self {
        Conform::Param(Box::new(e))
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
            return Err(ParamErrorKind::UnknownParam {
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
                return Err(ParamErrorKind::MissingParam {
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
        ParamErrorKind::InvalidParam {
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
/// [`InvalidParam`](ParamErrorKind::InvalidParam) diagnostic.
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
