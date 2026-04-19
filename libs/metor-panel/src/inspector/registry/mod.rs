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

use facet::{ConstTypeId, Facet, Peek, Poke};
use gpui::{AnyEntity, App, AppContext, Entity, Global, SharedString};
use metor_db::DB;

use crate::inspector::rows::{CommandRow, InspectorRow, NavRow};

mod builders;
mod defaults;
mod dispatch;

/// Context passed to field widget factories.
pub struct FieldBuildCtx<'a> {
    pub db: &'a Arc<DB>,
    pub label: SharedString,
    pub field_name: &'static str,
}

/// Factory that produces a single InspectorRow for a field of a given type.
/// Receives the owning entity + field index so it can build typed read/write
/// callbacks via [`crate::inspector::reflect::get_field`] / [`crate::inspector::reflect::set_field`].
pub type FieldWidgetFactory =
    Arc<dyn Fn(&FieldBuildCtx, &Peek<'_, '_>, AnyEntity, usize) -> Box<dyn InspectorRow>>;

/// Builder that produces the full row set for an entity type.
pub type TypeRowBuilder = Arc<dyn Fn(AnyEntity, &Arc<DB>, &App) -> Vec<Box<dyn InspectorRow>>>;

/// Visitor type used by [`EntityAdapter::peek`].
pub(crate) type PeekAdapterVisitor<'v> = dyn FnMut(&Peek<'_, 'static>) + 'v;
/// Visitor type used by [`EntityAdapter::poke`].
pub(crate) type PokeAdapterVisitor<'v> = dyn FnMut(Poke<'_, 'static>) + 'v;

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
                                let item_label = builders::find_label_field::<ItemT>(inner)
                                    .unwrap_or_else(|| {
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
                                        let mut sub_rows = crate::inspector::reflect::rows_for_entity(
                                            &entity, &db, cx,
                                        );
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
}
