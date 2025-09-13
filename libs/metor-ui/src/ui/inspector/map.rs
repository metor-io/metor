use bevy::{
    ecs::system::SystemParam,
    prelude::{Entity, Query},
};
use egui::{Color32, CornerRadius, RichText, Stroke};

use crate::ui::{colors::get_scheme, map::MapTile, widgets::WidgetSystem};

use crate::{EqlContext, object_3d::compile_eql_expr};

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

        let style = ui.style_mut();
        style.visuals.widgets.active.corner_radius = CornerRadius::ZERO;
        style.visuals.widgets.hovered.corner_radius = CornerRadius::ZERO;
        style.visuals.widgets.open.corner_radius = CornerRadius::ZERO;

        style.visuals.widgets.active.fg_stroke = Stroke::new(0.0, Color32::TRANSPARENT);
        style.visuals.widgets.active.bg_stroke = Stroke::new(0.0, Color32::TRANSPARENT);
        style.visuals.widgets.hovered.fg_stroke = Stroke::new(0.0, Color32::TRANSPARENT);
        style.visuals.widgets.hovered.bg_stroke = Stroke::new(0.0, Color32::TRANSPARENT);
        style.visuals.widgets.open.fg_stroke = Stroke::new(0.0, Color32::TRANSPARENT);
        style.visuals.widgets.open.bg_stroke = Stroke::new(0.0, Color32::TRANSPARENT);

        style.spacing.button_padding = [16.0, 16.0].into();

        style.visuals.widgets.active.bg_fill = get_scheme().bg_secondary;
        style.visuals.widgets.open.bg_fill = get_scheme().bg_secondary;
        style.visuals.widgets.inactive.bg_fill = get_scheme().bg_secondary;
        style.visuals.widgets.hovered.bg_fill = get_scheme().bg_secondary;

        ui.add(
            egui::Label::new(
                RichText::new("MAP LABEL")
                    .color(get_scheme().text_tertiary)
                    .size(12.),
            )
            .selectable(false),
        );
        ui.add_space(16.);

        ui.add_sized(
            egui::vec2(ui.available_width(), 50.0),
            egui::TextEdit::singleline(&mut tile.label).margin(egui::Margin::same(16)),
        );
        ui.add_space(32.);

        ui.add(
            egui::Label::new(
                RichText::new("EQL EXPRESSION")
                    .color(get_scheme().text_tertiary)
                    .size(12.),
            )
            .selectable(false),
        );
        ui.add_space(16.);

        let mut eql_text = tile.eql.eql.clone();
        let response = ui.add_sized(
            egui::vec2(ui.available_width(), 100.0),
            egui::TextEdit::multiline(&mut eql_text)
                .margin(egui::Margin::same(16))
                .desired_width(0.0),
        );

        if response.changed() {
            tile.eql.eql = eql_text.clone();
            // Recompile the EQL expression
            tile.eql.compiled_expr = state
                .eql_ctx
                .0
                .parse_str(&eql_text)
                .ok()
                .map(compile_eql_expr);
        }

        ui.add_space(8.);
        ui.add(
            egui::Label::new(
                RichText::new("Enter an EQL expression that evaluates to lat,lon pairs")
                    .color(get_scheme().text_tertiary)
                    .size(10.),
            )
            .selectable(false),
        );
    }
}
