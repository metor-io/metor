use bevy::{
    ecs::system::{SystemParam, SystemState},
    prelude::{Component, Entity, Query, Res, ResMut},
};
use bevy_egui::egui::Ui;
use metor_proto_bevy::EntityMap;
use metor_proto_wkt::ComponentValue;
use walkers::{
    HttpOptions, HttpTiles, Map, MapMemory, Position,
    extras::{Place, Places},
    lat_lon,
    sources::OpenStreetMap,
};

use crate::{
    dirs,
    object_3d::EditableEQL,
    ui::{SelectedObject, colors, widgets::WidgetSystem},
};

#[derive(Component)]
pub struct MapTile {
    pub eql: EditableEQL,
    pub label: String,
}

#[derive(Component)]
pub struct MapTileState {
    pub tiles: Option<HttpTiles>,
    pub map_memory: MapMemory,
}

impl MapTileState {
    pub fn new() -> Self {
        Self {
            tiles: None,
            map_memory: MapMemory::default(),
        }
    }
}

#[derive(SystemParam)]
pub struct MapTileWidget<'w, 's> {
    map_tiles: Query<'w, 's, &'static MapTile>,
    map_states: Query<'w, 's, &'static mut MapTileState>,
    entity_map: Res<'w, EntityMap>,
    component_values: Query<'w, 's, &'static ComponentValue>,
    _selected_object: ResMut<'w, SelectedObject>,
}

impl WidgetSystem for MapTileWidget<'_, '_> {
    type Args = Entity;
    type Output = ();

    fn ui_system(
        world: &mut bevy::prelude::World,
        state: &mut SystemState<Self>,
        ui: &mut Ui,
        entity: Self::Args,
    ) -> Self::Output {
        let mut state = state.get_mut(world);

        let Ok(map_tile) = state.map_tiles.get(entity) else {
            ui.label("No map tile");
            return;
        };

        // Initialize MapTileState if it doesn't exist
        let Ok(mut map_state) = state.map_states.get_mut(entity) else {
            ui.label("No map state");
            return;
        };

        let map_state = &mut *map_state;

        // Initialize tiles if needed
        if map_state.tiles.is_none() {
            let egui_ctx = ui.ctx().clone();
            map_state.tiles = Some(HttpTiles::new(OpenStreetMap, egui_ctx));
        }

        // Update markers based on EQL expression
        let Some(compiled_expr) = &map_tile.eql.compiled_expr else {
            ui.label("No EQL");
            return;
        };

        let pos = match compiled_expr.execute(&state.entity_map, &state.component_values) {
            Ok(component_value) => extract_positions(&component_value),
            Err(e) => {
                ui.label(format!("Error evaluating EQL: {}", e));
                return;
            }
        };
        let Some(pos) = pos else {
            ui.label("No position");
            return;
        };

        if map_state.tiles.is_none() {
            let dirs = dirs();
            let cache = dirs.cache_dir();
            map_state.tiles = Some(HttpTiles::with_options(
                OpenStreetMap,
                HttpOptions {
                    cache: Some(cache.join("osm-tiles")),
                    ..Default::default()
                },
                ui.ctx().clone(),
            ));
        }

        let map = Map::new(
            map_state
                .tiles
                .as_mut()
                .map(|tiles| tiles as &mut dyn walkers::Tiles),
            &mut map_state.map_memory,
            pos,
        )
        .pull_to_my_position_threshold(3.0)
        .zoom_with_ctrl(false)
        .with_plugin(Places::new(vec![Marker { pos }]));
        let response = ui.add(map);
        if response.double_clicked() {
            map_state.map_memory.follow_my_position();
        }
    }
}

struct Marker {
    pos: Position,
}

impl Place for Marker {
    fn position(&self) -> Position {
        self.pos
    }

    fn draw(&self, ui: &Ui, projector: &walkers::Projector) {
        let projected = projector.project(self.pos);
        ui.painter().circle_filled(
            egui::pos2(projected.x, projected.y),
            5.0,
            colors::REDDISH_DEFAULT,
        );
    }
}

fn extract_positions(value: &ComponentValue) -> Option<Position> {
    match value {
        ComponentValue::F64(array) => {
            use nox::ArrayBuf;
            let data = array.buf.as_buf();
            let lat = data.get(0)?;
            let long = data.get(1)?;
            Some(lat_lon(*lat, *long))
        }
        ComponentValue::F32(array) => {
            use nox::ArrayBuf;
            let data = array.buf.as_buf();
            let lat = data.get(0)?;
            let long = data.get(1)?;
            Some(lat_lon(*lat as f64, *long as f64))
        }
        _ => None,
    }
}
