//! Layout details which are absent from telemetry vtables.

use std::mem::{align_of, size_of};

use serde::{Deserialize, Serialize};

use crate::dynamic::{entry_align, map_stride, map_value_offset};
use crate::{FrameList, FrameMap};

/// Native scalar encoding used by the frame's raw bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ByteOrder {
    Little,
    Big,
}

impl ByteOrder {
    pub const NATIVE: Self = if cfg!(target_endian = "little") {
        Self::Little
    } else {
        Self::Big
    };
}

/// A frame's complete fixed region, including timestamp and explicit padding.
/// Sizes use fixed-width integers so manifests are independent of host usize.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrameLayout {
    pub fixed_size: u64,
    pub alignment: u64,
    /// The shared `Timestamp` (signed i64 microseconds) in the fixed region.
    /// Its epoch is specified by the endpoint's clock domain.
    pub timestamp_offset: Option<u64>,
    pub fields: Vec<FieldLayout>,
}

/// One Rust field, in declaration order. Names/types for telemetered values
/// live in the vtable; field names used only for padding are not wire identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldLayout {
    pub offset: u64,
    pub size: u64,
    pub alignment: u64,
    /// A skipped byte array used as explicit padding. Other skipped fields must
    /// still have a represented schema before they can participate in an ICD.
    pub padding: bool,
    pub dynamic: Option<DynamicBounds>,
}

/// Bounds and trailer alignment for a directly contained list or map.
/// Nested dynamic containers are rejected until recursive bounds are exported.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum DynamicBounds {
    List {
        max_count: u64,
        stride: u64,
        alignment: u64,
    },
    Map {
        max_count: u64,
        max_key_bytes: u64,
        stride: u64,
        alignment: u64,
        value_offset: u64,
        value_size: u64,
    },
}

/// Supplies const-generic bounds to the Frame derive. These must not be
/// inferred from total record size: different bounds can have the same size.
#[doc(hidden)]
pub trait DynamicField {
    fn bounds() -> DynamicBounds;
}

impl<T, const MAX: usize> DynamicField for FrameList<T, MAX> {
    fn bounds() -> DynamicBounds {
        DynamicBounds::List {
            max_count: MAX as u64,
            stride: size_of::<T>() as u64,
            alignment: align_of::<T>() as u64,
        }
    }
}

impl<V, const MAX: usize, const MAX_KEY: usize> DynamicField for FrameMap<V, MAX, MAX_KEY> {
    fn bounds() -> DynamicBounds {
        DynamicBounds::Map {
            max_count: MAX as u64,
            max_key_bytes: MAX_KEY as u64,
            stride: map_stride::<V>() as u64,
            alignment: entry_align::<V>() as u64,
            value_offset: map_value_offset::<V>() as u64,
            value_size: size_of::<V>() as u64,
        }
    }
}
