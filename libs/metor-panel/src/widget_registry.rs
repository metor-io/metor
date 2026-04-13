/// Type registry mapping Facet types to widget factories and row builders.
///
/// The registry has two layers:
/// - **Field widget factories**: produce a single `InspectorRow` for a field
///   of a given type (e.g., `Hsla` → ColorRow, `ComponentId` → component picker)
/// - **Type row builders**: produce the full row set for an entity type,
///   replacing the default reflect walk (e.g., Trace → reflected rows + Remove)
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

use facet::{ConstTypeId, Facet, Peek};
use gpui::{AnyEntity, App, Entity, Global, Hsla, SharedString, Window};
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

/// Type-erased setter: downcasted to the concrete field type by each factory.
pub type FieldSetter = Arc<dyn Fn(Box<dyn Any>, &mut Window, &mut App) + Send + Sync>;

/// Factory that produces a single InspectorRow for a field of a given type.
pub type FieldWidgetFactory = Arc<
    dyn Fn(&FieldBuildCtx, &Peek<'_, '_>, FieldSetter) -> Box<dyn InspectorRow> + Send + Sync,
>;

/// Builder that produces the full row set for an entity type.
pub type TypeRowBuilder =
    Arc<dyn Fn(AnyEntity, &Arc<DB>, &App) -> Vec<Box<dyn InspectorRow>> + Send + Sync>;

/// Per-field metadata that can't be expressed via Facet attributes.
#[derive(Clone)]
pub struct FieldOverride {
    pub range: Option<(f64, f64)>,
}

/// Handler that builds a NavRow for a `Vec<Entity<T>>` field, given the
/// parent entity, field label, and db.
pub type EntityListHandler =
    Arc<dyn Fn(gpui::AnyEntity, SharedString, &Arc<DB>, &App) -> Box<dyn InspectorRow> + Send + Sync>;

pub struct WidgetRegistry {
    field_widgets: HashMap<ConstTypeId, FieldWidgetFactory>,
    type_builders: HashMap<TypeId, TypeRowBuilder>,
    field_overrides: HashMap<(ConstTypeId, &'static str), FieldOverride>,
    /// Handlers for Vec<Entity<T>> fields, keyed by the ConstTypeId of
    /// Vec<Entity<T>> (the field type, not the item type).
    entity_list_handlers: HashMap<ConstTypeId, EntityListHandler>,
}

impl Global for WidgetRegistry {}

impl WidgetRegistry {
    pub fn init(db: Arc<DB>, cx: &mut App) {
        let mut reg = Self {
            field_widgets: HashMap::new(),
            type_builders: HashMap::new(),
            field_overrides: HashMap::new(),
            entity_list_handlers: HashMap::new(),
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

    pub fn field_override(&self, type_id: ConstTypeId, field_name: &'static str) -> Option<&FieldOverride> {
        self.field_overrides.get(&(type_id, field_name))
    }

    pub fn register_field_widget<T: Facet<'static>>(&mut self, factory: FieldWidgetFactory) {
        self.field_widgets.insert(T::SHAPE.id, factory);
    }

    pub fn register_type_builder<T: 'static>(&mut self, builder: TypeRowBuilder) {
        self.type_builders.insert(TypeId::of::<T>(), builder);
    }

    pub fn entity_list_handler(&self, field_type_id: ConstTypeId) -> Option<&EntityListHandler> {
        self.entity_list_handlers.get(&field_type_id)
    }

    /// Register a handler for a `Vec<Entity<T>>` field type.
    /// When the walker sees this field type, it delegates to this handler
    /// instead of the generic scalar/enum/struct fallback.
    pub fn register_entity_list<ParentT: Facet<'static> + 'static, ItemT: Facet<'static> + 'static>(
        &mut self,
        db: Arc<DB>,
        get_list: fn(&ParentT) -> &Vec<Entity<ItemT>>,
    ) {
        let db = db.clone();
        let field_type_id = <Vec<Entity<ItemT>> as Facet>::SHAPE.id;
        self.entity_list_handlers.insert(
            field_type_id,
            Arc::new(move |any_entity, label, db, cx| {
                let parent: Entity<ParentT> = any_entity.downcast().expect("parent type mismatch");
                let items: Vec<Entity<ItemT>> = get_list(parent.read(cx)).clone();
                let item_count = items.len();
                let db = db.clone();
                Box::new(NavRow {
                    label,
                    summary: SharedString::from(format!("{} items", item_count)),
                    build_children: Arc::new(move |cx| {
                        items
                            .iter()
                            .enumerate()
                            .map(|(i, entity)| {
                                let inner = entity.read(cx);
                                let item_label = find_label_field::<ItemT>(inner)
                                    .unwrap_or_else(|| SharedString::from(format!("Item {}", i + 1)));
                                let entity = entity.clone();
                                let db = db.clone();
                                Box::new(NavRow {
                                    label: item_label,
                                    summary: SharedString::new_static(""),
                                    build_children: Arc::new(move |cx| {
                                        crate::reflect::rows_for_entity(&entity, &db, cx)
                                    }),
                                }) as Box<dyn InspectorRow>
                            })
                            .collect()
                    }),
                })
            }),
        );
    }

    pub fn register_field_override<T: Facet<'static>>(
        &mut self,
        field_name: &'static str,
        over: FieldOverride,
    ) {
        self.field_overrides.insert((T::SHAPE.id, field_name), over);
    }

    fn register_defaults(&mut self, db: Arc<DB>) {
        self.register_hsla();
        self.register_shared_string();
        self.register_component_id(db.clone());
        self.register_trace_builder(db.clone());
        self.register_model_entry_builder(db.clone());
        self.register_entity_list::<TimeSeriesPlot, Trace>(
            db.clone(),
            |tsp| &tsp.traces,
        );
        self.register_entity_list::<Viewer3d, crate::elements::viewer_3d::ModelEntry>(
            db.clone(),
            |v| &v.models,
        );
        self.register_time_series_plot_builder(db.clone());
        self.register_viewer3d_builder(db);
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
        self.register_field_widget::<Hsla>(Arc::new(|ctx, peek, setter| {
            let color = *peek.get::<Hsla>().unwrap();
            let label = ctx.label.clone();
            Box::new(ColorRow {
                label,
                color,
                on_change: Arc::new(move |c, w, cx| {
                    setter(Box::new(c), w, cx);
                }),
            })
        }));
    }

    fn register_shared_string(&mut self) {
        self.register_field_widget::<SharedString>(Arc::new(|ctx, peek, setter| {
            let value = peek.get::<SharedString>().unwrap().clone();
            let label = ctx.label.clone();
            Box::new(TextRow {
                label,
                value,
                on_change: Arc::new(move |s, w, cx| {
                    setter(Box::new(SharedString::from(s)), w, cx);
                }),
            })
        }));
    }

    fn register_component_id(&mut self, db: Arc<DB>) {
        let db = db.clone();
        self.register_field_widget::<ComponentId>(Arc::new(move |ctx, peek, setter| {
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
                    build_component_picker(&db, &setter, cx)
                }),
            })
        }));
    }

    fn register_trace_builder(&mut self, _db: Arc<DB>) {
        self.register_type_builder::<crate::elements::time_series::Trace>(Arc::new(
            |any_entity, db, cx| {
                let entity: Entity<crate::elements::time_series::Trace> =
                    any_entity.downcast().expect("Trace type mismatch");
                crate::reflect::default_rows_for_entity(&entity, db, cx)
            },
        ));
    }

    fn register_model_entry_builder(&mut self, _db: Arc<DB>) {
        self.register_type_builder::<crate::elements::viewer_3d::ModelEntry>(Arc::new(
            |any_entity, db, cx| {
                let entity: Entity<crate::elements::viewer_3d::ModelEntry> =
                    any_entity.downcast().expect("ModelEntry type mismatch");
                crate::reflect::default_rows_for_entity(&entity, db, cx)
            },
        ));
    }

    fn register_time_series_plot_builder(&mut self, _db: Arc<DB>) {
        self.register_type_builder::<TimeSeriesPlot>(Arc::new(|any_entity, db, cx| {
            let plot: Entity<TimeSeriesPlot> =
                any_entity.downcast().expect("TimeSeriesPlot type mismatch");
            // Use default reflection — traces, custom_title, x_range, y_min/max are
            // all Facet-visible fields. Vec<Entity<Trace>> is handled by the
            // entity list handler.
            crate::reflect::default_rows_for_entity(&plot, db, cx)
        }));
    }

    fn register_viewer3d_builder(&mut self, _db: Arc<DB>) {
        self.register_type_builder::<Viewer3d>(Arc::new(|any_entity, db, cx| {
            let viewer: Entity<Viewer3d> =
                any_entity.downcast().expect("Viewer3d type mismatch");
            let mut rows = crate::reflect::default_rows_for_entity(&viewer, db, cx);
            // Append entity-level commands that aren't Facet fields
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
    setter: &FieldSetter,
    _cx: &App,
) -> Vec<Box<dyn InspectorRow>> {
    let components = crate::trace_picker::list_components(db);
    components
        .into_iter()
        .map(|(id, name)| {
            let setter = setter.clone();
            Box::new(CommandRow {
                label: SharedString::from(name),
                callback: Arc::new(move |w, cx| {
                    setter(Box::new(id), w, cx);
                }),
            }) as Box<dyn InspectorRow>
        })
        .collect()
}

