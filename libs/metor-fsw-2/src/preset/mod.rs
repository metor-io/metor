//! Target-shipped panel presets.
//!
//! A target declares recommended panel layouts statically — `m.add("presets",
//! Presets([...]))` in a `target.py` — and this system broadcasts them as one
//! latest-wins [`PresetDefs`] snapshot. The downlink retains snapshot
//! messages, so a panel connecting mid-target still receives the set and can
//! offer the layouts on connect.
//!
//! Layouts deserialize into the typed [`TileLayout`] tree at build, so a
//! malformed preset fails resolution instead of reaching the panel. Component
//! references live inside each item's opaque `state` blob, which this system
//! cannot rewrite — namespace-qualifying them is the config builder's job.

#[cfg(test)]
mod tests;

use metor_proto::types::Timestamp;
use metor_proto_wkt::{Preset, PresetDefs, TILE_LAYOUT_VERSION, TileLayout};
use serde::Deserialize;

use crate::message::MsgOut;
use crate::system::{BuildSystem, CyclicSystem, Out, System};

/// The presets one [`PresetSystem`] instance broadcasts, one [`PresetSpec`]
/// per repeated `preset` entry. An empty instance is legal and publishes an
/// empty set, clearing any previously recorded presets.
#[derive(Deserialize, Debug, Clone, Default)]
pub struct PresetsParams {
    #[serde(default)]
    pub preset: Vec<PresetSpec>,
}

/// One named layout. The document arrives as the structured [`TileLayout`]
/// tree; a version of zero (the builder default) is stamped with
/// [`TILE_LAYOUT_VERSION`] at publish.
#[derive(Deserialize, Debug, Clone)]
pub struct PresetSpec {
    pub name: String,
    pub layout: TileLayout,
}

/// The single telemetered port: the full preset set as one latest-wins
/// snapshot, retained by the downlink for late-joining connections.
#[derive(crate::SystemOutput)]
pub struct PresetOut {
    #[fsw(snapshot)]
    defs: MsgOut<PresetDefs>,
}

/// The preset broadcaster. Publishes the configured set once, on the first
/// execute; every later cycle is a no-op.
pub struct PresetSystem {
    presets: Option<Vec<PresetSpec>>,
}

impl BuildSystem for PresetSystem {
    type Params = PresetsParams;

    fn new(params: PresetsParams) -> Self {
        Self { presets: Some(params.preset) }
    }
}

impl System for PresetSystem {
    type Input = ();
    type Output = Out<PresetOut>;
    const NAME: &'static str = "presets";
}

impl CyclicSystem for PresetSystem {
    fn execute(&mut self, _now: Timestamp, _input: &mut (), output: &mut Out<PresetOut>) {
        let Some(presets) = self.presets.take() else {
            return;
        };
        let presets = presets
            .into_iter()
            .map(|spec| {
                let mut layout = spec.layout;
                if layout.version == 0 {
                    layout.version = TILE_LAYOUT_VERSION;
                }
                Preset {
                    name: spec.name,
                    layout: serde_json::to_string(&layout).expect("tile layout always serializes"),
                }
            })
            .collect();
        output.defs.publish(&PresetDefs { presets });
    }
}
