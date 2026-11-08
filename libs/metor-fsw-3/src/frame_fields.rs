//! A fixed frame's leaves, by path, read from its port's schema.

use metor_proto::types::{ComponentId, PrimType};
use metor_proto::vtable::{Op, RealizedField, VTable};
use metor_proto_wkt::ComponentMetadata;

use crate::record::RecordSchema;
use crate::system::PortDef;

/// One fixed leaf of a frame, by path relative to the frame's root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leaf {
    pub path: String,
    pub ty: PrimType,
    pub shape: Vec<usize>,
    pub offset: u32,
    pub len: u32,
}

/// A fixed frame's leaves, its size, and where its stamp lives, which every
/// leaf agrees on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldTable {
    /// In vtable order.
    pub leaves: Vec<Leaf>,
    pub len: u32,
    pub stamp_at: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FieldError {
    #[error("`{0}` is a message, not a frame")]
    Message(String),
    #[error("`{0}` holds a dynamic field")]
    Dynamic(String),
    #[error("`{0}` has a malformed vtable")]
    Malformed(String),
    #[error("`{0}`'s fields disagree on where its timestamp is")]
    MixedStamps(String),
    #[error("`{record}`: component {id:?} has no metadata under `{root}`")]
    Unnamed {
        record: String,
        root: String,
        id: ComponentId,
    },
}

impl FieldTable {
    /// Reads `def`'s schema, whose root is `def.name`. A fixed frame's
    /// `max_len` is its size.
    pub fn of(def: &PortDef) -> Result<Self, FieldError> {
        let RecordSchema::Frame { vtable, metadata } = &def.schema else {
            return Err(FieldError::Message(def.record.to_string()));
        };
        if has_dynamic_op(vtable) {
            return Err(FieldError::Dynamic(def.record.to_string()));
        }
        let mut table = Self {
            leaves: Vec::new(),
            len: u32::try_from(def.max_len).unwrap_or(u32::MAX),
            stamp_at: None,
        };
        let names = Names { def, metadata };
        vtable
            .for_each_field(None, &mut |field| table.push(&names, field))
            .map_err(|e: Walk| e.into_error(def))?;
        Ok(table)
    }

    /// The leaves equal to `path` or under `path.`.
    pub fn under<'a>(&'a self, path: &'a str) -> impl Iterator<Item = &'a Leaf> + 'a {
        self.leaves
            .iter()
            .filter(move |leaf| is_under(&leaf.path, path))
    }

    /// `"f64"`, `"f64[3]"`, `"f64[6,6]"`.
    pub fn signature(leaf: &Leaf) -> String {
        let ty = leaf.ty.as_str();
        if leaf.shape.is_empty() {
            return ty.to_string();
        }
        let dims: Vec<String> = leaf.shape.iter().map(usize::to_string).collect();
        format!("{ty}[{}]", dims.join(","))
    }

    fn push(&mut self, names: &Names<'_>, field: RealizedField<'_>) -> Result<(), Walk> {
        if field.element.is_some() || field.template_id.is_some() || field.dynamic {
            return Err(Walk::Field(FieldError::Dynamic(
                names.def.record.to_string(),
            )));
        }
        let stamp_at = field.timestamp_at.map(to_u32);
        if !self.leaves.is_empty() && stamp_at != self.stamp_at {
            return Err(Walk::Field(FieldError::MixedStamps(
                names.def.record.to_string(),
            )));
        }
        self.stamp_at = stamp_at;
        let count: usize = field.shape.iter().product();
        self.leaves.push(Leaf {
            path: names.path(field.component_id)?.to_string(),
            ty: field.ty,
            shape: field.shape.to_vec(),
            offset: to_u32(field.offset),
            len: to_u32(field.ty.size().saturating_mul(count)),
        });
        Ok(())
    }

    /// The element of a shaped leaf at `path`, as its own leaf.
    pub fn element(&self, path: &str) -> Result<Leaf, ElementError> {
        let (leaf, rest) = self.prefix_of(path).ok_or(ElementError::Unknown)?;
        let indices = indices(rest, &leaf.shape)?;
        let (off, len) =
            locate(&leaf.shape, leaf.ty.size(), &indices).ok_or(ElementError::Unknown)?;
        let shape = leaf
            .shape
            .get(indices.len()..)
            .ok_or(ElementError::Unknown)?;
        let offset = u32::try_from(off)
            .ok()
            .and_then(|off| leaf.offset.checked_add(off));
        Ok(Leaf {
            path: path.to_string(),
            ty: leaf.ty,
            shape: shape.to_vec(),
            offset: offset.ok_or(ElementError::Unknown)?,
            len: u32::try_from(len).map_err(|_| ElementError::Unknown)?,
        })
    }

    /// The leaf that is a proper prefix of `path`, and the rest after its `.`.
    fn prefix_of<'a>(&self, path: &'a str) -> Option<(&Leaf, &'a str)> {
        self.leaves.iter().find_map(|leaf| {
            let rest = path.strip_prefix(leaf.path.as_str())?.strip_prefix('.')?;
            Some((leaf, rest))
        })
    }
}

/// Why an element path does not resolve.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElementError {
    /// No leaf is a proper prefix, a segment is not a canonical decimal
    /// index, or there are more indices than dimensions.
    Unknown,
    /// An index at or past its dimension, of length `len`.
    Index { len: usize },
}

/// `rest`'s segments as indices into `shape`.
fn indices(rest: &str, shape: &[usize]) -> Result<Vec<usize>, ElementError> {
    let indices: Vec<usize> = rest
        .split('.')
        .map(index)
        .collect::<Option<_>>()
        .ok_or(ElementError::Unknown)?;
    if indices.len() > shape.len() {
        return Err(ElementError::Unknown);
    }
    match indices.iter().zip(shape).find(|(i, d)| i >= d) {
        Some((_, &len)) => Err(ElementError::Index { len }),
        None => Ok(indices),
    }
}

/// `(offset, len)` in bytes of the element at `indices` of a `shape` of
/// `size`-byte elements, relative to the leaf; `None` past a dimension.
fn locate(shape: &[usize], size: usize, indices: &[usize]) -> Option<(usize, usize)> {
    let (head, tail) = (shape.get(..indices.len())?, shape.get(indices.len()..)?);
    let len = tail.iter().try_fold(size, |acc, &d| acc.checked_mul(d))?;
    let mut stride = len;
    let mut off = 0usize;
    for (&i, &d) in indices.iter().zip(head).rev() {
        if i >= d {
            return None;
        }
        off = off.checked_add(i.checked_mul(stride)?)?;
        stride = stride.checked_mul(d)?;
    }
    Some((off, len))
}

/// A canonical decimal index: digits only, no leading zero but "0".
fn index(segment: &str) -> Option<usize> {
    let canonical = segment.bytes().all(|b| b.is_ascii_digit())
        && (segment == "0" || !segment.starts_with('0'));
    canonical.then(|| segment.parse().ok()).flatten()
}

/// Whether `leaf` is `path` or lies under `path.`.
pub(crate) fn is_under(leaf: &str, path: &str) -> bool {
    leaf.strip_prefix(path)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
}

fn has_dynamic_op(vtable: &VTable) -> bool {
    vtable
        .ops
        .iter()
        .any(|op| matches!(op, Op::List { .. } | Op::Map { .. }))
}

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Names a component by its metadata, relative to the port's root.
struct Names<'a> {
    def: &'a PortDef,
    metadata: &'a [ComponentMetadata],
}

impl Names<'_> {
    fn path(&self, id: ComponentId) -> Result<&str, Walk> {
        self.metadata
            .iter()
            .find(|m| m.component_id == id)
            .and_then(|m| m.name.strip_prefix(self.def.name.as_ref()))
            .and_then(|rest| rest.strip_prefix('.'))
            .ok_or_else(|| {
                Walk::Field(FieldError::Unnamed {
                    record: self.def.record.to_string(),
                    root: self.def.name.to_string(),
                    id,
                })
            })
    }
}

/// A walk's error: a refused field, or a malformed vtable.
enum Walk {
    Field(FieldError),
    Vtable,
}

impl Walk {
    fn into_error(self, def: &PortDef) -> FieldError {
        match self {
            Self::Field(e) => e,
            Self::Vtable => FieldError::Malformed(def.record.to_string()),
        }
    }
}

impl From<metor_proto::error::Error> for Walk {
    fn from(_: metor_proto::error::Error) -> Self {
        Self::Vtable
    }
}

#[cfg(all(test, feature = "host"))]
mod tests;

#[cfg(kani)]
mod proofs {
    use super::*;

    #[kani::proof]
    #[kani::unwind(4)]
    fn locate_in_bounds() {
        let dims: [usize; 2] = [kani::any(), kani::any()];
        let idx: [usize; 2] = [kani::any(), kani::any()];
        let (rank, count): (usize, usize) = (kani::any(), kani::any());
        kani::assume(rank <= 2 && count <= rank);
        kani::assume(dims.iter().all(|&d| d <= 8));
        let size = if kani::any() { 1 } else { 8 };
        let (shape, indices) = (&dims[..rank], &idx[..count]);
        let inside = indices.iter().zip(shape).all(|(i, d)| i < d);
        match locate(shape, size, indices) {
            Some((off, len)) => {
                assert!(inside);
                assert!(off + len <= size * shape.iter().product::<usize>());
            }
            None => assert!(!inside),
        }
    }
}
