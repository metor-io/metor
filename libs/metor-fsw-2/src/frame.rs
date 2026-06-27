//! The [`Frame`] trait — a thin marker bundling the four component traits plus the
//! frame identity and shared-timestamp accessor (frames.md §1.2).

use metor_fsw::{AsVTable, Componentize, Decomponentize, Metadatatize};
use metor_proto::types::{ComponentId, Timestamp};

/// A frame: a `#[repr(C)]` struct whose fields are components sharing one logical
/// timestamp, named by a [`ComponentId`].
///
/// `Frame` does **not** replace [`AsVTable`]/[`Metadatatize`]/[`Componentize`]/
/// [`Decomponentize`] — it bundles them and pins the frame name/id plus the shared
/// timestamp. Derive it with `#[derive(Frame)]`, which expands to all four
/// sub-derives + this impl.
pub trait Frame: AsVTable + Metadatatize + Componentize + Decomponentize {
    /// Frame name; the dotted prefix all member components hang off (and the
    /// `Op::Frame` tag id). Empty means "no prefix" (components sit at the root).
    const NAME: &'static str;
    /// fnv1a-64 of [`NAME`](Frame::NAME), top-bit masked — the same construction as
    /// every [`ComponentId`].
    const FRAME_ID: ComponentId = ComponentId::new(Self::NAME);
    /// The shared timestamp marked with `#[metor_fsw(timestamp)]`.
    fn timestamp(&self) -> Timestamp;
}
