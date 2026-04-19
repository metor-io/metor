/// Registry mapping Facet types to inspector row factories and entity adapters.
///
/// The registry has two layers:
/// - **Field widget factories**: produce a single `InspectorRow` for a field
///   of a given type (e.g., `Hsla` → ColorRow, `ComponentId` → component picker)
/// - **Type row builders**: produce the full row set for an entity type,
///   replacing the default reflect walk (e.g., Trace → reflected rows + Remove)
use std::any::TypeId;
use std::collections::HashMap;
use std::sync::Arc;

use facet::{ConstTypeId, Facet, Peek, Poke, ScalarType};
use gpui::{AnyEntity, App, AppContext, Entity, Global, Hsla, SharedString};
use metor_db::DB;
use metor_proto::types::ComponentId;

use crate::elements::time_series::{LinePlot, Override, Trace};
use crate::elements::viewer_3d::Viewer3d;
use crate::offset_parse::TimeRangeBehavior;
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
    pub(crate) peek: Arc<dyn for<'a, 'v> Fn(&'a AnyEntity, &'a App, &mut PeekAdapterVisitor<'v>)>,
    pub(crate) poke: Arc<dyn for<'v> Fn(&AnyEntity, &mut App, &mut PokeAdapterVisitor<'v>)>,
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

pub struct InspectorRegistry {
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

impl Global for InspectorRegistry {}

impl InspectorRegistry {
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
        let peek: Arc<dyn for<'a, 'v> Fn(&'a AnyEntity, &'a App, &mut PeekAdapterVisitor<'v>)> =
            Arc::new(|any_entity, cx, visit| {
                let Ok(entity) = any_entity.clone().downcast::<T>() else {
                    return;
                };
                let value = entity.read(cx);
                let peek = Peek::new(value);
                visit(&peek);
            });
        let poke: Arc<dyn for<'v> Fn(&AnyEntity, &mut App, &mut PokeAdapterVisitor<'v>)> =
            Arc::new(|any_entity, cx, visit| {
                let Ok(entity) = any_entity.clone().downcast::<T>() else {
                    return;
                };
                entity.update(cx, |target, cx| {
                    visit(Poke::new(target));
                    cx.notify();
                });
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
        _db: Arc<DB>,
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

                Box::new(NavRow::new(
                    label,
                    SharedString::from(format!("{} items", item_count)),
                    Box::new(move |cx| {
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

                                Box::new(NavRow::new(
                                    item_label,
                                    SharedString::new_static(""),
                                    Box::new(move |cx| {
                                        let mut sub_rows =
                                            crate::reflect::rows_for_entity(&entity, &db, cx);
                                        let remove_parent = parent_for_remove.clone();
                                        sub_rows.push(Box::new(CommandRow::new(
                                            "Remove",
                                            Arc::new(move |_w, cx| {
                                                remove_parent.update(cx, |p, cx| {
                                                    let list = get_list_mut(p);
                                                    if idx < list.len() {
                                                        list.remove(idx);
                                                    }
                                                    cx.notify();
                                                });
                                            }),
                                        )));
                                        sub_rows
                                    }),
                                )) as Box<dyn InspectorRow>
                            })
                            .collect();

                        // "Add" row — either instant-create or wizard cascade
                        match &add_behavior {
                            AddBehavior::Default(factory) => {
                                let add_parent = parent_for_add.clone();
                                let factory = factory.clone();
                                rows.push(Box::new(CommandRow::new(
                                    "Add",
                                    Arc::new(move |_w, cx| {
                                        let item = factory(cx);
                                        let entity = cx.new(|_| item);
                                        add_parent.update(cx, |p, cx| {
                                            get_list_mut(p).push(entity);
                                            cx.notify();
                                        });
                                    }),
                                )));
                            }
                            AddBehavior::Wizard(wizard) => {
                                let add_parent = parent_for_add.clone();
                                let db = db.clone();
                                let wizard = wizard.clone();
                                rows.push(Box::new(NavRow::new(
                                    "Add",
                                    SharedString::new_static(""),
                                    Box::new(move |cx| {
                                        wizard(add_parent.clone().into_any(), &db, cx)
                                    }),
                                )));
                            }
                        }

                        rows
                    }),
                ))
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

    /// Build a row for a single struct field. Consults registered overrides
    /// first (per-field-type widgets, entity-list handlers), then falls back
    /// to shape-based defaults via [`Self::default_row_for_shape`].
    pub fn row_for_field(
        &self,
        ctx: &FieldBuildCtx,
        peek: &Peek<'_, '_>,
        any_entity: &AnyEntity,
        field_idx: usize,
        parent_shape_id: ConstTypeId,
        cx: &App,
    ) -> Option<Box<dyn InspectorRow>> {
        let field_shape = peek.shape();

        if let Some(factory) = self.field_widget(field_shape.id) {
            return Some(factory(ctx, peek, any_entity.clone(), field_idx));
        }

        if let Some(handler) = self.entity_list_handler(field_shape.id) {
            return Some(handler(any_entity.clone(), ctx.label.clone(), ctx.db, cx));
        }

        let field_override = self.field_override(parent_shape_id, ctx.field_name);
        self.default_row_for_shape(ctx, peek, any_entity, field_idx, field_override)
    }

    /// Shape-based default row dispatch: bool, numeric scalars, `String`,
    /// `Option<ComponentId>`, and `enum`. Returns `None` for shapes outside
    /// this set so callers can choose to skip or fall through.
    pub fn default_row_for_shape(
        &self,
        ctx: &FieldBuildCtx,
        peek: &Peek<'_, '_>,
        any_entity: &AnyEntity,
        field_idx: usize,
        field_override: Option<&FieldOverride>,
    ) -> Option<Box<dyn InspectorRow>> {
        let shape = peek.shape();

        if shape.id == <bool as Facet>::SHAPE.id {
            let val = *peek.get::<bool>().ok()?;
            let any_entity = any_entity.clone();
            return Some(Box::new(BoolRow {
                label: ctx.label.clone(),
                value: val,
                toggle: Arc::new(move |v, _w, cx| {
                    crate::reflect::set_field::<bool>(&any_entity, field_idx, v, cx)
                }),
            }));
        }

        if let Some(scalar) = peek.scalar_type() {
            if let Some(val) = scalar_as_f64(peek, scalar) {
                let label = ctx.label.clone();
                if let Some((min, max)) = field_override.and_then(|o| o.range) {
                    let write_entity = any_entity.clone();
                    let read_entity = any_entity.clone();
                    return Some(Box::new(SliderRow {
                        label,
                        read_value: Arc::new(move |cx| {
                            read_scalar(&read_entity, field_idx, scalar, cx)
                        }),
                        min,
                        max,
                        on_change: Arc::new(move |v, _w, cx| {
                            write_scalar(&write_entity, field_idx, scalar, v, cx);
                        }),
                    }));
                }
                let any_entity = any_entity.clone();
                return Some(Box::new(ScalarRow {
                    label,
                    value: val,
                    on_change: Arc::new(move |v, _w, cx| {
                        write_scalar(&any_entity, field_idx, scalar, v, cx);
                    }),
                }));
            }
        }

        if shape.id == <String as Facet>::SHAPE.id {
            let val = peek.get::<String>().ok()?.clone();
            let any_entity = any_entity.clone();
            return Some(Box::new(TextRow::new(
                ctx.label.clone(),
                SharedString::from(val),
                Arc::new(move |s, _w, cx| {
                    crate::reflect::set_field::<String>(&any_entity, field_idx, s, cx)
                }),
            )));
        }

        if let Ok(peek_option) = peek.clone().into_option() {
            return Some(build_option_row(
                ctx,
                peek_option,
                any_entity.clone(),
                field_idx,
            ));
        }

        if let Ok(peek_enum) = peek.clone().into_enum() {
            let selected = peek_enum
                .variant_name_active()
                .unwrap_or("unknown")
                .to_string();
            let options: Vec<SharedString> = peek_enum
                .variants()
                .iter()
                .map(|v| SharedString::from(v.name))
                .collect();
            let any_entity = any_entity.clone();
            return Some(Box::new(EnumRow {
                label: ctx.label.clone(),
                selected: SharedString::from(selected),
                options,
                on_select: Arc::new(move |name, _w, cx| {
                    crate::reflect::set_enum_variant(&any_entity, field_idx, &name, cx);
                }),
            }));
        }

        None
    }

    fn register_defaults(&mut self, db: Arc<DB>) {
        self.register_hsla();
        self.register_shared_string();
        self.register_component_id(db.clone());
        self.register_override_f64();
        self.register_override_shared_string();
        self.register_time_range_behavior();
        self.register_inspectable::<crate::elements::Monitor>();
        self.register_entity_list::<LinePlot, Trace>(
            db.clone(),
            |lp| &lp.traces,
            |lp| &mut lp.traces,
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
            Box::new(TextRow::new(
                label,
                value,
                Arc::new(move |s, _w, cx| {
                    crate::reflect::set_field::<SharedString>(
                        &any_entity,
                        idx,
                        SharedString::from(s),
                        cx,
                    );
                }),
            ))
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
            Box::new(NavRow::new(
                label,
                current_name.unwrap_or_else(|| SharedString::from(format!("{}", current))),
                Box::new(move |cx| build_component_picker(&db, any_entity.clone(), idx, cx)),
            ))
        }));
    }

    /// Widget for `Override<f64>`. Top-level row shows `Auto` or the number;
    /// cascading reveals a `ScalarRow` that writes `Custom(v)` and a command
    /// row that flips it back to `Auto`.
    fn register_override_f64(&mut self) {
        self.register_field_widget::<Override<f64>>(Arc::new(|ctx, peek, any_entity, idx| {
            let current = peek.get::<Override<f64>>().unwrap().clone();
            build_override_row(
                ctx.label.clone(),
                current,
                any_entity,
                idx,
                |v| SharedString::from(format!("{}", v)),
                |label, initial, any_entity, idx| {
                    let write = any_entity.clone();
                    Box::new(ScalarRow {
                        label,
                        value: initial,
                        on_change: Arc::new(move |v, _w, cx| {
                            crate::reflect::set_field::<Override<f64>>(
                                &write,
                                idx,
                                Override::Custom(v),
                                cx,
                            );
                        }),
                    })
                },
            )
        }));
    }

    /// Widget for `Override<SharedString>`. Mirrors [`Self::register_override_f64`]
    /// but uses a `TextRow` as the value editor.
    fn register_override_shared_string(&mut self) {
        self.register_field_widget::<Override<SharedString>>(Arc::new(
            |ctx, peek, any_entity, idx| {
                let current = peek.get::<Override<SharedString>>().unwrap().clone();
                build_override_row(
                    ctx.label.clone(),
                    current,
                    any_entity,
                    idx,
                    |s| s.clone(),
                    |label, initial, any_entity, idx| {
                        let write = any_entity.clone();
                        Box::new(TextRow::new(
                            label,
                            initial,
                            Arc::new(move |s, _w, cx| {
                                crate::reflect::set_field::<Override<SharedString>>(
                                    &write,
                                    idx,
                                    Override::Custom(SharedString::from(s)),
                                    cx,
                                );
                            }),
                        ))
                    },
                )
            },
        ));
    }

    /// Widget for `TimeRangeBehavior`. Top-level row shows the current preset
    /// name (or the formatted range when off-preset); cascading reveals a list
    /// of presets plus a free-form text field for arbitrary input.
    fn register_time_range_behavior(&mut self) {
        self.register_field_widget::<TimeRangeBehavior>(Arc::new(
            |ctx, peek, any_entity, idx| {
                let current = *peek.get::<TimeRangeBehavior>().unwrap();
                let summary = preset_label(&current)
                    .map(SharedString::from)
                    .unwrap_or_else(|| SharedString::from(format!("{}", current)));
                let label = ctx.label.clone();
                Box::new(NavRow::new(
                    label,
                    summary,
                    Box::new(move |cx| {
                        let current = crate::reflect::get_field::<TimeRangeBehavior>(
                            &any_entity, idx, cx,
                        )
                        .unwrap_or_default();
                        let mut rows: Vec<Box<dyn InspectorRow>> = TimeRangeBehavior::PRESETS
                            .iter()
                            .map(|(name, value)| {
                                let entity = any_entity.clone();
                                let value = *value;
                                Box::new(CommandRow::new(
                                    *name,
                                    Arc::new(move |_w, cx| {
                                        crate::reflect::set_field::<TimeRangeBehavior>(
                                            &entity, idx, value, cx,
                                        );
                                    }),
                                )) as Box<dyn InspectorRow>
                            })
                            .collect();
                        let entity = any_entity.clone();
                        rows.push(Box::new(TextRow::new(
                            SharedString::new_static("Custom"),
                            SharedString::from(format!("{}", current)),
                            Arc::new(move |s, _w, cx| {
                                if let Ok(value) = s.parse::<TimeRangeBehavior>() {
                                    crate::reflect::set_field::<TimeRangeBehavior>(
                                        &entity, idx, value, cx,
                                    );
                                }
                            }),
                        )));
                        rows
                    }),
                ))
            },
        ));
    }

    fn register_viewer3d_builder(&mut self, _db: Arc<DB>) {
        self.register_type_builder::<Viewer3d>(Arc::new(|any_entity, db, cx| {
            let viewer: Entity<Viewer3d> = any_entity
                .clone()
                .downcast()
                .expect("Viewer3d type mismatch");
            let mut rows = crate::reflect::default_rows_for_any_entity(&any_entity, db, cx);
            let add_viewer = viewer.clone();
            rows.push(Box::new(CommandRow::new(
                "Add Model",
                Arc::new(move |_w, cx| {
                    add_viewer.update(cx, |v, cx| v.add_model("", "", cx));
                }),
            )));
            let reset_viewer = viewer.clone();
            rows.push(Box::new(CommandRow::new(
                "Reset Camera",
                Arc::new(move |_w, cx| {
                    reset_viewer.update(cx, |v, cx| v.reset_camera(cx));
                }),
            )));
            rows
        }));
    }

    fn register_dashboard_builder(&mut self, _db: Arc<DB>) {
        self.register_type_builder::<crate::tiles::dashboard::DashboardPanel>(Arc::new(
            |any_entity, db, cx| {
                let entity: Entity<crate::tiles::dashboard::DashboardPanel> =
                    any_entity.downcast().expect("DashboardPanel type mismatch");
                crate::tiles::dashboard::dashboard_rows(entity, db.clone(), cx)
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
            Box::new(CommandRow::new(
                SharedString::from(name),
                Arc::new(move |_w, cx| {
                    crate::reflect::set_field::<ComponentId>(&any_entity, idx, id, cx);
                }),
            )) as Box<dyn InspectorRow>
        })
        .collect()
}

/// Return the preset display name if `value` matches one of the
/// [`TimeRangeBehavior::PRESETS`].
fn preset_label(value: &TimeRangeBehavior) -> Option<&'static str> {
    TimeRangeBehavior::PRESETS
        .iter()
        .find(|(_, preset)| preset == value)
        .map(|(name, _)| *name)
}

/// Construct the top-level `NavRow` for an `Override<T>` field. Its summary
/// reflects the current variant, and cascading reveals a value editor plus an
/// "Auto" command row.
fn build_override_row<T>(
    label: SharedString,
    current: Override<T>,
    any_entity: AnyEntity,
    idx: usize,
    fmt_custom: impl Fn(&T) -> SharedString + 'static,
    value_row: impl Fn(SharedString, T, AnyEntity, usize) -> Box<dyn InspectorRow> + 'static,
) -> Box<dyn InspectorRow>
where
    T: Facet<'static> + Clone + Default + 'static,
{
    let summary = match &current {
        Override::Auto => SharedString::new_static("Auto"),
        Override::Custom(v) => fmt_custom(v),
    };
    let label_for_value = label.clone();
    Box::new(NavRow::new(
        label,
        summary,
        Box::new(move |cx| {
            let initial = crate::reflect::get_field::<Override<T>>(&any_entity, idx, cx)
                .unwrap_or(Override::Auto);
            let value_initial = match &initial {
                Override::Custom(v) => v.clone(),
                Override::Auto => T::default(),
            };
            let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();
            rows.push(value_row(
                label_for_value.clone(),
                value_initial,
                any_entity.clone(),
                idx,
            ));
            let auto_entity = any_entity.clone();
            rows.push(Box::new(CommandRow::new(
                "Auto",
                Arc::new(move |_w, cx| {
                    crate::reflect::set_field::<Override<T>>(&auto_entity, idx, Override::Auto, cx);
                }),
            )));
            rows
        }),
    ))
}

/// Build an `Option<T>` row. Currently only `Option<ComponentId>` is wired —
/// other inner types render as a `None`/`Set` summary with no picker.
fn build_option_row(
    ctx: &FieldBuildCtx,
    peek_option: facet::PeekOption<'_, '_>,
    any_entity: AnyEntity,
    field_idx: usize,
) -> Box<dyn InspectorRow> {
    let label = ctx.label.clone();
    let is_some = peek_option.is_some();
    let inner_shape = peek_option.def().t;
    let db = ctx.db.clone();

    let summary = if is_some {
        SharedString::from("Set")
    } else {
        SharedString::from("None")
    };

    Box::new(NavRow::new(
        label,
        summary,
        Box::new(move |_cx| {
            let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();

            if inner_shape.id != <ComponentId as Facet>::SHAPE.id {
                return rows;
            }

            if is_some {
                let any_entity = any_entity.clone();
                rows.push(Box::new(CommandRow::new(
                    "Clear",
                    Arc::new(move |_w, cx| {
                        crate::reflect::set_field::<Option<ComponentId>>(
                            &any_entity, field_idx, None, cx,
                        );
                    }),
                )));
            } else {
                for (id, name) in crate::trace_picker::list_components(&db) {
                    let any_entity = any_entity.clone();
                    rows.push(Box::new(CommandRow::new(
                        SharedString::from(name),
                        Arc::new(move |_w, cx| {
                            crate::reflect::set_field::<Option<ComponentId>>(
                                &any_entity,
                                field_idx,
                                Some(id),
                                cx,
                            );
                        }),
                    )));
                }
            }

            rows
        }),
    ))
}

fn scalar_as_f64(peek: &Peek<'_, '_>, scalar: ScalarType) -> Option<f64> {
    match scalar {
        ScalarType::F32 => peek.get::<f32>().ok().map(|v| *v as f64),
        ScalarType::F64 => peek.get::<f64>().ok().copied(),
        ScalarType::I32 => peek.get::<i32>().ok().map(|v| *v as f64),
        ScalarType::I64 => peek.get::<i64>().ok().map(|v| *v as f64),
        ScalarType::U32 => peek.get::<u32>().ok().map(|v| *v as f64),
        ScalarType::U64 => peek.get::<u64>().ok().map(|v| *v as f64),
        _ => None,
    }
}

fn read_scalar(any_entity: &AnyEntity, idx: usize, scalar: ScalarType, cx: &App) -> f64 {
    match scalar {
        ScalarType::F32 => crate::reflect::get_field::<f32>(any_entity, idx, cx).map(|v| v as f64),
        ScalarType::F64 => crate::reflect::get_field::<f64>(any_entity, idx, cx),
        ScalarType::I32 => crate::reflect::get_field::<i32>(any_entity, idx, cx).map(|v| v as f64),
        ScalarType::I64 => crate::reflect::get_field::<i64>(any_entity, idx, cx).map(|v| v as f64),
        ScalarType::U32 => crate::reflect::get_field::<u32>(any_entity, idx, cx).map(|v| v as f64),
        ScalarType::U64 => crate::reflect::get_field::<u64>(any_entity, idx, cx).map(|v| v as f64),
        _ => None,
    }
    .unwrap_or(0.0)
}

fn write_scalar(any_entity: &AnyEntity, idx: usize, scalar: ScalarType, v: f64, cx: &mut App) {
    match scalar {
        ScalarType::F32 => crate::reflect::set_field::<f32>(any_entity, idx, v as f32, cx),
        ScalarType::F64 => crate::reflect::set_field::<f64>(any_entity, idx, v, cx),
        ScalarType::I32 => crate::reflect::set_field::<i32>(any_entity, idx, v as i32, cx),
        ScalarType::I64 => crate::reflect::set_field::<i64>(any_entity, idx, v as i64, cx),
        ScalarType::U32 => crate::reflect::set_field::<u32>(any_entity, idx, v as u32, cx),
        ScalarType::U64 => crate::reflect::set_field::<u64>(any_entity, idx, v as u64, cx),
        _ => {}
    }
}

/// Build a trace construction wizard: component picker → element picker →
/// append the new trace(s) to the existing `LinePlot`'s `traces` list.
///
/// Color-index basis is the parent's current trace count, so newly added
/// traces continue the color palette where it left off.
fn build_trace_add_wizard(
    parent: gpui::AnyEntity,
    db: &Arc<DB>,
    _cx: &App,
) -> Vec<Box<dyn InspectorRow>> {
    let parent_for_basis = parent.clone();
    let color_basis: crate::trace_picker::ColorBasis = Arc::new(move |cx: &App| {
        parent_for_basis
            .clone()
            .downcast::<LinePlot>()
            .map(|p| p.read(cx).traces.len())
            .unwrap_or(0)
    });

    let on_select: crate::trace_picker::OnTracesSelected =
        Arc::new(move |traces, _w, cx| {
            let parent: Entity<LinePlot> =
                parent.clone().downcast().expect("parent type mismatch");
            let new_entities: Vec<_> = traces.into_iter().map(|t| cx.new(|_| t)).collect();
            parent.update(cx, |lp, cx| {
                lp.traces.extend(new_entities);
                cx.notify();
            });
        });

    crate::trace_picker::select_traces_wizard_rows(db.clone(), color_basis, on_select)
}
