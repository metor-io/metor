/// Facet shape walker that produces InspectorRow widgets from entity reflection.
///
/// [`rows_for_any_entity`] is the type-erased entry point: it prefers a
/// registered [`TypeRowBuilder`] override, then falls back to the adapter-
/// driven [`default_rows_for_any_entity`] walk. The walk delegates per-field
/// dispatch to [`InspectorRegistry::row_for_field`], so concrete row construction
/// lives in `inspector_registry`. Widget callbacks write values back through the
/// monomorphic [`set_field`] / [`get_field`] helpers, which dispatch through
/// the registry's per-type [`EntityAdapter`].
use std::sync::Arc;

use facet::{Facet, FieldFlags, PokeStruct};
use gpui::{AnyEntity, App, Entity, SharedString};
use metor_db::DB;

use crate::inspector_registry::{FieldBuildCtx, InspectorRegistry};
use crate::widgets::InspectorRow;

/// Resolve rows for any entity. Returns `None` only when the entity's type
/// has neither a `TypeRowBuilder` override nor an `EntityAdapter`.
pub fn rows_for_any_entity(
    entity: &AnyEntity,
    db: &Arc<DB>,
    cx: &App,
) -> Option<Vec<Box<dyn InspectorRow>>> {
    let registry = cx.global::<InspectorRegistry>();
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
/// rows. Per-field dispatch is delegated to [`InspectorRegistry::row_for_field`].
pub fn default_rows_for_any_entity(
    any_entity: &AnyEntity,
    db: &Arc<DB>,
    cx: &App,
) -> Vec<Box<dyn InspectorRow>> {
    let registry = cx.global::<InspectorRegistry>();
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

            let Ok(field_peek) = peek_struct.field(idx) else {
                continue;
            };

            let ctx = FieldBuildCtx {
                db,
                label: SharedString::from(field_def.name),
                field_name: field_def.name,
            };

            if let Some(row) =
                registry.row_for_field(&ctx, &field_peek, any_entity, idx, parent_shape_id, cx)
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
        .global::<InspectorRegistry>()
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
        .global::<InspectorRegistry>()
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
        .global::<InspectorRegistry>()
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
