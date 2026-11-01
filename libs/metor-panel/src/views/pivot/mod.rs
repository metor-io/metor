//! Pivot grids: alike subtrees read as one table of instances × fields.
//!
//! The rotation lives here, apart from any one view, so every surface that
//! tabulates frames — the outline's inline pivots today — builds the same
//! [`Grid`] and honours the same [`Arrangement`].

pub(crate) mod model;

pub(crate) use model::{Arrangement, Grid, alike, children, common_suffix, signature, take_slot};
