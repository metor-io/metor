//! Bridge from Facet reflection to [`InspectorRow`] widgets.
//!
//! [`rows_for_any_entity`] is the type-erased entry: it prefers a registered
//! whole-type override, then falls back to walking fields via the
//! registry's [`EntityAdapter`]. Read-back and write-back go through
//! [`get_field`] / [`set_field`], which hide the per-type poke/peek calls so
//! row widgets can stay generic.
use std::sync::Arc;

use facet::{Facet, FieldFlags, PokeStruct};
use gpui::{AnyEntity, App, Entity, SharedString};
use metor_db::DB;

use crate::inspector::registry::{FieldBuildCtx, InspectorRegistry};
use crate::inspector::rows::InspectorRow;

/// Build inspector rows for `entity`.
///
/// Returns `None` when the type is unregistered in both the whole-type
/// builder map and the adapter map. Callers should fall back to a generic
/// "nothing to inspect" placeholder rather than panicking.
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

/// Typed shortcut for callers that already have an `Entity<T>`.
pub fn rows_for_entity<T: 'static>(
    entity: &Entity<T>,
    db: &Arc<DB>,
    cx: &App,
) -> Vec<Box<dyn InspectorRow>> {
    rows_for_any_entity(&entity.clone().into_any(), db, cx).unwrap_or_default()
}

/// Default field walk used when no whole-type builder is registered.
///
/// Each non-skipped Facet field is routed to
/// [`InspectorRegistry::row_for_field`], which picks the concrete widget
/// (slider, checkbox, nested inspector, etc.).
pub fn default_rows_for_any_entity(
    any_entity: &AnyEntity,
    db: &Arc<DB>,
    cx: &App,
) -> Vec<Box<dyn InspectorRow>> {
    let registry = cx.global::<InspectorRegistry>();
    let Some(adapter) = registry.entity_adapter(any_entity.entity_type()).cloned() else {
        return vec![];
    };
    let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();
    (adapter.peek)(any_entity, cx, &mut |peek| {
        let Ok(peek_struct) = (*peek).into_struct() else {
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
                label: SharedString::from(
                    crate::inspect::field_label(field_def).unwrap_or(field_def.name),
                ),
                field_name: field_def.name,
                field_def,
            };

            if let Some(row) = registry.row_for_field(&ctx, &field_peek, any_entity, idx, cx) {
                rows.push(row);
            }
        }
    });
    rows
}

/// Read field `field_idx` of `any_entity` as `V`.
///
/// Returns `None` when the entity type has no adapter, the field index is
/// out of range, or the field's static type is not `V`.
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
        let Ok(ps) = (*peek).into_struct() else {
            return;
        };
        let Ok(fp) = ps.field(field_idx) else { return };
        if let Ok(v) = fp.get::<V>() {
            out = Some(v.clone());
        }
    });
    out
}

/// Write `value` into field `field_idx` of `any_entity`.
///
/// Silently no-ops on adapter or type mismatch; Facet's own `set` guards
/// the type check so a wrong `V` cannot corrupt memory.
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
    // The poke callback is `FnMut`; `value` must be moved exactly once, so
    // the slot-and-take pattern works where a plain capture would not.
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

/// Set an enum field to the variant named `variant_name`.
///
/// Separate from [`set_field`] because the generic path can't express
/// "variant by name"; this drops down to `PokeEnum` to write the
/// discriminant directly.
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

/// Low-level enum write. Returns `false` when `field_idx` is not an enum,
/// any variant has a payload, the name is unknown, or the repr is unsupported.
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
    // A discriminant-only write cannot initialize or drop variant payloads.
    if poke_enum
        .variants()
        .iter()
        .any(|v| !v.data.fields.is_empty())
    {
        return false;
    }
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
            // SAFETY: all variants are fieldless, the discriminant belongs to a
            // declared variant, and repr(u8) stores its tag at offset zero.
            data.as_mut_byte_ptr().write(discriminant as u8);
        },
        facet::EnumRepr::U16 => unsafe {
            // SAFETY: as above; repr(u16) also guarantees tag alignment.
            (data.as_mut_byte_ptr() as *mut u16).write(discriminant as u16);
        },
        facet::EnumRepr::U32 => unsafe {
            // SAFETY: as above; repr(u32) also guarantees tag alignment.
            (data.as_mut_byte_ptr() as *mut u32).write(discriminant as u32);
        },
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Facet, Debug, PartialEq)]
    #[repr(u8)]
    enum Mode {
        Off,
        Named(String),
    }

    #[derive(Facet)]
    struct Config {
        mode: Mode,
    }

    #[test]
    fn discriminant_edits_refuse_payload_enums_in_both_directions() {
        let mut config = Config { mode: Mode::Off };
        assert!(!set_enum_by_name(
            &mut facet::Poke::new(&mut config).into_struct().unwrap(),
            0,
            "Named"
        ));
        assert_eq!(config.mode, Mode::Off);
        config.mode = Mode::Named("retained".into());
        assert!(!set_enum_by_name(
            &mut facet::Poke::new(&mut config).into_struct().unwrap(),
            0,
            "Off"
        ));
        assert_eq!(config.mode, Mode::Named("retained".into()));
    }

    #[derive(Facet, Debug, PartialEq)]
    #[repr(u16)]
    enum Choice {
        First = 3,
        Second = 300,
    }
    #[derive(Facet)]
    struct Plain {
        choice: Choice,
    }

    #[test]
    fn fieldless_enum_edits_preserve_explicit_discriminants() {
        let mut config = Plain {
            choice: Choice::First,
        };
        assert!(set_enum_by_name(
            &mut facet::Poke::new(&mut config).into_struct().unwrap(),
            0,
            "Second"
        ));
        assert_eq!(config.choice, Choice::Second);
        assert!(!set_enum_by_name(
            &mut facet::Poke::new(&mut config).into_struct().unwrap(),
            0,
            "Missing"
        ));
        assert_eq!(config.choice, Choice::Second);
    }
}
