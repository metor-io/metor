use std::{collections::BTreeMap, str::FromStr, time::Duration};

use bevy::{
    asset::{AssetServer, Assets},
    ecs::{
        query::With,
        system::{Commands, InRef, IntoSystem, Query, Res, ResMut, System},
        world::World,
    },
    log::info,
    pbr::{StandardMaterial, wireframe::WireframeConfig},
    prelude::In,
    render::view::Visibility,
};
use bevy_infinite_grid::InfiniteGrid;
use egui_tiles::TileId;
use fuzzy_matcher::{FuzzyMatcher, skim::SkimMatcherV2};
use impeller2::types::{ComponentId, PrimType, msg_id};
use impeller2_bevy::{
    ComponentMetadataRegistry, ComponentSchemaRegistry, CurrentStreamId, EntityMap, PacketTx,
};
use impeller2_kdl::ToKdl;
use impeller2_wkt::{
    ComponentValue, IsRecording, Material, Mesh, Object3D, SetDbConfig, SetStreamState,
    UpdateComponent,
};
use miette::IntoDiagnostic;
use nox::{Array, ArrayBuf};

use crate::{
    EqlContext, Offset, SelectedTimeRange, TimeRangeBehavior,
    object_3d::compile_eql_expr,
    plugins::navigation_gizmo::RenderLayerAlloc,
    ui::{
        HdrEnabled, colors,
        plot::GraphBundle,
        schematic::{
            CurrentSchematic, LoadSchematicParams, SchematicLiveReloadRx, eql_to_component_tree,
            load_schematic_file,
        },
        tiles::{self, TileState},
    },
};

pub struct PalettePage {
    items: Vec<PaletteItem>,
    pub label: Option<String>,
    initialized: bool,
    pub prompt: Option<String>,
}

impl PalettePage {
    pub fn new(items: Vec<PaletteItem>) -> Self {
        Self {
            items,
            initialized: false,
            label: None,
            prompt: None,
        }
    }

    pub fn label(mut self, label: impl ToString) -> Self {
        self.label = Some(label.to_string());
        self
    }

    pub fn initialize(&mut self, world: &mut World) {
        if !self.initialized {
            for item in &mut self.items {
                item.system.initialize(world);
                item.label.init(world);
            }
        }
    }

    pub fn filter(&mut self, filter: &str) -> Vec<MatchedPaletteItem<'_>> {
        let matcher = SkimMatcherV2::default();
        let mut items: Vec<_> = self
            .items
            .iter_mut()
            .filter_map(|item| {
                let label = match &item.label {
                    LabelSource::Label(l) => l.as_str(),
                    LabelSource::System(_) => "",
                };
                let Some((mut score, match_indices)) = matcher.fuzzy_indices(label, filter) else {
                    return if item.default {
                        Some(MatchedPaletteItem {
                            item,
                            score: 0,
                            match_indices: Vec::new(),
                        })
                    } else {
                        None
                    };
                };
                if match_indices.len() == label.len() {
                    score *= 16
                }
                Some(MatchedPaletteItem {
                    item,
                    score,
                    match_indices,
                })
            })
            .collect();
        items.sort_by(|a, b| b.score.cmp(&a.score));
        items
    }

    pub fn prompt(mut self, prompt: impl ToString) -> Self {
        self.prompt = Some(prompt.to_string());
        self
    }
}

pub struct PaletteItem {
    pub label: LabelSource,
    pub header: String,
    pub icon: PaletteIcon,
    pub system: Box<dyn System<In = In<String>, Out = PaletteEvent>>,
    pub default: bool,
}

pub enum LabelSource {
    Label(String),
    System(Box<dyn System<In = InRef<'static, str>, Out = String>>),
}

impl LabelSource {
    pub fn placeholder(placeholder: impl ToString) -> Self {
        let placeholder = placeholder.to_string();
        LabelSource::system(move |label: InRef<'_, str>| {
            if label.is_empty() {
                placeholder.clone()
            } else {
                label.0.to_string()
            }
        })
    }

    pub fn system<M, I: IntoSystem<InRef<'static, str>, String, M>>(system: I) -> Self {
        LabelSource::System(Box::new(I::into_system(system)))
    }

    pub fn init(&mut self, world: &mut World) {
        if let LabelSource::System(system) = self {
            system.initialize(world);
        }
    }

    pub fn get(&mut self, world: &mut World, filter: &str) -> String {
        match self {
            LabelSource::Label(l) => l.clone(),
            LabelSource::System(system) => system.run(filter, world),
        }
    }
}

impl From<&str> for LabelSource {
    fn from(value: &str) -> Self {
        LabelSource::Label(value.into())
    }
}

impl From<String> for LabelSource {
    fn from(val: String) -> Self {
        LabelSource::Label(val)
    }
}

pub enum PaletteIcon {
    None,
    Link,
}

impl PaletteItem {
    pub fn new<M, I: IntoSystem<In<String>, PaletteEvent, M>>(
        label: impl Into<LabelSource>,
        header: impl ToString,
        system: I,
    ) -> Self {
        Self {
            label: label.into(),
            header: header.to_string(),
            system: Box::new(I::into_system(system)),
            icon: PaletteIcon::None,
            default: false,
        }
    }

    pub fn icon(mut self, icon: PaletteIcon) -> Self {
        self.icon = icon;
        self
    }

    pub fn default(mut self) -> Self {
        self.default = true;
        self
    }
}

pub enum PaletteEvent {
    NextPage {
        prev_page_label: Option<String>,
        next_page: PalettePage,
    },
    Exit,
    Error(String),
}

impl From<PalettePage> for PaletteEvent {
    fn from(v: PalettePage) -> Self {
        Self::NextPage {
            prev_page_label: None,
            next_page: v,
        }
    }
}

pub struct MatchedPaletteItem<'a> {
    pub item: &'a mut PaletteItem,
    pub score: i64,
    pub match_indices: Vec<usize>,
}

const VIEWPORT_LABEL: &str = "Viewport";
const TILES_LABEL: &str = "Tiles";
const SIMULATION_LABEL: &str = "Simulation";
const TIME_LABEL: &str = "Time";
const PRESETS_LABEL: &str = "Presets";

pub fn create_action(tile_id: Option<TileId>) -> PaletteItem {
    PaletteItem::new("Create Action", TILES_LABEL, move |_: In<String>| {
        PalettePage::new(vec![
                    PaletteItem::new(
                        LabelSource::placeholder("Enter a label for the button"),
                        "",
                        move |In(label): In<String>| {
                            let msg_label = label.clone();
                            PalettePage::new(vec![
                                PaletteItem::new("send_msg", "Presets", move |_: In<String>| {
                                let msg_label = msg_label.clone();
                                    PalettePage::new(vec![PaletteItem::new(
                                        LabelSource::placeholder("Enter the name of the msg to send"),
                                        "Enter the name of the msg to send",
                                        move |In(name): In<String>| {
                                            let msg_label = msg_label.clone();
                                            PalettePage::new(vec![PaletteItem::new(
                                                LabelSource::placeholder("Msg"),
                                                "Contents of the msg as a lua table - {foo = \"bar\"}",
                                                move |In(msg): In<String>, mut tile_state: ResMut<tiles::TileState>| {
                                                    tile_state.create_action_tile(
                                                        msg_label.clone(),
                                                        format!("client:send_msg({name:?}, {msg})"),
                                                        tile_id,
                                                    );
                                                    PaletteEvent::Exit
                                                },
                                            ).default()])
                                            .into()
                                        },
                                    ).default()])
                                    .into()
                                }),
                                PaletteItem::new(
                                    LabelSource::placeholder("Enter a lua command (i.e client:send_table)"),
                                    "Enter a custom lua command",
                                    move |lua: In<String>, mut tile_state: ResMut<tiles::TileState>| {
                                        tile_state.create_action_tile(label.clone(), lua.0, tile_id);
                                        PaletteEvent::Exit
                                    },
                                )
                                .default(),
                            ]).prompt("Enter a lua command to send")
                            .into()
                        },
                    )
                    .default(),
                ])
                .prompt("Enter a label for the action button")
                .into()
    })
}

pub fn create_graph(tile_id: Option<TileId>) -> PaletteItem {
    PaletteItem::new(
        "Create Graph",
        TILES_LABEL,
        move |_: In<String>, context: Res<EqlContext>| {
            PalettePage::new(graph_parts(&context.0.component_parts, tile_id, vec![]))
                .prompt("Select a component to graph")
                .into()
        },
    )
}

fn graph_parts(
    parts: &BTreeMap<String, eql::ComponentPart>,
    tile_id: Option<TileId>,
    eql_chain: Vec<String>,
) -> Vec<PaletteItem> {
    let done = if !eql_chain.is_empty() {
        let eql_chain = eql_chain.clone();
        Some(PaletteItem::new("Create Graph", "", {
            move |_: In<String>,
                  mut render_layer_alloc: ResMut<RenderLayerAlloc>,
                  mut tile_state: ResMut<tiles::TileState>,
                  eql_ctx: Res<EqlContext>,
                  schema_reg: Res<ComponentSchemaRegistry>| {
                let eql = eql_chain.clone().join(",");
                let components = match eql_to_component_tree(&eql_ctx, &schema_reg, &eql) {
                    Ok(tree) => tree,
                    Err(err) => return PaletteEvent::Error(err.to_string()),
                };

                let bundle =
                    GraphBundle::new(&mut render_layer_alloc, components, "Graph".to_string());
                tile_state.create_graph_tile(tile_id, bundle);
                PaletteEvent::Exit
            }
        }))
    } else {
        None
    };
    let eql_chain = eql_chain.clone();
    done.into_iter()
        .chain(parts_iter(parts).map(|part| {
            let name = part.name.clone();
            let eql_chain = eql_chain.clone();
            PaletteItem::new(
                name.clone(),
                "EQL",
                move |_: In<String>, eql_ctx: Res<EqlContext>| {
                    let mut eql_chain = eql_chain.clone();
                    eql_chain.push(name.clone());
                    PaletteEvent::NextPage {
                        prev_page_label: Some(name.clone()),
                        next_page: PalettePage::new(graph_parts(
                            &eql_ctx.0.component_parts,
                            tile_id,
                            eql_chain,
                        )),
                    }
                },
            )
        }))
        .collect()
}

fn parts_iter(
    parts: &BTreeMap<String, eql::ComponentPart>,
) -> Box<dyn Iterator<Item = &eql::ComponentPart> + '_> {
    Box::new(
        parts
            .iter()
            .flat_map(|(_, part)| std::iter::once(part).chain(parts_iter(&part.children))),
    )
}

fn update_component() -> PaletteItem {
    PaletteItem::new(
        "Update Component",
        "Components",
        move |_: In<String>, context: Res<EqlContext>, schema: Res<ComponentSchemaRegistry>| {
            PalettePage::new(update_component_parts(&context.0.component_parts, &schema))
                .prompt("Select a component to update")
                .into()
        },
    )
}

fn update_component_parts(
    parts: &BTreeMap<String, eql::ComponentPart>,
    schema: &ComponentSchemaRegistry,
) -> Vec<PaletteItem> {
    parts_iter(parts)
        .filter_map(|part| Some((part, schema.get(&part.id)?)))
        .map(|(part, schema)| {
            let name = part.name.clone();
            let id = part.id;
            let is_bool = schema.prim_type() == PrimType::Bool && schema.size() == 1;
            PaletteItem::new(
                part.name.clone(),
                "Component",
                move |_: In<String>, metadata: Res<ComponentMetadataRegistry>| {
                    if is_bool {
                            return PaletteEvent::NextPage {
                                prev_page_label: Some(name.clone()),
                                next_page: PalettePage::new(
                                    [(false, "false"), (true, "true")].into_iter()
                                    .map(|(i, var)| {
                                            PaletteItem::new(
                                                var.to_string(),
                                                "Variant",
                                                move |_: In<String>, tx: Res<PacketTx>| {
                                                    let value = ComponentValue::Bool(
                                                        Array::from_shape_vec(
                                                            smallvec::smallvec![],
                                                            vec![i],
                                                        )
                                                        .unwrap(),
                                                    );
                                                    tx.send_msg(UpdateComponent { id, value });
                                                    PaletteEvent::Exit
                                                },
                                            )
                                        })
                                        .collect(),
                                ),
                            };
                    }
                    else if let Some(metadata) = metadata.get(&id) {
                        if let Some(vars) = metadata.enum_variants() {
                            return PaletteEvent::NextPage {
                                prev_page_label: Some(name.clone()),
                                next_page: PalettePage::new(
                                    vars.into_iter()
                                        .enumerate()
                                        .map(|(i, var)| {
                                            PaletteItem::new(
                                                var.to_string(),
                                                "Variant",
                                                move |_: In<String>, tx: Res<PacketTx>, schema: Res<ComponentSchemaRegistry>| {
                                                    let Some(schema) = schema.get(&id) else {
                                                        return PaletteEvent::Exit;
                                                    };
                                                    let value = ComponentValue::U64(
                                                        Array::from_shape_vec(
                                                            smallvec::smallvec![],
                                                            vec![i as u64],
                                                        )
                                                        .unwrap(),
                                                    );
                                                    let value = value.cast(schema.prim_type());
                                                    tx.send_msg(UpdateComponent { id, value });
                                                    PaletteEvent::Exit
                                                },
                                            )
                                        })
                                        .collect(),
                                ),
                            };
                        } else if metadata.is_string() {
                            return PaletteEvent::NextPage {
                                prev_page_label: Some(name.clone()),
                                next_page: PalettePage::new(vec![
                                     PaletteItem::new(
                                         LabelSource::placeholder("Enter a valid string"),
                                         "Content",
                                         move |string: In<String>, tx: Res<PacketTx>, schema: Res<ComponentSchemaRegistry>| {
                                             let Some(schema) = schema.get(&id) else {
                                                 return PaletteEvent::Exit;
                                             };
                                             if string.len() > schema.size() {
                                                 return PaletteEvent::Error(format!("This string is longer than the max length of {:?}", schema.size()));
                                             }
                                             let mut vec = vec![0u8; schema.size()];
                                             vec[..string.len()].copy_from_slice(string.as_bytes());
                                             let value = ComponentValue::U8(
                                                 Array::from_shape_vec(schema.shape().into(), vec).unwrap()
                                             );
                                             tx.send_msg(dbg!(UpdateComponent { id, value }));
                                             PaletteEvent::Exit
                                         },
                                     ).default()
                                ]),
                            };
                        }
                    }
                    PaletteEvent::NextPage {
                        prev_page_label: Some(name.clone()),
                        next_page: PalettePage::new(update_component_eql(id)),
                    }
                },
            )
        })
        .collect()
}

fn update_component_eql(component_id: ComponentId) -> Vec<PaletteItem> {
    vec![
        PaletteItem::new(
            LabelSource::placeholder("Enter an eql expression"),
            "Enter an eql expression",
            move |eql: In<String>,
                  ctx: Res<EqlContext>,
                  entity_map: Res<EntityMap>,
                  values: Query<&'static ComponentValue>,
                  tx: Res<PacketTx>,
                  schema: Res<ComponentSchemaRegistry>| {
                match ctx.0.parse_str(&eql.0) {
                    Ok(eql) => {
                        let eql = compile_eql_expr(eql);
                        let value = match dbg!(eql.execute(&entity_map, &values)) {
                            Ok(result) => result,
                            Err(err) => return PaletteEvent::Error(err.to_string()),
                        };
                        let Some(schema) = schema.get(&component_id) else {
                            return PaletteEvent::Exit;
                        };
                        let value = value.cast(schema.prim_type());
                        tx.send_msg(UpdateComponent {
                            id: component_id,
                            value,
                        });
                        PaletteEvent::Exit
                    }
                    Err(err) => PaletteEvent::Error(err.to_string()),
                }
            },
        )
        .default(),
    ]
}

pub fn create_monitor(tile_id: Option<TileId>) -> PaletteItem {
    PaletteItem::new(
        "Create Monitor",
        TILES_LABEL,
        move |_: In<String>, eql: Res<EqlContext>| {
            PalettePage::new(monitor_parts(&eql.0.component_parts, tile_id))
                .prompt("Select a component to monitor")
                .into()
        },
    )
}

fn monitor_parts(
    parts: &BTreeMap<String, eql::ComponentPart>,
    tile_id: Option<TileId>,
) -> Vec<PaletteItem> {
    parts
        .iter()
        .map(|(name, part)| {
            let part = part.clone();
            PaletteItem::new(
                name.clone(),
                "Component",
                move |_: In<String>, mut tile_state: ResMut<tiles::TileState>| {
                    if let Some(component) = &part.component {
                        tile_state.create_monitor_tile(component.id, tile_id);
                        PaletteEvent::Exit
                    } else {
                        PalettePage::new(monitor_parts(&part.children, tile_id)).into()
                    }
                },
            )
        })
        .collect()
}

fn toggle_body_axes() -> PaletteItem {
    PaletteItem::new("Toggle Body Axes", VIEWPORT_LABEL, |_: In<String>| {
        // TODO: This functionality needs to be updated once BodyAxes is migrated from EntityId to ComponentId
        // For now, return an empty page
        PalettePage::new(vec![])
            .prompt("Body axes functionality is temporarily disabled during refactor")
            .into()
    })
}

pub fn create_viewport(tile_id: Option<TileId>) -> PaletteItem {
    PaletteItem::new(
        "Create Viewport",
        TILES_LABEL,
        move |_: In<String>, mut tile_state: ResMut<TileState>| {
            tile_state.create_viewport_tile(tile_id);
            PaletteEvent::Exit
        },
    )
}

pub fn create_query_table(tile_id: Option<TileId>) -> PaletteItem {
    PaletteItem::new(
        "Create Query Table",
        TILES_LABEL,
        move |_: In<String>, mut tile_state: ResMut<tiles::TileState>| {
            tile_state.create_query_table_tile(tile_id);
            PaletteEvent::Exit
        },
    )
}

pub fn create_query_plot(tile_id: Option<TileId>) -> PaletteItem {
    PaletteItem::new(
        "Create Query Plot",
        TILES_LABEL,
        move |_: In<String>, mut tile_state: ResMut<tiles::TileState>| {
            tile_state.create_query_plot_tile(tile_id);
            PaletteEvent::Exit
        },
    )
}

pub fn create_hierarchy(tile_id: Option<TileId>) -> PaletteItem {
    PaletteItem::new(
        "Create Hierarchy",
        TILES_LABEL,
        move |_: In<String>, mut tile_state: ResMut<tiles::TileState>| {
            tile_state.create_hierarchy_tile(tile_id);
            PaletteEvent::Exit
        },
    )
}

pub fn create_inspector(tile_id: Option<TileId>) -> PaletteItem {
    PaletteItem::new(
        "Create Inspector",
        TILES_LABEL,
        move |_: In<String>, mut tile_state: ResMut<tiles::TileState>| {
            tile_state.create_inspector_tile(tile_id);
            PaletteEvent::Exit
        },
    )
}

pub fn create_schematic_tree(tile_id: Option<TileId>) -> PaletteItem {
    PaletteItem::new(
        "Create Schematic Tree",
        TILES_LABEL,
        move |_: In<String>, mut tile_state: ResMut<tiles::TileState>| {
            tile_state.create_tree_tile(tile_id);
            PaletteEvent::Exit
        },
    )
}

pub fn create_dashboard(tile_id: Option<TileId>) -> PaletteItem {
    PaletteItem::new(
        "Create Dashboard",
        TILES_LABEL,
        move |_: In<String>, mut tile_state: ResMut<tiles::TileState>| {
            tile_state.create_dashboard_tile(Default::default(), "Dashboard".to_string(), tile_id);
            PaletteEvent::Exit
        },
    )
}

pub fn create_sidebars() -> PaletteItem {
    PaletteItem::new(
        "Create Sidebars",
        TILES_LABEL,
        move |_: In<String>, mut tile_state: ResMut<tiles::TileState>| {
            tile_state.create_sidebars_layout();
            PaletteEvent::Exit
        },
    )
}
pub fn create_video_stream(tile_id: Option<TileId>) -> PaletteItem {
    PaletteItem::new(
        "Create Video Stream",
        TILES_LABEL,
        move |_: In<String>| -> PaletteEvent {
            PalettePage::new(vec![
                PaletteItem::new(
                    LabelSource::placeholder(
                        "Enter the name of the msg containing the video frames",
                    ),
                    "",
                    move |In(msg_name): In<String>, mut tile_state: ResMut<tiles::TileState>| {
                        let msg_name = msg_name.trim();
                        let label = format!("Video Stream {}", msg_name);
                        tile_state.create_video_stream_tile(msg_id(msg_name), label, tile_id);
                        PaletteEvent::Exit
                    },
                )
                .default(),
            ])
            .into()
        },
    )
}

pub fn create_map(tile_id: Option<TileId>) -> PaletteItem {
    PaletteItem::new(
        "Create Map",
        TILES_LABEL,
        move |_: In<String>| -> PaletteEvent {
            PalettePage::new(vec![
                PaletteItem::new(
                    LabelSource::placeholder(
                        "Enter EQL expression that evaluates to lat,lon pairs (e.g., 'entity.position[0:2]')",
                    ),
                    "Enter an EQL expression that returns an array of lat,lon coordinates",
                    move |In(eql): In<String>, mut tile_state: ResMut<tiles::TileState>| {
                        tile_state.create_map_tile(eql.trim().to_string(), tile_id);
                        PaletteEvent::Exit
                    },
                )
                .default(),
            ])
            .prompt("Enter EQL expression for map markers")
            .into()
        },
    )
}

fn set_playback_speed() -> PaletteItem {
    PaletteItem::new("Set Playback Speed", TIME_LABEL, |_: In<String>| {
        let speeds = [
            0.001, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 3.0, 4.0, 5.0, 10.0, 20.0, 30.0, 40.0,
            50.0, 100.0,
        ];
        let next_page = PalettePage::new(
            speeds
                .into_iter()
                .map(|speed| {
                    PaletteItem::new(
                        speed.to_string(),
                        "SPEED".to_string(),
                        move |_: In<String>,
                              packet_tx: Res<PacketTx>,
                              stream_id: Res<CurrentStreamId>| {
                            packet_tx.send_msg(SetStreamState {
                                id: stream_id.0,
                                playing: None,
                                timestamp: None,
                                time_step: Some(Duration::from_secs_f64(speed / 60.0)),
                                frequency: None,
                            });
                            PaletteEvent::Exit
                        },
                    )
                })
                .collect(),
        );

        PaletteEvent::NextPage {
            prev_page_label: None,
            next_page,
        }
    })
}

fn fix_current_time_range() -> PaletteItem {
    PaletteItem::new(
        "Fix Current Time Range",
        TIME_LABEL,
        |_: In<String>,
         selected_range: Res<SelectedTimeRange>,
         mut behavior: ResMut<TimeRangeBehavior>| {
            behavior.start = Offset::Fixed(selected_range.0.start);
            behavior.end = Offset::Fixed(selected_range.0.end);
            PaletteEvent::Exit
        },
    )
}

fn set_time_range_behavior() -> PaletteItem {
    PaletteItem::new("Set Time Range", TIME_LABEL, |_: In<String>| {
        PalettePage::new(vec![
            PaletteItem::new(
                LabelSource::placeholder(
                    "Enter start offset (e.g., '+5m', '-10s', '=2023-01-01T00:00:00Z')",
                ),
                "Start Offset",
                move |start_str: In<String>| {
                    let Ok(start_offset) = Offset::from_str(&start_str.0) else {
                        return PaletteEvent::Error(format!(
                            "Invalid start offset format: {}",
                            start_str.0
                        ));
                    };

                    PalettePage::new(vec![
                        PaletteItem::new(
                            LabelSource::placeholder(
                                "Enter end offset (e.g., '+5m', '-10s', '=2023-01-01T00:00:00Z')",
                            ),
                            "End Offset",
                            move |end_str: In<String>, mut behavior: ResMut<TimeRangeBehavior>| {
                                let Ok(end_offset) = Offset::from_str(&end_str.0) else {
                                    return PaletteEvent::Error(format!(
                                        "Invalid end offset format: {}",
                                        end_str.0
                                    ));
                                };

                                behavior.start = start_offset;
                                behavior.end = end_offset;
                                PaletteEvent::Exit
                            },
                        )
                        .default(),
                    ])
                    .prompt("Enter the end offset")
                    .into()
                },
            )
            .default(),
        ])
        .prompt("Enter the start offset")
        .into()
    })
}

pub fn save_schematic() -> PaletteItem {
    PaletteItem::new("Save Schematic", PRESETS_LABEL, |_name: In<String>| {
        PalettePage::new(vec![save_preset_inner()]).into()
    })
}

pub fn save_schematic_db() -> PaletteItem {
    PaletteItem::new(
        "Save Schematic To Db",
        PRESETS_LABEL,
        |_name: In<String>, tx: Res<PacketTx>, schematic: Res<CurrentSchematic>| {
            let kdl = schematic.0.to_kdl();
            tx.send_msg(SetDbConfig::schematic_content(kdl));
            PaletteEvent::Exit
        },
    )
}

pub fn save_preset_inner() -> PaletteItem {
    PaletteItem::new(
        LabelSource::placeholder("Enter a name for the schematic"),
        "",
        move |In(name): In<String>, schematic: Res<CurrentSchematic>| {
            let dirs = crate::dirs();
            let dir = dirs.data_dir().join("schematics");
            let _ = std::fs::create_dir(&dir);
            let kdl = schematic.0.to_kdl();
            let path = dir.join(&name).with_extension("kdl");
            info!(?path, "saving schematic");
            let _ = std::fs::write(path, kdl);
            PaletteEvent::Exit
        },
    )
    .default()
}

pub fn load_schematic() -> PaletteItem {
    PaletteItem::new("Load Schematic", PRESETS_LABEL, |_: In<String>| {
        let dirs = crate::dirs();
        let dir = dirs.data_dir().join("schematics");
        let Ok(elems) = std::fs::read_dir(dir) else {
            return PaletteEvent::Exit;
        };

        let mut items = vec![load_schematic_picker()];
        for elem in elems {
            let Ok(elem) = elem else { continue };
            let path = elem.path();
            let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            items.push(load_schematic_inner(file_name.to_string()))
        }
        PalettePage::new(items).into()
    })
}

pub fn load_schematic_picker() -> PaletteItem {
    PaletteItem::new(
        "From File",
        "",
        move |_: In<String>, params: LoadSchematicParams, rx: ResMut<SchematicLiveReloadRx>| {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("kdl", &["kdl"])
                .pick_file()
            {
                if let Err(err) = load_schematic_file(&path, params, rx)
                    .inspect_err(|err| {
                        dbg!(err);
                    })
                    .into_diagnostic()
                {
                    return PaletteEvent::Error(err.to_string());
                }
            }
            PaletteEvent::Exit
        },
    )
}

pub fn load_schematic_inner(name: String) -> PaletteItem {
    PaletteItem::new(
        name.clone(),
        "",
        move |_: In<String>, params: LoadSchematicParams, rx: ResMut<SchematicLiveReloadRx>| {
            let dirs = crate::dirs();
            let path = dirs.data_dir().join("schematics").join(name.clone());
            if let Err(err) = load_schematic_file(&path, params, rx) {
                PaletteEvent::Error(err.to_string())
            } else {
                PaletteEvent::Exit
            }
        },
    )
}

pub fn set_color_scheme() -> PaletteItem {
    PaletteItem::new("Set Color Scheme", PRESETS_LABEL, |_: In<String>| {
        let schemes = [
            ("DARK", &colors::DARK),
            ("LIGHT", &colors::LIGHT),
            ("CATPPUCINI LATTE", &colors::CATPPUCINI_LATTE),
            ("CATPPUCINI MOCHA", &colors::CATPPUCINI_MOCHA),
            ("CATPPUCINI MACCHIATO", &colors::CATPPUCINI_MACCHIATO),
            ("AYU DARK", &colors::AYU_DARK),
            ("AYU LIGHT", &colors::AYU_LIGHT),
        ];
        let mut items = vec![];
        for (name, schema) in schemes {
            items.push(PaletteItem::new(name, "", move |_: In<String>| {
                colors::set_schema(schema);
                PaletteEvent::Exit
            }));
        }
        PalettePage::new(items).into()
    })
}

fn create_object_3d_with_color(eql: String, expr: eql::Expr, mesh: Mesh) -> PaletteEvent {
    PalettePage::new(vec![
        PaletteItem::new(
            LabelSource::placeholder("Enter color (hex or name, default: #cccccc)"),
            "Enter color as rgb",
            move |In(color_str): In<String>,
                  mut commands: Commands,
                  eql_ctx: Res<EqlContext>,
                  entity_map: Res<EntityMap>,
                  component_value_maps: Query<&'static ComponentValue>,
                  mut material_assets: ResMut<Assets<StandardMaterial>>,
                  mut mesh_assets: ResMut<Assets<bevy::prelude::Mesh>>,
                  assets: Res<AssetServer>| {
                let color_str = color_str.trim();
                let (r, g, b) =
                    parse_color(color_str, &eql_ctx.0, &entity_map, component_value_maps)
                        .unwrap_or((0.8, 0.8, 0.8));

                let mesh_source = impeller2_wkt::Object3DMesh::Mesh {
                    mesh: mesh.clone(),
                    material: Material::color(r, g, b),
                };

                crate::object_3d::create_object_3d_entity(
                    &mut commands,
                    Object3D {
                        eql: eql.clone(),
                        mesh: mesh_source,
                        aux: (),
                    },
                    expr.clone(),
                    &mut material_assets,
                    &mut mesh_assets,
                    &assets,
                );

                PaletteEvent::Exit
            },
        )
        .default(),
    ])
    .prompt("Enter color for mesh")
    .into()
}

fn parse_color(
    expr: &str,
    ctx: &eql::Context,
    entity_map: &EntityMap,
    component_value_maps: Query<&'static ComponentValue>,
) -> Option<(f32, f32, f32)> {
    let expr = ctx.parse_str(expr).ok()?;
    let expr = crate::object_3d::compile_eql_expr(expr);
    let val = expr.execute(entity_map, &component_value_maps).ok()?;

    let ComponentValue::F64(array) = val else {
        return None;
    };
    let buf = array.buf.as_buf();
    match buf {
        [r, g, b, ..] => Some((*r as f32, *g as f32, *b as f32)),
        _ => None,
    }
}

pub fn create_3d_object() -> PaletteItem {
    PaletteItem::new("Create 3D Object", TILES_LABEL, move |_: In<String>| {
        PalettePage::new(vec![
            PaletteItem::new(
                LabelSource::placeholder("Enter EQL expression (e.g., 'entity.position')"),
                "Enter an EQL expression that resolves to a 7-component array [qx, qy, qz, qw, px, py, pz]",
                move |In(eql): In<String>, eql_context: Res<EqlContext>| {
                    let expr = match eql_context.0.parse_str(&eql) {
                        Ok(expr) => expr,
                        Err(err) => {
                            return PaletteEvent::Error(err.to_string())
                        }
                    };
                    PalettePage::new(vec![
                        PaletteItem::new(
                            "GLTF",
                            "",
                            {
                                let eql = eql.clone();
                                let expr = expr.clone();
                                move |_: In<String>| {
                                    let eql = eql.clone();
                                    let expr = expr.clone();
                                    PalettePage::new(vec![
                                        PaletteItem::new(
                                            LabelSource::placeholder("Enter GLTF path"),
                                            "Enter path to GLTF file for the 3D object visualization",
                                            move |In(gltf_path): In<String>,
                                                  mut commands: Commands,
                                                  mut material_assets: ResMut<Assets<StandardMaterial>>,
                                                  mut mesh_assets: ResMut<Assets<bevy::prelude::Mesh>>,
                                                  assets: Res<AssetServer>,
                                                  | {
                                                let obj = impeller2_wkt::Object3DMesh::Glb(gltf_path.trim().to_string());

                                                crate::object_3d::create_object_3d_entity(
                                                    &mut commands,
                                                    Object3D { eql: eql.clone(), mesh: obj, aux: () },
                                                    expr.clone(),
                                                    &mut material_assets,
                                                    &mut mesh_assets,
                                                    &assets
                                                );

                                                PaletteEvent::Exit
                                            },
                                        ).default()
                                    ])
                                    .prompt("Enter GLTF path")
                                    .into()
                                }
                            },
                        ),
                        PaletteItem::new(
                            "Sphere",
                            "",
                            {
                                let eql = eql.clone();
                                let expr = expr.clone();
                                move |_: In<String>| {
                                    let eql = eql.clone();
                                    let expr = expr.clone();
                                    PalettePage::new(vec![
                                        PaletteItem::new(
                                            LabelSource::placeholder("Enter radius (default: 1.0)"),
                                            "Enter the radius for the sphere",
                                            move |In(radius_str): In<String>| {
                                                let radius = radius_str.trim().parse::<f32>().unwrap_or(1.0);
                                                create_object_3d_with_color(
                                                    eql.clone(),
                                                    expr.clone(),
                                                    Mesh::sphere(radius)
                                                )
                                            },
                                        ).default()
                                    ])
                                    .prompt("Enter sphere radius")
                                    .into()
                                }
                            },
                        ),
                        PaletteItem::new(
                            "Cylinder",
                            "",
                            {
                                let eql = eql.clone();
                                let expr = expr.clone();
                                move |_: In<String>| {
                                    let eql = eql.clone();
                                    let expr = expr.clone();
                                    PalettePage::new(vec![
                                        PaletteItem::new(
                                            LabelSource::placeholder("Enter radius and height (default: 1.0 2.0)"),
                                            "Enter the radius and height for the cylinder",
                                            move |In(dimensions_str): In<String>| {
                                                let parts: Vec<f32> = dimensions_str
                                                    .split_whitespace()
                                                    .filter_map(|s| s.parse().ok())
                                                    .collect();

                                                let (radius, height) = match parts.as_slice() {
                                                    [r, h] => (*r, *h),
                                                    [r] => (*r, *r * 2.0),
                                                    _ => (1.0, 2.0),
                                                };

                                                create_object_3d_with_color(
                                                    eql.clone(),
                                                    expr.clone(),
                                                    Mesh::Cylinder { radius, height }
                                                )
                                            },
                                        ).default()
                                    ])
                                    .prompt("Enter cylinder dimensions")
                                    .into()
                                }
                            },
                        ),
                        PaletteItem::new(
                            "Cuboid",
                            "",
                            {
                                let eql = eql.clone();
                                let expr = expr.clone();
                                move |_: In<String>| {
                                    let eql = eql.clone();
                                    let expr = expr.clone();
                                    PalettePage::new(vec![
                                        PaletteItem::new(
                                            LabelSource::placeholder("Enter dimensions (x y z, default: 1 1 1)"),
                                            "Enter the dimensions for the cuboid (width height depth)",
                                            move |In(dimensions): In<String>| {
                                                let parts: Vec<f32> = dimensions
                                                    .split_whitespace()
                                                    .filter_map(|s| s.parse().ok())
                                                    .collect();

                                                let (x, y, z) = match parts.as_slice() {
                                                    [x, y, z] => (*x, *y, *z),
                                                    [x] => (*x, *x, *x),
                                                    _ => (1.0, 1.0, 1.0),
                                                };

                                                create_object_3d_with_color(
                                                    eql.clone(),
                                                    expr.clone(),
                                                    Mesh::cuboid(x, y, z)
                                                )
                                            },
                                        ).default()
                                    ])
                                    .prompt("Enter cuboid dimensions")
                                    .into()
                                }
                            },
                        ),
                    ])
                    .prompt("Choose 3D object visualization type")
                    .into()
                },
            ).default()
        ])
        .prompt("Enter EQL expression for 3D object positioning")
        .into()
    })
}

pub fn create_tiles(tile_id: TileId) -> PalettePage {
    PalettePage::new(vec![
        create_graph(Some(tile_id)),
        create_action(Some(tile_id)),
        create_monitor(Some(tile_id)),
        create_viewport(Some(tile_id)),
        create_query_table(Some(tile_id)),
        create_query_plot(Some(tile_id)),
        create_video_stream(Some(tile_id)),
        create_map(Some(tile_id)),
        create_hierarchy(Some(tile_id)),
        create_schematic_tree(Some(tile_id)),
        create_dashboard(Some(tile_id)),
        create_inspector(Some(tile_id)),
        create_sidebars(),
    ])
}

impl Default for PalettePage {
    fn default() -> PalettePage {
        PalettePage::new(vec![
            PaletteItem::new(
                "Toggle Wireframe",
                VIEWPORT_LABEL,
                |_: In<String>, mut wireframe: ResMut<WireframeConfig>| {
                    wireframe.global = !wireframe.global;
                    PaletteEvent::Exit
                },
            ),
            PaletteItem::new(
                "Toggle HDR",
                VIEWPORT_LABEL,
                |_: In<String>, mut hdr: ResMut<HdrEnabled>| {
                    hdr.0 = !hdr.0;
                    PaletteEvent::Exit
                },
            ),
            PaletteItem::new(
                "Toggle Grid",
                VIEWPORT_LABEL,
                |_: In<String>, mut grid_visibility: Query<&mut Visibility, With<InfiniteGrid>>| {
                    let all_hidden = grid_visibility
                        .iter()
                        .all(|grid_visibility| grid_visibility == Visibility::Hidden);

                    for mut grid_visibility in grid_visibility.iter_mut() {
                        *grid_visibility = if all_hidden {
                            Visibility::Visible
                        } else {
                            Visibility::Hidden
                        };
                    }

                    PaletteEvent::Exit
                },
            ),
            toggle_body_axes(),
            PaletteItem::new(
                "Toggle Recording",
                SIMULATION_LABEL,
                |_: In<String>, packet_tx: Res<PacketTx>, mut simulating: ResMut<IsRecording>| {
                    simulating.0 = !simulating.0;
                    packet_tx.send_msg(SetDbConfig {
                        recording: Some(simulating.0),
                        ..Default::default()
                    });
                    PaletteEvent::Exit
                },
            ),
            set_playback_speed(),
            fix_current_time_range(),
            set_time_range_behavior(),
            create_graph(None),
            create_action(None),
            create_monitor(None),
            create_viewport(None),
            create_query_table(None),
            create_query_plot(None),
            create_video_stream(None),
            create_map(None),
            create_hierarchy(None),
            create_inspector(None),
            create_schematic_tree(None),
            create_dashboard(None),
            create_sidebars(),
            create_3d_object(),
            save_schematic(),
            save_schematic_db(),
            load_schematic(),
            set_color_scheme(),
            update_component(),
        ])
    }
}
