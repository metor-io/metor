//! Per-`PrimType` element access shared by the program runtime and the views
//! that read raw component bytes.
//!
//! Values are read as `f64`, which loses precision for `u64`/`i64` above 2^53;
//! that is plenty for visualization-rate plumbing.

use metor_proto::types::{ElementValue, PrimType};

/// Typed scalar — the boundary type when an inspector row edits one element
/// of a component in place. Mirrors `PrimType`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TypedScalar {
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    I8(i8),
    I16(i16),
    I32(i32),
    I64(i64),
    Bool(bool),
    F32(f32),
    F64(f64),
}

impl TypedScalar {
    pub fn dtype(self) -> PrimType {
        match self {
            Self::U8(_) => PrimType::U8,
            Self::U16(_) => PrimType::U16,
            Self::U32(_) => PrimType::U32,
            Self::U64(_) => PrimType::U64,
            Self::I8(_) => PrimType::I8,
            Self::I16(_) => PrimType::I16,
            Self::I32(_) => PrimType::I32,
            Self::I64(_) => PrimType::I64,
            Self::Bool(_) => PrimType::Bool,
            Self::F32(_) => PrimType::F32,
            Self::F64(_) => PrimType::F64,
        }
    }

    /// Parse user input as `dtype` exactly, rejecting fractional or
    /// out-of-range text rather than casting it.
    pub fn parse(text: &str, dtype: PrimType) -> Option<Self> {
        let text = text.trim();
        Some(match dtype {
            PrimType::U8 => Self::U8(text.parse().ok()?),
            PrimType::U16 => Self::U16(text.parse().ok()?),
            PrimType::U32 => Self::U32(text.parse().ok()?),
            PrimType::U64 => Self::U64(text.parse().ok()?),
            PrimType::I8 => Self::I8(text.parse().ok()?),
            PrimType::I16 => Self::I16(text.parse().ok()?),
            PrimType::I32 => Self::I32(text.parse().ok()?),
            PrimType::I64 => Self::I64(text.parse().ok()?),
            PrimType::F32 => Self::F32(text.parse().ok()?),
            PrimType::F64 => Self::F64(text.parse().ok()?),
            PrimType::Bool => Self::Bool(match text {
                "true" | "1" => true,
                "false" | "0" => false,
                _ => return None,
            }),
        })
    }

    pub fn as_f64(self) -> f64 {
        match self {
            Self::U8(v) => v as f64,
            Self::U16(v) => v as f64,
            Self::U32(v) => v as f64,
            Self::U64(v) => v as f64,
            Self::I8(v) => v as f64,
            Self::I16(v) => v as f64,
            Self::I32(v) => v as f64,
            Self::I64(v) => v as f64,
            Self::Bool(v) => v as u8 as f64,
            Self::F32(v) => v as f64,
            Self::F64(v) => v,
        }
    }

    pub fn from_f64(v: f64, dtype: PrimType) -> Self {
        match dtype {
            PrimType::U8 => Self::U8(v as u8),
            PrimType::U16 => Self::U16(v as u16),
            PrimType::U32 => Self::U32(v as u32),
            PrimType::U64 => Self::U64(v as u64),
            PrimType::I8 => Self::I8(v as i8),
            PrimType::I16 => Self::I16(v as i16),
            PrimType::I32 => Self::I32(v as i32),
            PrimType::I64 => Self::I64(v as i64),
            PrimType::Bool => Self::Bool(v != 0.0),
            PrimType::F32 => Self::F32(v as f32),
            PrimType::F64 => Self::F64(v),
        }
    }
}

impl From<TypedScalar> for ElementValue {
    fn from(value: TypedScalar) -> Self {
        match value {
            TypedScalar::U8(v) => Self::U8(v),
            TypedScalar::U16(v) => Self::U16(v),
            TypedScalar::U32(v) => Self::U32(v),
            TypedScalar::U64(v) => Self::U64(v),
            TypedScalar::I8(v) => Self::I8(v),
            TypedScalar::I16(v) => Self::I16(v),
            TypedScalar::I32(v) => Self::I32(v),
            TypedScalar::I64(v) => Self::I64(v),
            TypedScalar::Bool(v) => Self::Bool(v),
            TypedScalar::F32(v) => Self::F32(v),
            TypedScalar::F64(v) => Self::F64(v),
        }
    }
}

/// Read element `idx` of a row-major buffer of `dtype` as `f64`. Caller is
/// responsible for ensuring `idx` is in range.
pub fn read_f64_at(buf: &[u8], dtype: PrimType, idx: usize) -> f64 {
    let sz = dtype.size();
    let off = idx * sz;
    let chunk = &buf[off..off + sz];
    match dtype {
        PrimType::U8 => chunk[0] as f64,
        PrimType::U16 => u16::from_le_bytes(chunk.try_into().unwrap()) as f64,
        PrimType::U32 => u32::from_le_bytes(chunk.try_into().unwrap()) as f64,
        PrimType::U64 => u64::from_le_bytes(chunk.try_into().unwrap()) as f64,
        PrimType::I8 => i8::from_le_bytes(chunk.try_into().unwrap()) as f64,
        PrimType::I16 => i16::from_le_bytes(chunk.try_into().unwrap()) as f64,
        PrimType::I32 => i32::from_le_bytes(chunk.try_into().unwrap()) as f64,
        PrimType::I64 => i64::from_le_bytes(chunk.try_into().unwrap()) as f64,
        PrimType::Bool => (chunk[0] != 0) as u8 as f64,
        PrimType::F32 => f32::from_le_bytes(chunk.try_into().unwrap()) as f64,
        PrimType::F64 => f64::from_le_bytes(chunk.try_into().unwrap()),
    }
}

/// Cast `value` to `dtype` and append its little-endian bytes to `out`.
#[cfg(test)]
pub fn write_f64_as(out: &mut Vec<u8>, dtype: PrimType, value: f64) {
    match dtype {
        PrimType::U8 => out.push(value as u8),
        PrimType::U16 => out.extend_from_slice(&(value as u16).to_le_bytes()),
        PrimType::U32 => out.extend_from_slice(&(value as u32).to_le_bytes()),
        PrimType::U64 => out.extend_from_slice(&(value as u64).to_le_bytes()),
        PrimType::I8 => out.extend_from_slice(&(value as i8).to_le_bytes()),
        PrimType::I16 => out.extend_from_slice(&(value as i16).to_le_bytes()),
        PrimType::I32 => out.extend_from_slice(&(value as i32).to_le_bytes()),
        PrimType::I64 => out.extend_from_slice(&(value as i64).to_le_bytes()),
        PrimType::Bool => out.push((value != 0.0) as u8),
        PrimType::F32 => out.extend_from_slice(&(value as f32).to_le_bytes()),
        PrimType::F64 => out.extend_from_slice(&value.to_le_bytes()),
    }
}
