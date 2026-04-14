/// Type registry mapping Facet types to widget factories and row builders.
///
/// The registry has two layers:
/// - **Field widget factories**: produce a single `InspectorRow` for a field
///   of a given type (e.g., `Hsla` → ColorRow, `ComponentId` → component picker)
/// - **Type row builders**: produce the full row set for an entity type,
///   replacing the default reflect walk (e.g., Trace → reflected rows + Remove)
use std::any::TypeId;
use std::collections::HashMap;
use std::sync::Arc;

use facet::{ConstTypeId, Facet, Peek, Poke};
use gpui::{AnyEntity, App, AppContext, Entity, Global, Hsla, SharedString};
use metor_db::DB;
use metor_proto::types::ComponentId;

use crate::elements::time_series::{TimeSeriesPlot, Trace};
use crate::elements::viewer_3d::Viewer3d;
use crate::widgets::{
    BoolRow, ColorRow, CommandRow, EnumRow, InspectorRow, NavRow, ScalarRow, SliderRow, TextRow,
};

/// Context passed to field widget factories.
pub struct FieldBuildCtx<'a> {
    pub db: &'a Arc<DB>,
    pub label: SharedString,
    pub field_name: &'static str,
}

/// Factory that produces a single InspectorRow for a field of a given type.
/// Receives the owning entity + field index so it can build typed read/write
/// callbacks via [`crate::reflect::get_field`] / [`crate::reflect::set_field`].
pub type FieldWidgetFactory =
    Arc<dyn Fn(&FieldBuildCtx, &Peek<'_, '_>, AnyEntity, usize) -> Box<dyn InspectorRow>>;

/// Builder that produces the full row set for an entity type.
pub type TypeRowBuilder = Arc<dyn Fn(AnyEntity, &Arc<DB>, &App) -> Vec<Box<dyn InspectorRow>>>;

/// Visitor type used by [`EntityAdapter::peek`].
type PeekAdapterVisitor<'v> = dyn FnMut(&Peek<'_, 'static>) + 'v;
/// Visitor type used by [`EntityAdapter::poke`].
type PokeAdapterVisitor<'v> = dyn FnMut(Poke<'_, 'static>) + 'v;

/// Adapter letting the reflection walker go from an [`AnyEntity`] to a typed
/// [`Peek`] or [`Poke`] without the call site knowing the concrete type.
pub struct EntityAdapter {
    pub(crate) peek: Arc<
        dyn for<'a, 'v> Fn(&'a AnyEntity, &'a App, &mut PeekAdapterVisitor<'v>),
    >,
    pub(crate) poke: Arc<
        dyn for<'v> Fn(&AnyEntity, &mut App, &mut PokeAdapterVisitor<'v>),
    >,
    pub shape_id: ConstTypeId,
}

/// Per-field metadata that can't be expressed via Facet attributes.
#[derive(Clone)]
pub struct FieldOverride {
    pub range: Option<(f64, f64)>,
}

/// Handler that builds a NavRow for a `Vec<Entity<T>>` field, given the
/// parent entity, field label, and db.
pub type EntityListHandler =
    Arc<dyn Fn(gpui::AnyEntity, SharedString, &Arc<DB>, &App) -> Box<dyn InspectorRow>>;

/// How the "Add" button behaves in a `Vec<Entity<T>>` list.
///
/// `Default` creates an item immediately. `Wizard` cascades to a
/// multi-step page that gathers required fields before creating.
pub enum AddBehavior<T: 'static> {
    /// Instant creation with a default value factory.
    Default(Arc<dyn Fn(&mut App) -> T>),
    /// Multi-step wizard. Receives the parent entity so the wizard
    /// can push the created item into the list.
    Wizard(Arc<dyn Fn(AnyEntity, &Arc<DB>, &App) -> Vec<Box<dyn InspectorRow>>>),
}

impl<T: 'static> Clone for AddBehavior<T> {
    fn clone(&self) -> Self {
        match self {
            Self::Default(f) => Self::Default(f.clone()),
            Self::Wizard(f) => Self::Wizard(f.clone()),
        }
    }
}

pub struct WidgetRegistry {
    field_widgets: HashMap<ConstTypeId, FieldWidgetFactory>,
    type_builders: HashMap<TypeId, TypeRowBuilder>,
    field_overrides: HashMap<(ConstTypeId, &'static str), FieldOverride>,
    /// Handlers for Vec<Entity<T>> fields, keyed by the ConstTypeId of
    /// Vec<Entity<T>> (the field type, not the item type).
    entity_list_handlers: HashMap<ConstTypeId, EntityListHandler>,
    /// Typed `AnyEntity` → `Peek`/`Poke` bridges, keyed by the gpui `TypeId`
    /// of the entity's inner value. Populated alongside any Facet-bounded
    /// registration (`register_field_override`, `register_entity_list`) and
    /// directly via [`Self::register_inspectable`].
    entity_adapters: HashMap<TypeId, Arc<EntityAdapter>>,
}

impl Global for WidgetRegistry {}

impl WidgetRegistry {
    pub fn init(db: Arc<DB>, cx: &mut App) {
        let mut reg = Self {
            field_widgets: HashMap::new(),
            type_builders: HashMap::new(),
            field_overrides: HashMap::new(),
            entity_list_handlers: HashMap::new(),
            entity_adapters: HashMap::new(),
        };

        reg.register_defaults(db);
        cx.set_global(reg);
    }

    pub fn field_widget(&self, type_id: ConstTypeId) -> Option<&FieldWidgetFactory> {
        self.field_widgets.get(&type_id)
    }

    pub fn type_builder(&self, type_id: TypeId) -> Option<&TypeRowBuilder> {
        self.type_builders.get(&type_id)
    }

    pub fn field_override(
        &self,
        type_id: ConstTypeId,
        field_name: &'static str,
    ) -> Option<&FieldOverride> {
        self.field_overrides.get(&(type_id, field_name))
    }

    pub fn register_field_widget<T: Facet<'static>>(&mut self, factory: FieldWidgetFactory) {
        self.field_widgets.insert(T::SHAPE.id, factory);
    }

    /// Register a custom override that replaces the default adapter-driven
    /// walk for this entity type. Used for types that need extra non-Facet
    /// rows (e.g. Viewer3d) or a completely custom page (e.g. DashboardPanel).
    pub fn register_type_builder<T: 'static>(&mut self, builder: TypeRowBuilder) {
        self.type_builders.insert(TypeId::of::<T>(), builder);
    }

    /// Register an `AnyEntity`→`Peek`/`Poke` adapter for a Facet type so the
    /// reflection walker can inspect it without knowing its concrete `T`.
    pub fn register_inspectable<T: Facet<'static> + 'static>(&mut self) {
        let peek: Arc<
            dyn for<'a, 'v> Fn(&'a AnyEntity, &'a App, &mut PeekAdapterVisitor<'v>),
        > = Arc::new(|any_entity, cx, visit| {
            let Ok(entity) = any_entity.clone().downcast::<T>() else {
                return;
            };
            let value = entity.read(cx);
            let peek = Peek::new(value);
            visit(&peek);
        });
        let poke: Arc<
            dyn for<'v> Fn(&AnyEntity, &mut App, &mut PokeAdapterVisitor<'v>),
        > = Arc::new(|any_entity, cx, visit| {
            let Ok(entity) = any_entity.clone().downcast::<T>() else {
                return;
            };
            entity.update(cx, |target, _cx| visit(Poke::new(target)));
        });
        self.entity_adapters.insert(
            TypeId::of::<T>(),
            Arc::new(EntityAdapter {
                peek,
                poke,
                shape_id: T::SHAPE.id,
            }),
        );
    }

    pub fn entity_adapter(&self, type_id: TypeId) -> Option<&Arc<EntityAdapter>> {
        self.entity_adapters.get(&type_id)
    }

    pub fn entity_list_handler(&self, field_type_id: ConstTypeId) -> Option<&EntityListHandler> {
        self.entity_list_handlers.get(&field_type_id)
    }

    /// How the "Add" button behaves in an entity list.
    /// Register a handler for a `Vec<Entity<T>>` field type.
    /// When the walker sees this field type, it delegates to this handler
    /// instead of the generic scalar/enum/struct fallback.
    pub fn register_entity_list<
        ParentT: Facet<'static> + 'static,
        ItemT: Facet<'static> + 'static,
    >(
        &mut self,
        db: Arc<DB>,
        get_list: fn(&ParentT) -> &Vec<Entity<ItemT>>,
        get_list_mut: fn(&mut ParentT) -> &mut Vec<Entity<ItemT>>,
        add_behavior: AddBehavior<ItemT>,
    ) {
        self.register_inspectable::<ParentT>();
        self.register_inspectable::<ItemT>();
        let field_type_id = <Vec<Entity<ItemT>> as Facet>::SHAPE.id;
        self.entity_list_handlers.insert(
            field_type_id,
            Arc::new(move |any_entity, label, db, cx| {
                let parent: Entity<ParentT> = any_entity.downcast().expect("parent type mismatch");
                let items: Vec<Entity<ItemT>> = get_list(parent.read(cx)).clone();
                let item_count = items.len();
                let db = db.clone();
                let parent_for_add = parent.clone();
                let add_behavior = add_behavior.clone();

                Box::new(NavRow {
                    label,
                    summary: SharedString::from(format!("{} items", item_count)),
                    build_children: Arc::new(move |cx| {
                        let mut rows: Vec<Box<dyn InspectorRow>> = items
                            .iter()
                            .enumerate()
                            .map(|(i, entity)| {
                                let inner = entity.read(cx);
                                let item_label =
                                    find_label_field::<ItemT>(inner).unwrap_or_else(|| {
                                        SharedString::from(format!("Item {}", i + 1))
                                    });
                                let entity = entity.clone();
                                let db = db.clone();
                                let parent_for_remove = parent_for_add.clone();
                                let idx = i;

                                Box::new(NavRow {
                                    label: item_label,
                                    summary: SharedString::new_static(""),
                                    build_children: Arc::new(move |cx| {
                                        let mut sub_rows =
                                            crate::reflect::rows_for_entity(&entity, &db, cx);
                                        let remove_parent = parent_for_remove.clone();
                                        sub_rows.push(Box::new(CommandRow {
                                            label: "Remove".into(),
                                            callback: Arc::new(move |_w, cx| {
                                                remove_parent.update(cx, |p, cx| {
                                                    let list = get_list_mut(p);
                                                    if idx < list.len() {
                                                        list.remove(idx);
                                                    }
                                                    cx.notify();
                                                });
                                            }),
                                        }));
                                        sub_rows
                                    }),
                                }) as Box<dyn InspectorRow>
                            })
                            .collect();

                        // "Add" row — either instant-create or wizard cascade
                        match &add_behavior {
                            AddBehavior::Default(factory) => {
                                let add_parent = parent_for_add.clone();
                                let factory = factory.clone();
                                rows.push(Box::new(CommandRow {
                                    label: "Add".into(),
                                    callback: Arc::new(move |_w, cx| {
                                        let item = factory(cx);
                                        let entity = cx.new(|_| item);
                                        add_parent.update(cx, |p, cx| {
                                            get_list_mut(p).push(entity);
                                            cx.notify();
                                        });
                                    }),
                                }));
                            }
                            AddBehavior::Wizard(wizard) => {
                                let add_parent = parent_for_add.clone();
                                let db = db.clone();
                                let wizard = wizard.clone();
                                rows.push(Box::new(NavRow {
                                    label: "Add".into(),
                                    summary: SharedString::new_static(""),
                                    build_children: Arc::new(move |cx| {
                                        wizard(add_parent.clone().into_any(), &db, cx)
                                    }),
                                }));
                            }
                        }

                        rows
                    }),
                })
            }),
        );
    }

    pub fn register_field_override<T: Facet<'static> + 'static>(
        &mut self,
        field_name: &'static str,
        over: FieldOverride,
    ) {
        self.register_inspectable::<T>();
        self.field_overrides.insert((T::SHAPE.id, field_name), over);
    }

    fn register_defaults(&mut self, db: Arc<DB>) {
        self.register_hsla();
        self.register_shared_string();
        self.register_component_id(db.clone());
        self.register_inspectable::<crate::elements::Monitor>();
        self.register_entity_list::<TimeSeriesPlot, Trace>(
            db.clone(),
            |tsp| &tsp.traces,
            |tsp| &mut tsp.traces,
            AddBehavior::Wizard(Arc::new(|parent, db, cx| {
                build_trace_add_wizard(parent, db, cx)
            })),
        );
        self.register_entity_list::<Viewer3d, crate::elements::viewer_3d::ModelEntry>(
            db.clone(),
            |v| &v.models,
            |v| &mut v.models,
            AddBehavior::Default(Arc::new(|_cx| {
                crate::elements::viewer_3d::ModelEntry::empty()
            })),
        );
        self.register_viewer3d_builder(db.clone());
        self.register_dashboard_builder(db);
        self.register_field_override::<crate::elements::time_series::Trace>(
            "stroke_width",
            FieldOverride {
                range: Some((0.5, 10.0)),
            },
        );
        self.register_field_override::<crate::elements::viewer_3d::Viewer3d>(
            "camera_fov",
            FieldOverride {
                range: Some((0.1, 3.14)),
            },
        );
    }

    fn register_hsla(&mut self) {
        self.register_field_widget::<Hsla>(Arc::new(|ctx, peek, any_entity, idx| {
            let color = *peek.get::<Hsla>().unwrap();
            let label = ctx.label.clone();
            let read_entity = any_entity.clone();
            Box::new(ColorRow {
                label,
                color,
                read_color: Arc::new(move |cx| {
                    crate::reflect::get_field::<Hsla>(&read_entity, idx, cx).unwrap_or(color)
                }),
                on_change: Arc::new(move |c, _w, cx| {
                    crate::reflect::set_field::<Hsla>(&any_entity, idx, c, cx);
                }),
            })
        }));
    }

    fn register_shared_string(&mut self) {
        self.register_field_widget::<SharedString>(Arc::new(|ctx, peek, any_entity, idx| {
            let value = peek.get::<SharedString>().unwrap().clone();
            let label = ctx.label.clone();
            Box::new(TextRow {
                label,
                value,
                on_change: Arc::new(move |s, _w, cx| {
                    crate::reflect::set_field::<SharedString>(
                        &any_entity,
                        idx,
                        SharedString::from(s),
                        cx,
                    );
                }),
            })
        }));
    }

    fn register_component_id(&mut self, db: Arc<DB>) {
        let db = db.clone();
        self.register_field_widget::<ComponentId>(Arc::new(move |ctx, peek, any_entity, idx| {
            let current = *peek.get::<ComponentId>().unwrap();
            let current_name = db.with_state(|s| {
                s.get_component_metadata(current)
                    .map(|m| SharedString::from(m.name.clone()))
            });
            let label = ctx.label.clone();
            let db = ctx.db.clone();
            Box::new(NavRow {
                label,
                summary: current_name.unwrap_or_else(|| SharedString::from(format!("{}", current))),
                build_children: Arc::new(move |cx| {
                    build_component_picker(&db, any_entity.clone(), idx, cx)
                }),
            })
        }));
    }

    fn register_viewer3d_builder(&mut self, _db: Arc<DB>) {
        self.register_type_builder::<Viewer3d>(Arc::new(|any_entity, db, cx| {
            let viewer: Entity<Viewer3d> = any_entity
                .clone()
                .downcast()
                .expect("Viewer3d type mismatch");
            let mut rows = crate::reflect::default_rows_for_any_entity(&any_entity, db, cx);
            let add_viewer = viewer.clone();
            rows.push(Box::new(CommandRow {
                label: "Add Model".into(),
                callback: Arc::new(move |_w, cx| {
                    add_viewer.update(cx, |v, cx| v.add_model("", "", cx));
                }),
            }));
            let reset_viewer = viewer.clone();
            rows.push(Box::new(CommandRow {
                label: "Reset Camera".into(),
                callback: Arc::new(move |_w, cx| {
                    reset_viewer.update(cx, |v, cx| v.reset_camera(cx));
                }),
            }));
            rows
        }));
    }

    fn register_dashboard_builder(&mut self, _db: Arc<DB>) {
        self.register_type_builder::<crate::tiles::dashboard::DashboardPanel>(Arc::new(
            |any_entity, db, cx| {
                let entity: Entity<crate::tiles::dashboard::DashboardPanel> =
                    any_entity.downcast().expect("DashboardPanel type mismatch");
                let page = crate::tiles::dashboard::dashboard_palette_page(entity, db.clone(), cx);
                crate::inspector::palette_page_to_rows(page)
            },
        ));
    }
}

/// Extract the first SharedString field from a Facet struct as a display label.
fn find_label_field<T: Facet<'static>>(value: &T) -> Option<SharedString> {
    let peek = facet::Peek::new(value);
    let peek_struct = peek.into_struct().ok()?;
    for (i, field_def) in peek_struct.ty().fields.iter().enumerate() {
        if field_def.shape().id == <SharedString as Facet>::SHAPE.id {
            if let Ok(field_peek) = peek_struct.field(i) {
                if let Ok(s) = field_peek.get::<SharedString>() {
                    if !s.is_empty() {
                        return Some(s.clone());
                    }
                }
            }
        }
    }
    None
}

fn build_component_picker(
    db: &Arc<DB>,
    any_entity: AnyEntity,
    idx: usize,
    _cx: &App,
) -> Vec<Box<dyn InspectorRow>> {
    let components = crate::trace_picker::list_components(db);
    components
        .into_iter()
        .map(|(id, name)| {
            let any_entity = any_entity.clone();
            Box::new(CommandRow {
                label: SharedString::from(name),
                callback: Arc::new(move |_w, cx| {
                    crate::reflect::set_field::<ComponentId>(&any_entity, idx, id, cx);
                }),
            }) as Box<dyn InspectorRow>
        })
        .collect()
}

/// Build a trace construction wizard: component picker → element picker → create trace.
fn build_trace_add_wizard(
    parent: gpui::AnyEntity,
    db: &Arc<DB>,
    _cx: &App,
) -> Vec<Box<dyn InspectorRow>> {
    let components = crate::trace_picker::list_components(db);
    let db = db.clone();

    components
        .into_iter()
        .map(|(comp_id, comp_name)| {
            let parent = parent.clone();
            let db = db.clone();
            let comp_name = comp_name.clone();

            Box::new(NavRow {
                label: SharedString::from(comp_name.clone()),
                summary: SharedString::new_static(""),
                build_children: Arc::new(move |cx| {
                    let elem_names = crate::trace_picker::element_names_for_component(&db, comp_id);
                    let elem_names = if elem_names.is_empty() {
                        vec!["value".to_string()]
                    } else {
                        elem_names
                    };

                    let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();

                    // "All" option — adds every element as a trace at once
                    if elem_names.len() > 1 {
                        let parent = parent.clone();
                        let comp_name = comp_name.clone();
                        let elem_count = elem_names.len();
                        let names = elem_names.clone();
                        rows.push(Box::new(CommandRow {
                            label: SharedString::from(format!("{} (all)", comp_name)),
                            callback: Arc::new(move |_w, cx| {
                                let parent: Entity<TimeSeriesPlot> =
                                    parent.clone().downcast().expect("parent type mismatch");
                                let theme = crate::theme::theme(cx);
                                let base_idx = parent.read(cx).traces.len();
                                let mut new_entities = Vec::with_capacity(elem_count);
                                for (idx, elem_name) in names.iter().enumerate() {
                                    let color = theme.line_colors
                                        [(base_idx + idx) % theme.line_colors.len()];
                                    let display = if elem_name.is_empty() {
                                        format!("[{}]", idx)
                                    } else {
                                        elem_name.clone()
                                    };
                                    let mut trace = Trace::new(comp_id, idx, color);
                                    trace.label =
                                        SharedString::from(format!("{}.{}", comp_name, display));
                                    new_entities.push(cx.new(|_| trace));
                                }
                                parent.update(cx, |tsp, cx| {
                                    tsp.traces.extend(new_entities);
                                    cx.notify();
                                });
                            }),
                        }));
                    }

                    // Individual element options
                    for (idx, elem_name) in elem_names.into_iter().enumerate() {
                        let parent = parent.clone();
                        let comp_name = comp_name.clone();
                        let display = if elem_name.is_empty() {
                            format!("[{}]", idx)
                        } else {
                            elem_name
                        };
                        let label_text = format!("{}.{}", comp_name, display);

                        rows.push(Box::new(CommandRow {
                            label: SharedString::from(label_text),
                            callback: Arc::new(move |_w, cx| {
                                let parent: Entity<TimeSeriesPlot> =
                                    parent.clone().downcast().expect("parent type mismatch");
                                let theme = crate::theme::theme(cx);
                                let color_idx = parent.read(cx).traces.len();
                                let color = theme.line_colors[color_idx % theme.line_colors.len()];
                                let mut trace = Trace::new(comp_id, idx, color);
                                trace.label =
                                    SharedString::from(format!("{}.{}", comp_name, display));
                                let entity = cx.new(|_| trace);
                                parent.update(cx, |tsp, cx| {
                                    tsp.traces.push(entity);
                                    cx.notify();
                                });
                            }),
                        }));
                    }

                    rows
                }),
            }) as Box<dyn InspectorRow>
        })
        .collect()
}
