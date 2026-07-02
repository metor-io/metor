//! A `serde::Deserializer` over one KDL node's **params surface** — the single
//! decoder both params paths share (docs/design-kdl-serde.md).
//!
//! The surface of a node is its non-reserved line properties (`k=v`), its
//! non-reserved positional arguments (there must be none beyond `skip_args` —
//! extras are a spanned error), and its child nodes. The mapping (design §1.2):
//!
//! - property `k=v` / child `k v` (one positional arg) → struct field `k` (scalar)
//! - child `k a b c` (≥2 args) / repeated `k …` children → `Vec<T>`/tuple
//! - child `k p=1 q=2` and/or `k { … }` → nested struct / map (recursive)
//! - integer literals coerce to float fields (`rate=200` ⇒ `200.0`); integers are
//!   range-checked per the asked width
//! - `#null` / an absent key → `Option::None`; a missing non-`Option` field is a
//!   spanned missing-param error
//! - an unmatched key is a spanned unknown-param error for **every** struct
//!   (deny-unknown-fields without the attribute: `deserialize_ignored_any` errors)
//! - a repeated property, or one key as both a property and a child, is a spanned
//!   error (explicit beats KDL's last-wins)
//!
//! The **static** path deserializes a typed `Params` ([`from_kdl_node`]); the
//! **dl** path deserializes a `serde_json::Value` plus a key → span side table
//! ([`params_value`]) that the schema-validation pass feeds to postcard-dyn.
//! Spans are captured *inside* the deserializer ([`DeError`]) and converted to
//! [`LoadError`] only at the boundary.

use std::collections::HashMap;
use std::fmt;

use kdl::{KdlEntry, KdlNode, KdlValue};
use miette::SourceSpan;
use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

use super::LoadError;

// ---------------------------------------------------------------------------
// Entry points (the only surface `wiring` uses)
// ---------------------------------------------------------------------------

/// Static path: deserialize a typed `Params` from `node`'s params surface.
///
/// `src` is the document text (miette source context), `system` the instance
/// name for diagnostics, `reserved` the line-property keys that belong to the
/// wiring surface (`&["type", "artifact"]` on a `system` node, `&["occupant"]`
/// on an `allow` node), and `skip_args` the leading positional args the wiring
/// surface owns (the instance name on `system` ⇒ 1; none on `allow` ⇒ 0).
pub(crate) fn from_kdl_node<T: de::DeserializeOwned>(
    node: &KdlNode,
    src: &str,
    system: &str,
    reserved: &'static [&'static str],
    skip_args: usize,
) -> Result<T, LoadError> {
    T::deserialize(KdlNodeDe {
        node,
        reserved,
        skip_args,
    })
    .map_err(|e| e.into_load_error(node, src, system))
}

/// Dl path: the same deserializer targeting `serde_json::Value`, plus the
/// top-level key → span side table the schema-validation pass uses for
/// named, spanned diagnostics.
pub(crate) fn params_value(
    node: &KdlNode,
    src: &str,
    system: &str,
    reserved: &'static [&'static str],
    skip_args: usize,
) -> Result<(Value, HashMap<String, SourceSpan>), LoadError> {
    let value = from_kdl_node::<Value>(node, src, system, reserved, skip_args)?;
    let mut spans: HashMap<String, SourceSpan> = HashMap::new();
    for entry in node.entries() {
        if let Some(name) = entry.name() {
            let key = name.value();
            if !reserved.contains(&key) {
                spans.entry(key.to_string()).or_insert_with(|| entry.span());
            }
        }
    }
    if let Some(children) = node.children() {
        for child in children.nodes() {
            spans
                .entry(child.name().value().to_string())
                .or_insert_with(|| child.span());
        }
    }
    Ok((value, spans))
}

// ---------------------------------------------------------------------------
// The error (design §2): spans captured inside, `LoadError` at the boundary
// ---------------------------------------------------------------------------

/// The de-internal error. Implements [`serde::de::Error`]; converted to
/// [`LoadError`] only at the [`from_kdl_node`]/[`params_value`] boundary.
#[derive(Debug)]
pub(crate) struct DeError {
    kind: DeErrorKind,
    /// The most specific span known. `None` only for visitor-originated
    /// `custom` errors until the enclosing map access attaches one.
    span: Option<SourceSpan>,
    /// The property/child name in scope, attached by the innermost map access.
    property: Option<String>,
}

#[derive(Debug)]
enum DeErrorKind {
    /// serde `custom`/`invalid_value` from a visitor (e.g. a hand-written
    /// `Deserialize`) — message only, span attached by context.
    Custom(String),
    /// serde-derive's end-of-map missing required field.
    MissingField { field: &'static str },
    /// An unmatched key: our `deserialize_ignored_any`, or a
    /// `deny_unknown_fields` struct.
    UnknownField { field: String },
    /// Raised by our code with the exact value span (we see the `KdlValue` and
    /// the wanted type before any visitor runs).
    InvalidType { expected: String, found: String },
    /// Integer out of range for the asked width (`u8 = 300`, …).
    OutOfRange { expected: String },
    /// Extra positional argument / repeated property / property-vs-child
    /// ambiguity. The message is phrased to read as "expected {msg}".
    Shape(String),
}

impl DeError {
    fn shape(msg: impl Into<String>, span: SourceSpan) -> Self {
        DeError {
            kind: DeErrorKind::Shape(msg.into()),
            span: Some(span),
            property: None,
        }
    }

    fn invalid_type_at(expected: impl Into<String>, found: impl Into<String>, span: SourceSpan) -> Self {
        DeError {
            kind: DeErrorKind::InvalidType {
                expected: expected.into(),
                found: found.into(),
            },
            span: Some(span),
            property: None,
        }
    }

    fn out_of_range(expected: impl Into<String>, span: SourceSpan) -> Self {
        DeError {
            kind: DeErrorKind::OutOfRange {
                expected: expected.into(),
            },
            span: Some(span),
            property: None,
        }
    }

    fn unknown_field_at(field: impl Into<String>, span: SourceSpan) -> Self {
        DeError {
            kind: DeErrorKind::UnknownField { field: field.into() },
            span: Some(span),
            property: None,
        }
    }

    /// Attach the surrounding property + span to a context-less error (a
    /// visitor-originated `custom`); an error that already knows its exact span
    /// keeps it (fill-if-`None`, so the innermost context wins).
    fn with_context(mut self, key: &str, span: SourceSpan) -> Self {
        if self.property.is_none() {
            self.property = Some(key.to_string());
        }
        if self.span.is_none() {
            self.span = Some(span);
        }
        self
    }

    fn with_span(mut self, span: SourceSpan) -> Self {
        if self.span.is_none() {
            self.span = Some(span);
        }
        self
    }

    /// Boundary conversion (design §2.1): no serde types leak into [`LoadError`].
    pub(crate) fn into_load_error(self, node: &KdlNode, src: &str, system: &str) -> LoadError {
        let span = self.span.unwrap_or_else(|| node.span());
        let src = src.to_string();
        let system = system.to_string();
        // The property in scope; shape errors at the node level fall back to the
        // node name so the message never reads "for ``".
        let property = self
            .property
            .unwrap_or_else(|| node.name().value().to_string());
        match self.kind {
            DeErrorKind::MissingField { field } => LoadError::MissingParam {
                property: field.to_string(),
                system,
                src,
                span,
            },
            DeErrorKind::UnknownField { field } => LoadError::UnknownParam {
                property: field,
                system,
                src,
                span,
            },
            DeErrorKind::InvalidType { expected, found } => LoadError::InvalidParam {
                property,
                system,
                expected: format!("{expected} (got {found})"),
                src,
                span,
            },
            DeErrorKind::OutOfRange { expected } => LoadError::InvalidParam {
                property,
                system,
                expected,
                src,
                span,
            },
            DeErrorKind::Custom(msg) | DeErrorKind::Shape(msg) => LoadError::InvalidParam {
                property,
                system,
                expected: msg,
                src,
                span,
            },
        }
    }
}

impl fmt::Display for DeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            DeErrorKind::Custom(msg) | DeErrorKind::Shape(msg) => f.write_str(msg),
            DeErrorKind::MissingField { field } => write!(f, "missing required param `{field}`"),
            DeErrorKind::UnknownField { field } => write!(f, "unknown param `{field}`"),
            DeErrorKind::InvalidType { expected, found } => {
                write!(f, "invalid type: expected {expected}, got {found}")
            }
            DeErrorKind::OutOfRange { expected } => {
                write!(f, "integer out of range: expected {expected}")
            }
        }
    }
}

impl std::error::Error for DeError {}

impl de::Error for DeError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        DeError {
            kind: DeErrorKind::Custom(msg.to_string()),
            span: None,
            property: None,
        }
    }

    fn missing_field(field: &'static str) -> Self {
        DeError {
            kind: DeErrorKind::MissingField { field },
            span: None,
            property: None,
        }
    }

    fn unknown_field(field: &str, _expected: &'static [&'static str]) -> Self {
        DeError {
            kind: DeErrorKind::UnknownField {
                field: field.to_string(),
            },
            span: None,
            property: None,
        }
    }

    fn invalid_type(unexp: de::Unexpected, exp: &dyn de::Expected) -> Self {
        DeError {
            kind: DeErrorKind::InvalidType {
                expected: exp.to_string(),
                found: unexp.to_string(),
            },
            span: None,
            property: None,
        }
    }
}

// ---------------------------------------------------------------------------
// The node deserializer (the map over the params surface)
// ---------------------------------------------------------------------------

/// Deserializes any `T: Deserialize` from one KDL node's params surface.
pub(crate) struct KdlNodeDe<'de> {
    node: &'de KdlNode,
    /// Line-property keys owned by the wiring surface, never yielded as params.
    reserved: &'static [&'static str],
    /// Leading positional args owned by the wiring surface (the instance name).
    skip_args: usize,
}

/// One field's value source, produced by the surface walk.
enum FieldSource<'de> {
    /// A line property `k=v`.
    Entry(&'de KdlEntry),
    /// A child node: `k v`, `k a b c`, `k p=1 { … }`, `k { … }`.
    Node(&'de KdlNode),
    /// Repeated same-name children: `k 1` `k 2` → a sequence.
    Nodes(Vec<&'de KdlNode>),
}

/// Collect the params surface of `node` in document order (properties, then
/// children grouped by name), enforcing the shape rules: no positional args
/// beyond `skip_args`, no repeated property, no key as both property and child,
/// no child named like a reserved property.
fn surface<'de>(
    node: &'de KdlNode,
    reserved: &'static [&'static str],
    skip_args: usize,
) -> Result<Vec<(&'de str, SourceSpan, FieldSource<'de>)>, DeError> {
    let mut fields: Vec<(&'de str, SourceSpan, FieldSource<'de>)> = Vec::new();
    let mut args_seen = 0usize;
    for entry in node.entries() {
        match entry.name() {
            None => {
                args_seen += 1;
                if args_seen > skip_args {
                    return Err(DeError::shape(
                        "no positional arguments (params are `key=value` properties or child nodes)",
                        entry.span(),
                    ));
                }
            }
            Some(name) => {
                let key = name.value();
                if reserved.contains(&key) {
                    continue;
                }
                if fields.iter().any(|(k, ..)| *k == key) {
                    return Err(DeError::shape(
                        format!("the property `{key}` at most once (it is repeated)"),
                        entry.span(),
                    ));
                }
                fields.push((key, entry.span(), FieldSource::Entry(entry)));
            }
        }
    }
    if let Some(children) = node.children() {
        for child in children.nodes() {
            let key = child.name().value();
            if reserved.contains(&key) {
                return Err(DeError::shape(
                    format!("`{key}` only as a node-line property (it is reserved), not a child"),
                    child.span(),
                ));
            }
            match fields.iter_mut().find(|(k, ..)| *k == key) {
                Some((_, _, source)) => match source {
                    FieldSource::Entry(_) => {
                        return Err(DeError::shape(
                            format!("`{key}` as either a property or a child node, not both"),
                            child.span(),
                        ));
                    }
                    FieldSource::Node(prev) => {
                        let prev: &'de KdlNode = prev;
                        *source = FieldSource::Nodes(vec![prev, child]);
                    }
                    FieldSource::Nodes(nodes) => nodes.push(child),
                },
                None => fields.push((key, child.span(), FieldSource::Node(child))),
            }
        }
    }
    Ok(fields)
}

impl<'de> Deserializer<'de> for KdlNodeDe<'de> {
    type Error = DeError;

    // The `serde_json::Value` target: the params surface is a JSON object.
    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        self.deserialize_map(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        let fields = surface(self.node, self.reserved, self.skip_args)?;
        visitor.visit_map(NodeMapAccess {
            iter: fields.into_iter(),
            pending: None,
        })
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, DeError> {
        self.deserialize_map(visitor)
    }

    // `type Params = ()`: succeeds on a param-less node, rejects stray params.
    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        let fields = surface(self.node, self.reserved, self.skip_args)?;
        if let Some((key, span, _)) = fields.first() {
            return Err(DeError::unknown_field_at(*key, *span));
        }
        visitor.visit_unit()
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, DeError> {
        self.deserialize_unit(visitor)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        visitor.visit_some(self)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, DeError> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        visitor.visit_unit()
    }

    // A scalar/seq/enum asked of a whole node's surface: drive the map and let
    // the visitor's own `invalid_type` report it (span attached at the boundary).
    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf seq tuple tuple_struct enum identifier
    }
}

/// The map access over a node's params surface.
struct NodeMapAccess<'de> {
    iter: std::vec::IntoIter<(&'de str, SourceSpan, FieldSource<'de>)>,
    pending: Option<(&'de str, SourceSpan, FieldSource<'de>)>,
}

impl<'de> MapAccess<'de> for NodeMapAccess<'de> {
    type Error = DeError;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, DeError> {
        match self.iter.next() {
            None => Ok(None),
            Some(field) => {
                let key = field.0;
                let span = field.1;
                self.pending = Some(field);
                seed.deserialize(de::value::StrDeserializer::new(key))
                    .map(Some)
                    .map_err(|e: DeError| e.with_span(span))
            }
        }
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, DeError> {
        let (key, span, source) = self.pending.take().expect("next_value_seed before a key");
        seed.deserialize(FieldDe { source, key, span })
            // Attach the property + span to visitor-originated (context-less)
            // errors; born-with-span errors keep their more precise span.
            .map_err(|e| e.with_context(key, span))
    }
}

// ---------------------------------------------------------------------------
// The value-position deserializer (one field)
// ---------------------------------------------------------------------------

/// The value-position deserializer the map access hands to `next_value_seed`.
struct FieldDe<'de> {
    source: FieldSource<'de>,
    key: &'de str,
    /// The entry's / child node's span.
    span: SourceSpan,
}

/// A human name of a KDL value's own type, for `InvalidType.found`.
fn value_desc(value: &KdlValue) -> &'static str {
    match value {
        KdlValue::String(_) => "a string",
        KdlValue::Integer(_) => "an integer",
        KdlValue::Float(_) => "a float",
        KdlValue::Bool(_) => "a boolean",
        KdlValue::Null => "#null",
    }
}

/// Drive `visitor` with one scalar KDL value (the `deserialize_any` leaf).
fn visit_scalar<'de, V: Visitor<'de>>(
    visitor: V,
    value: &'de KdlValue,
    span: SourceSpan,
) -> Result<V::Value, DeError> {
    let result = match value {
        KdlValue::String(s) => visitor.visit_borrowed_str(s),
        KdlValue::Integer(i) => {
            if let Ok(x) = i64::try_from(*i) {
                visitor.visit_i64(x)
            } else if let Ok(x) = u64::try_from(*i) {
                visitor.visit_u64(x)
            } else {
                visitor.visit_i128(*i)
            }
        }
        KdlValue::Float(f) => visitor.visit_f64(*f),
        KdlValue::Bool(b) => visitor.visit_bool(*b),
        KdlValue::Null => visitor.visit_unit(),
    };
    result.map_err(|e: DeError| e.with_span(span))
}

/// How a child node's own surface is shaped (drives `deserialize_any`/seq).
enum NodeShape {
    /// Properties and/or children ⇒ a nested map.
    Nested,
    /// Positional args only (0, 1, or many) ⇒ unit / scalar / seq.
    Args(usize),
}

fn node_shape(node: &KdlNode) -> NodeShape {
    let named = node.entries().iter().any(|e| e.name().is_some());
    let has_children = node.children().is_some_and(|c| !c.nodes().is_empty());
    if named || has_children {
        NodeShape::Nested
    } else {
        NodeShape::Args(node.entries().len())
    }
}

/// A node's positional (nameless) argument entries.
fn node_args(node: &KdlNode) -> impl Iterator<Item = &KdlEntry> {
    node.entries().iter().filter(|e| e.name().is_none())
}

impl<'de> FieldDe<'de> {
    /// The scalar value + its exact span, if this source is scalar-shaped
    /// (a property, or a childless prop-less node with exactly one arg).
    fn scalar_value(&self) -> Option<(&'de KdlValue, SourceSpan)> {
        match &self.source {
            FieldSource::Entry(e) => Some((e.value(), e.span())),
            FieldSource::Node(n) => match node_shape(n) {
                NodeShape::Args(1) => {
                    let arg = node_args(n).next().expect("one arg");
                    Some((arg.value(), arg.span()))
                }
                _ => None,
            },
            FieldSource::Nodes(_) => None,
        }
    }

    /// A human name of what this source holds, for `InvalidType.found`.
    fn found_desc(&self) -> String {
        match &self.source {
            FieldSource::Entry(e) => value_desc(e.value()).to_string(),
            FieldSource::Node(n) => match node_shape(n) {
                NodeShape::Nested => "a nested node".to_string(),
                NodeShape::Args(1) => {
                    value_desc(node_args(n).next().expect("one arg").value()).to_string()
                }
                NodeShape::Args(0) => "a bare node (no value)".to_string(),
                NodeShape::Args(_) => "a multi-value node".to_string(),
            },
            FieldSource::Nodes(_) => "repeated child nodes".to_string(),
        }
    }

    /// The scalar value, or a born-with-span invalid-type error naming `expected`.
    fn scalar(&self, expected: &str) -> Result<(&'de KdlValue, SourceSpan), DeError> {
        self.scalar_value()
            .ok_or_else(|| DeError::invalid_type_at(expected, self.found_desc(), self.span))
    }
}

/// Integer widths: range-checked at the value's exact span.
macro_rules! de_int {
    ($method:ident, $visit:ident, $ty:ty, $expected:expr) => {
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
            let (value, span) = self.scalar($expected)?;
            match value {
                KdlValue::Integer(i) => {
                    let x = <$ty>::try_from(*i).map_err(|_| DeError::out_of_range($expected, span))?;
                    visitor.$visit(x).map_err(|e: DeError| e.with_span(span))
                }
                other => Err(DeError::invalid_type_at($expected, value_desc(other), span)),
            }
        }
    };
}

/// Floats: accept an integer literal where a float is wanted (`rate=200`).
macro_rules! de_float {
    ($method:ident, $expected:expr) => {
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
            let (value, span) = self.scalar($expected)?;
            match value {
                KdlValue::Float(f) => visitor.visit_f64(*f).map_err(|e: DeError| e.with_span(span)),
                KdlValue::Integer(i) => {
                    visitor.visit_f64(*i as f64).map_err(|e: DeError| e.with_span(span))
                }
                other => Err(DeError::invalid_type_at($expected, value_desc(other), span)),
            }
        }
    };
}

impl<'de> Deserializer<'de> for FieldDe<'de> {
    type Error = DeError;

    de_int!(deserialize_i8, visit_i8, i8, "an 8-bit signed integer");
    de_int!(deserialize_i16, visit_i16, i16, "a 16-bit signed integer");
    de_int!(deserialize_i32, visit_i32, i32, "a 32-bit signed integer");
    de_int!(deserialize_i64, visit_i64, i64, "a 64-bit signed integer");
    de_int!(deserialize_i128, visit_i128, i128, "a 128-bit signed integer");
    de_int!(deserialize_u8, visit_u8, u8, "an 8-bit non-negative integer");
    de_int!(deserialize_u16, visit_u16, u16, "a 16-bit non-negative integer");
    de_int!(deserialize_u32, visit_u32, u32, "a 32-bit non-negative integer");
    de_int!(deserialize_u64, visit_u64, u64, "a 64-bit non-negative integer");
    de_int!(deserialize_u128, visit_u128, u128, "a 128-bit non-negative integer");

    de_float!(deserialize_f32, "a number");
    de_float!(deserialize_f64, "a number");

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        let (value, span) = self.scalar("a boolean (#true/#false)")?;
        match value {
            KdlValue::Bool(b) => visitor.visit_bool(*b).map_err(|e: DeError| e.with_span(span)),
            other => Err(DeError::invalid_type_at(
                "a boolean (#true/#false)",
                value_desc(other),
                span,
            )),
        }
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        let (value, span) = self.scalar("a string")?;
        match value {
            KdlValue::String(s) => {
                visitor.visit_borrowed_str(s).map_err(|e: DeError| e.with_span(span))
            }
            other => Err(DeError::invalid_type_at("a string", value_desc(other), span)),
        }
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        self.deserialize_str(visitor)
    }

    fn deserialize_char<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        let (value, span) = self.scalar("a one-character string")?;
        match value {
            KdlValue::String(s) if s.chars().count() == 1 => visitor
                .visit_char(s.chars().next().expect("one char"))
                .map_err(|e: DeError| e.with_span(span)),
            KdlValue::String(_) => Err(DeError::invalid_type_at(
                "a one-character string",
                "a longer string",
                span,
            )),
            other => Err(DeError::invalid_type_at(
                "a one-character string",
                value_desc(other),
                span,
            )),
        }
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, DeError> {
        Err(DeError::invalid_type_at(
            "byte params are not expressible in KDL",
            self.found_desc(),
            self.span,
        ))
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        self.deserialize_bytes(visitor)
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        match self.scalar_value() {
            Some((KdlValue::Null, span)) => {
                visitor.visit_unit().map_err(|e: DeError| e.with_span(span))
            }
            _ => Err(DeError::invalid_type_at(
                "#null",
                self.found_desc(),
                self.span,
            )),
        }
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, DeError> {
        self.deserialize_unit(visitor)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        // Present-with-`#null` is an explicit `None`; anything else is `Some`.
        if let Some((KdlValue::Null, _)) = self.scalar_value() {
            return visitor.visit_none();
        }
        visitor.visit_some(self)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, DeError> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        let key = self.key;
        match self.source {
            // Repeated same-name children: one element per node.
            FieldSource::Nodes(nodes) => visitor.visit_seq(NodesSeq {
                iter: nodes.into_iter(),
                key,
            }),
            FieldSource::Node(n) => match node_shape(n) {
                // `k a b c` (positional args only): one element per arg.
                NodeShape::Args(_) => visitor.visit_seq(ArgsSeq {
                    iter: node_args(n).collect::<Vec<_>>().into_iter(),
                    key,
                }),
                // A single nested child for a one-element sequence.
                NodeShape::Nested => visitor.visit_seq(NodesSeq {
                    iter: vec![n].into_iter(),
                    key,
                }),
            },
            FieldSource::Entry(e) => Err(DeError::invalid_type_at(
                "a sequence (a child node `k a b c`, or repeated `k …` children)",
                value_desc(e.value()),
                e.span(),
            )),
        }
    }

    fn deserialize_tuple<V: Visitor<'de>>(
        self,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, DeError> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, DeError> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        match self.source {
            FieldSource::Node(n) => KdlNodeDe {
                node: n,
                reserved: &[],
                skip_args: 0,
            }
            .deserialize_map(visitor),
            _ => Err(DeError::invalid_type_at(
                "a nested node (`k key=value …` or `k { … }`)",
                self.found_desc(),
                self.span,
            )),
        }
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, DeError> {
        self.deserialize_map(visitor)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, DeError> {
        // A fieldless enum from a string (`state="running"`).
        let (value, span) = self.scalar("a string variant name")?;
        match value {
            KdlValue::String(s) => visitor
                .visit_enum(de::value::StrDeserializer::<DeError>::new(s))
                .map_err(|e: DeError| e.with_span(span)),
            other => Err(DeError::invalid_type_at(
                "a string variant name",
                value_desc(other),
                span,
            )),
        }
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        self.deserialize_str(visitor)
    }

    // The typo guard: serde-derive drains an unmatched key through here, so every
    // params struct is deny-unknown-fields with the key's exact span (design §1.3).
    fn deserialize_ignored_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, DeError> {
        Err(DeError::unknown_field_at(self.key, self.span))
    }

    // The self-describing path (`serde_json::Value`): shape decides.
    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        match &self.source {
            FieldSource::Entry(e) => visit_scalar(visitor, e.value(), e.span()),
            FieldSource::Nodes(_) => self.deserialize_seq(visitor),
            FieldSource::Node(n) => match node_shape(n) {
                NodeShape::Nested => KdlNodeDe {
                    node: n,
                    reserved: &[],
                    skip_args: 0,
                }
                .deserialize_map(visitor),
                NodeShape::Args(0) => visitor
                    .visit_unit()
                    .map_err(|e: DeError| e.with_span(self.span)),
                NodeShape::Args(1) => {
                    let arg = node_args(n).next().expect("one arg");
                    visit_scalar(visitor, arg.value(), arg.span())
                }
                NodeShape::Args(_) => self.deserialize_seq(visitor),
            },
        }
    }
}

/// Sequence elements from child nodes (repeated children, or one nested child).
struct NodesSeq<'de> {
    iter: std::vec::IntoIter<&'de KdlNode>,
    key: &'de str,
}

impl<'de> SeqAccess<'de> for NodesSeq<'de> {
    type Error = DeError;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, DeError> {
        match self.iter.next() {
            None => Ok(None),
            Some(node) => {
                let span = node.span();
                seed.deserialize(FieldDe {
                    source: FieldSource::Node(node),
                    key: self.key,
                    span,
                })
                .map(Some)
                .map_err(|e| e.with_context(self.key, span))
            }
        }
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.iter.len())
    }
}

/// Sequence elements from one child's positional args (`k a b c`).
struct ArgsSeq<'de> {
    iter: std::vec::IntoIter<&'de KdlEntry>,
    key: &'de str,
}

impl<'de> SeqAccess<'de> for ArgsSeq<'de> {
    type Error = DeError;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, DeError> {
        match self.iter.next() {
            None => Ok(None),
            Some(entry) => {
                let span = entry.span();
                seed.deserialize(FieldDe {
                    source: FieldSource::Entry(entry),
                    key: self.key,
                    span,
                })
                .map(Some)
                .map_err(|e| e.with_context(self.key, span))
            }
        }
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.iter.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kdl::KdlDocument;

    fn parse_node(src: &str) -> (KdlDocument, String) {
        (src.parse::<KdlDocument>().expect("fixture KDL parses"), src.to_string())
    }

    /// Deserialize `T` from the first node of `src`, with a `system`-node surface
    /// (reserved `type`/`artifact`, one leading positional name arg).
    fn de_system<T: de::DeserializeOwned>(src: &str) -> Result<T, LoadError> {
        let (doc, text) = parse_node(src);
        from_kdl_node(&doc.nodes()[0], &text, "test", &["type", "artifact"], 1)
    }

    #[derive(serde::Deserialize, Debug, PartialEq)]
    struct Flat {
        count: i64,
        rate: f64,
        label: String,
        enabled: bool,
        offset: Option<f64>,
    }

    #[test]
    fn flat_properties_deserialize() {
        let got: Flat = de_system(
            r#"system "x" type="T" count=3 rate=2 label="hi" enabled=#true"#,
        )
        .unwrap();
        assert_eq!(
            got,
            Flat {
                count: 3,
                rate: 2.0, // int literal coerces to the float field
                label: "hi".into(),
                enabled: true,
                offset: None, // absent Option
            }
        );
    }

    #[test]
    fn scalar_child_nodes_are_fields_too() {
        // `k v` child form (the slot `allow { gain 0.8 }` shape).
        let got: Flat = de_system(
            r#"system "x" type="T" { count 3; rate 0.5; label "hi"; enabled #false }"#,
        )
        .unwrap();
        assert_eq!(got.rate, 0.5);
        assert!(!got.enabled);
    }

    #[test]
    fn null_property_is_explicit_none() {
        let got: Flat =
            de_system(r#"system "x" type="T" count=1 rate=1.0 label="l" enabled=#true offset=#null"#)
                .unwrap();
        assert_eq!(got.offset, None);
    }

    #[test]
    fn missing_required_field_is_missing_param() {
        let err = de_system::<Flat>(r#"system "x" type="T" rate=1.0 label="l" enabled=#true"#)
            .unwrap_err();
        match err {
            LoadError::MissingParam { property, system, .. } => {
                assert_eq!(property, "count");
                assert_eq!(system, "test");
            }
            other => panic!("expected MissingParam, got {other:?}"),
        }
    }

    #[test]
    fn unknown_field_is_spanned_unknown_param() {
        let src = r#"system "x" type="T" count=1 rate=1.0 label="l" enabled=#true typo=5"#;
        let err = de_system::<Flat>(src).unwrap_err();
        match err {
            LoadError::UnknownParam { property, span, .. } => {
                assert_eq!(property, "typo");
                // The span points at the `typo=5` entry, not the whole node.
                let at = src.find("typo=5").unwrap();
                assert!(span.offset() >= at, "span {span:?} inside the entry (at {at})");
            }
            other => panic!("expected UnknownParam, got {other:?}"),
        }
    }

    #[test]
    fn wrong_type_is_spanned_invalid_param() {
        let src = r#"system "x" type="T" count="oops" rate=1.0 label="l" enabled=#true"#;
        let err = de_system::<Flat>(src).unwrap_err();
        match err {
            LoadError::InvalidParam { property, span, .. } => {
                assert_eq!(property, "count");
                let at = src.find(r#"count="oops""#).unwrap();
                assert!(span.offset() >= at, "span {span:?} at the entry (at {at})");
            }
            other => panic!("expected InvalidParam, got {other:?}"),
        }
    }

    #[test]
    fn integer_out_of_range_is_invalid_param() {
        #[derive(serde::Deserialize, Debug)]
        struct Small {
            #[allow(dead_code)]
            n: u8,
        }
        let err = de_system::<Small>(r#"system "x" type="T" n=300"#).unwrap_err();
        assert!(matches!(err, LoadError::InvalidParam { ref property, .. } if property == "n"), "{err:?}");
    }

    #[derive(serde::Deserialize, Debug, PartialEq)]
    struct Nested {
        pid: Pid,
        taps: Vec<u64>,
        label: Option<String>,
    }

    #[derive(serde::Deserialize, Debug, PartialEq)]
    struct Pid {
        p: f64,
        i: f64,
        d: f64,
    }

    #[test]
    fn nested_struct_and_vec_children() {
        let got: Nested = de_system(
            r#"system "x" type="T" {
    pid p=1.0 i=0.5 d=0.1
    taps 1 2 3
}"#,
        )
        .unwrap();
        assert_eq!(
            got,
            Nested {
                pid: Pid { p: 1.0, i: 0.5, d: 0.1 },
                taps: vec![1, 2, 3],
                label: None,
            }
        );
    }

    #[test]
    fn repeated_children_are_a_sequence() {
        #[derive(serde::Deserialize, Debug, PartialEq)]
        struct Multi {
            tap: Vec<u64>,
        }
        let got: Multi = de_system(r#"system "x" type="T" { tap 1; tap 2; tap 3 }"#).unwrap();
        assert_eq!(got.tap, vec![1, 2, 3]);
    }

    #[test]
    fn repeated_nested_children_are_a_vec_of_structs() {
        #[derive(serde::Deserialize, Debug, PartialEq)]
        struct Gains {
            gain: Vec<Pid>,
        }
        let got: Gains = de_system(
            r#"system "x" type="T" { gain p=1.0 i=0.0 d=0.0; gain p=2.0 i=0.1 d=0.2 }"#,
        )
        .unwrap();
        assert_eq!(got.gain.len(), 2);
        assert_eq!(got.gain[1].p, 2.0);
    }

    #[test]
    fn nested_unknown_field_names_the_inner_key() {
        let err = de_system::<Nested>(
            r#"system "x" type="T" { pid p=1.0 i=0.5 d=0.1 q=9; taps 1 }"#,
        )
        .unwrap_err();
        assert!(
            matches!(err, LoadError::UnknownParam { ref property, .. } if property == "q"),
            "{err:?}"
        );
    }

    #[test]
    fn fieldless_enum_from_string() {
        #[derive(serde::Deserialize, Debug, PartialEq)]
        #[serde(rename_all = "snake_case")]
        enum Mode {
            Fast,
            Slow,
        }
        #[derive(serde::Deserialize, Debug)]
        struct WithMode {
            mode: Mode,
        }
        let got: WithMode = de_system(r#"system "x" type="T" mode="fast""#).unwrap();
        assert_eq!(got.mode, Mode::Fast);
        let err = de_system::<WithMode>(r#"system "x" type="T" mode="warp""#).unwrap_err();
        assert!(matches!(err, LoadError::InvalidParam { ref property, .. } if property == "mode"), "{err:?}");
    }

    #[test]
    fn serde_default_attribute_applies() {
        fn four() -> i64 {
            4
        }
        #[derive(serde::Deserialize)]
        struct Defaulted {
            #[serde(default = "four")]
            depth: i64,
        }
        let got: Defaulted = de_system(r#"system "x" type="T""#).unwrap();
        assert_eq!(got.depth, 4);
    }

    #[test]
    fn repeated_property_is_an_error() {
        let err = de_system::<Flat>(
            r#"system "x" type="T" count=1 count=2 rate=1.0 label="l" enabled=#true"#,
        )
        .unwrap_err();
        assert!(matches!(err, LoadError::InvalidParam { .. }), "{err:?}");
    }

    #[test]
    fn property_and_child_with_one_key_is_an_error() {
        let err = de_system::<Flat>(
            r#"system "x" type="T" count=1 rate=1.0 label="l" enabled=#true { count 2 }"#,
        )
        .unwrap_err();
        assert!(matches!(err, LoadError::InvalidParam { .. }), "{err:?}");
    }

    #[test]
    fn extra_positional_argument_is_an_error() {
        let err = de_system::<Flat>(
            r#"system "x" "stray" type="T" count=1 rate=1.0 label="l" enabled=#true"#,
        )
        .unwrap_err();
        assert!(matches!(err, LoadError::InvalidParam { .. }), "{err:?}");
    }

    #[test]
    fn unit_params_reject_stray_properties() {
        de_system::<()>(r#"system "x" type="T""#).unwrap();
        let err = de_system::<()>(r#"system "x" type="T" bogus=1"#).unwrap_err();
        assert!(
            matches!(err, LoadError::UnknownParam { ref property, .. } if property == "bogus"),
            "{err:?}"
        );
    }

    #[test]
    fn reserved_keys_never_reach_the_params() {
        // `type=`/`artifact=` skipped; the one positional name arg skipped.
        #[derive(serde::Deserialize)]
        struct One {
            gain: f64,
        }
        let got: One = de_system(r#"system "x" type="T" artifact="a" gain=0.5"#).unwrap();
        assert_eq!(got.gain, 0.5);
    }

    #[test]
    fn value_target_maps_the_full_surface() {
        let (doc, text) = parse_node(
            r#"system "x" type="T" gain=0.5 name="n" on=#true {
    pid p=1 i=2.5 d=#null
    taps 1 2
    tag "a"
    tag "b"
}"#,
        );
        let (value, spans) =
            params_value(&doc.nodes()[0], &text, "test", &["type", "artifact"], 1).unwrap();
        let obj = value.as_object().unwrap();
        assert_eq!(obj["gain"], serde_json::json!(0.5));
        assert_eq!(obj["name"], serde_json::json!("n"));
        assert_eq!(obj["on"], serde_json::json!(true));
        assert_eq!(obj["pid"], serde_json::json!({ "p": 1, "i": 2.5, "d": null }));
        assert_eq!(obj["taps"], serde_json::json!([1, 2]));
        assert_eq!(obj["tag"], serde_json::json!(["a", "b"]));
        // The side table names every top-level key.
        for key in ["gain", "name", "on", "pid", "taps", "tag"] {
            assert!(spans.contains_key(key), "span table has `{key}`");
        }
        assert!(!spans.contains_key("type"), "reserved keys are not params");
    }
}
