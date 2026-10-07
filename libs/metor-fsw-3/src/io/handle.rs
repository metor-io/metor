use zerocopy::little_endian::U32;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

/// Names one op in a queue; its completion carries the same token.
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

/// An index into a handle table, plus the generation that issued it.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, IntoBytes, FromBytes, KnownLayout, Immutable, Unaligned,
)]
#[repr(C)]
pub struct RawHandle {
    index: U32,
    generation: U32,
}

impl RawHandle {
    /// `openat`'s current directory.
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
