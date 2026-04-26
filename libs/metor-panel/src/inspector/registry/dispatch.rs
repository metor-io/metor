//! Per-field dispatch inside the walker: pick a concrete row or fall back to
//! shape-based defaults. Also hosts the generic scalar read/write routines
//! that let a single `SliderRow` or `ScalarRow` work across every numeric type.

use std::sync::Arc;

use facet::{ConstTypeId, Facet, Peek, ScalarType};
use gpui::{AnyEntity, App, SharedString};

use crate::inspector::rows::{BoolRow, EnumRow, InspectorRow, ScalarRow, SliderRow, TextRow};

use super::{FieldBuildCtx, FieldOverride, InspectorRegistry, builders};

impl InspectorRegistry {
    /// Resolve the row for one struct field.
    ///
    /// Tries, in order: a type-keyed field widget factory, an entity-list
    /// handler (for `Vec<Entity<T>>`), then the shape-based defaults.
    /// Returns `None` when nothing in the registry can render the field.
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

    /// Shape-driven defaults for fields with no registered override.
    ///
    /// Handles `bool`, numeric scalars (with an optional slider range from
    /// [`FieldOverride`]), `String`, `Option`, and `enum`. Everything else
    /// returns `None` so the walker can skip the field silently.
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
            return Some(Box::new(BoolRow::new(
                ctx.label.clone(),
                val,
                Arc::new(move |v, _w, cx| {
                    crate::inspector::reflect::set_field::<bool>(&any_entity, field_idx, v, cx)
                }),
            )));
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
                    crate::inspector::reflect::set_field::<String>(&any_entity, field_idx, s, cx)
                }),
            )));
        }

        if let Ok(peek_option) = peek.clone().into_option() {
            return Some(builders::build_option_row(
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
                    crate::inspector::reflect::set_enum_variant(&any_entity, field_idx, &name, cx);
                }),
            }));
        }

        None
    }
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
        ScalarType::F32 => {
            crate::inspector::reflect::get_field::<f32>(any_entity, idx, cx).map(|v| v as f64)
        }
        ScalarType::F64 => crate::inspector::reflect::get_field::<f64>(any_entity, idx, cx),
        ScalarType::I32 => {
            crate::inspector::reflect::get_field::<i32>(any_entity, idx, cx).map(|v| v as f64)
        }
        ScalarType::I64 => {
            crate::inspector::reflect::get_field::<i64>(any_entity, idx, cx).map(|v| v as f64)
        }
        ScalarType::U32 => {
            crate::inspector::reflect::get_field::<u32>(any_entity, idx, cx).map(|v| v as f64)
        }
        ScalarType::U64 => {
            crate::inspector::reflect::get_field::<u64>(any_entity, idx, cx).map(|v| v as f64)
        }
        _ => None,
    }
    .unwrap_or(0.0)
}

fn write_scalar(any_entity: &AnyEntity, idx: usize, scalar: ScalarType, v: f64, cx: &mut App) {
    match scalar {
        ScalarType::F32 => {
            crate::inspector::reflect::set_field::<f32>(any_entity, idx, v as f32, cx)
        }
        ScalarType::F64 => crate::inspector::reflect::set_field::<f64>(any_entity, idx, v, cx),
        ScalarType::I32 => {
            crate::inspector::reflect::set_field::<i32>(any_entity, idx, v as i32, cx)
        }
        ScalarType::I64 => {
            crate::inspector::reflect::set_field::<i64>(any_entity, idx, v as i64, cx)
        }
        ScalarType::U32 => {
            crate::inspector::reflect::set_field::<u32>(any_entity, idx, v as u32, cx)
        }
        ScalarType::U64 => {
            crate::inspector::reflect::set_field::<u64>(any_entity, idx, v as u64, cx)
        }
        _ => {}
    }
}
