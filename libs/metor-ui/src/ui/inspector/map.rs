use bevy::{
    ecs::system::SystemParam,
    prelude::{Entity, Query},
};
use egui::{Color32, CornerRadius, RichText, Stroke};

use crate::{
    EqlContext,
    ui::{
        colors::get_scheme, label, map::MapTile, theme::configure_input_with_border,
        utils::MarginSides, widgets::WidgetSystem,
    },
};

use super::eql_textfield;

#[derive(SystemParam)]
pub struct InspectorMap<'w, 's> {
    map_tiles: Query<'w, 's, &'static mut MapTile>,
    eql_ctx: bevy::prelude::Res<'w, EqlContext>,
}

impl WidgetSystem for InspectorMap<'_, '_> {
    type Args = Entity;
    type Output = ();

    fn ui_system(
        world: &mut bevy::prelude::World,
        state: &mut bevy::ecs::system::SystemState<Self>,
        ui: &mut egui::Ui,
        entity: Self::Args,
    ) -> Self::Output {
        let mut state = state.get_mut(world);
        let Ok(mut tile) = state.map_tiles.get_mut(entity) else {
            return;
        };

        ui.spacing_mut().item_spacing.y = 8.0;

        label::editable_label_with_buttons(
            ui,
            [],
            &mut tile.label,
            get_scheme().text_primary,
            egui::Margin::same(0).top(10.0).bottom(14.0),
        );

        egui::Frame::NONE.show(ui, |ui| {
            ui.label(
                egui::RichText::new("EQL Expression (lat, lon)").color(get_scheme().text_secondary),
            );
            configure_input_with_border(ui.style_mut());

            let query_res = eql_textfield(ui, true, &state.eql_ctx.0, &mut tile.eql.eql);

            if query_res.changed() {
                match state.eql_ctx.0.parse_str(&tile.eql.eql) {
                    Ok(expr) => {
                        tile.eql.compiled_expr = Some(crate::object_3d::compile_eql_expr(expr));
                    }
                    Err(err) => {
                        ui.colored_label(get_scheme().error, err.to_string());
                    }
                }
            }
        });
    }
}
