use crate::EqlContext;
use bevy::ecs::{
    system::{ResMut, SystemParam, SystemState},
    world::World,
};
use bevy::prelude::Res;
use bevy_egui::egui;
use fuzzy_matcher::{FuzzyMatcher, skim::SkimMatcherV2};
use impeller2_bevy::EntityMap;
use std::collections::BTreeMap;

use crate::ui::{EntityFilter, EntityPair, SelectedObject, colors::get_scheme};

use super::{inspector::search, schematic::Branch, widgets::WidgetSystem};

#[derive(SystemParam)]
pub struct HierarchyContent<'w> {
    entity_filter: ResMut<'w, EntityFilter>,
    selected_object: ResMut<'w, SelectedObject>,
    eql_ctx: Res<'w, EqlContext>,
    entity_map: Res<'w, EntityMap>,
}

pub struct Hierarchy {
    pub search: egui::TextureId,
    pub entity: egui::TextureId,
    pub chevron: egui::TextureId,
}

impl WidgetSystem for HierarchyContent<'_> {
    type Args = Hierarchy;
    type Output = ();

    fn ui_system(
        world: &mut World,
        state: &mut SystemState<Self>,
        ui: &mut egui::Ui,
        icons: Self::Args,
    ) {
        ui.painter().rect_filled(
            ui.max_rect(),
            egui::CornerRadius::ZERO,
            get_scheme().bg_primary,
        );

        let HierarchyContent {
            entity_filter,
            mut selected_object,
            eql_ctx,
            entity_map,
        } = state.get_mut(world);

        let search_text = entity_filter.0.clone();
        header(ui, entity_filter, icons.search);
        entity_list(
            ui,
            &eql_ctx,
            &entity_map,
            &mut selected_object,
            &search_text,
            icons,
        );
    }
}

pub fn header(
    ui: &mut egui::Ui,
    mut entity_filter: ResMut<EntityFilter>,
    search_icon: egui::TextureId,
) -> egui::Response {
    egui::Frame::NONE
        .inner_margin(egui::Margin::symmetric(8, 8))
        .show(ui, |ui| {
            search(ui, &mut entity_filter.0, search_icon);
        })
        .response
}

pub fn entity_list(
    ui: &mut egui::Ui,
    eql_ctx: &EqlContext,
    entity_map: &EntityMap,
    selected_object: &mut ResMut<SelectedObject>,
    entity_filter: &str,
    icons: Hierarchy,
) -> egui::Response {
    let tree_rect = ui.max_rect();
    egui::ScrollArea::both()
        .show(ui, |ui| {
            ui.vertical(|ui| {
                let matcher = SkimMatcherV2::default().smart_case().use_cache(true);
                let parts = filter_component_parts_recurisve(
                    &eql_ctx.0.component_parts,
                    &matcher,
                    entity_filter,
                );

                let default_open = parts.len() <= 5;
                for part in parts.values() {
                    component_part(
                        ui,
                        tree_rect,
                        &icons,
                        part,
                        entity_map,
                        selected_object,
                        default_open,
                    );
                }

                ui.allocate_space(ui.available_size());
            })
        })
        .inner
        .response
}

fn component_part(
    ui: &mut egui::Ui,
    tree_rect: egui::Rect,
    icons: &Hierarchy,
    part: &eql::ComponentPart,
    entity_map: &EntityMap,
    selected_object: &mut SelectedObject,
    default_open: bool,
) {
    let selected = selected_object.is_entity_selected(part.id);
    let list_item = Branch::new(part.name.clone(), icons.entity, icons.chevron, tree_rect)
        .id(part.id.0 + part.children.len() as u64)
        .selected(selected)
        .leaf(part.children.is_empty())
        .default_open(default_open)
        .show(ui, |ui| {
            let default_children = part.children.len() <= 5;
            for part in part.children.values() {
                component_part(
                    ui,
                    tree_rect,
                    icons,
                    part,
                    entity_map,
                    selected_object,
                    default_children || part.children.len() == 1,
                );
            }
        });

    if list_item.inner.clicked() {
        let Some(entity) = entity_map.get(&part.id) else {
            return;
        };
        if let SelectedObject::Entity(prev) = selected_object {
            *selected_object = if prev.impeller == part.id {
                SelectedObject::None
            } else {
                SelectedObject::Entity(EntityPair {
                    bevy: *entity,
                    impeller: part.id,
                })
            };
        } else {
            *selected_object = SelectedObject::Entity(EntityPair {
                bevy: *entity,
                impeller: part.id,
            })
        }
    }
}

fn filter_component_parts_recurisve(
    children: &BTreeMap<String, eql::ComponentPart>,
    matcher: &SkimMatcherV2,
    query: &str,
) -> BTreeMap<String, eql::ComponentPart> {
    if query.is_empty() {
        return children.clone();
    }
    children
        .iter()
        .filter_map(|(name, child)| {
            let part = eql::ComponentPart {
                name: child.name.clone(),
                id: child.id,
                component: child.component.clone(),
                children: filter_component_parts_recurisve(&child.children, matcher, query),
            };
            if matcher.fuzzy_match(&child.name, query).is_none() && part.children.is_empty() {
                None
            } else {
                Some((name.to_string(), part))
            }
        })
        .collect()
}
