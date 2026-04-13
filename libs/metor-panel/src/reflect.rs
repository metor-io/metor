/// Facet shape walker that produces InspectorRow widgets from entity reflection.
///
/// The core function [`rows_for_entity`] checks the [`WidgetRegistry`] for
/// type-level builders first, then falls back to [`default_rows_for_entity`]
/// which walks Facet struct fields and maps them to widgets.
use std::any::{Any, TypeId};
use std::sync::Arc;

use facet::{Def, Facet, FieldFlags, Peek, Poke, ScalarType};
use gpui::{App, Entity, Hsla, SharedString};
use metor_db::DB;
use metor_proto::types::ComponentId;

use crate::widget_registry::{FieldBuildCtx, FieldSetter, WidgetRegistry};
use crate::widgets::{
    BoolRow, CommandRow, EnumRow, InspectorRow, NavRow, ScalarRow, SliderRow, TextRow,
};

/// Generate inspector rows for a type-erased entity via the type builder registry.
///
/// Returns `None` if no type builder is registered for this entity's type.
pub fn rows_for_any_entity(
    entity: &gpui::AnyEntity,
    db: &Arc<DB>,
    cx: &App,
) -> Option<Vec<Box<dyn InspectorRow>>> {
    let registry = cx.global::<WidgetRegistry>();
    let builder = registry.type_builder(entity.entity_type())?.clone();
    Some(builder(entity.clone(), db, cx))
}

/// Generate inspector rows for an entity, checking the type builder registry first.
pub fn rows_for_entity<T: Facet<'static> + 'static>(
    entity: &Entity<T>,
    db: &Arc<DB>,
    cx: &App,
) -> Vec<Box<dyn InspectorRow>> {
    let registry = cx.global::<WidgetRegistry>();
    if let Some(builder) = registry.type_builder(TypeId::of::<T>()) {
        let builder = builder.clone();
        return builder(entity.clone().into_any(), db, cx);
    }
    default_rows_for_entity(entity, db, cx)
}

/// Walk a Facet struct's fields and produce widget rows using shape-based defaults
/// and field widget overrides from the registry.
pub fn default_rows_for_entity<T: Facet<'static> + 'static>(
    entity: &Entity<T>,
    db: &Arc<DB>,
    cx: &App,
) -> Vec<Box<dyn InspectorRow>> {
    let value = entity.read(cx);
    let peek = Peek::new(value);

    let Ok(peek_struct) = peek.into_struct() else {
        return vec![];
    };

    let registry = cx.global::<WidgetRegistry>();
    let struct_ty = peek_struct.ty();
    let parent_shape_id = T::SHAPE.id;
    let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();

    for (idx, field_def) in struct_ty.fields.iter().enumerate() {
        if field_def.flags.contains(FieldFlags::SKIP) {
            continue;
        }

        let field_shape = field_def.shape();
        let label = SharedString::from(field_def.name);

        let Ok(field_peek) = peek_struct.field(idx) else {
            continue;
        };

        // Build the type-erased setter for this field
        let entity_clone = entity.clone();
        let field_idx = idx;
        let setter: FieldSetter = Arc::new(move |val: Box<dyn Any>, _w, cx| {
            entity_clone.update(cx, |target, _cx| {
                set_field_from_any(target, field_idx, val);
            });
        });

        // Build a type-erased reader for this field (used by live widgets like ColorRow)
        let entity_for_field = entity.clone();
        let read_field: Arc<dyn Fn(&App) -> Box<dyn Any>> = Arc::new(move |cx| {
            let value = entity_for_field.read(cx);
            let peek = Peek::new(value);
            if let Ok(ps) = peek.into_struct() {
                if let Ok(fp) = ps.field(field_idx) {
                    if let Ok(v) = fp.get::<Hsla>() {
                        return Box::new(*v);
                    }
                    if let Ok(v) = fp.get::<String>() {
                        return Box::new(v.clone());
                    }
                    if let Ok(v) = fp.get::<SharedString>() {
                        return Box::new(v.clone());
                    }
                }
            }
            Box::new(())
        });

        let ctx = FieldBuildCtx {
            db,
            label: label.clone(),
            field_name: field_def.name,
            read_field,
        };

        // Check registry for a field widget factory for this type
        if let Some(factory) = registry.field_widget(field_shape.id) {
            rows.push(factory(&ctx, &field_peek, setter));
            continue;
        }

        // Check for Vec<Entity<T>> list handler
        if let Some(handler) = registry.entity_list_handler(field_shape.id) {
            let handler = handler.clone();
            rows.push(handler(entity.clone().into_any(), label, db, cx));
            continue;
        }

        // Check for field override (e.g., range for slider)
        let field_override = registry.field_override(parent_shape_id, field_def.name);

        // Build a live-read closure for numeric fields (used by SliderRow)
        let entity_for_read = entity.clone();
        let read_f64: Arc<dyn Fn(&App) -> f64> = Arc::new(move |cx| {
            let value = entity_for_read.read(cx);
            let peek = Peek::new(value);
            let Ok(ps) = peek.into_struct() else { return 0.0 };
            let Ok(fp) = ps.field(field_idx) else { return 0.0 };
            if let Some(scalar) = fp.scalar_type() {
                match scalar {
                    ScalarType::F32 => fp.get::<f32>().ok().map(|v| *v as f64),
                    ScalarType::F64 => fp.get::<f64>().ok().copied(),
                    ScalarType::I32 => fp.get::<i32>().ok().map(|v| *v as f64),
                    ScalarType::I64 => fp.get::<i64>().ok().map(|v| *v as f64),
                    ScalarType::U32 => fp.get::<u32>().ok().map(|v| *v as f64),
                    ScalarType::U64 => fp.get::<u64>().ok().map(|v| *v as f64),
                    _ => None,
                }
                .unwrap_or(0.0)
            } else {
                0.0
            }
        });

        // Fall back to shape-based defaults
        if let Some(row) = build_default_row(&ctx, &field_peek, setter, field_override, read_f64, cx) {
            rows.push(row);
        }
    }

    rows
}

/// Build a widget row from the Facet shape when no registry override exists.
fn build_default_row(
    ctx: &FieldBuildCtx,
    peek: &Peek<'_, '_>,
    setter: FieldSetter,
    field_override: Option<&crate::widget_registry::FieldOverride>,
    read_f64: Arc<dyn Fn(&App) -> f64>,
    cx: &App,
) -> Option<Box<dyn InspectorRow>> {
    let shape = peek.shape();

    // Bool
    if shape.id == <bool as Facet>::SHAPE.id {
        let val = *peek.get::<bool>().ok()?;
        let label = ctx.label.clone();
        return Some(Box::new(BoolRow {
            label,
            value: val,
            toggle: Arc::new(move |v, w, cx| setter(Box::new(v), w, cx)),
        }));
    }

    // Numeric scalars
    if let Some(scalar) = peek.scalar_type() {
        let f64_val = match scalar {
            ScalarType::F32 => peek.get::<f32>().ok().map(|v| *v as f64),
            ScalarType::F64 => peek.get::<f64>().ok().copied(),
            ScalarType::I32 => peek.get::<i32>().ok().map(|v| *v as f64),
            ScalarType::I64 => peek.get::<i64>().ok().map(|v| *v as f64),
            ScalarType::U32 => peek.get::<u32>().ok().map(|v| *v as f64),
            ScalarType::U64 => peek.get::<u64>().ok().map(|v| *v as f64),
            _ => None,
        };
        if let Some(val) = f64_val {
            let label = ctx.label.clone();
            if let Some(over) = field_override {
                if let Some((min, max)) = over.range {
                    let setter_clone = setter.clone();
                    return Some(Box::new(SliderRow {
                        label,
                        read_value: read_f64,
                        min,
                        max,
                        on_change: Arc::new(move |v, w, cx| {
                            match scalar {
                                ScalarType::F32 => setter_clone(Box::new(v as f32), w, cx),
                                ScalarType::F64 => setter_clone(Box::new(v), w, cx),
                                _ => setter_clone(Box::new(v), w, cx),
                            }
                        }),
                    }));
                }
            }
            let setter_clone = setter.clone();
            return Some(Box::new(ScalarRow {
                label,
                value: val,
                on_change: Arc::new(move |v, w, cx| {
                    match scalar {
                        ScalarType::F32 => setter_clone(Box::new(v as f32), w, cx),
                        ScalarType::F64 => setter_clone(Box::new(v), w, cx),
                        _ => setter_clone(Box::new(v), w, cx),
                    }
                }),
            }));
        }
    }

    // String
    if shape.id == <String as Facet>::SHAPE.id {
        let val = peek.get::<String>().ok()?.clone();
        let label = ctx.label.clone();
        return Some(Box::new(TextRow {
            label,
            value: SharedString::from(val),
            on_change: Arc::new(move |s, w, cx| setter(Box::new(s), w, cx)),
        }));
    }

    // Option<T> — show inner type with None/Clear handling
    if let Ok(peek_option) = peek.clone().into_option() {
        let label = ctx.label.clone();
        let is_some = peek_option.is_some();
        let inner_shape = peek_option.def().t;

        // Check registry for a widget factory for the inner type
        let registry = cx.global::<WidgetRegistry>();
        let has_inner_widget = registry.field_widget(inner_shape.id).is_some();

        let summary = if is_some {
            SharedString::from("Set")
        } else {
            SharedString::from("None")
        };

        let setter_for_clear = setter.clone();
        let db = ctx.db.clone();

        return Some(Box::new(NavRow {
            label,
            summary,
            build_children: Arc::new(move |cx| {
                let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();

                if is_some {
                    // Show the inner value's widget if we have a factory
                    if has_inner_widget {
                        // Delegate to the inner type's widget via the setter
                        // The inner widget will set the Some variant
                    }

                    // "Clear" button sets to None
                    let clear_setter = setter_for_clear.clone();
                    rows.push(Box::new(CommandRow {
                        label: "Clear".into(),
                        callback: Arc::new(move |w, cx| {
                            // We need to set the field to None.
                            // Since Option<T> is the field type, we pass None::<T> encoded
                            // For Option<ComponentId>, this would be Option::<ComponentId>::None
                            // But we don't know T at runtime... use a sentinel
                            clear_setter(Box::new(OptionAction::Clear), w, cx);
                        }),
                    }));
                } else {
                    // Show component list if inner type is ComponentId
                    if inner_shape.id == <ComponentId as Facet>::SHAPE.id {
                        let components = crate::trace_picker::list_components(&db);
                        let setter = setter_for_clear.clone();
                        for (id, name) in components {
                            let setter = setter.clone();
                            rows.push(Box::new(CommandRow {
                                label: SharedString::from(name),
                                callback: Arc::new(move |w, cx| {
                                    setter(Box::new(OptionAction::Set(Box::new(id))), w, cx);
                                }),
                            }));
                        }
                    }
                }

                rows
            }),
        }));
    }

    // Enum
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
        let label = ctx.label.clone();
        return Some(Box::new(EnumRow {
            label,
            selected: SharedString::from(selected),
            options,
            on_select: Arc::new(move |s, w, cx| setter(Box::new(s), w, cx)),
        }));
    }

    None
}

/// Sentinel for Option field mutations.
pub enum OptionAction {
    Clear,
    Set(Box<dyn Any>),
}

/// Apply a type-erased `Box<dyn Any>` value to a Facet struct field using Poke.
///
/// Tries to downcast to known concrete types and uses `set_field` on the
/// PokeStruct. Falls back silently if the type doesn't match.
fn set_field_from_any<T: Facet<'static>>(target: &mut T, field_idx: usize, val: Box<dyn Any>) {
    let mut poke = Poke::new(target);
    let Ok(mut ps) = poke.into_struct() else {
        return;
    };

    // Check OptionAction first (consumes the Box)
    if val.is::<OptionAction>() {
        let action = *val.downcast::<OptionAction>().unwrap();
        match action {
            OptionAction::Clear => {
                let _ = ps.set_field(field_idx, Option::<ComponentId>::None);
            }
            OptionAction::Set(inner) => {
                if let Some(id) = inner.downcast_ref::<ComponentId>() {
                    let _ = ps.set_field(field_idx, Some(*id));
                }
            }
        }
        return;
    }

    // Try each concrete type
    if let Some(v) = val.downcast_ref::<bool>() {
        let _ = ps.set_field(field_idx, *v);
    } else if let Some(v) = val.downcast_ref::<f32>() {
        let _ = ps.set_field(field_idx, *v);
    } else if let Some(v) = val.downcast_ref::<f64>() {
        let _ = ps.set_field(field_idx, *v);
    } else if let Some(v) = val.downcast_ref::<i32>() {
        let _ = ps.set_field(field_idx, *v);
    } else if let Some(v) = val.downcast_ref::<i64>() {
        let _ = ps.set_field(field_idx, *v);
    } else if let Some(v) = val.downcast_ref::<u32>() {
        let _ = ps.set_field(field_idx, *v);
    } else if let Some(v) = val.downcast_ref::<u64>() {
        let _ = ps.set_field(field_idx, *v);
    } else if let Some(variant_name) = val.downcast_ref::<SharedString>() {
        // Try enum variant by name first, then fall back to SharedString field
        if !try_set_enum_by_name(&mut ps, field_idx, variant_name) {
            let _ = ps.set_field(field_idx, variant_name.clone());
        }
    } else if let Some(v) = val.downcast_ref::<String>() {
        // Try enum variant by name first, then fall back to String field
        if !try_set_enum_by_name(&mut ps, field_idx, v.as_str()) {
            let _ = ps.set_field(field_idx, v.clone());
        }
    } else if let Some(v) = val.downcast_ref::<Hsla>() {
        let _ = ps.set_field(field_idx, *v);
    } else if let Some(v) = val.downcast_ref::<ComponentId>() {
        let _ = ps.set_field(field_idx, *v);
    }
}

/// Try to set a struct field to an enum variant by name.
/// Returns `true` if the field was an enum and the variant was found.
fn try_set_enum_by_name(
    ps: &mut facet::PokeStruct<'_, '_>,
    field_idx: usize,
    variant_name: &str,
) -> bool {
    let Ok(field_poke) = ps.field(field_idx) else {
        return false;
    };
    let Ok(poke_enum) = field_poke.into_enum() else {
        return false;
    };
    let enum_repr = poke_enum.enum_repr();
    let Some(variant) = poke_enum
        .variants()
        .iter()
        .find(|v| v.name == variant_name)
    else {
        return false;
    };
    let Some(discriminant) = variant.discriminant else {
        return false;
    };

    // Write the discriminant directly based on the enum representation
    let mut inner = poke_enum.into_inner();
    let data = inner.data_mut();
    match enum_repr {
        facet::EnumRepr::U8 => unsafe {
            data.as_mut_byte_ptr().write(discriminant as u8);
        },
        facet::EnumRepr::U16 => unsafe {
            (data.as_mut_byte_ptr() as *mut u16).write(discriminant as u16);
        },
        facet::EnumRepr::U32 => unsafe {
            (data.as_mut_byte_ptr() as *mut u32).write(discriminant as u32);
        },
        _ => return false,
    }
    true
}
