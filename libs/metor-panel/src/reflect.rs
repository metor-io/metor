/// Facet shape walker that produces InspectorRow widgets from entity reflection.
///
/// The core function [`rows_for_entity`] checks the [`WidgetRegistry`] for
/// type-level builders first, then falls back to [`default_rows_for_entity`]
/// which walks Facet struct fields and maps them to widgets.
use std::any::{Any, TypeId};
use std::sync::Arc;

use facet::{Facet, FieldFlags, Peek, Poke, ScalarType, Type, UserType};
use gpui::{App, Entity, Hsla, SharedString, Window};
use metor_db::DB;
use metor_proto::types::ComponentId;

use crate::widget_registry::{FieldBuildCtx, FieldSetter, WidgetRegistry};
use crate::widgets::{
    BoolRow, EnumRow, InspectorRow, NavRow, ScalarRow, SliderRow, TextRow,
};

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

        let ctx = FieldBuildCtx {
            db,
            label: label.clone(),
            field_name: field_def.name,
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

        // Fall back to shape-based defaults
        if let Some(row) = build_default_row(&ctx, &field_peek, setter, field_override) {
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
                        value: val,
                        min,
                        max,
                        on_change: Arc::new(move |v, w, cx| {
                            // Convert back to the original scalar type
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

/// Apply a type-erased `Box<dyn Any>` value to a Facet struct field using Poke.
///
/// Tries to downcast to known concrete types and uses `set_field` on the
/// PokeStruct. Falls back silently if the type doesn't match.
fn set_field_from_any<T: Facet<'static>>(target: &mut T, field_idx: usize, val: Box<dyn Any>) {
    let mut poke = Poke::new(target);
    let Ok(mut ps) = poke.into_struct() else {
        return;
    };

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
    } else if let Some(v) = val.downcast_ref::<String>() {
        let _ = ps.set_field(field_idx, v.clone());
    } else if let Some(v) = val.downcast_ref::<SharedString>() {
        let _ = ps.set_field(field_idx, v.clone());
    } else if let Some(v) = val.downcast_ref::<Hsla>() {
        let _ = ps.set_field(field_idx, *v);
    } else if let Some(v) = val.downcast_ref::<ComponentId>() {
        let _ = ps.set_field(field_idx, *v);
    }
    // Enum setter: receives the variant name as String, need to set via Poke enum
    // For now, enum setting is handled by the EnumRow which knows the concrete type
}
