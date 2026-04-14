/// Facet shape walker that produces InspectorRow widgets from entity reflection.
///
/// The core function [`rows_for_entity`] checks the [`WidgetRegistry`] for
/// type-level builders first, then falls back to [`default_rows_for_entity`]
/// which walks Facet struct fields and maps them to widgets.
use std::any::TypeId;
use std::sync::Arc;

use facet::{Facet, FieldFlags, Peek, Poke, PokeStruct, ScalarType};
use gpui::{App, Entity, SharedString};
use metor_db::DB;
use metor_proto::types::ComponentId;

use crate::widget_registry::{FieldBuildCtx, FieldReader, FieldSetter, FieldWrite, WidgetRegistry};
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

        let setter = make_setter(entity, idx);
        let ctx = FieldBuildCtx {
            db,
            label: label.clone(),
            field_name: field_def.name,
        };

        if let Some(factory) = registry.field_widget(field_shape.id) {
            let reader = make_reader(entity, idx);
            rows.push(factory(&ctx, &field_peek, setter, reader));
            continue;
        }

        if let Some(handler) = registry.entity_list_handler(field_shape.id) {
            let handler = handler.clone();
            rows.push(handler(entity.clone().into_any(), label, db, cx));
            continue;
        }

        let field_override = registry.field_override(parent_shape_id, field_def.name);
        if let Some(row) = build_default_row(&ctx, &field_peek, setter, field_override, entity, idx)
        {
            rows.push(row);
        }
    }

    rows
}

/// Per-field setter: captures the typed entity + field index and applies
/// widget-supplied [`FieldWrite`]s through a single `Poke::set_field` call.
fn make_setter<T: Facet<'static> + 'static>(entity: &Entity<T>, idx: usize) -> FieldSetter {
    let entity = entity.clone();
    Arc::new(move |write, _w, cx| {
        entity.update(cx, move |target, _cx| {
            if let Ok(mut ps) = Poke::new(target).into_struct() {
                write.apply(&mut ps, idx);
            }
        });
    })
}

/// Per-field live reader: exposes the current typed value through a Peek
/// visitor so factories can extract without `Box<dyn Any>` round-trips.
fn make_reader<T: Facet<'static> + 'static>(entity: &Entity<T>, idx: usize) -> FieldReader {
    let entity = entity.clone();
    FieldReader::new(Arc::new(move |cx, visit| {
        let value = entity.read(cx);
        let peek = Peek::new(value);
        if let Ok(ps) = peek.into_struct() {
            if let Ok(fp) = ps.field(idx) {
                visit(&fp);
            }
        }
    }))
}

/// Build a widget row from the Facet shape when no registry override exists.
fn build_default_row<T: Facet<'static> + 'static>(
    ctx: &FieldBuildCtx,
    peek: &Peek<'_, '_>,
    setter: FieldSetter,
    field_override: Option<&crate::widget_registry::FieldOverride>,
    entity: &Entity<T>,
    field_idx: usize,
) -> Option<Box<dyn InspectorRow>> {
    let shape = peek.shape();

    if shape.id == <bool as Facet>::SHAPE.id {
        let val = *peek.get::<bool>().ok()?;
        return Some(Box::new(BoolRow {
            label: ctx.label.clone(),
            value: val,
            toggle: Arc::new(move |v, w, cx| setter(FieldWrite::of(v), w, cx)),
        }));
    }

    if let Some(scalar) = peek.scalar_type() {
        if let Some(val) = scalar_as_f64(peek, scalar) {
            let label = ctx.label.clone();
            if let Some((min, max)) = field_override.and_then(|o| o.range) {
                let slider_setter = setter.clone();
                let read_f64 = make_scalar_reader(entity, field_idx, scalar);
                return Some(Box::new(SliderRow {
                    label,
                    read_value: read_f64,
                    min,
                    max,
                    on_change: Arc::new(move |v, w, cx| {
                        if let Some(write) = scalar_write(scalar, v) {
                            slider_setter(write, w, cx);
                        }
                    }),
                }));
            }
            return Some(Box::new(ScalarRow {
                label,
                value: val,
                on_change: Arc::new(move |v, w, cx| {
                    if let Some(write) = scalar_write(scalar, v) {
                        setter(write, w, cx);
                    }
                }),
            }));
        }
    }

    if shape.id == <String as Facet>::SHAPE.id {
        let val = peek.get::<String>().ok()?.clone();
        return Some(Box::new(TextRow {
            label: ctx.label.clone(),
            value: SharedString::from(val),
            on_change: Arc::new(move |s, w, cx| setter(FieldWrite::of(s), w, cx)),
        }));
    }

    if let Ok(peek_option) = peek.clone().into_option() {
        return Some(build_option_row(ctx, peek_option, setter));
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
        let entity = entity.clone();
        return Some(Box::new(EnumRow {
            label: ctx.label.clone(),
            selected: SharedString::from(selected),
            options,
            on_select: Arc::new(move |name, _w, cx| {
                entity.update(cx, |target, _cx| {
                    let Ok(mut ps) = Poke::new(target).into_struct() else {
                        return;
                    };
                    set_enum_by_name(&mut ps, field_idx, &name);
                });
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
    setter: FieldSetter,
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

    Box::new(NavRow {
        label,
        summary,
        build_children: Arc::new(move |_cx| {
            let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();

            if inner_shape.id != <ComponentId as Facet>::SHAPE.id {
                return rows;
            }

            if is_some {
                let clear_setter = setter.clone();
                rows.push(Box::new(CommandRow {
                    label: "Clear".into(),
                    callback: Arc::new(move |w, cx| {
                        clear_setter(FieldWrite::of(Option::<ComponentId>::None), w, cx);
                    }),
                }));
            } else {
                for (id, name) in crate::trace_picker::list_components(&db) {
                    let setter = setter.clone();
                    rows.push(Box::new(CommandRow {
                        label: SharedString::from(name),
                        callback: Arc::new(move |w, cx| {
                            setter(FieldWrite::of(Some(id)), w, cx);
                        }),
                    }));
                }
            }

            rows
        }),
    })
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

fn scalar_write(scalar: ScalarType, v: f64) -> Option<FieldWrite> {
    Some(match scalar {
        ScalarType::F32 => FieldWrite::of(v as f32),
        ScalarType::F64 => FieldWrite::of(v),
        ScalarType::I32 => FieldWrite::of(v as i32),
        ScalarType::I64 => FieldWrite::of(v as i64),
        ScalarType::U32 => FieldWrite::of(v as u32),
        ScalarType::U64 => FieldWrite::of(v as u64),
        _ => return None,
    })
}

fn make_scalar_reader<T: Facet<'static> + 'static>(
    entity: &Entity<T>,
    idx: usize,
    scalar: ScalarType,
) -> Arc<dyn Fn(&App) -> f64> {
    let reader = make_reader(entity, idx);
    Arc::new(move |cx| {
        match scalar {
            ScalarType::F32 => reader.get::<f32>(cx).map(|v| v as f64),
            ScalarType::F64 => reader.get::<f64>(cx),
            ScalarType::I32 => reader.get::<i32>(cx).map(|v| v as f64),
            ScalarType::I64 => reader.get::<i64>(cx).map(|v| v as f64),
            ScalarType::U32 => reader.get::<u32>(cx).map(|v| v as f64),
            ScalarType::U64 => reader.get::<u64>(cx).map(|v| v as f64),
            _ => None,
        }
        .unwrap_or(0.0)
    })
}

/// Write an enum variant by name into field `idx` of a struct. Returns `true`
/// on success; silently no-ops if the field isn't an enum or the variant
/// doesn't exist.
fn set_enum_by_name(ps: &mut PokeStruct<'_, '_>, field_idx: usize, variant_name: &str) -> bool {
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
