/// Facet shape walker that produces InspectorRow widgets from entity reflection.
///
/// [`rows_for_any_entity`] is the type-erased entry point: it prefers a
/// registered [`TypeRowBuilder`] override, then falls back to the adapter-
/// driven [`default_rows_for_any_entity`] walk. Widget callbacks write values
/// back through the monomorphic [`set_field`] / [`get_field`] helpers, which
/// dispatch through the registry's per-type [`EntityAdapter`].
use std::sync::Arc;

use facet::{Facet, FieldFlags, Peek, PokeStruct, ScalarType};
use gpui::{AnyEntity, App, Entity, SharedString};
use metor_db::DB;
use metor_proto::types::ComponentId;

use crate::widget_registry::{FieldBuildCtx, WidgetRegistry};
use crate::widgets::{
    BoolRow, CommandRow, EnumRow, InspectorRow, NavRow, ScalarRow, SliderRow, TextRow,
};

/// Resolve rows for any entity. Returns `None` only when the entity's type
/// has neither a `TypeRowBuilder` override nor an `EntityAdapter`.
pub fn rows_for_any_entity(
    entity: &AnyEntity,
    db: &Arc<DB>,
    cx: &App,
) -> Option<Vec<Box<dyn InspectorRow>>> {
    let registry = cx.global::<WidgetRegistry>();
    if let Some(builder) = registry.type_builder(entity.entity_type()).cloned() {
        return Some(builder(entity.clone(), db, cx));
    }
    if registry.entity_adapter(entity.entity_type()).is_some() {
        return Some(default_rows_for_any_entity(entity, db, cx));
    }
    None
}

/// Convenience wrapper for callers that already hold a typed `Entity<T>`.
pub fn rows_for_entity<T: 'static>(
    entity: &Entity<T>,
    db: &Arc<DB>,
    cx: &App,
) -> Vec<Box<dyn InspectorRow>> {
    rows_for_any_entity(&entity.clone().into_any(), db, cx).unwrap_or_default()
}

/// Walk a Facet struct's fields via the registry adapter and produce widget
/// rows using shape-based defaults and field widget overrides.
pub fn default_rows_for_any_entity(
    any_entity: &AnyEntity,
    db: &Arc<DB>,
    cx: &App,
) -> Vec<Box<dyn InspectorRow>> {
    let registry = cx.global::<WidgetRegistry>();
    let Some(adapter) = registry.entity_adapter(any_entity.entity_type()).cloned() else {
        return vec![];
    };
    let parent_shape_id = adapter.shape_id;

    let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();
    (adapter.peek)(any_entity, cx, &mut |peek| {
        let Ok(peek_struct) = peek.clone().into_struct() else {
            return;
        };

        for (idx, field_def) in peek_struct.ty().fields.iter().enumerate() {
            if field_def.flags.contains(FieldFlags::SKIP) {
                continue;
            }

            let field_shape = field_def.shape();
            let label = SharedString::from(field_def.name);

            let Ok(field_peek) = peek_struct.field(idx) else {
                continue;
            };

            let ctx = FieldBuildCtx {
                db,
                label: label.clone(),
                field_name: field_def.name,
            };

            if let Some(factory) = registry.field_widget(field_shape.id) {
                rows.push(factory(&ctx, &field_peek, any_entity.clone(), idx));
                continue;
            }

            if let Some(handler) = registry.entity_list_handler(field_shape.id) {
                let handler = handler.clone();
                rows.push(handler(any_entity.clone(), label, db, cx));
                continue;
            }

            let field_override = registry.field_override(parent_shape_id, field_def.name);
            if let Some(row) = build_default_row(&ctx, &field_peek, any_entity, idx, field_override)
            {
                rows.push(row);
            }
        }
    });
    rows
}

/// Read a Facet field from an entity without knowing its concrete type.
/// Returns `None` if no adapter is registered or the field is missing/typed
/// differently than `V`.
pub fn get_field<V: Facet<'static> + Clone + 'static>(
    any_entity: &AnyEntity,
    field_idx: usize,
    cx: &App,
) -> Option<V> {
    let adapter = cx
        .global::<WidgetRegistry>()
        .entity_adapter(any_entity.entity_type())?
        .clone();
    let mut out = None;
    (adapter.peek)(any_entity, cx, &mut |peek| {
        let Ok(ps) = peek.clone().into_struct() else {
            return;
        };
        let Ok(fp) = ps.field(field_idx) else { return };
        if let Ok(v) = fp.get::<V>() {
            out = Some(v.clone());
        }
    });
    out
}

/// Write a typed Facet value into a struct field. No-op if the adapter is
/// missing or the field type doesn't match `V` — Facet's `set_field` enforces
/// the latter and silently rejects mismatches.
pub fn set_field<V: Facet<'static> + 'static>(
    any_entity: &AnyEntity,
    field_idx: usize,
    value: V,
    cx: &mut App,
) {
    let Some(adapter) = cx
        .global::<WidgetRegistry>()
        .entity_adapter(any_entity.entity_type())
        .cloned()
    else {
        return;
    };
    // `FnMut` can't move `value` out across repeated calls; slot-and-take
    // satisfies that constraint while still consuming the value once.
    let mut slot = Some(value);
    (adapter.poke)(any_entity, cx, &mut |poke| {
        let Some(v) = slot.take() else { return };
        let Ok(mut ps) = poke.into_struct() else {
            return;
        };
        let Ok(mut field_poke) = ps.field(field_idx) else {
            return;
        };
        let _ = field_poke.set(v);
    });
}

/// Write an enum field by variant name. Goes through the adapter because
/// `set_field<V>` can't express "variant by name" — we need direct
/// `PokeEnum` discriminant access.
pub fn set_enum_variant(
    any_entity: &AnyEntity,
    field_idx: usize,
    variant_name: &str,
    cx: &mut App,
) {
    let Some(adapter) = cx
        .global::<WidgetRegistry>()
        .entity_adapter(any_entity.entity_type())
        .cloned()
    else {
        return;
    };
    (adapter.poke)(any_entity, cx, &mut |poke| {
        let Ok(mut ps) = poke.into_struct() else {
            return;
        };
        set_enum_by_name(&mut ps, field_idx, variant_name);
    });
}

fn build_default_row(
    ctx: &FieldBuildCtx,
    peek: &Peek<'_, '_>,
    any_entity: &AnyEntity,
    field_idx: usize,
    field_override: Option<&crate::widget_registry::FieldOverride>,
) -> Option<Box<dyn InspectorRow>> {
    let shape = peek.shape();

    if shape.id == <bool as Facet>::SHAPE.id {
        let val = *peek.get::<bool>().ok()?;
        let any_entity = any_entity.clone();
        return Some(Box::new(BoolRow {
            label: ctx.label.clone(),
            value: val,
            toggle: Arc::new(move |v, _w, cx| set_field::<bool>(&any_entity, field_idx, v, cx)),
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
            Arc::new(move |s, _w, cx| set_field::<String>(&any_entity, field_idx, s, cx)),
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
                set_enum_variant(&any_entity, field_idx, &name, cx);
            }),
        }));
    }

    None
}

/// Build an Option<T> row. Currently only `Option<ComponentId>` is wired —
/// other inner types render as a single "None"/"Set" status with no picker.
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
                        set_field::<Option<ComponentId>>(&any_entity, field_idx, None, cx);
                    }),
                )));
            } else {
                for (id, name) in crate::trace_picker::list_components(&db) {
                    let any_entity = any_entity.clone();
                    rows.push(Box::new(CommandRow::new(
                        SharedString::from(name),
                        Arc::new(move |_w, cx| {
                            set_field::<Option<ComponentId>>(&any_entity, field_idx, Some(id), cx);
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
        ScalarType::F32 => get_field::<f32>(any_entity, idx, cx).map(|v| v as f64),
        ScalarType::F64 => get_field::<f64>(any_entity, idx, cx),
        ScalarType::I32 => get_field::<i32>(any_entity, idx, cx).map(|v| v as f64),
        ScalarType::I64 => get_field::<i64>(any_entity, idx, cx).map(|v| v as f64),
        ScalarType::U32 => get_field::<u32>(any_entity, idx, cx).map(|v| v as f64),
        ScalarType::U64 => get_field::<u64>(any_entity, idx, cx).map(|v| v as f64),
        _ => None,
    }
    .unwrap_or(0.0)
}

fn write_scalar(any_entity: &AnyEntity, idx: usize, scalar: ScalarType, v: f64, cx: &mut App) {
    match scalar {
        ScalarType::F32 => set_field::<f32>(any_entity, idx, v as f32, cx),
        ScalarType::F64 => set_field::<f64>(any_entity, idx, v, cx),
        ScalarType::I32 => set_field::<i32>(any_entity, idx, v as i32, cx),
        ScalarType::I64 => set_field::<i64>(any_entity, idx, v as i64, cx),
        ScalarType::U32 => set_field::<u32>(any_entity, idx, v as u32, cx),
        ScalarType::U64 => set_field::<u64>(any_entity, idx, v as u64, cx),
        _ => {}
    }
}

/// Write an enum variant by name into field `idx` of a struct. Returns `true`
/// on success; silently no-ops if the field isn't an enum or the variant
/// doesn't exist.
fn set_enum_by_name(
    ps: &mut PokeStruct<'_, 'static>,
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
    let Some(variant) = poke_enum.variants().iter().find(|v| v.name == variant_name) else {
        return false;
    };
    let Some(discriminant) = variant.discriminant else {
        return false;
    };

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
