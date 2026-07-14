//! Persistence shape for a System Graph tile. Round-trips through `facet_json`
//! as the tile's `PaneItem::Config`. Only view state lives here — the topology
//! itself is always the latest [`WiringManifest`](metor_proto_wkt::WiringManifest)
//! from the store, never persisted.

use std::collections::{BTreeSet, HashMap};

use gpui::SharedString;

use crate::graph_layout::Direction;

/// View state the tile persists: the pan offset, any manually-dragged node
/// positions, which scopes the operator has collapsed, and the flow
/// direction.
#[derive(Clone, Debug, Default, facet::Facet)]
pub struct SystemGraphConfig {
    pub viewport: Viewport,
    /// Manual position overrides keyed by node id. Presence of any override
    /// suppresses auto-layout for that node; "re-layout" clears them all.
    pub overrides: Vec<NodeOverride>,
    /// Scope paths the operator has collapsed into aggregate group nodes.
    pub collapsed: Vec<String>,
    /// Flow direction of the auto-layout. Defaulted so documents saved
    /// before the field existed still load.
    #[facet(default)]
    pub direction: Direction,
}

/// Pan offset in graph space (`screen = graph - viewport`).
#[derive(Clone, Debug, Default, facet::Facet)]
pub struct Viewport {
    pub x: f32,
    pub y: f32,
}

/// One manually-positioned node.
#[derive(Clone, Debug, facet::Facet)]
pub struct NodeOverride {
    pub id: String,
    pub x: f32,
    pub y: f32,
}

impl SystemGraphConfig {
    pub fn overrides_map(&self) -> HashMap<SharedString, (f32, f32)> {
        self.overrides
            .iter()
            .map(|o| (SharedString::from(o.id.clone()), (o.x, o.y)))
            .collect()
    }

    pub fn collapsed_set(&self) -> BTreeSet<String> {
        self.collapsed.iter().cloned().collect()
    }

    pub fn from_state(
        viewport: Viewport,
        overrides: &HashMap<SharedString, (f32, f32)>,
        collapsed: &BTreeSet<String>,
        direction: Direction,
    ) -> Self {
        let mut overrides: Vec<NodeOverride> = overrides
            .iter()
            .map(|(id, (x, y))| NodeOverride {
                id: id.to_string(),
                x: *x,
                y: *y,
            })
            .collect();
        // Stable order so on-disk diffs stay minimal.
        overrides.sort_by(|a, b| a.id.cmp(&b.id));
        Self {
            viewport,
            overrides,
            collapsed: collapsed.iter().cloned().collect(),
            direction,
        }
    }
}
