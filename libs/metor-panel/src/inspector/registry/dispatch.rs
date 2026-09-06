//! Per-field dispatch inside the walker: pick a concrete row or fall back to
//! shape-based defaults. Also hosts the generic scalar read/write routines
//! that let a single `SliderRow` or `ScalarRow` work across every numeric type.

use std::sync::Arc;

use facet::{Facet, Peek, ScalarType};
use gpui::{AnyEntity, App, SharedString};

use crate::inspector::rows::{BoolRow, EnumRow, InspectorRow, ScalarRow, SliderRow, TextRow};

use super::{FieldBuildCtx, InspectorRegistry, builders};
use crate::dynamic::tensor::TypedScalar;

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
        cx: &App,
    ) -> Option<Box<dyn InspectorRow>> {
        let field_shape = peek.shape();

        if let Some(factory) = self.field_widget(field_shape.id) {
            return Some(factory(ctx, peek, any_entity.clone(), field_idx));
        }

        if let Some(handler) = self.entity_list_handler(field_shape.id) {
            return Some(handler(any_entity.clone(), ctx.label.clone(), ctx.db, cx));
        }

        self.default_row_for_shape(ctx, peek, any_entity, field_idx)
    }

    /// Shape-driven defaults for fields with no registered override.
    ///
    /// Handles `bool`, numeric scalars, `String`, `Option`, and `enum`.
    /// Everything else returns `None` so the walker can skip the field silently.
    pub fn default_row_for_shape(
        &self,
        ctx: &FieldBuildCtx,
        peek: &Peek<'_, '_>,
        any_entity: &AnyEntity,
        field_idx: usize,
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

        if let Some(scalar) = peek.scalar_type()
            && let Some(val) = scalar_value(peek, scalar)
        {
            let label = ctx.label.clone();
            if let Some((min, max)) = crate::inspect::field_range(ctx.field_def) {
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
                        let value = if matches!(val, TypedScalar::F32(_) | TypedScalar::F64(_)) {
                            v
                        } else {
                            v.round()
                        };
                        write_value(
                            &write_entity,
                            field_idx,
                            scalar,
                            TypedScalar::from_f64(value, val.dtype()),
                            cx,
                        );
                    }),
                }));
            }
            let any_entity = any_entity.clone();
            return Some(Box::new(ScalarRow::typed(
                label,
                val,
                Arc::new(move |v, _w, cx| {
                    write_value(&any_entity, field_idx, scalar, v, cx);
                }),
            )));
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

        if let Ok(peek_option) = (*peek).into_option() {
            return Some(builders::build_option_row(
                ctx,
                peek_option,
                any_entity.clone(),
                field_idx,
            ));
        }

        if let Ok(peek_enum) = (*peek).into_enum() {
            if peek_enum
                .variants()
                .iter()
                .any(|v| !v.data.fields.is_empty())
            {
                return None;
            }
            let selected = peek_enum
                .variant_name_active()
                .unwrap_or("unknown")
                .to_string();
            let allowed = crate::inspect::field_variants(ctx.field_def);
            let options: Vec<SharedString> = peek_enum
                .variants()
                .iter()
                .filter(|v| {
                    allowed
                        .is_none_or(|allowed| allowed.split(',').any(|name| name.trim() == v.name))
                })
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

fn scalar_value(peek: &Peek<'_, '_>, scalar: ScalarType) -> Option<TypedScalar> {
    Some(match scalar {
        ScalarType::F32 => TypedScalar::F32(*peek.get::<f32>().ok()?),
        ScalarType::F64 => TypedScalar::F64(*peek.get::<f64>().ok()?),
        ScalarType::I8 => TypedScalar::I8(*peek.get::<i8>().ok()?),
        ScalarType::I16 => TypedScalar::I16(*peek.get::<i16>().ok()?),
        ScalarType::I32 => TypedScalar::I32(*peek.get::<i32>().ok()?),
        ScalarType::I64 => TypedScalar::I64(*peek.get::<i64>().ok()?),
        ScalarType::U8 => TypedScalar::U8(*peek.get::<u8>().ok()?),
        ScalarType::U16 => TypedScalar::U16(*peek.get::<u16>().ok()?),
        ScalarType::U32 => TypedScalar::U32(*peek.get::<u32>().ok()?),
        ScalarType::U64 => TypedScalar::U64(*peek.get::<u64>().ok()?),
        ScalarType::USize => {
            if usize::BITS == 32 {
                TypedScalar::U32(*peek.get::<usize>().ok()? as u32)
            } else {
                TypedScalar::U64(*peek.get::<usize>().ok()? as u64)
            }
        }
        ScalarType::ISize => {
            if isize::BITS == 32 {
                TypedScalar::I32(*peek.get::<isize>().ok()? as i32)
            } else {
                TypedScalar::I64(*peek.get::<isize>().ok()? as i64)
            }
        }
        _ => return None,
    })
}

fn write_value(
    entity: &AnyEntity,
    idx: usize,
    scalar: ScalarType,
    value: TypedScalar,
    cx: &mut App,
) {
    use crate::inspector::reflect::set_field;
    match (scalar, value) {
        (ScalarType::USize, TypedScalar::U32(v)) => set_field(entity, idx, v as usize, cx),
        (ScalarType::ISize, TypedScalar::I32(v)) => set_field(entity, idx, v as isize, cx),
        (ScalarType::USize, TypedScalar::U64(v)) => {
            if let Ok(v) = usize::try_from(v) {
                set_field(entity, idx, v, cx);
            }
        }
        (ScalarType::ISize, TypedScalar::I64(v)) => {
            if let Ok(v) = isize::try_from(v) {
                set_field(entity, idx, v, cx);
            }
        }
        (_, TypedScalar::F32(v)) => set_field(entity, idx, v, cx),
        (_, TypedScalar::F64(v)) => set_field(entity, idx, v, cx),
        (_, TypedScalar::I8(v)) => set_field(entity, idx, v, cx),
        (_, TypedScalar::I16(v)) => set_field(entity, idx, v, cx),
        (_, TypedScalar::I32(v)) => set_field(entity, idx, v, cx),
        (_, TypedScalar::I64(v)) => set_field(entity, idx, v, cx),
        (_, TypedScalar::U8(v)) => set_field(entity, idx, v, cx),
        (_, TypedScalar::U16(v)) => set_field(entity, idx, v, cx),
        (_, TypedScalar::U32(v)) => set_field(entity, idx, v, cx),
        (_, TypedScalar::U64(v)) => set_field(entity, idx, v, cx),
        (_, TypedScalar::Bool(v)) => set_field(entity, idx, v, cx),
    }
}

fn read_scalar(any_entity: &AnyEntity, idx: usize, scalar: ScalarType, cx: &App) -> f64 {
    let Some(adapter) = cx
        .global::<InspectorRegistry>()
        .entity_adapter(any_entity.entity_type())
    else {
        return 0.0;
    };
    let mut value = 0.0;
    (adapter.peek)(any_entity, cx, &mut |peek| {
        let Ok(fields) = (*peek).into_struct() else {
            return;
        };
        let Ok(field) = fields.field(idx) else {
            return;
        };
        if let Some(scalar) = scalar_value(&field, scalar) {
            value = scalar.as_f64();
        }
    });
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inspector::rows::RowAction;
    use gpui::AppContext;

    #[derive(Facet)]
    struct Numbers {
        counter: u64,
        small: u8,
    }

    #[gpui::test]
    fn reflected_integer_edits_preserve_precision_and_reject_invalid_values(
        cx: &mut gpui::TestAppContext,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let db = Arc::new(metor_db::DB::create(temp.path().join("db")).unwrap());
        let window = cx.add_empty_window();
        window.update(|window, cx| {
            InspectorRegistry::init(db.clone(), cx);
            cx.global_mut::<InspectorRegistry>()
                .register_inspectable::<Numbers>();
            let entity = cx.new(|_| Numbers {
                counter: 9_007_199_254_740_993,
                small: 7,
            });
            let mut rows = crate::inspector::reflect::rows_for_entity(&entity, &db, cx);
            let RowAction::StartEdit {
                current_text,
                on_commit,
            } = rows[0].activate(window, cx)
            else {
                panic!("numeric editor")
            };
            assert_eq!(current_text, "9007199254740993");
            on_commit("18446744073709551615".into(), window, cx);
            assert_eq!(entity.read(cx).counter, u64::MAX);
            for invalid in ["256", "-1", "1.5"] {
                let RowAction::StartEdit { on_commit, .. } = rows[1].activate(window, cx) else {
                    panic!("numeric editor")
                };
                on_commit(invalid.into(), window, cx);
                assert_eq!(entity.read(cx).small, 7);
            }
        });
    }
}
