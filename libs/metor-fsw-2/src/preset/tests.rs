use metor_proto_wkt::{TILE_LAYOUT_VERSION, TileItem, TileLayout, TileNode, TilePane};

use super::{PresetSpec, PresetsParams};

fn layout() -> TileLayout {
    TileLayout {
        version: 0,
        global_time_range: String::new(),
        root: TileNode::Pane(TilePane {
            active_index: 0,
            tab_orientation: Default::default(),
            hide_tab_bar: false,
            locked_size: None,
            items: vec![TileItem {
                kind: "time_series_plot".into(),
                state: r#"{"label":"rates"}"#.into(),
            }],
        }),
    }
}

/// Params deserialize from the JSON shape the Python builder emits — the
/// layout arrives as the structured tree, so a malformed preset fails at
/// resolution rather than reaching the panel.
#[test]
fn params_deserialize_from_builder_json() {
    let json = r#"{"preset":[{"name":"ops","layout":{
        "global_time_range":"",
        "root":{"Pane":{"active_index":0,"items":[{"kind":"log","state":"{}"}]}}
    }}]}"#;
    let params: PresetsParams = serde_json::from_str(json).expect("params parse");
    assert_eq!(params.preset.len(), 1);
    assert_eq!(params.preset[0].name, "ops");
    assert_eq!(
        params.preset[0].layout.version, 0,
        "an absent version stays 0 until publish stamps it"
    );
}

/// Pins the id the Python recorder's `component_id()` computes for a
/// qualified name against `ComponentId::new` — the two must agree or preset
/// traces silently point at nothing.
#[test]
fn python_component_id_parity() {
    use metor_proto::types::ComponentId;
    assert_eq!(
        ComponentId::new("sat1.plant.gyro.rates").0,
        3325449500645109259
    );
}

#[cfg(not(miri))]
mod system {
    use metor_proto::types::ComponentId;
    use metor_proto_wkt::PresetDefs;

    use super::super::{PresetSystem, PresetsParams};
    use super::*;
    use crate::coordinator::init::cyclic_node;
    use crate::{BuildSystem, ClockMode, Coordinator, CoordinatorConfig, MsgIn, System};

    fn tap(coord: &Coordinator, key: &str) -> MsgIn<PresetDefs> {
        let registry = coord.registry();
        let entry = registry
            .get(ComponentId::new(key))
            .unwrap_or_else(|| panic!("registry entry `{key}`"));
        MsgIn::new(entry.view().expect("reader slot"))
    }

    /// The whole path: a configured preset publishes once as a snapshot,
    /// version-stamped, and the embedded JSON hydrates back into the tree.
    #[stellarator::test]
    async fn presets_broadcast_once() {
        let mut b = crate::coordinator::init::InitGraph::new(CoordinatorConfig {
            cycle_rate: 1000.0,
            clock: ClockMode::Wall,
            ..CoordinatorConfig::default()
        });
        b.push_node(cyclic_node(
            PresetSystem::NAME.into(),
            PresetSystem::new(PresetsParams {
                preset: vec![PresetSpec {
                    name: "ops".into(),
                    layout: layout(),
                }],
            }),
        ));
        let coord = b.build().unwrap();
        let mut defs = tap(&coord, "presets.PresetDefs");
        let mut coord = coord;
        coord.run_for(3).await;

        let mut got = Vec::new();
        defs.drain(|d| got.push(d)).unwrap();
        assert_eq!(got.len(), 1, "one snapshot record, not one per cycle");
        assert_eq!(got[0].presets.len(), 1);
        assert_eq!(got[0].presets[0].name, "ops");

        let hydrated: TileLayout =
            serde_json::from_str(&got[0].presets[0].layout).expect("layout json parses");
        assert_eq!(
            hydrated.version, TILE_LAYOUT_VERSION,
            "publish stamps the version"
        );
        let TileNode::Pane(pane) = hydrated.root else {
            panic!("expected pane root");
        };
        assert_eq!(pane.items[0].kind, "time_series_plot");
    }
}
