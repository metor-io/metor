//! The program descriptor the compiler emits and the runtime reads.
//!
//! A descriptor lists every frame and every system type of one compiled
//! file, with the byte layout of each block the compiled code reads and
//! writes. Function entries are indices into the program's function table.

use serde::{Deserialize, Serialize};

/// The descriptor format this runtime reads.
pub const DESC_VERSION: u32 = 1;

/// Everything one compiled file defines.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProgramDesc {
    pub version: u32,
    pub frames: Vec<FrameDesc>,
    pub types: Vec<TypeDesc>,
}

/// A scalar type, as stored in a block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Prim {
    Bool,
    U8,
    U16,
    U32,
    U64,
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
}

impl Prim {
    /// The size in bytes, which is also the alignment.
    pub const fn size(self) -> u32 {
        match self {
            Prim::Bool | Prim::U8 | Prim::I8 => 1,
            Prim::U16 | Prim::I16 => 2,
            Prim::U32 | Prim::I32 | Prim::F32 => 4,
            Prim::U64 | Prim::I64 | Prim::F64 => 8,
        }
    }

    pub const fn is_float(self) -> bool {
        matches!(self, Prim::F32 | Prim::F64)
    }

    pub const fn is_signed(self) -> bool {
        matches!(self, Prim::I8 | Prim::I16 | Prim::I32 | Prim::I64)
    }
}

/// A `#[repr(C)]` frame: a record whose bytes are its value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FrameDesc {
    /// The record name, as `#[derive(Frame)]` would name the struct.
    pub name: String,
    pub size: u32,
    pub align: u32,
    /// The offset of the `i64` timestamp.
    pub timestamp: u32,
    /// Every scalar leaf but the timestamp, in field order.
    pub leaves: Vec<LeafDesc>,
}

/// One scalar inside a frame.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LeafDesc {
    /// The dotted path below the frame's root, such as `pos.0`.
    pub path: String,
    pub offset: u32,
    pub prim: Prim,
}

/// How the runtime steps a type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Cyclic,
    Sequence,
}

/// One system or sequence type.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TypeDesc {
    pub name: String,
    pub kind: Kind,
    pub doc: String,
    pub inputs: Vec<PortDesc>,
    pub outputs: Vec<PortDesc>,
    pub blocks: BlockSizes,
    pub params: Vec<ParamDesc>,
    pub entry: EntryDesc,
    /// The messages of the type's `fail` statements and timeouts, by index.
    pub fails: Vec<String>,
    /// The state offset of the `u32` index into `fails` a failure sets.
    pub fail_slot: u32,
    pub alerts: Vec<AlertDesc>,
    /// The index in `outputs` of the `alerts` output, if the type has alerts.
    pub alerts_port: Option<u32>,
    /// The worst-case instruction count of one call of `entry.step`.
    pub cost: u64,
}

/// The byte sizes of an instance's blocks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockSizes {
    pub inputs: u32,
    pub outputs: u32,
    pub state: u32,
    pub params: u32,
}

/// One port and where its frame lives in the port block.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PortDesc {
    pub name: String,
    /// The index into [`ProgramDesc::frames`].
    pub frame: u32,
    pub offset: u32,
}

/// One instance param.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ParamDesc {
    pub name: String,
    pub prim: Prim,
    pub offset: u32,
    pub default: serde_json::Value,
}

/// The compiled functions of one type, as indices into the function table.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EntryDesc {
    /// Sets the state block's initial values.
    pub init: u32,
    /// A system's `step`, or a sequence's `resume`.
    pub step: u32,
    pub redlines: Vec<RedlineDesc>,
    pub on_abort: Option<u32>,
}

/// One `redline "name": cond`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RedlineDesc {
    pub name: String,
    pub func: u32,
}

/// One `alert name severity: cond`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AlertDesc {
    pub name: String,
    pub severity: String,
}

impl ProgramDesc {
    /// Returns the type called `name`.
    pub fn ty(&self, name: &str) -> Option<&TypeDesc> {
        self.types.iter().find(|ty| ty.name == name)
    }

    /// Returns the frame a port carries.
    pub fn frame_of(&self, port: &PortDesc) -> Option<&FrameDesc> {
        self.frames.get(port.frame as usize)
    }
}

impl FrameDesc {
    /// Returns the leaf at `path`.
    pub fn leaf(&self, path: &str) -> Option<&LeafDesc> {
        self.leaves.iter().find(|leaf| leaf.path == path)
    }
}
