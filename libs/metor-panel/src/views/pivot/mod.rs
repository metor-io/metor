//! Pivot grids: alike subtrees read as one table of instances × fields.
//!
//! The rotation lives here, apart from any one view, so every surface that
//! tabulates frames — the outline's inline pivots and the standalone
//! [`PivotTable`] — builds the same [`Grid`] and honours the same
//! [`Arrangement`].

pub(crate) mod cell;
pub(crate) mod model;
pub(crate) mod source;
pub(crate) mod table;

pub(crate) use cell::{
    FieldDrag, InstanceDrag, field_cell, field_label, field_menu_rows, live_strip,
};
pub(crate) use model::{Arrangement, Frame, Grid, alike, children, common_suffix, signature};
pub use table::{PivotTable, PivotTableConfig};
