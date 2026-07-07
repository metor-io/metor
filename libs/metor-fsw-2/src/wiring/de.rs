//! A `serde::Deserializer` that reads one KDL node's parameters.
//!
//! The parameters of a node are its named properties (`k=v`) and its child
//! nodes. Keys listed in `reserved` and the first `skip_args` positional
//! arguments belong to the caller and are skipped; any further positional
//! argument is an error. Values map onto Rust types by shape. A property or
//! a single-argument child fills a scalar field. A child with several
//! positional arguments, or several children sharing one name, fills a
//! sequence. A child carrying properties or children of its own fills a
//! nested struct or map, recursively. An integer literal is accepted where a
//! float is wanted (`rate=200` reads as `200.0`), and integers are
//! range-checked against the requested width. `#null` or an absent key reads
//! as `Option::None`, while a missing non-`Option` field is reported as a
//! missing parameter.
//!
//! Every struct rejects unknown keys. serde-derive drains an unmatched key
//! through `deserialize_ignored_any`, and this deserializer turns that into
//! an error carrying the key's exact span, so typos are caught without
//! `deny_unknown_fields` on each params type. A repeated property, or one
//! key spelled both as a property and as a child, is likewise an error
//! rather than a silent last-one-wins.
//!
//! There are two entry points. [`from_kdl_node`] produces a typed value.
//! [`params_value`] produces a self-describing `serde_json::Value` along
//! with a table mapping each top-level key to its span, for callers that
//! validate the value against a schema later and still want named, spanned
//! diagnostics.
//!
//! Errors are [`DeError`] internally and become [`LoadError`] only at the
//! entry points. A `DeError` raised by this module is born knowing the exact
//! span of the offending value; one raised inside a visitor (serde's
//! `custom`, `missing_field`, and friends) starts without one and picks up
//! the nearest enclosing property name and span on the way out. Both
//! attachments fill only empty fields, so the innermost, most precise
//! information always wins.

use std::collections::HashMap;
use std::fmt;

use kdl::{KdlEntry, KdlNode, KdlValue};
use miette::SourceSpan;
use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

use super::LoadError;

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Deserialize a typed value from `node`'s parameters.
///
/// `src` is the document text errors quote from and `system` names the
/// instance in diagnostics. `reserved` lists the property keys the caller
/// consumes itself, and `skip_args` how many leading positional arguments it
/// owns; a `system "name" type="T"` node passes `&["type", "artifact"]` and
/// 1, an `allow` node passes `&["occupant"]` and 0.
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

/// Deserialize `node`'s parameters into a `serde_json::Value`, plus a table
/// mapping each top-level key to its span for later diagnostics.
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
// The error
// ---------------------------------------------------------------------------

/// The deserializer-internal error, converted to [`LoadError`] at the entry
/// points.
#[derive(Debug)]
pub(crate) struct DeError {
    kind: DeErrorKind,
    /// The most specific span known. `None` only for visitor-originated
    /// errors until an enclosing map access attaches one.
    span: Option<SourceSpan>,
    /// The property or child name in scope, attached by the innermost map
    /// access.
    property: Option<String>,
}

#[derive(Debug)]
enum DeErrorKind {
    /// A message-only error from a visitor (`custom`, `invalid_value`); the
    /// span is attached by the enclosing context.
    Custom(String),
    /// serde-derive's end-of-map report of a missing required field.
    MissingField { field: &'static str },
    /// An unmatched key, from our `deserialize_ignored_any` or a
    /// `deny_unknown_fields` struct.
    UnknownField { field: String },
    /// A wrong-shaped value, raised here with the value's exact span before
    /// any visitor runs.
    InvalidType { expected: String, found: String },
    /// An integer that does not fit the requested width (`u8 = 300`).
    OutOfRange { expected: String },
    /// An extra positional argument, a repeated property, or a key spelled
    /// as both property and child. The message reads as "expected {msg}".
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

    fn invalid_type_at(
        expected: impl Into<String>,
        found: impl Into<String>,
        span: SourceSpan,
    ) -> Self {
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
            kind: DeErrorKind::UnknownField {
                field: field.into(),
            },
            span: Some(span),
            property: None,
        }
    }

    /// Attach the surrounding property name and span, filling only empty
    /// fields so an error that already knows its exact position keeps it.
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

    /// Convert to [`LoadError`], falling back to the node's own span and
    /// name when nothing more precise was recorded.
    pub(crate) fn into_load_error(self, node: &KdlNode, src: &str, system: &str) -> LoadError {
        let span = self.span.unwrap_or_else(|| node.span());
        let src = src.to_string();
        let system = system.to_string();
        // Shape errors at the node level carry no property; fall back to the
        // node name so the message never quotes an empty key.
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
// The node deserializer
// ---------------------------------------------------------------------------

/// Deserializes any `T: Deserialize` from one KDL node's parameters.
pub(crate) struct KdlNodeDe<'de> {
    node: &'de KdlNode,
    /// Property keys the caller consumes itself, never yielded as params.
    reserved: &'static [&'static str],
    /// Leading positional arguments the caller owns.
    skip_args: usize,
}

/// Where one field's value comes from.
enum FieldSource<'de> {
    /// A line property `k=v`.
    Entry(&'de KdlEntry),
    /// A child node, in any of its shapes (`k v`, `k a b c`, `k p=1 { … }`).
    Node(&'de KdlNode),
    /// Several children sharing one name, read as a sequence.
    Nodes(Vec<&'de KdlNode>),
}

/// Collect `node`'s parameters in document order, properties first and then
/// children grouped by name, enforcing the shape rules from the module doc.
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

    // `serde_json::Value` lands here; the parameters read as a JSON object.
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

    // A unit target accepts a parameter-less node and rejects stray keys.
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

    // Asking a whole node for a scalar, sequence, or enum cannot succeed;
    // drive the map and let the visitor's own `invalid_type` report it.
    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf seq tuple tuple_struct enum identifier
    }
}

/// Map access over a node's parameters.
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
            // Attach the property name and span to context-less visitor
            // errors; errors born with a span keep their more precise one.
            .map_err(|e| e.with_context(key, span))
    }
}

// ---------------------------------------------------------------------------
// The value-position deserializer (one field)
// ---------------------------------------------------------------------------

/// Deserializes one field's value.
struct FieldDe<'de> {
    source: FieldSource<'de>,
    key: &'de str,
    /// The span of the entry or child node the value came from.
    span: SourceSpan,
}

/// A human name for a KDL value's type, used in invalid-type messages.
fn value_desc(value: &KdlValue) -> &'static str {
    match value {
        KdlValue::String(_) => "a string",
        KdlValue::Integer(_) => "an integer",
        KdlValue::Float(_) => "a float",
        KdlValue::Bool(_) => "a boolean",
        KdlValue::Null => "#null",
    }
}

/// Drive `visitor` with one scalar KDL value.
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

/// The shape of a child node's own contents.
enum NodeShape {
    /// Properties and/or children, read as a nested map.
    Nested,
    /// Only positional arguments (possibly none), read as unit, scalar, or
    /// sequence.
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

/// A node's positional (unnamed) argument entries.
fn node_args(node: &KdlNode) -> impl Iterator<Item = &KdlEntry> {
    node.entries().iter().filter(|e| e.name().is_none())
}

impl<'de> FieldDe<'de> {
    /// The scalar value and its exact span, when this source is a property
    /// or a bare child with exactly one argument.
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

    /// A human name for what this source holds, used in invalid-type
    /// messages.
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

    /// The scalar value, or an invalid-type error naming `expected` at this
    /// field's span.
    fn scalar(&self, expected: &str) -> Result<(&'de KdlValue, SourceSpan), DeError> {
        self.scalar_value()
            .ok_or_else(|| DeError::invalid_type_at(expected, self.found_desc(), self.span))
    }
}

/// Integer widths, range-checked at the value's exact span.
macro_rules! de_int {
    ($method:ident, $visit:ident, $ty:ty, $expected:expr) => {
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
            let (value, span) = self.scalar($expected)?;
            match value {
                KdlValue::Integer(i) => {
                    let x =
                        <$ty>::try_from(*i).map_err(|_| DeError::out_of_range($expected, span))?;
                    visitor.$visit(x).map_err(|e: DeError| e.with_span(span))
                }
                other => Err(DeError::invalid_type_at($expected, value_desc(other), span)),
            }
        }
    };
}

/// Floats also accept an integer literal (`rate=200` reads as `200.0`).
macro_rules! de_float {
    ($method:ident, $expected:expr) => {
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
            let (value, span) = self.scalar($expected)?;
            match value {
                KdlValue::Float(f) => visitor
                    .visit_f64(*f)
                    .map_err(|e: DeError| e.with_span(span)),
                KdlValue::Integer(i) => visitor
                    .visit_f64(*i as f64)
                    .map_err(|e: DeError| e.with_span(span)),
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
    de_int!(
        deserialize_i128,
        visit_i128,
        i128,
        "a 128-bit signed integer"
    );
    de_int!(
        deserialize_u8,
        visit_u8,
        u8,
        "an 8-bit non-negative integer"
    );
    de_int!(
        deserialize_u16,
        visit_u16,
        u16,
        "a 16-bit non-negative integer"
    );
    de_int!(
        deserialize_u32,
        visit_u32,
        u32,
        "a 32-bit non-negative integer"
    );
    de_int!(
        deserialize_u64,
        visit_u64,
        u64,
        "a 64-bit non-negative integer"
    );
    de_int!(
        deserialize_u128,
        visit_u128,
        u128,
        "a 128-bit non-negative integer"
    );

    de_float!(deserialize_f32, "a number");
    de_float!(deserialize_f64, "a number");

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        let (value, span) = self.scalar("a boolean (#true/#false)")?;
        match value {
            KdlValue::Bool(b) => visitor
                .visit_bool(*b)
                .map_err(|e: DeError| e.with_span(span)),
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
            KdlValue::String(s) => visitor
                .visit_borrowed_str(s)
                .map_err(|e: DeError| e.with_span(span)),
            other => Err(DeError::invalid_type_at(
                "a string",
                value_desc(other),
                span,
            )),
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
        // A key present with `#null` is an explicit `None`; anything else is
        // `Some`.
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
            // Repeated same-name children yield one element per node.
            FieldSource::Nodes(nodes) => visitor.visit_seq(NodesSeq {
                iter: nodes.into_iter(),
                key,
            }),
            FieldSource::Node(n) => match node_shape(n) {
                // `k a b c` yields one element per positional argument.
                NodeShape::Args(_) => visitor.visit_seq(ArgsSeq {
                    iter: node_args(n).collect::<Vec<_>>().into_iter(),
                    key,
                }),
                // A single nested child reads as a one-element sequence.
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
        // A fieldless enum spelled as a string (`state="running"`).
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

    // serde-derive drains an unmatched key through here; erroring makes
    // every struct reject unknown keys, with the key's exact span.
    fn deserialize_ignored_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, DeError> {
        Err(DeError::unknown_field_at(self.key, self.span))
    }

    // The self-describing path; the source's shape decides what the visitor
    // sees.
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

/// Sequence elements drawn from child nodes.
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

/// Sequence elements drawn from one child's positional arguments.
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
        (
            src.parse::<KdlDocument>().expect("fixture KDL parses"),
            src.to_string(),
        )
    }

    /// Deserialize `T` from the first node of `src` with a `system` node's
    /// surface, reserving `type`/`artifact` and one leading name argument.
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
        let got: Flat =
            de_system(r#"system "x" type="T" count=3 rate=2 label="hi" enabled=#true"#).unwrap();
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
        // Scalar fields may also be spelled as `k v` children.
        let got: Flat =
            de_system(r#"system "x" type="T" { count 3; rate 0.5; label "hi"; enabled #false }"#)
                .unwrap();
        assert_eq!(got.rate, 0.5);
        assert!(!got.enabled);
    }

    #[test]
    fn null_property_is_explicit_none() {
        let got: Flat = de_system(
            r#"system "x" type="T" count=1 rate=1.0 label="l" enabled=#true offset=#null"#,
        )
        .unwrap();
        assert_eq!(got.offset, None);
    }

    #[test]
    fn missing_required_field_is_missing_param() {
        let err = de_system::<Flat>(r#"system "x" type="T" rate=1.0 label="l" enabled=#true"#)
            .unwrap_err();
        match err {
            LoadError::MissingParam {
                property, system, ..
            } => {
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
                assert!(
                    span.offset() >= at,
                    "span {span:?} inside the entry (at {at})"
                );
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
        assert!(
            matches!(err, LoadError::InvalidParam { ref property, .. } if property == "n"),
            "{err:?}"
        );
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
                pid: Pid {
                    p: 1.0,
                    i: 0.5,
                    d: 0.1
                },
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
        let got: Gains =
            de_system(r#"system "x" type="T" { gain p=1.0 i=0.0 d=0.0; gain p=2.0 i=0.1 d=0.2 }"#)
                .unwrap();
        assert_eq!(got.gain.len(), 2);
        assert_eq!(got.gain[1].p, 2.0);
    }

    #[test]
    fn nested_unknown_field_names_the_inner_key() {
        let err =
            de_system::<Nested>(r#"system "x" type="T" { pid p=1.0 i=0.5 d=0.1 q=9; taps 1 }"#)
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
        assert!(
            matches!(err, LoadError::InvalidParam { ref property, .. } if property == "mode"),
            "{err:?}"
        );
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

    /// The alarm grammar deserializes end to end: repeated `alarm` children,
    /// property and scalar-child fields, `Option` bands with per-side
    /// `Option` thresholds, the `Severity` enum from a string,
    /// integer-to-float coercion, and the `try_from` semantic validation.
    #[test]
    fn alarm_params_grammar_round_trips() {
        use metor_proto_wkt::Severity;

        use crate::alarm::AlarmsParams;

        let got: AlarmsParams = de_system(
            r#"system "alarms" type="Alarms" {
    alarm id="ADCS_RATE_HIGH" name="Body Rate High" severity="warning" {
        description "Body rate exceeds the safe envelope"
        target component="plant.sensors.gyro_b" element=1
        warning above=0.5 below=-0.5
        critical above=1 below=-1
        debounce 2
        hysteresis 0.05
        latching #true
    }
    alarm id="BATT_LOW" name="Battery Low" {
        target component="power.battery.soc"
        critical below=0.2
    }
}"#,
        )
        .unwrap();

        assert_eq!(got.alarm.len(), 2);
        let a = &got.alarm[0];
        assert_eq!(a.id, "ADCS_RATE_HIGH");
        assert_eq!(a.target.component, "plant.sensors.gyro_b");
        assert_eq!(a.target.element, Some(1));
        assert_eq!(a.warning.unwrap().above, Some(0.5));
        assert_eq!(a.critical.unwrap().above, Some(1.0)); // int literal coerces
        assert_eq!(a.debounce, 2);
        assert_eq!(a.hysteresis, 0.05);
        assert!(a.latching);
        assert_eq!(a.severity, Severity::Warning);

        let b = &got.alarm[1];
        assert_eq!(b.target.element, None);
        assert!(b.warning.is_none());
        assert_eq!(b.debounce, 1); // defaults
        assert!(!b.latching);
        assert_eq!(b.severity, Severity::Critical); // lowest configured band

        // An alarm-less node is legal.
        let empty: AlarmsParams = de_system(r#"system "alarms" type="Alarms""#).unwrap();
        assert!(empty.alarm.is_empty());
    }

    /// A semantically invalid alarm is a spanned invalid-param naming the
    /// `alarm` key (the `try_from` refinement), and a positional id is
    /// rejected because nested nodes take no positional arguments.
    #[test]
    fn alarm_params_misconfig_is_spanned() {
        use crate::alarm::AlarmsParams;

        // No band configured.
        let src = r#"system "alarms" type="Alarms" {
    alarm id="X" name="X" { target component="a.b.c" }
}"#;
        let err = de_system::<AlarmsParams>(src).unwrap_err();
        match err {
            LoadError::InvalidParam {
                ref property, span, ..
            } => {
                assert_eq!(property, "alarm");
                let at = src.find("alarm id=").unwrap();
                assert!(
                    span.offset() >= at,
                    "span {span:?} at the alarm child (at {at})"
                );
            }
            other => panic!("expected InvalidParam, got {other:?}"),
        }

        // The `alarm "X"` positional-id spelling is not the grammar.
        let err = de_system::<AlarmsParams>(
            r#"system "alarms" type="Alarms" {
    alarm "X" name="X" { target component="a.b.c"; warning above=1.0 }
}"#,
        )
        .unwrap_err();
        assert!(matches!(err, LoadError::InvalidParam { .. }), "{err:?}");
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
        assert_eq!(
            obj["pid"],
            serde_json::json!({ "p": 1, "i": 2.5, "d": null })
        );
        assert_eq!(obj["taps"], serde_json::json!([1, 2]));
        assert_eq!(obj["tag"], serde_json::json!(["a", "b"]));
        // The side table names every top-level key.
        for key in ["gain", "name", "on", "pid", "taps", "tag"] {
            assert!(spans.contains_key(key), "span table has `{key}`");
        }
        assert!(!spans.contains_key("type"), "reserved keys are not params");
    }
}
