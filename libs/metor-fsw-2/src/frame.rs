//! The [`Frame`] trait, implemented via `#[derive(Frame)]`.

use metor_fsw::{AsVTable, Componentize, Decomponentize, Metadatatize};
use metor_proto::types::{ComponentId, Timestamp};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// A `#[repr(C)]` struct whose fields are components sharing one logical
/// timestamp.
///
/// `Frame` bundles the four component traits and adds the frame's name, its
/// [`ComponentId`], and an accessor for the shared timestamp. Implement it
/// with `#[derive(Frame)]`, which derives the component traits as well.
///
/// The zerocopy traits are supertraits because a frame's `#[repr(C)]` bytes
/// are its wire bytes. Requiring them here turns a forgotten zerocopy derive
/// into an error on the frame definition rather than in whichever consumer
/// first moves the frame through a port. `IntoBytes` also rejects implicit
/// padding at the definition site; pad explicitly with `_pad` arrays where
/// the layout needs it.
pub trait Frame:
    AsVTable
    + Metadatatize
    + Componentize
    + Decomponentize
    + IntoBytes
    + FromBytes
    + KnownLayout
    + Immutable
{
    /// The frame's name, used as the dotted prefix of every member
    /// component's path. Empty means no prefix, so components sit at the
    /// root.
    const NAME: &'static str;
    /// Identifier hashed from [`NAME`](Frame::NAME), by the same
    /// construction as every other [`ComponentId`].
    const FRAME_ID: ComponentId = ComponentId::new(Self::NAME);
    /// The shared timestamp, read from the field marked
    /// `#[metor_fsw(timestamp)]`.
    fn timestamp(&self) -> Timestamp;
}
