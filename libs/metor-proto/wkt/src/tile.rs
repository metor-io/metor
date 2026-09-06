//! Serialized form of a metor-panel tile layout.
//!
//! The tree mirrors the panel's split-pane system: a layout is a recursive
//! [`TileNode`] of splits and panes, where each pane holds tabbed
//! [`TileItem`]s. Item state is an opaque JSON blob owned by the panel that
//! produced it, so this schema stays independent of any concrete panel's
//! config shape. The panel uses this tree as its on-disk layout document;
//! preset messages carry the same document as a JSON string (the tree is
//! recursive, which a postcard schema cannot express).

use serde::{Deserialize, Serialize};

/// Current layout document version.
///
/// Bumped only on a structural break in the document. Field-level changes
/// rely on `#[serde(default)]` instead.
pub const TILE_LAYOUT_VERSION: u32 = 5;

/// A complete tile layout document.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TileLayout {
    /// Zero is never written by the panel — an absent version marks a
    /// document whose producer (a preset builder) delegates stamping to
    /// whoever publishes it.
    #[serde(default)]
    pub version: u32,
    /// App-wide default time window in the panel's time-range string grammar
    /// (e.g. `"LAST 30m"`); plots with an auto range follow it. Empty (and
    /// absent, in older layouts) means full range.
    #[serde(default)]
    pub global_time_range: String,
    /// Versioned display-time intent. Older readers use global_time_range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temporal: Option<TemporalLayout>,
    pub root: TileNode,
}

/// Time expressions are persisted as canonical text so preset producers need
/// no dependency on a panel-specific parser or runtime clock.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct TemporalLayout {
    pub version: u32,
    pub view_time: String,
    pub range_start: String,
    pub range_end: String,
    pub timezone: String,
    #[serde(default)]
    pub elapsed_display: bool,
    #[serde(default)]
    pub t0: Option<i64>,
    pub source_clock: Option<u64>,
    #[serde(default)]
    pub wall_clock: bool,
    pub scope_prefix: String,
    pub rate: f64,
    pub step_micros: i64,
}

/// Node in the layout tree: a leaf pane or a recursive split.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum TileNode {
    Pane(TilePane),
    Split(TileSplit),
}

/// Split node: axis, per-child flex weights, and recursive children.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TileSplit {
    pub axis: SplitAxis,
    pub flexes: Vec<f32>,
    pub children: Vec<TileNode>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitAxis {
    Horizontal,
    Vertical,
}

/// Pane: which tab was active plus the items themselves.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TilePane {
    pub active_index: usize,
    #[serde(default)]
    pub tab_orientation: TabOrientation,
    #[serde(default)]
    pub hide_tab_bar: bool,
    #[serde(default)]
    pub locked_size: Option<(f32, f32)>,
    pub items: Vec<TileItem>,
}

/// How a pane's tab bar is laid out.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TabOrientation {
    #[default]
    Horizontal,
    Vertical,
}

/// One pane item, tagged with the serialization key used to pick a
/// deserializer at load time. `state` is a JSON blob the owning panel
/// produced from its own config struct.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TileItem {
    pub kind: String,
    pub state: String,
}
