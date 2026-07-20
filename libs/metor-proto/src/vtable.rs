//! VTables allow you to dynamically formulate and parse tables.
//!
//! A VTable is made up of a series of "fields", each with an associated offset and length.
//! Each field has an associated expression that describes the associated component and entity id.
//! Expressions can also contain metadata for the field, like the schema, location of the timestamp,
//! or even the rate it should be populated at.
//!
//! VTables are used by `metor-db` to let the user send data and receive data in the format and shape
//! they expect. This let's you specify a struct in Rust and send it directly into the db, with no serialization step.
//! `metor-db` also allows you to subscribe to a particular VTable. The database will handle building tables from each of the fields
//! described in the VTable.
//!
//! At this point it is likely easier to see an example of how to formulate a VTable.
//! ```rust
//! use metor_proto::types::PrimType;
//! use metor_proto::vtable::VTable;
//! use metor_proto::vtable::builder::*;
//! struct IMU {
//!     timestamp: u64,
//!     gyro: [f64; 3],
//!     accel: [f64; 3],
//! }
//!
//! impl IMU {
//!     fn as_vtable() -> VTable {
//!         let time = table!(IMU::timestamp);
//!         vtable([
//!             field!(
//!                 IMU::gyro,
//!                 schema(
//!                     PrimType::F64,
//!                     &[3],
//!                     timestamp(time.clone(), component("gyro")),
//!                 ),
//!             ),
//!             field!(
//!                 IMU::accel,
//!                 schema(
//!                     PrimType::F64,
//!                     &[3],
//!                     timestamp(time.clone(), component("accel")),
//!                 ),
//!             ),
//!         ])
//!     }
//! }
//! ```

use core::ops::Range;

use serde::{Deserialize, Serialize};
use zerocopy::{FromBytes, IntoBytes, TryFromBytes};

use crate::{
    buf::Buf,
    com_de::Decomponentize,
    error::Error,
    hash::PathHasher,
    types::{ComponentId, ComponentView, PacketId, PrimType, Timestamp},
};

/// Operations that can be performed in a VTable
///
/// Each operation represents a different way to reference or manipulate data within a VTable.
#[derive(Debug, Serialize, Deserialize, Clone, postcard_schema::Schema)]
#[repr(u8)]
pub enum Op {
    Data {
        offset: Offset,
        len: u32,
    },
    Table {
        offset: Offset,
        len: u32,
    },
    None,
    Component {
        component_id: OpRef,
    },
    Schema {
        ty: OpRef,
        dim: OpRef,
        arg: OpRef,
    },

    Timestamp {
        source: OpRef,
        arg: OpRef,
    },

    Ext {
        arg: OpRef,
        id: PacketId,
        data: OpRef,
    },

    /// Tags the field's op chain as belonging to a frame named by `component_id`.
    ///
    /// Metadata op (like [`Op::Timestamp`]): records the frame id and continues
    /// evaluating at `arg`. `component_id` references a [`Op::Data`] op holding the
    /// frame's [`ComponentId`].
    Frame {
        component_id: OpRef,
        arg: OpRef,
    },

    /// Dynamic list terminal — a runtime-sized, indexed sequence of element frames.
    ///
    /// The owning [`Field`]'s `offset`/`len` address an 8-byte slot
    /// `{ trailer_off: u32, byte_len: u32 }` in the fixed region. Elements are laid
    /// out back-to-back in the trailer with byte `stride`; `count = byte_len / stride`.
    /// `name` references a [`Op::Data`] op holding the field's path prefix
    /// (e.g. `"processes"`). `members` is a contiguous range of member-template
    /// [`Field`]s whose offsets are relative to each element's base. A member's chain
    /// may itself terminate in `List`/`Map`, giving nested dynamics.
    List {
        name: OpRef,
        members: ElementFields,
        stride: u32,
    },

    /// Dynamic map terminal — a runtime-sized sequence of name-keyed element frames.
    ///
    /// Like [`Op::List`], but each trailer entry is
    /// `{ key_off: u32, key_len: u32, <pad> value }`: the key *bytes* live in a name
    /// pool elsewhere in the trailer (addressed by `key_off`/`key_len`), and the value
    /// sub-frame begins at `value_offset` within the entry. Keys must not contain `.`
    /// (it would alias the dotted-path grammar) — realization rejects such keys.
    Map {
        name: OpRef,
        members: ElementFields,
        stride: u32,
        value_offset: u32,
    },

    /// Dynamic leaf terminal — used in member templates instead of [`Op::Component`].
    ///
    /// Appends `name` (a [`Op::Data`] op holding the member name, e.g. `"pid"`) to the
    /// running dotted path accumulated by the enclosing `List`/`Map` expansion, then
    /// finalizes it into a [`ComponentId`]. Equivalent component to
    /// `ComponentId::new("<prefix>.<key>.<name>")`.
    PathComponent {
        name: OpRef,
    },
}

/// A contiguous range of member-template [`Field`]s in a [`VTable`]'s `fields` list.
///
/// Referenced by [`Op::List`]/[`Op::Map`]; the named fields describe one element and
/// are *not* iterated as top-level fields (they are claimed templates).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, postcard_schema::Schema,
)]
#[repr(C)]
pub struct ElementFields {
    pub start: u32,
    pub count: u32,
}

/// Maximum dynamic-frame nesting depth honored during realization.
///
/// Bounds recursion (and therefore stack use) on the no-alloc path. Exceeding it is
/// reported as [`Error::InvalidOp`].
pub const MAX_DYNAMIC_DEPTH: usize = 8;

/// A field within a VTable
///
/// Each field has an offset, length, and an associated operation reference.
#[derive(Debug, Serialize, Deserialize, Clone, postcard_schema::Schema)]
pub struct Field {
    pub offset: Offset,
    pub len: u32,
    pub arg: OpRef,
}

const _ASSERT_OP_SIZE: () = const {
    assert!(core::mem::size_of::<Op>() <= 64);
};

/// A reference to an operation in the VTable's operation list
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, postcard_schema::Schema,
)]
#[repr(transparent)]
pub struct OpRef(u32);

impl OpRef {
    /// Converts the operation reference to a usable index
    pub fn to_index(self) -> usize {
        self.0 as usize
    }
}

/// Represents an offset position within a buffer
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, postcard_schema::Schema,
)]
#[repr(transparent)]
pub struct Offset(u32);

impl From<u32> for Offset {
    fn from(val: u32) -> Self {
        Offset(val)
    }
}

impl Offset {
    /// Converts the offset to a usable index
    pub fn to_index(self) -> usize {
        self.0 as usize
    }
}

#[cfg(feature = "alloc")]
type DefaultOps = alloc::vec::Vec<Op>;

#[cfg(not(feature = "alloc"))]
type DefaultOps = heapless::Vec<Op, 32>;

#[cfg(feature = "alloc")]
type DefaultData = alloc::vec::Vec<u8>;
#[cfg(not(feature = "alloc"))]
type DefaultData = heapless::Vec<u8, 32>;
#[cfg(feature = "alloc")]
type DefaultFields = alloc::vec::Vec<Field>;
#[cfg(not(feature = "alloc"))]
type DefaultFields = heapless::Vec<Field, 32>;

/// A description of the layout of a table
#[derive(Debug, Serialize, Deserialize, Clone, Default, postcard_schema::Schema)]
pub struct VTable<
    Ops: Buf<Op> = DefaultOps,
    Data: Buf<u8> = DefaultData,
    Fields: Buf<Field> = DefaultFields,
> {
    #[serde(bound(deserialize = ""))]
    pub ops: Ops,
    #[serde(bound(deserialize = ""))]
    pub fields: Fields,
    #[serde(bound(deserialize = ""))]
    pub data: Data,
}

/// A component ID realized from an operation
#[derive(Clone, Copy, Debug)]
pub struct RealizedComponent {
    pub component_id: ComponentId,
}

/// A schema realized from an operation, containing dimension, type, and argument information
pub struct RealizedSchema<'a> {
    pub dim: &'a [u64],
    pub ty: PrimType,
    pub arg: OpRef,
}

/// A timestamp realized from an operation, possibly including range information
pub struct RealizedTimestamp {
    pub timestamp: Option<Timestamp>,
    pub range: Option<Range<usize>>,
    pub arg: OpRef,
}

/// A slice of a table realized from an operation
pub struct RealizedTableSlice<'a> {
    pub slice: Option<&'a [u8]>,
    pub range: Range<usize>,
}

/// An extension realized from an operation, containing ID, data, and argument information
pub struct RealizedExt<'a> {
    pub id: PacketId,
    pub data: &'a [u8],
    pub arg: OpRef,
}

/// A frame-identity op realized from a VTable (see [`Op::Frame`]).
#[derive(Clone, Copy, Debug)]
pub struct RealizedFrame {
    pub component_id: ComponentId,
    pub arg: OpRef,
}

/// A dynamic list/map terminal realized from a VTable (see [`Op::List`]/[`Op::Map`]).
#[derive(Clone, Copy, Debug)]
pub struct RealizedDynamic<'a> {
    /// The field's dotted-path prefix (e.g. `"processes"`).
    pub name: &'a str,
    /// The member-template field range describing one element.
    pub members: ElementFields,
    /// Byte stride of one element (list) or one `(key, value)` entry (map).
    pub stride: u32,
    /// Byte offset of the value sub-frame within an entry; `0` for lists.
    pub value_offset: u32,
    /// Whether this is a map (`true`) or a list (`false`).
    pub is_map: bool,
}

/// A dynamic leaf terminal realized from a VTable (see [`Op::PathComponent`]).
#[derive(Clone, Copy, Debug)]
pub struct RealizedPathLeaf<'a> {
    /// The member name segment (e.g. `"pid"`).
    pub name: &'a str,
}

/// A realized operation, which is an operation that has been evaluated against a table
pub enum RealizedOp<'a> {
    Data(&'a [u8]),
    Table(RealizedTableSlice<'a>),
    Component(RealizedComponent),
    Schema(RealizedSchema<'a>),
    Timestamp(RealizedTimestamp),
    Ext(RealizedExt<'a>),
    None,
    Frame(RealizedFrame),
    List(RealizedDynamic<'a>),
    Map(RealizedDynamic<'a>),
    PathComponent(RealizedPathLeaf<'a>),
}

/// Identifies which element of a dynamic frame a [`RealizedField`] came from.
#[derive(Clone, Copy, Debug)]
pub enum ElementKey<'a> {
    /// A list element's positional index.
    Index(u32),
    /// A map entry's name key.
    Key(&'a str),
}

/// State threaded through the field-realization walk.
///
/// For top-level fields this is [`WalkCtx::default`] (base 0, empty path, no inherited
/// frame/timestamp). Dynamic expansion derives a child context per element with the
/// element base offset, the extended dotted path, the element key, and the inherited
/// frame/timestamp.
#[derive(Clone, Copy)]
struct WalkCtx<'a> {
    base: usize,
    path: PathHasher,
    element: Option<ElementKey<'a>>,
    /// The dotted path of the enclosing dynamic container (the [`Op::List`]/
    /// [`Op::Map`] `name`), so a lazily-created keyed member can be named
    /// `<container>.<key>.<member>` at ingest instead of by numeric id.
    container: Option<&'a str>,
    frame: Option<ComponentId>,
    timestamp: Option<Timestamp>,
    depth: usize,
}

impl Default for WalkCtx<'_> {
    fn default() -> Self {
        Self {
            base: 0,
            path: PathHasher::new(),
            element: None,
            container: None,
            frame: None,
            timestamp: None,
            depth: 0,
        }
    }
}

/// A field realized from a VTable, containing entity and component IDs,
/// shape, type, component view, and timestamp information
pub struct RealizedField<'a> {
    pub component_id: ComponentId,
    pub shape: &'a [usize],
    pub ty: PrimType,
    pub offset: usize,
    pub view: Option<ComponentView<'a>>,
    pub timestamp: Option<Timestamp>,
    /// The frame this field belongs to, if tagged by an [`Op::Frame`].
    pub frame: Option<ComponentId>,
    /// The dynamic-frame element this field came from, if any (`None` for static fields).
    pub element: Option<ElementKey<'a>>,
    /// The enclosing dynamic container's dotted path and this member's leaf
    /// name (`None` for static fields). With [`element`](Self::element) they
    /// reconstruct the member's dotted name — `<container>.<key>.<member>` —
    /// which the hashed [`component_id`](Self::component_id) alone cannot.
    pub container: Option<&'a str>,
    pub member: Option<&'a str>,
    /// Whether this field was realized through a dynamic terminal
    /// ([`Op::PathComponent`], reached via [`Op::List`]/[`Op::Map`]). In
    /// schema/registration mode (`table = None`) such a field is a member
    /// *template* (e.g. `processes.pid`) that never receives samples directly;
    /// in ingest mode it is one concrete element-member (`processes.0.pid`).
    pub dynamic: bool,
}

impl<'a> RealizedOp<'a> {
    /// Returns the operation as a byte slice if it contains data
    pub fn as_slice(&self) -> Option<&'a [u8]> {
        match self {
            RealizedOp::Data(data) => Some(data),
            RealizedOp::Table(table) => table.slice,
            _ => None,
        }
    }

    /// Attempts to interpret the operation as a component ID
    pub fn as_component_id(&self) -> Option<ComponentId> {
        let data = self.as_slice()?;
        let data: [u8; 8] = data.try_into().ok()?;
        Some(ComponentId(u64::from_le_bytes(data)))
    }

    /// Attempts to interpret the operation as a primitive type
    pub fn as_prim_ty(&self) -> Option<PrimType> {
        let data = self.as_slice()?;
        PrimType::try_read_from_bytes(data).ok()
    }

    /// Returns a reference to the realized timestamp if this operation is a timestamp
    pub fn as_timestamp(&self) -> Option<&RealizedTimestamp> {
        match self {
            RealizedOp::Timestamp(timestamp) => Some(timestamp),
            _ => None,
        }
    }

    /// Returns the range of a table if this operation represents a table
    pub fn as_table_range(&self) -> Option<Range<usize>> {
        match self {
            RealizedOp::Table(t) => Some(t.range.clone()),
            _ => None,
        }
    }
}

impl<Ops: Buf<Op>, Data: Buf<u8>, Fields: Buf<Field>> VTable<Ops, Data, Fields> {
    /// Whether this VTable contains any dynamic terminal ([`Op::List`]/
    /// [`Op::Map`]) and therefore realizes runtime-sized, lazily-created
    /// components rather than a fixed component set.
    pub fn is_dynamic(&self) -> bool {
        self.ops
            .iter()
            .any(|op| matches!(op, Op::List { .. } | Op::Map { .. }))
    }

    /// Retrieves an operation by its reference
    #[inline]
    pub fn get_op(&self, op_ref: OpRef) -> Result<&Op, Error> {
        self.ops
            .as_slice()
            .get(op_ref.to_index())
            .ok_or(Error::OpRefNotFound)
    }

    /// Evaluates an operation, and turns it into a [`RealizedOp`]
    ///
    /// `realize` follows each [`Offset`] and transforms it into an actual reference or data.
    /// Evaluates an operation, turning it into a [`RealizedOp`]
    ///
    /// `realize` follows each [`Offset`] and transforms it into an actual reference or data
    pub fn realize<'a>(
        &'a self,
        op_ref: OpRef,
        table: Option<&'a [u8]>,
    ) -> Result<RealizedOp<'a>, Error> {
        let op = self.get_op(op_ref)?;
        match op {
            Op::Data { offset, len } => {
                let data = self.data.as_slice();
                let data = data
                    .get(offset.to_index()..offset.to_index() + *len as usize)
                    .ok_or(Error::BufferOverflow)?;
                Ok(RealizedOp::Data(data))
            }
            Op::Table { offset, len } => {
                let range = offset.to_index()..offset.to_index() + *len as usize;
                let table = if let Some(table) = table {
                    Some(table.get(range.clone()).ok_or(Error::BufferUnderflow)?)
                } else {
                    None
                };
                Ok(RealizedOp::Table(RealizedTableSlice {
                    slice: table,
                    range,
                }))
            }
            Op::Component { component_id } => {
                let component_id = self
                    .realize(*component_id, table)?
                    .as_component_id()
                    .ok_or(Error::InvalidOp)?;
                Ok(RealizedOp::Component(RealizedComponent { component_id }))
            }
            Op::Schema { ty, dim, arg } => {
                let ty = self
                    .realize(*ty, table)?
                    .as_prim_ty()
                    .ok_or(Error::InvalidOp)?;
                let dim = self
                    .realize(*dim, table)?
                    .as_slice()
                    .ok_or(Error::InvalidOp)?;
                let dim = <[u64]>::try_ref_from_bytes(dim)?;
                Ok(RealizedOp::Schema(RealizedSchema { ty, dim, arg: *arg }))
            }
            Op::Timestamp { source, arg } => {
                let source = self.realize(*source, table)?;
                let timestamp = if let Some(data) = source.as_slice() {
                    Some(Timestamp::read_from_bytes(data)?)
                } else {
                    None
                };
                Ok(RealizedOp::Timestamp(RealizedTimestamp {
                    timestamp,
                    arg: *arg,
                    range: source.as_table_range(),
                }))
            }
            Op::None => Ok(RealizedOp::None),
            Op::Ext { arg, id, data } => {
                let data = self
                    .realize(*data, table)?
                    .as_slice()
                    .ok_or(Error::InvalidOp)?;
                Ok(RealizedOp::Ext(RealizedExt {
                    id: *id,
                    data,
                    arg: *arg,
                }))
            }
            Op::Frame { component_id, arg } => {
                let component_id = self
                    .realize(*component_id, table)?
                    .as_component_id()
                    .ok_or(Error::InvalidOp)?;
                Ok(RealizedOp::Frame(RealizedFrame {
                    component_id,
                    arg: *arg,
                }))
            }
            Op::List {
                name,
                members,
                stride,
            } => {
                let name = self.realize_str(*name, table)?;
                Ok(RealizedOp::List(RealizedDynamic {
                    name,
                    members: *members,
                    stride: *stride,
                    value_offset: 0,
                    is_map: false,
                }))
            }
            Op::Map {
                name,
                members,
                stride,
                value_offset,
            } => {
                let name = self.realize_str(*name, table)?;
                Ok(RealizedOp::Map(RealizedDynamic {
                    name,
                    members: *members,
                    stride: *stride,
                    value_offset: *value_offset,
                    is_map: true,
                }))
            }
            Op::PathComponent { name } => {
                let name = self.realize_str(*name, table)?;
                Ok(RealizedOp::PathComponent(RealizedPathLeaf { name }))
            }
        }
    }

    /// Resolves an op reference to a UTF-8 string held in the VTable's `data` buffer.
    fn realize_str<'a>(&'a self, op_ref: OpRef, table: Option<&'a [u8]>) -> Result<&'a str, Error> {
        let bytes = self.realize(op_ref, table)?.as_slice().ok_or(Error::InvalidOp)?;
        core::str::from_utf8(bytes).map_err(|_| Error::InvalidOp)
    }

    /// Drives realization of every top-level field, invoking `f` once per realized
    /// component.
    ///
    /// Unlike a one-field-one-component mapping, a dynamic field ([`Op::List`]/
    /// [`Op::Map`]) expands into many components — one per element-member — so this
    /// uses a push-style callback rather than returning an iterator. It is the shared
    /// engine behind both [`VTable::apply`] and (under `alloc`) [`VTable::realize_fields`],
    /// and is no-alloc friendly (no heap; recursion bounded by [`MAX_DYNAMIC_DEPTH`]).
    ///
    /// Member-template fields (those claimed by a `List`/`Map` op) are skipped at the
    /// top level — they are only realized as part of their owning dynamic field.
    pub fn for_each_field<'a>(
        &'a self,
        table: Option<&'a [u8]>,
        f: &mut dyn FnMut(RealizedField<'a>) -> Result<(), Error>,
    ) -> Result<(), Error> {
        for (i, field) in self.fields.iter().enumerate() {
            if self.is_template_field(i) {
                continue;
            }
            self.walk_field(field, WalkCtx::default(), table, f)?;
        }
        Ok(())
    }

    /// Returns whether field index `i` is a member template claimed by some `List`/`Map`
    /// op, and therefore should not be walked as a top-level field.
    fn is_template_field(&self, i: usize) -> bool {
        self.ops.iter().any(|op| {
            let members = match op {
                Op::List { members, .. } | Op::Map { members, .. } => members,
                _ => return false,
            };
            let start = members.start as usize;
            i >= start && i < start + members.count as usize
        })
    }

    /// Walks a single field's op chain against `table`, emitting realized component(s).
    ///
    /// `ctx` carries the containing element's base offset, the accumulated dotted-name
    /// prefix, the inherited frame/timestamp, the element key, and the recursion depth.
    fn walk_field<'a>(
        &'a self,
        field: &Field,
        ctx: WalkCtx<'a>,
        table: Option<&'a [u8]>,
        f: &mut dyn FnMut(RealizedField<'a>) -> Result<(), Error>,
    ) -> Result<(), Error> {
        if ctx.depth > MAX_DYNAMIC_DEPTH {
            return Err(Error::InvalidOp);
        }
        let mut realized_op = self.realize(field.arg, table)?;
        // Frame and timestamp are inherited from the enclosing frame, but the field's
        // own `Frame`/`Timestamp` ops override them.
        let mut timestamp: Option<Timestamp> = ctx.timestamp;
        let mut schema: Option<RealizedSchema<'a>> = None;
        let mut frame: Option<ComponentId> = ctx.frame;
        loop {
            match realized_op {
                RealizedOp::Component(RealizedComponent { component_id }) => {
                    return self.emit_leaf(field, &ctx, component_id, frame, &schema, timestamp, false, None, table, f);
                }
                RealizedOp::PathComponent(leaf) => {
                    let mut leaf_path = ctx.path;
                    leaf_path.push(leaf.name);
                    let component_id = leaf_path.finish();
                    return self.emit_leaf(field, &ctx, component_id, frame, &schema, timestamp, true, Some(leaf.name), table, f);
                }
                RealizedOp::Schema(s) => {
                    let s = schema.insert(s);
                    realized_op = self.realize(s.arg, table)?;
                }
                RealizedOp::Timestamp(t) => {
                    if t.timestamp.is_some() {
                        timestamp = t.timestamp;
                    }
                    realized_op = self.realize(t.arg, table)?;
                }
                RealizedOp::Frame(fr) => {
                    frame = Some(fr.component_id);
                    realized_op = self.realize(fr.arg, table)?;
                }
                RealizedOp::Ext(e) => {
                    realized_op = self.realize(e.arg, table)?;
                }
                RealizedOp::List(dynm) | RealizedOp::Map(dynm) => {
                    return self.expand_dynamic(field, &ctx, frame, timestamp, &dynm, table, f);
                }
                _ => return Err(Error::InvalidOp),
            }
        }
    }

    /// Builds and emits a single realized component for a leaf (static [`Op::Component`]
    /// or dynamic [`Op::PathComponent`]).
    #[allow(clippy::too_many_arguments)]
    fn emit_leaf<'a>(
        &'a self,
        field: &Field,
        ctx: &WalkCtx<'a>,
        component_id: ComponentId,
        frame: Option<ComponentId>,
        schema: &Option<RealizedSchema<'a>>,
        timestamp: Option<Timestamp>,
        dynamic: bool,
        member: Option<&'a str>,
        table: Option<&'a [u8]>,
        f: &mut dyn FnMut(RealizedField<'a>) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let schema = schema.as_ref().ok_or(Error::SchemaNotFound)?;
        // NOTE(sphw): bogan version of zerocopy::transmute_ref; see the original
        // realize_fields for the upstream zerocopy issue this works around.
        let shape: &[usize] = <[usize]>::ref_from_bytes(schema.dim.as_bytes())?;
        let offset = ctx.base + field.offset.to_index();
        let view = if let Some(table) = table {
            let data = table
                .get(offset..offset + field.len as usize)
                .ok_or(Error::BufferUnderflow)?;
            Some(ComponentView::try_from_bytes_shape(data, shape, schema.ty)?)
        } else {
            None
        };
        f(RealizedField {
            component_id,
            view,
            timestamp,
            offset,
            shape,
            ty: schema.ty,
            frame,
            element: ctx.element,
            container: ctx.container,
            member,
            dynamic,
        })
    }

    /// Expands a dynamic [`Op::List`]/[`Op::Map`] terminal, walking each element's
    /// member templates and threading the dotted-name path and inherited frame/timestamp.
    ///
    /// With `table = None` (schema/registration mode) the element count is unknown, so
    /// each member template is emitted once with no element key and no view — enough to
    /// surface the member ty/shape.
    #[allow(clippy::too_many_arguments)]
    fn expand_dynamic<'a>(
        &'a self,
        field: &Field,
        ctx: &WalkCtx<'a>,
        frame: Option<ComponentId>,
        timestamp: Option<Timestamp>,
        dynm: &RealizedDynamic<'a>,
        table: Option<&'a [u8]>,
        f: &mut dyn FnMut(RealizedField<'a>) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let mut prefix_path = ctx.path;
        prefix_path.push(dynm.name);
        let members_start = dynm.members.start as usize;
        let members_end = members_start + dynm.members.count as usize;

        let Some(table) = table else {
            // Schema/registration mode: emit each member template once.
            let child = WalkCtx {
                base: 0,
                path: prefix_path,
                element: None,
                container: Some(dynm.name),
                frame,
                timestamp,
                depth: ctx.depth + 1,
            };
            for mi in members_start..members_end {
                let member = self.fields.as_slice().get(mi).ok_or(Error::OpRefNotFound)?;
                self.walk_field(member, child, None, f)?;
            }
            return Ok(());
        };

        let stride = dynm.stride as usize;
        if stride == 0 {
            return Err(Error::InvalidOp);
        }
        let slot_off = ctx.base + field.offset.to_index();
        let (trailer_off, byte_len) = read_slot(table, slot_off)?;
        let count = byte_len / stride;
        for i in 0..count {
            let entry_base = trailer_off + i * stride;
            let (member_base, key) = if dynm.is_map {
                let key_off = read_u32(table, entry_base)? as usize;
                let key_len = read_u32(table, entry_base + 4)? as usize;
                let key_bytes = table
                    .get(key_off..key_off + key_len)
                    .ok_or(Error::BufferUnderflow)?;
                let key = core::str::from_utf8(key_bytes).map_err(|_| Error::InvalidOp)?;
                // A '.' in a key would alias the dotted-path grammar (it would create
                // an extra, ambiguous path segment); reject it.
                if key.contains('.') {
                    return Err(Error::InvalidComponentData);
                }
                (entry_base + dynm.value_offset as usize, ElementKey::Key(key))
            } else {
                (entry_base, ElementKey::Index(i as u32))
            };
            let mut elem_path = prefix_path;
            match key {
                ElementKey::Index(idx) => elem_path.push_index(idx),
                ElementKey::Key(k) => elem_path.push(k),
            }
            let child = WalkCtx {
                base: member_base,
                path: elem_path,
                element: Some(key),
                container: Some(dynm.name),
                frame,
                timestamp,
                depth: ctx.depth + 1,
            };
            for mi in members_start..members_end {
                let member = self.fields.as_slice().get(mi).ok_or(Error::OpRefNotFound)?;
                self.walk_field(member, child, Some(table), f)?;
            }
        }
        Ok(())
    }

    /// Evaluates each field in the VTable, returning an iterator of [`RealizedField`]s.
    ///
    /// Dynamic fields expand into many realized components, so this collects through
    /// [`VTable::for_each_field`]; it is therefore `alloc`-gated. No-alloc callers
    /// should use [`VTable::for_each_field`] directly.
    #[cfg(feature = "alloc")]
    pub fn realize_fields<'a>(
        &'a self,
        table: Option<&'a [u8]>,
    ) -> impl Iterator<Item = Result<RealizedField<'a>, Error>> + 'a {
        let mut out: alloc::vec::Vec<Result<RealizedField<'a>, Error>> = alloc::vec::Vec::new();
        let res = self.for_each_field(table, &mut |rf| {
            out.push(Ok(rf));
            Ok(())
        });
        if let Err(err) = res {
            out.push(Err(err));
        }
        out.into_iter()
    }

    /// Parses the provided table and applies the values to the sink.
    ///
    /// This evaluates each field in the VTable against the provided table data and
    /// applies the resulting values to the sink. Dynamic frames expand into one sink
    /// value per element-member.
    pub fn apply<D: Decomponentize>(
        &self,
        table: &[u8],
        sink: &mut D,
    ) -> Result<Result<(), D::Error>, Error> {
        let mut sink_err: Option<D::Error> = None;
        let res = self.for_each_field(Some(table), &mut |rf| {
            let view = rf.view.expect("table not found");
            match sink.apply_value(rf.component_id, view, rf.timestamp) {
                Ok(()) => Ok(()),
                Err(err) => {
                    sink_err = Some(err);
                    // Sentinel to stop iteration; unwrapped before propagating below.
                    Err(Error::InvalidOp)
                }
            }
        });
        if let Some(err) = sink_err {
            return Ok(Err(err));
        }
        res?;
        Ok(Ok(()))
    }
}

/// Reads an 8-byte dynamic-field slot `{ trailer_off: u32, byte_len: u32 }`.
fn read_slot(table: &[u8], offset: usize) -> Result<(usize, usize), Error> {
    let trailer_off = read_u32(table, offset)? as usize;
    let byte_len = read_u32(table, offset + 4)? as usize;
    Ok((trailer_off, byte_len))
}

/// Reads a little-endian `u32` at `offset`.
fn read_u32(table: &[u8], offset: usize) -> Result<u32, Error> {
    let bytes = table
        .get(offset..offset + 4)
        .ok_or(Error::BufferUnderflow)?;
    Ok(u32::from_le_bytes(bytes.try_into().expect("slice is 4 bytes")))
}

#[cfg(feature = "alloc")]
/// Tools for building VTables programmatically
pub mod builder {
    use alloc::collections::BTreeMap;
    use alloc::sync::Arc;
    use alloc::vec::Vec;
    use zerocopy::Immutable;

    use super::*;
    /// A builder for VTable operations
    pub enum OpBuilder {
        Data {
            align: usize,
            data: Vec<u8>,
        },
        Table {
            offset: Offset,
            len: u32,
        },
        Component {
            component_id: Arc<OpBuilder>,
        },
        Schema {
            ty: Arc<OpBuilder>,
            dim: Arc<OpBuilder>,
            arg: Arc<OpBuilder>,
        },
        Timestamp {
            timestamp: Arc<OpBuilder>,
            arg: Arc<OpBuilder>,
        },
        Ext {
            id: PacketId,
            data: Arc<OpBuilder>,
            arg: Arc<OpBuilder>,
        },
        Frame {
            component_id: Arc<OpBuilder>,
            arg: Arc<OpBuilder>,
        },
        List {
            name: Arc<OpBuilder>,
            members: Vec<FieldBuilder>,
            stride: u32,
        },
        Map {
            name: Arc<OpBuilder>,
            members: Vec<FieldBuilder>,
            stride: u32,
            value_offset: u32,
        },
        PathComponent {
            name: Arc<OpBuilder>,
        },
    }

    /// A builder for VTable fields
    #[derive(Clone)]
    pub struct FieldBuilder {
        offset: Offset,
        len: u32,
        arg: Arc<OpBuilder>,
    }

    impl FieldBuilder {
        pub fn offset_by(self, offset: impl Into<Offset>) -> Self {
            let offset = offset.into();
            let arg = offset_table_ops(&self.arg, offset).unwrap_or(self.arg);
            Self {
                offset: Offset(self.offset.0 + offset.0),
                len: self.len,
                arg,
            }
        }

        /// Wraps this field's op chain in a timestamp op, so the field's value is
        /// recorded with the timestamp read from `timestamp` (usually a [`raw_table`]
        /// op pointing at a [`crate::types::Timestamp`] in the table)
        pub fn with_timestamp(self, timestamp: Arc<OpBuilder>) -> Self {
            Self {
                arg: super::builder::timestamp(timestamp, self.arg),
                ..self
            }
        }

        /// Wraps this field's op chain in a [`frame`] op, tagging every component
        /// realized from this field as belonging to the frame named by
        /// `component_id`. Mirrors [`with_timestamp`](FieldBuilder::with_timestamp);
        /// the shared `frame(...)` `Arc` is deduplicated by [`VTableBuilder`] so the
        /// frame id is stored once even when every field carries it.
        pub fn with_frame(self, component_id: impl Into<ComponentId>) -> Self {
            Self {
                arg: super::builder::frame(component_id, self.arg),
                ..self
            }
        }
    }

    /// Shifts every `Table` op in the chain by `offset`, returning `None` if the chain
    /// contains no `Table` ops
    ///
    /// Table ops inside a field's op chain (e.g. a timestamp source) are relative to the
    /// same struct as the field itself, so they shift together when the field is offset.
    /// Untouched subtrees keep their `Arc` identity so op deduplication still applies.
    fn offset_table_ops(op: &Arc<OpBuilder>, offset: Offset) -> Option<Arc<OpBuilder>> {
        match op.as_ref() {
            OpBuilder::Data { .. } => None,
            OpBuilder::Table { offset: o, len } => Some(Arc::new(OpBuilder::Table {
                offset: Offset(o.0 + offset.0),
                len: *len,
            })),
            OpBuilder::Component { component_id } => offset_table_ops(component_id, offset)
                .map(|component_id| Arc::new(OpBuilder::Component { component_id })),
            OpBuilder::Schema { ty, dim, arg } => {
                let new_ty = offset_table_ops(ty, offset);
                let new_dim = offset_table_ops(dim, offset);
                let new_arg = offset_table_ops(arg, offset);
                if new_ty.is_none() && new_dim.is_none() && new_arg.is_none() {
                    return None;
                }
                Some(Arc::new(OpBuilder::Schema {
                    ty: new_ty.unwrap_or_else(|| ty.clone()),
                    dim: new_dim.unwrap_or_else(|| dim.clone()),
                    arg: new_arg.unwrap_or_else(|| arg.clone()),
                }))
            }
            OpBuilder::Timestamp { timestamp, arg } => {
                let new_timestamp = offset_table_ops(timestamp, offset);
                let new_arg = offset_table_ops(arg, offset);
                if new_timestamp.is_none() && new_arg.is_none() {
                    return None;
                }
                Some(Arc::new(OpBuilder::Timestamp {
                    timestamp: new_timestamp.unwrap_or_else(|| timestamp.clone()),
                    arg: new_arg.unwrap_or_else(|| arg.clone()),
                }))
            }
            OpBuilder::Ext { id, data, arg } => {
                let new_data = offset_table_ops(data, offset);
                let new_arg = offset_table_ops(arg, offset);
                if new_data.is_none() && new_arg.is_none() {
                    return None;
                }
                Some(Arc::new(OpBuilder::Ext {
                    id: *id,
                    data: new_data.unwrap_or_else(|| data.clone()),
                    arg: new_arg.unwrap_or_else(|| arg.clone()),
                }))
            }
            OpBuilder::Frame { component_id, arg } => {
                // `component_id` is a Data op (no table ops); only `arg` can shift.
                offset_table_ops(arg, offset).map(|arg| {
                    Arc::new(OpBuilder::Frame {
                        component_id: component_id.clone(),
                        arg,
                    })
                })
            }
            // Dynamic terminals carry no shiftable table ops: `name` is a Data op and the
            // member templates' offsets are relative to the element base, not the table.
            OpBuilder::List { .. } | OpBuilder::Map { .. } | OpBuilder::PathComponent { .. } => {
                None
            }
        }
    }

    /// Creates a data operation builder from the provided data
    pub fn data<T: IntoBytes + Immutable + ?Sized>(data: &T) -> Arc<OpBuilder> {
        let align = core::mem::align_of_val(data);
        Arc::new(OpBuilder::Data {
            align,
            data: data.as_bytes().to_vec(),
        })
    }

    /// Creates a table operation builder with the specified offset and length
    pub fn raw_table(offset: impl Into<Offset>, len: u32) -> Arc<OpBuilder> {
        Arc::new(OpBuilder::Table {
            offset: offset.into(),
            len,
        })
    }

    /// Creates a component operation builder from a component ID
    pub fn component(component_id: impl Into<ComponentId>) -> Arc<OpBuilder> {
        let component_id = component_id.into();
        let component_id = data(&component_id);
        Arc::new(OpBuilder::Component { component_id })
    }

    /// Creates a schema operation builder from a primitive type, dimensions, and an argument
    pub fn schema(ty: PrimType, dim: &[u64], arg: Arc<OpBuilder>) -> Arc<OpBuilder> {
        let ty = data(&ty);
        let dim = data(dim);
        Arc::new(OpBuilder::Schema { ty, dim, arg })
    }

    /// Creates a timestamp operation builder from a timestamp source and an argument
    pub fn timestamp(timestamp: Arc<OpBuilder>, arg: Arc<OpBuilder>) -> Arc<OpBuilder> {
        Arc::new(OpBuilder::Timestamp { timestamp, arg })
    }

    /// Creates an extension operation builder from a message and an argument
    ///
    ///  Extensions are used to attache extra metadata to a field.
    ///  For instance metor-db uses extensions to specify the rate you want to receive a field at.
    pub fn ext<D: crate::types::Msg>(data: D, arg: Arc<OpBuilder>) -> Arc<OpBuilder> {
        let data = postcard::to_allocvec(&data).expect("data serialize failed");
        let data = Arc::new(OpBuilder::Data { align: 1, data });
        Arc::new(OpBuilder::Ext {
            id: D::ID,
            data,
            arg,
        })
    }

    /// Creates a UTF-8 name op in the data side-table (align 1).
    ///
    /// Used for dynamic-frame path prefixes ([`list`]/[`map`]) and member names
    /// ([`path_component`]).
    pub fn name(s: &str) -> Arc<OpBuilder> {
        Arc::new(OpBuilder::Data {
            align: 1,
            data: s.as_bytes().to_vec(),
        })
    }

    /// Creates a frame-identity op (see [`Op::Frame`]) wrapping `arg`.
    pub fn frame(component_id: impl Into<ComponentId>, arg: Arc<OpBuilder>) -> Arc<OpBuilder> {
        let component_id = component_id.into();
        let component_id = data(&component_id);
        Arc::new(OpBuilder::Frame { component_id, arg })
    }

    /// Creates a dynamic-leaf terminal (see [`Op::PathComponent`]) naming `member_name`.
    ///
    /// The final component id is the dotted path accumulated by the enclosing
    /// [`list`]/[`map`] (`<prefix>.<key>.<member_name>`).
    pub fn path_component(member_name: &str) -> Arc<OpBuilder> {
        Arc::new(OpBuilder::PathComponent {
            name: name(member_name),
        })
    }

    /// Creates a dynamic list terminal (see [`Op::List`]).
    ///
    /// `prefix` is the field's path prefix (e.g. `"processes"`); `members` describe one
    /// element with offsets relative to the element base; `stride` is the element size.
    pub fn list(
        prefix: &str,
        members: impl IntoIterator<Item = FieldBuilder>,
        stride: u32,
    ) -> Arc<OpBuilder> {
        Arc::new(OpBuilder::List {
            name: name(prefix),
            members: members.into_iter().collect(),
            stride,
        })
    }

    /// Creates a dynamic map terminal (see [`Op::Map`]).
    ///
    /// Entries are `{ key_off: u32, key_len: u32, <pad> value }`; `value_offset` locates
    /// the value sub-frame within an entry. Member template offsets are relative to the
    /// value sub-frame base.
    pub fn map(
        prefix: &str,
        members: impl IntoIterator<Item = FieldBuilder>,
        stride: u32,
        value_offset: u32,
    ) -> Arc<OpBuilder> {
        Arc::new(OpBuilder::Map {
            name: name(prefix),
            members: members.into_iter().collect(),
            stride,
            value_offset,
        })
    }

    /// Creates a field builder with the specified offset, length, and argument
    pub fn raw_field(offset: impl Into<Offset>, len: u32, arg: Arc<OpBuilder>) -> FieldBuilder {
        FieldBuilder {
            offset: offset.into(),
            len,
            arg,
        }
    }

    #[macro_export]
    /// Creates a `Op::Table` from the specified struct field
    macro_rules! table {
        ($t:tt::$field:ident $(,)?) => {{
            let offset = core::mem::offset_of!($t, $field);
            let size = $crate::vtable::builder::field_size!($t, $field) as u32;
            raw_table(offset as u32, size)
        }};
    }

    pub use table;

    /// Creates a [`Field`] from the specified struct field, and ops
    ///
    /// # Usage
    /// ```rust
    /// use metor_proto::vtable::builder::*;
    /// struct Foo { bar: [f64; 3] }
    /// vtable([field!(Foo::bar, component("bar"))]);
    /// ```
    #[macro_export]
    macro_rules! field {
        ($t:tt::$field:ident, $arg: expr $(,)?) => {{
            let offset = core::mem::offset_of!($t, $field);
            let size = $crate::vtable::builder::field_size!($t, $field) as u32;
            $crate::vtable::builder::raw_field(offset as u32, size, $arg)
        }};
    }

    pub use field;

    /// Returns the size of a field of a struct
    ///
    /// # Usage
    /// ```rust
    /// struct Foo { bar: [f64; 3]}
    /// assert_eq!(metor_proto::vtable::builder::field_size!(Foo, bar), 3 * 8)
    /// ```
    ///
    /// source: <https://stackoverflow.com/a/70222282>
    #[macro_export]
    macro_rules! field_size {
        ($t:ident, $field:ident) => {{
            let m = core::mem::MaybeUninit::<$t>::uninit();
            // According to https://doc.rust-lang.org/stable/std/ptr/macro.addr_of_mut.html#examples,
            // you can dereference an uninitialized MaybeUninit pointer in addr_of!
            // Raw pointer deref in const contexts is stabilized in 1.58:
            // https://github.com/rust-lang/rust/pull/89551
            let p = unsafe {
                core::ptr::addr_of!((*(&m as *const _ as *const $t)).$field)
            };

            const fn size_of_raw<T>(_: *const T) -> usize {
                core::mem::size_of::<T>()
            }
            size_of_raw(p)
        }};
    }

    pub use field_size;

    /// A builder for constructing VTables.
    ///
    /// For most cases you should use [`vtable`] instead
    #[derive(Default)]
    pub struct VTableBuilder {
        vtable: VTable<Vec<Op>, Vec<u8>, Vec<Field>>,
        visited: BTreeMap<usize, OpRef>,
        // Visited ops are deduplicated by address, so they must be kept alive for the
        // builder's lifetime - otherwise a later op allocated at a freed address would
        // alias a stale `OpRef`
        retained: Vec<Arc<OpBuilder>>,
    }

    impl VTableBuilder {
        /// Visits an operation builder, adding it to the VTable and returning its `OpRef`
        pub fn visit(&mut self, op: &Arc<OpBuilder>) -> OpRef {
            let id = Arc::as_ptr(op) as usize;
            if let Some(r) = self.visited.get(&id) {
                return *r;
            }
            self.retained.push(op.clone());
            let op = match op.as_ref() {
                OpBuilder::Data { align, data } => {
                    let len = data.len() as u32;
                    let padding = (align - (self.vtable.data.len() % align)) % align;

                    for _ in 0..padding {
                        self.vtable.data.push(0);
                    }
                    let offset = Offset(self.vtable.data.len() as u32);
                    self.vtable.data.extend_from_slice(data);
                    Op::Data { offset, len }
                }
                OpBuilder::Table { offset, len } => Op::Table {
                    offset: *offset,
                    len: *len,
                },
                OpBuilder::Component { component_id } => {
                    let component_id = self.visit(component_id);
                    Op::Component { component_id }
                }
                OpBuilder::Schema { ty, dim, arg } => {
                    let ty = self.visit(ty);
                    let dim = self.visit(dim);
                    let arg = self.visit(arg);
                    Op::Schema { ty, dim, arg }
                }
                OpBuilder::Timestamp { timestamp, arg } => {
                    let source = self.visit(timestamp);
                    let arg = self.visit(arg);
                    Op::Timestamp { source, arg }
                }
                OpBuilder::Ext { id, data, arg } => {
                    let arg = self.visit(arg);
                    let data = self.visit(data);
                    Op::Ext { id: *id, data, arg }
                }
                OpBuilder::Frame { component_id, arg } => {
                    let component_id = self.visit(component_id);
                    let arg = self.visit(arg);
                    Op::Frame { component_id, arg }
                }
                OpBuilder::PathComponent { name } => {
                    let name = self.visit(name);
                    Op::PathComponent { name }
                }
                OpBuilder::List {
                    name,
                    members,
                    stride,
                } => {
                    let name = self.visit(name);
                    let members = self.visit_members(members);
                    Op::List {
                        name,
                        members,
                        stride: *stride,
                    }
                }
                OpBuilder::Map {
                    name,
                    members,
                    stride,
                    value_offset,
                } => {
                    let name = self.visit(name);
                    let members = self.visit_members(members);
                    Op::Map {
                        name,
                        members,
                        stride: *stride,
                        value_offset: *value_offset,
                    }
                }
            };
            let op_ref = OpRef(self.vtable.ops.len() as u32);
            self.vtable.ops.push(op);
            self.visited.insert(id, op_ref);
            op_ref
        }

        /// Appends a dynamic element's member-template fields to the VTable as a
        /// contiguous block, returning the [`ElementFields`] range that names them.
        ///
        /// Each member's op chain is visited first (which may append *nested* template
        /// fields earlier in the list), then the direct member [`Field`]s are pushed
        /// together so this op's range stays contiguous.
        fn visit_members(&mut self, members: &[FieldBuilder]) -> ElementFields {
            let resolved: Vec<(Offset, u32, OpRef)> = members
                .iter()
                .map(|m| (m.offset, m.len, self.visit(&m.arg)))
                .collect();
            let start = self.vtable.fields.len() as u32;
            let count = resolved.len() as u32;
            for (offset, len, arg) in resolved {
                self.vtable.fields.push(Field { offset, len, arg });
            }
            ElementFields { start, count }
        }
    }

    /// Creates a VTable from the provided field builders
    pub fn vtable(
        fields: impl IntoIterator<Item = FieldBuilder>,
    ) -> VTable<Vec<Op>, Vec<u8>, Vec<Field>> {
        let mut builder = VTableBuilder::default();
        for field in fields.into_iter() {
            let field = Field {
                offset: field.offset,
                len: field.len,
                arg: builder.visit(&field.arg),
            };
            builder.vtable.fields.push(field);
        }
        builder.vtable
    }
}

#[cfg(test)]
mod tests {
    use core::convert::Infallible;
    use nox::array::ArrayViewExt;
    use std::collections::HashMap;

    use nox::{Array, ArrayBuf, Dyn};
    use zerocopy::{Immutable, IntoBytes};

    use crate::{
        com_de::Decomponentize,
        types::{ComponentId, ComponentView, PrimType, Timestamp},
    };

    #[derive(Default)]
    struct TestSink {
        timestamp: Option<Timestamp>,
        f32_components: HashMap<ComponentId, Array<f32, Dyn>>,
        f64_components: HashMap<ComponentId, Array<f64, Dyn>>,
    }

    impl Decomponentize for TestSink {
        type Error = Infallible;
        fn apply_value(
            &mut self,
            component_id: ComponentId,
            value: ComponentView<'_>,
            timestamp: Option<Timestamp>,
        ) -> Result<(), Self::Error> {
            if let Some(timestamp) = timestamp {
                self.timestamp = Some(timestamp);
            }
            match value {
                ComponentView::F32(view) => {
                    self.f32_components
                        .insert(component_id, view.to_dyn_owned());
                }

                ComponentView::F64(view) => {
                    self.f64_components
                        .insert(component_id, view.to_dyn_owned());
                }
                _ => todo!(),
            }
            Ok(())
        }
    }

    #[test]
    fn test_basic_builder() {
        use super::builder::*;

        #[derive(IntoBytes, Immutable)]
        struct Foo {
            timestamp: Timestamp,
            test: [f32; 4],
            bar: f64,
        }

        let time = table!(Foo::timestamp);
        let v = vtable([
            field!(
                Foo::test,
                schema(
                    PrimType::F32,
                    &[4],
                    timestamp(time.clone(), component("test")),
                ),
            ),
            field!(
                Foo::bar,
                schema(PrimType::F64, &[], timestamp(time, component("bar")))
            ),
        ]);

        let foo = Foo {
            timestamp: Timestamp(1000),
            test: [1.0, 2.0, 3.0, 4.0],
            bar: 5.0,
        };
        let mut sink = TestSink::default();
        v.apply(foo.as_bytes(), &mut sink).unwrap().unwrap();
        let test = sink.f32_components.get(&ComponentId::new("test")).unwrap();
        assert_eq!(test.buf.as_buf(), &[1.0, 2.0, 3.0, 4.0]);

        let bar = sink.f64_components.get(&ComponentId::new("bar")).unwrap();
        assert_eq!(bar.buf.as_buf(), &[5.0]);
        assert_eq!(sink.timestamp, Some(foo.timestamp));
    }

    #[test]
    fn test_field_with_timestamp_offset() {
        use super::builder::*;

        #[derive(IntoBytes, Immutable)]
        struct Inner {
            timestamp: Timestamp,
            value: f64,
        }

        #[derive(IntoBytes, Immutable)]
        struct Outer {
            _pad: u64,
            inner: Inner,
        }

        // Build the field as if for `Inner`, then offset it into `Outer`; the
        // timestamp table op embedded in the chain must shift along with the field.
        let time = table!(Inner::timestamp);
        let field = field!(Inner::value, schema(PrimType::F64, &[], component("value")))
            .with_timestamp(time)
            .offset_by(core::mem::offset_of!(Outer, inner) as u32);
        let v = vtable([field]);

        let outer = Outer {
            _pad: 0,
            inner: Inner {
                timestamp: Timestamp(4242),
                value: 7.0,
            },
        };
        let mut sink = TestSink::default();
        v.apply(outer.as_bytes(), &mut sink).unwrap().unwrap();
        let value = sink.f64_components.get(&ComponentId::new("value")).unwrap();
        assert_eq!(value.buf.as_buf(), &[7.0]);
        assert_eq!(sink.timestamp, Some(Timestamp(4242)));
    }

    // ---- dynamic frame tests (vtable-dynamic.md §5) ----

    use super::builder::*;
    use super::{Error, RealizedField};
    use std::collections::HashMap as Map;

    /// Records every scalar component it sees as an `f64`, plus its timestamp.
    #[derive(Default)]
    struct DynSink {
        values: Map<ComponentId, f64>,
        timestamps: Map<ComponentId, Option<Timestamp>>,
    }

    impl Decomponentize for DynSink {
        type Error = Infallible;
        fn apply_value(
            &mut self,
            component_id: ComponentId,
            value: ComponentView<'_>,
            timestamp: Option<Timestamp>,
        ) -> Result<(), Self::Error> {
            self.values.insert(component_id, value.to_f64());
            self.timestamps.insert(component_id, timestamp);
            Ok(())
        }
    }

    fn put_u32(buf: &mut Vec<u8>, v: u32) {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    fn put_u64(buf: &mut Vec<u8>, v: u64) {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    fn put_f64(buf: &mut Vec<u8>, v: f64) {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    fn put_i64(buf: &mut Vec<u8>, v: i64) {
        buf.extend_from_slice(&v.to_le_bytes());
    }

    fn process_members() -> Vec<super::builder::FieldBuilder> {
        vec![
            raw_field(0, 8, schema(PrimType::U64, &[], path_component("pid"))),
            raw_field(8, 8, schema(PrimType::F64, &[], path_component("cpu_usage"))),
        ]
    }

    /// §5a — `FrameList<Process>` with 2 elements.
    #[test]
    fn test_dynamic_list() {
        // top-level `processes` field: offset 8 is the slot, len 8; the list inherits
        // the frame timestamp at offset 0.
        let v = vtable([raw_field(
            8,
            8,
            timestamp(raw_table(0, 8), list("processes", process_members(), 16)),
        )]);

        let mut t = Vec::new();
        put_i64(&mut t, 1000); // timestamp @0
        put_u32(&mut t, 16); // slot.trailer_off @8
        put_u32(&mut t, 32); // slot.byte_len @12
        put_u64(&mut t, 1001); // [0].pid @16
        put_f64(&mut t, 0.5); // [0].cpu @24
        put_u64(&mut t, 1002); // [1].pid @32
        put_f64(&mut t, 0.25); // [1].cpu @40
        assert_eq!(t.len(), 48);

        let mut sink = DynSink::default();
        v.apply(&t, &mut sink).unwrap().unwrap();

        assert_eq!(sink.values[&ComponentId::new("processes.0.pid")], 1001.0);
        assert_eq!(sink.values[&ComponentId::new("processes.0.cpu_usage")], 0.5);
        assert_eq!(sink.values[&ComponentId::new("processes.1.pid")], 1002.0);
        assert_eq!(sink.values[&ComponentId::new("processes.1.cpu_usage")], 0.25);
        // every element-member inherits the shared frame timestamp
        for id in ["processes.0.pid", "processes.0.cpu_usage", "processes.1.pid"] {
            assert_eq!(sink.timestamps[&ComponentId::new(id)], Some(Timestamp(1000)));
        }
    }

    /// §5b — `FrameMap<Name, Process>` with 2 entries; keys live in the trailer name pool.
    #[test]
    fn test_dynamic_map() {
        let v = vtable([raw_field(
            8,
            8,
            timestamp(raw_table(0, 8), map("processes", process_members(), 24, 8)),
        )]);

        let mut t = Vec::new();
        put_i64(&mut t, 1000); // timestamp @0
        put_u32(&mut t, 16); // slot.trailer_off @8 (entry array)
        put_u32(&mut t, 48); // slot.byte_len @12
        // entry[0] @16
        put_u32(&mut t, 64); // key_off -> "htop"
        put_u32(&mut t, 4); // key_len
        put_u64(&mut t, 1001); // value.pid @24
        put_f64(&mut t, 0.5); // value.cpu @32
        // entry[1] @40
        put_u32(&mut t, 68); // key_off -> "init"
        put_u32(&mut t, 4); // key_len
        put_u64(&mut t, 1002); // value.pid @48
        put_f64(&mut t, 0.25); // value.cpu @56
        // name pool @64
        t.extend_from_slice(b"htop");
        t.extend_from_slice(b"init");
        assert_eq!(t.len(), 72);

        let mut sink = DynSink::default();
        v.apply(&t, &mut sink).unwrap().unwrap();

        assert_eq!(sink.values[&ComponentId::new("processes.htop.pid")], 1001.0);
        assert_eq!(sink.values[&ComponentId::new("processes.htop.cpu_usage")], 0.5);
        assert_eq!(sink.values[&ComponentId::new("processes.init.pid")], 1002.0);
        assert_eq!(sink.values[&ComponentId::new("processes.init.cpu_usage")], 0.25);
        assert_eq!(
            sink.timestamps[&ComponentId::new("processes.htop.pid")],
            Some(Timestamp(1000))
        );
    }

    /// §5b variant — a map key containing `.` is rejected.
    #[test]
    fn test_dynamic_map_dot_key_rejected() {
        let v = vtable([raw_field(
            8,
            8,
            map("processes", process_members(), 24, 8),
        )]);

        let mut t = Vec::new();
        put_i64(&mut t, 0); // timestamp @0 (unused)
        put_u32(&mut t, 16); // slot.trailer_off
        put_u32(&mut t, 24); // slot.byte_len (one entry)
        put_u32(&mut t, 40); // key_off -> "a.b"
        put_u32(&mut t, 3); // key_len
        put_u64(&mut t, 1001);
        put_f64(&mut t, 0.5);
        t.extend_from_slice(b"a.b"); // @40
        assert_eq!(t.len(), 43);

        let mut sink = DynSink::default();
        let err = v.apply(&t, &mut sink).unwrap_err();
        assert!(matches!(err, Error::InvalidComponentData));
    }

    /// §5c — nested `FrameMap<Name, Host>` where `Host { threads: FrameList<Thread> }`.
    #[test]
    fn test_dynamic_nested() {
        // Host has a single member, the inner thread list, whose slot is at value
        // offset 0; Thread has a single `state: u8` member.
        let thread_members = || vec![raw_field(0, 1, schema(PrimType::U8, &[], path_component("state")))];
        let host_members = || vec![raw_field(0, 8, list("threads", thread_members(), 1))];

        let v = vtable([raw_field(
            8,
            8,
            timestamp(raw_table(0, 8), map("processes", host_members(), 16, 8)),
        )]);

        let mut t = Vec::new();
        put_i64(&mut t, 1000); // timestamp @0
        put_u32(&mut t, 16); // slot_outer.trailer_off @8
        put_u32(&mut t, 16); // slot_outer.byte_len @12 (one 16-byte entry)
        // entry[0] @16
        put_u32(&mut t, 40); // key_off -> "htop"
        put_u32(&mut t, 4); // key_len
        // Host value @24 (entry 16 + value_offset 8): inner list slot
        put_u32(&mut t, 44); // slot_inner.trailer_off
        put_u32(&mut t, 1); // slot_inner.byte_len (one 1-byte thread)
        // pad 32..40 so the name pool / trailer line up with the offsets above
        while t.len() < 40 {
            t.push(0);
        }
        t.extend_from_slice(b"htop"); // @40
        t.push(2); // thread[0].state @44
        assert_eq!(t.len(), 45);

        let mut sink = DynSink::default();
        v.apply(&t, &mut sink).unwrap().unwrap();

        assert_eq!(
            sink.values[&ComponentId::new("processes.htop.threads.0.state")],
            2.0
        );
        assert_eq!(
            sink.timestamps[&ComponentId::new("processes.htop.threads.0.state")],
            Some(Timestamp(1000))
        );
    }

    /// Schema/registration mode (`table = None`) surfaces each member template's ty/shape.
    #[test]
    fn test_dynamic_registration_mode() {
        let v = vtable([raw_field(8, 8, list("processes", process_members(), 16))]);

        let fields: Vec<_> = v.realize_fields(None).map(|r| r.unwrap()).collect();
        // two member templates, emitted once, with no view
        assert_eq!(fields.len(), 2);
        let by_id: Map<ComponentId, &RealizedField> =
            fields.iter().map(|f| (f.component_id, f)).collect();
        let pid = by_id[&ComponentId::new("processes.pid")];
        assert_eq!(pid.ty, PrimType::U64);
        assert!(pid.shape.is_empty());
        assert!(pid.view.is_none());
        let cpu = by_id[&ComponentId::new("processes.cpu_usage")];
        assert_eq!(cpu.ty, PrimType::F64);
    }
}
