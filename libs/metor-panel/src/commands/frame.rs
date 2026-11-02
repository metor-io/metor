//! A frame command's record, as its vtable lays it out.
//!
//! The bytes a frame command sends are its `#[repr(C)]` record exactly, so
//! the layout comes from walking the vtable the way the db ingests a table:
//! each field's component, prim type, shape and byte offset, plus the offset
//! of the timestamp its fields share. The vtable carries no field names;
//! those come from the component metadata the link registers alongside it.

use gpui::SharedString;
use metor_proto::types::{ComponentId, PrimType, Timestamp};
use metor_proto::vtable::{Field, Op, VTable};
use metor_proto_wkt::ComponentMetadata;
use serde_json::{Map, Value};

use super::encode::{EncodeError, float, integer};
use crate::dynamic::tensor::TypedScalar;

/// The fields of a frame record and where each sits in it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FrameLayout {
    fields: Vec<FrameField>,
    timestamps: Vec<usize>,
    size: usize,
    prefix: Option<SharedString>,
}

/// One field of a frame: a component's slot in the record.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FrameField {
    /// The component's name below the frame's path, its draft key.
    pub label: SharedString,
    pub ty: PrimType,
    pub shape: Vec<usize>,
    pub offset: usize,
    /// Per-element labels from the component's metadata, when it names
    /// them (`x,y,z`).
    pub element_names: Vec<SharedString>,
}

impl FrameField {
    pub(crate) fn len(&self) -> usize {
        self.shape.iter().product()
    }

    /// The label of element `ix`: its declared name, or its index.
    pub(crate) fn element_label(&self, ix: usize) -> SharedString {
        if let Some(name) = self.element_names.get(ix) {
            return name.clone();
        }
        let mut rest = ix;
        let mut index = vec![0; self.shape.len()];
        for (slot, dim) in index.iter_mut().zip(&self.shape).rev() {
            *slot = rest % dim.max(&1);
            rest /= dim.max(&1);
        }
        let index: Vec<String> = index.iter().map(usize::to_string).collect();
        format!("{}[{}]", self.label, index.join(",")).into()
    }
}

impl FrameLayout {
    /// Lay out `vtable`'s record, naming each field through `metadata`.
    /// A dynamic frame has no fixed record, so it is refused.
    pub(crate) fn new(
        vtable: &VTable<Vec<Op>, Vec<u8>, Vec<Field>>,
        metadata: &dyn Fn(ComponentId) -> Option<ComponentMetadata>,
    ) -> Result<Self, SharedString> {
        if vtable.is_dynamic() {
            return Err("Frames with lists or maps are not supported yet".into());
        }
        let mut found = Vec::new();
        let mut timestamps = Vec::new();
        vtable
            .for_each_field::<metor_proto::error::Error>(None, &mut |field| {
                found.push((
                    field.component_id,
                    field.ty,
                    field.shape.to_vec(),
                    field.offset,
                ));
                if let Some(at) = field.timestamp_at
                    && !timestamps.contains(&at)
                {
                    timestamps.push(at);
                }
                Ok(())
            })
            .map_err(|err| SharedString::from(format!("Unreadable frame layout: {err:?}")))?;
        if found.is_empty() {
            return Err("The frame has no fields".into());
        }

        let named: Vec<(Option<ComponentMetadata>, _)> = found
            .into_iter()
            .map(|field| (metadata(field.0), field))
            .collect();
        let prefix = common_prefix(named.iter().filter_map(|(meta, _)| meta.as_ref()));
        let fields: Vec<FrameField> = named
            .into_iter()
            .map(|(meta, (id, ty, shape, offset))| {
                let label = match (&meta, &prefix) {
                    (Some(meta), Some(prefix)) => meta
                        .name
                        .strip_prefix(prefix.as_str())
                        .and_then(|rest| rest.strip_prefix('.'))
                        .unwrap_or(&meta.name)
                        .to_string(),
                    (Some(meta), None) => meta.name.clone(),
                    (None, _) => id.to_string(),
                };
                let element_names = meta
                    .as_ref()
                    .map(|meta| meta.element_names())
                    .filter(|names| !names.is_empty())
                    .map(|names| {
                        names
                            .split(',')
                            .map(|n| n.trim().to_string().into())
                            .collect()
                    })
                    .unwrap_or_default();
                FrameField {
                    label: label.into(),
                    ty,
                    shape,
                    offset,
                    element_names,
                }
            })
            .collect();

        // A `#[repr(C)]` record is as long as its last member, rounded up to
        // its widest member's alignment.
        let mut extent = 0;
        let mut align = 1;
        for field in &fields {
            extent = extent.max(field.offset + field.len() * field.ty.size());
            align = align.max(field.ty.alignment());
        }
        for at in &timestamps {
            extent = extent.max(at + size_of::<i64>());
            align = align.max(align_of::<i64>());
        }
        Ok(Self {
            fields,
            timestamps,
            size: extent.next_multiple_of(align),
            prefix: prefix.map(SharedString::from),
        })
    }

    pub(crate) fn fields(&self) -> &[FrameField] {
        &self.fields
    }

    /// The path the frame's components are named under: the port it is
    /// laid out for, which tells apart two vehicles taking the same frame.
    pub(crate) fn prefix(&self) -> Option<&SharedString> {
        self.prefix.as_ref()
    }

    pub(crate) fn default_draft(&self) -> Value {
        Value::Object(
            self.fields
                .iter()
                .map(|field| {
                    let zero = || match field.ty {
                        PrimType::Bool => Value::Bool(false),
                        PrimType::F32 | PrimType::F64 => Value::from(0.0),
                        _ => Value::from(0u64),
                    };
                    let value = match field.shape.is_empty() {
                        true => zero(),
                        false => Value::Array((0..field.len()).map(|_| zero()).collect()),
                    };
                    (field.label.to_string(), value)
                })
                .collect::<Map<_, _>>(),
        )
    }

    /// The record `draft` describes, its timestamps set to `now`. Padding
    /// stays zero.
    pub(crate) fn encode(&self, draft: &Value, now: Timestamp) -> Result<Vec<u8>, EncodeError> {
        let mut out = vec![0u8; self.size];
        for at in &self.timestamps {
            out[*at..*at + size_of::<i64>()].copy_from_slice(&now.0.to_le_bytes());
        }
        for field in &self.fields {
            let label = field.label.as_ref();
            let value = draft.get(label).ok_or_else(|| EncodeError::Mismatch {
                field: label.into(),
                expected: "a value".into(),
            })?;
            let elements: Vec<&Value> = match (field.shape.is_empty(), value) {
                (true, value) => vec![value],
                (false, Value::Array(items)) if items.len() == field.len() => {
                    items.iter().collect()
                }
                (false, _) => {
                    return Err(EncodeError::Mismatch {
                        field: label.into(),
                        expected: format!("an array of {}", field.len()).into(),
                    });
                }
            };
            for (ix, element) in elements.into_iter().enumerate() {
                let name = match field.shape.is_empty() {
                    true => label.to_string(),
                    false => field.element_label(ix).to_string(),
                };
                let bytes = le_bytes(scalar(element, &name, field.ty)?);
                let at = field.offset + ix * field.ty.size();
                out[at..at + bytes.len()].copy_from_slice(&bytes);
            }
        }
        Ok(out)
    }
}

/// `value` as a `ty` element, refusing what `ty` cannot hold.
fn scalar(value: &Value, field: &str, ty: PrimType) -> Result<TypedScalar, EncodeError> {
    let int = |name, min: i128, max: i128| integer(value, field, name, min, max);
    Ok(match ty {
        PrimType::U8 => TypedScalar::U8(int("u8", 0, u8::MAX.into())? as u8),
        PrimType::U16 => TypedScalar::U16(int("u16", 0, u16::MAX.into())? as u16),
        PrimType::U32 => TypedScalar::U32(int("u32", 0, u32::MAX.into())? as u32),
        PrimType::U64 => TypedScalar::U64(int("u64", 0, u64::MAX.into())? as u64),
        PrimType::I8 => TypedScalar::I8(int("i8", i8::MIN.into(), i8::MAX.into())? as i8),
        PrimType::I16 => TypedScalar::I16(int("i16", i16::MIN.into(), i16::MAX.into())? as i16),
        PrimType::I32 => TypedScalar::I32(int("i32", i32::MIN.into(), i32::MAX.into())? as i32),
        PrimType::I64 => TypedScalar::I64(int("i64", i64::MIN.into(), i64::MAX.into())? as i64),
        PrimType::F32 => TypedScalar::F32(float(value, field, f32::MAX.into(), "f32")? as f32),
        PrimType::F64 => TypedScalar::F64(float(value, field, f64::MAX, "f64")?),
        PrimType::Bool => match value.as_bool() {
            Some(flag) => TypedScalar::Bool(flag),
            None => {
                return Err(EncodeError::Mismatch {
                    field: field.into(),
                    expected: "true or false".into(),
                });
            }
        },
    })
}

fn le_bytes(value: TypedScalar) -> Vec<u8> {
    match value {
        TypedScalar::U8(v) => v.to_le_bytes().to_vec(),
        TypedScalar::U16(v) => v.to_le_bytes().to_vec(),
        TypedScalar::U32(v) => v.to_le_bytes().to_vec(),
        TypedScalar::U64(v) => v.to_le_bytes().to_vec(),
        TypedScalar::I8(v) => v.to_le_bytes().to_vec(),
        TypedScalar::I16(v) => v.to_le_bytes().to_vec(),
        TypedScalar::I32(v) => v.to_le_bytes().to_vec(),
        TypedScalar::I64(v) => v.to_le_bytes().to_vec(),
        TypedScalar::F32(v) => v.to_le_bytes().to_vec(),
        TypedScalar::F64(v) => v.to_le_bytes().to_vec(),
        TypedScalar::Bool(v) => vec![v as u8],
    }
}

/// The dotted path every name in `metadata` shares, keeping at least one
/// segment of each name as its own.
fn common_prefix<'a>(metadata: impl Iterator<Item = &'a ComponentMetadata>) -> Option<String> {
    let mut prefix: Option<Vec<&str>> = None;
    for meta in metadata {
        let segments: Vec<&str> = meta.name.split('.').collect();
        let keep = segments.len().saturating_sub(1);
        prefix = Some(match prefix {
            None => segments[..keep].to_vec(),
            Some(prefix) => prefix
                .iter()
                .zip(&segments[..keep])
                .take_while(|(a, b)| a == b)
                .map(|(a, _)| *a)
                .collect(),
        });
    }
    prefix
        .filter(|segments| !segments.is_empty())
        .map(|segments| segments.join("."))
}
