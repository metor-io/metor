use zerocopy::little_endian::U32;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

/// Identifies an op. Its completion carries the same token.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, IntoBytes, FromBytes, KnownLayout, Immutable, Unaligned,
)]
#[repr(transparent)]
pub struct Token(U32);

impl Token {
    pub const fn new(token: u32) -> Self {
        Self(U32::new(token))
    }

    pub fn get(self) -> u32 {
        self.0.get()
    }
}

/// A handle table index plus a generation, which detects stale handles.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, IntoBytes, FromBytes, KnownLayout, Immutable, Unaligned,
)]
#[repr(C)]
pub struct RawHandle {
    index: U32,
    generation: U32,
}

impl RawHandle {
    /// The process's current directory, for `Open`.
    pub const CWD: Self = Self::new(u32::MAX, 0);

    pub const fn new(index: u32, generation: u32) -> Self {
        Self {
            index: U32::new(index),
            generation: U32::new(generation),
        }
    }

    pub fn index(self) -> u32 {
        self.index.get()
    }

    pub fn generation(self) -> u32 {
        self.generation.get()
    }
}
