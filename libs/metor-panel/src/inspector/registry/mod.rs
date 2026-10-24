//! Type-indexed overrides that the reflection walker consults while
//! turning an entity into an inspector page.
//!
//! Two layers coexist:
//!
//! - **Field widget factories** replace the default row for a single typed
//!   field (e.g., `Hsla` renders as a color swatch, `ComponentId` as a picker).
//! - **Type row builders** replace the whole reflection walk for a given
//!   entity type, used when a view needs custom rows that don't correspond
//!   to Facet fields.
//!
//! Every registration also installs an [`EntityAdapter`] so the walker can
//! reach the typed `Peek`/`Poke` from an `AnyEntity`.
use std::any::TypeId;
use std::collections::HashMap;
use std::sync::Arc;

use facet::{ConstTypeId, Facet, Field, Peek, Poke};
use gpui::{AnyEntity, App, AppContext, Entity, Global, SharedString};
use metor_db::DB;

use crate::inspector::rows::{CommandRow, InspectorRow, NavRow};

mod builders;
mod defaults;
mod dispatch;

/// Inputs threaded to every field widget factory.
pub struct FieldBuildCtx<'a> {
    pub db: &'a Arc<DB>,
    pub label: SharedString,
    pub field_name: &'static str,
    pub field_def: &'static Field,
}

/// Builds a single inspector row for one field.
///
/// Receives the owning entity plus the field index, enabling callbacks to
/// read and write through [`crate::inspector::reflect::get_field`] and
/// [`crate::inspector::reflect::set_field`] without referring to the
/// concrete parent type.
pub type FieldWidgetFactory =
    Arc<dyn Fn(&FieldBuildCtx, &Peek<'_, '_>, AnyEntity, usize) -> Box<dyn InspectorRow>>;

/// Builds the complete row set for an entity type, bypassing field reflection.
pub type TypeRowBuilder = Arc<dyn Fn(AnyEntity, &Arc<DB>, &App) -> Vec<Box<dyn InspectorRow>>>;

pub(crate) type PeekAdapterVisitor<'v> = dyn FnMut(&Peek<'_, 'static>) + 'v;
pub(crate) type PokeAdapterVisitor<'v> = dyn FnMut(Poke<'_, 'static>) + 'v;

/// Type-erased bridge from [`AnyEntity`] to Facet `Peek`/`Poke`.
///
/// Installed by [`InspectorRegistry::register_inspectable`]; the walker
/// looks this up to reach typed reflection without knowing the entity's
/// concrete `T`.
pub struct EntityAdapter {
    pub(crate) peek: Arc<dyn for<'a, 'v> Fn(&'a AnyEntity, &'a App, &mut PeekAdapterVisitor<'v>)>,
    pub(crate) poke: Arc<dyn for<'v> Fn(&AnyEntity, &mut App, &mut PokeAdapterVisitor<'v>)>,
    notify_edited: Arc<dyn Fn(&AnyEntity, &mut App)>,
    pub shape_id: ConstTypeId,
}

/// Builds the nav row for a `Vec<Entity<T>>` field.
pub type EntityListHandler =
    Arc<dyn Fn(gpui::AnyEntity, SharedString, &Arc<DB>, &App) -> Box<dyn InspectorRow>>;

/// How the "Add" affordance works inside a `Vec<Entity<T>>` list.
pub enum AddBehavior<T: 'static> {
    /// Push a freshly-defaulted item with no user interaction.
    Default(Arc<dyn Fn(&mut App) -> T>),
    /// Cascade into a multi-step wizard that eventually appends the items.
    /// The wizard reads and writes the parent's list through [`EntityList`],
    /// so its append runs the parent's edit hook like any other edit.
    Wizard(Arc<dyn Fn(EntityList<T>, &Arc<DB>, &App) -> Vec<Box<dyn InspectorRow>>>),
}

/// One parent's `Vec<Entity<T>>` field, erased of the parent type.
///
/// Built by [`InspectorRegistry::register_entity_list`] from its typed
/// accessors; appending goes through [`InspectorRegistry::notify_edited`] so
/// every add path triggers the same hook a reflected field write would.
pub struct EntityList<T: 'static> {
    items: Arc<dyn Fn(&App) -> Vec<Entity<T>>>,
    append: Arc<dyn Fn(Vec<T>, &mut App)>,
}

impl<T: 'static> Clone for EntityList<T> {
    fn clone(&self) -> Self {
        Self {
            items: self.items.clone(),
            append: self.append.clone(),
        }
    }
}

impl<T: 'static> EntityList<T> {
    fn new<ParentT: 'static>(
        parent: Entity<ParentT>,
        get_list: fn(&ParentT) -> &Vec<Entity<T>>,
        get_list_mut: fn(&mut ParentT) -> &mut Vec<Entity<T>>,
    ) -> Self {
        let read_parent = parent.clone();
        Self {
            items: Arc::new(move |cx| get_list(read_parent.read(cx)).clone()),
            append: Arc::new(move |items, cx| {
                let entities: Vec<_> = items.into_iter().map(|item| cx.new(|_| item)).collect();
                parent.update(cx, |p, _| get_list_mut(p).extend(entities));
                InspectorRegistry::notify_edited(&parent.clone().into_any(), cx);
            }),
        }
    }

    pub fn items(&self, cx: &App) -> Vec<Entity<T>> {
        (self.items)(cx)
    }

    pub fn len(&self, cx: &App) -> usize {
        self.items(cx).len()
    }

    pub fn append(&self, items: Vec<T>, cx: &mut App) {
        (self.append)(items, cx)
    }
}

impl<T: 'static> Clone for AddBehavior<T> {
    fn clone(&self) -> Self {
        match self {
            Self::Default(f) => Self::Default(f.clone()),
            Self::Wizard(f) => Self::Wizard(f.clone()),
        }
    }
}

/// Single global holding every field, type, and list override.
///
/// Install once at startup via [`InspectorRegistry::init`]; thereafter it's
/// read-only from the render thread.
pub struct InspectorRegistry {
    field_widgets: HashMap<ConstTypeId, FieldWidgetFactory>,
    type_builders: HashMap<TypeId, TypeRowBuilder>,
    /// Keyed by the `ConstTypeId` of `Vec<Entity<T>>` itself, not of `T`.
    entity_list_handlers: HashMap<ConstTypeId, EntityListHandler>,
    entity_adapters: HashMap<TypeId, Arc<EntityAdapter>>,
}

impl Global for InspectorRegistry {}

impl InspectorRegistry {
    pub fn init(db: Arc<DB>, cx: &mut App) {
        let mut reg = Self {
            field_widgets: HashMap::new(),
            type_builders: HashMap::new(),
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

    pub fn register_field_widget<T: Facet<'static>>(&mut self, factory: FieldWidgetFactory) {
        self.field_widgets.insert(T::SHAPE.id, factory);
    }

    /// Replace the reflection walk for `T` with a custom row builder.
    ///
    /// Used when a view needs rows that don't map onto Facet fields (e.g.
    /// `Viewer3d`'s gizmo controls, `DashboardPanel`'s widget grid).
    pub fn register_type_builder<T: 'static>(&mut self, builder: TypeRowBuilder) {
        self.type_builders.insert(TypeId::of::<T>(), builder);
    }

    /// Install the `AnyEntity`↔`Peek`/`Poke` bridge for `T`.
    ///
    /// Called automatically by the higher-level registration helpers; only
    /// call directly when registering a type that has no field widgets or
    /// list handlers but still needs to be inspected.
    pub fn register_inspectable<T: Facet<'static> + 'static>(&mut self) {
        if !self.entity_adapters.contains_key(&TypeId::of::<T>()) {
            self.register_inspectable_with_edit::<T>(|_, _| {});
        }
    }

    /// Register reflection and a synchronous hook for edits that update resources.
    /// The hook runs before the editor releases temporary handles, such as a
    /// newly compiled expression being assigned to a component field.
    pub fn register_inspectable_with_edit<T: Facet<'static> + 'static>(
        &mut self,
        on_edit: impl Fn(&mut T, &mut gpui::Context<T>) + 'static,
    ) {
        let on_edit = Arc::new(on_edit);
        let notify_edited = {
            let on_edit = on_edit.clone();
            Arc::new(move |entity: &AnyEntity, cx: &mut App| {
                if let Ok(entity) = entity.clone().downcast::<T>() {
                    entity.update(cx, |target, cx| {
                        on_edit(target, cx);
                        cx.notify();
                    });
                }
            })
        };
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
            Arc::new(move |any_entity, cx, visit| {
                let Ok(entity) = any_entity.clone().downcast::<T>() else {
                    return;
                };
                entity.update(cx, |target, cx| {
                    visit(Poke::new(target));
                    on_edit(target, cx);
                    cx.notify();
                });
            });
        self.entity_adapters.insert(
            TypeId::of::<T>(),
            Arc::new(EntityAdapter {
                peek,
                poke,
                notify_edited,
                shape_id: T::SHAPE.id,
            }),
        );
    }

    /// Apply the registered edit hook after a typed list mutation, just as a
    /// reflected field write does, before notifying the entity's observers.
    pub(crate) fn notify_edited(entity: &AnyEntity, cx: &mut App) {
        let adapter = cx
            .global::<Self>()
            .entity_adapter(entity.entity_type())
            .cloned();
        if let Some(adapter) = adapter {
            (adapter.notify_edited)(entity, cx);
        }
    }

    pub fn entity_adapter(&self, type_id: TypeId) -> Option<&Arc<EntityAdapter>> {
        self.entity_adapters.get(&type_id)
    }

    pub fn entity_list_handler(&self, field_type_id: ConstTypeId) -> Option<&EntityListHandler> {
        self.entity_list_handlers.get(&field_type_id)
    }

    /// Register behavior for a `Vec<Entity<ItemT>>` field on `ParentT`.
    ///
    /// Produces a nav row that lists items, cascades into each one's
    /// reflected inspector, and offers an "Add" affordance whose behavior
    /// is controlled by [`AddBehavior`].
    pub fn register_entity_list<
        ParentT: Facet<'static> + 'static,
        ItemT: Facet<'static> + 'static,
    >(
        &mut self,
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
                let parent_for_remove = parent.clone();
                let list = EntityList::new(parent, get_list, get_list_mut);
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
                                let parent_for_remove = parent_for_remove.clone();
                                let idx = i;

                                Box::new(NavRow::new(
                                    item_label,
                                    SharedString::new_static(""),
                                    Box::new(move |cx| {
                                        let mut sub_rows =
                                            crate::inspector::reflect::rows_for_entity(
                                                &entity, &db, cx,
                                            );
                                        let remove_parent = parent_for_remove.clone();
                                        sub_rows.push(Box::new(CommandRow::new(
                                            "Remove",
                                            Arc::new(move |_w, cx| {
                                                remove_parent.update(cx, |p, _| {
                                                    let list = get_list_mut(p);
                                                    if idx < list.len() {
                                                        list.remove(idx);
                                                    }
                                                });
                                                Self::notify_edited(
                                                    &remove_parent.clone().into_any(),
                                                    cx,
                                                );
                                            }),
                                        )));
                                        sub_rows
                                    }),
                                )) as Box<dyn InspectorRow>
                            })
                            .collect();

                        match &add_behavior {
                            AddBehavior::Default(factory) => {
                                let list = list.clone();
                                let factory = factory.clone();
                                rows.push(Box::new(CommandRow::new(
                                    "Add",
                                    Arc::new(move |_w, cx| {
                                        let item = factory(cx);
                                        list.append(vec![item], cx);
                                    }),
                                )));
                            }
                            AddBehavior::Wizard(wizard) => {
                                let list = list.clone();
                                let db = db.clone();
                                let wizard = wizard.clone();
                                rows.push(Box::new(NavRow::new(
                                    "Add",
                                    SharedString::new_static(""),
                                    Box::new(move |cx| wizard(list.clone(), &db, cx)),
                                )));
                            }
                        }

                        rows
                    }),
                ))
            }),
        );
    }
}
