/// Bridge between Facet reflection and gpui entities.
///
/// Provides a registry that maps gpui entity types to Facet reflection
/// operations, enabling type-erased inspection and mutation of entities
/// through Facet's Peek/Poke system.
use std::any::TypeId;
use std::collections::HashMap;

use facet::{Facet, Peek, ScalarType};
use gpui::{AnyEntity, App, Entity, Hsla, SharedString, Window};

use crate::inspect_attrs;
use crate::inspectable::{FieldId, InspectionField, InspectionValue};

/// Function that reads an entity and produces inspection fields.
type PeekFieldsFn = Box<dyn Fn(&AnyEntity, &App) -> Vec<InspectionField> + Send + Sync>;

/// Function that applies a field change to an entity by field name.
type SetFieldFn =
    Box<dyn Fn(&AnyEntity, &str, InspectionValue, &mut Window, &mut App) + Send + Sync>;

/// Registry mapping gpui entity types to Facet-based reflection operations.
///
/// Register types via [`InspectRegistry::register`], then use
/// [`InspectRegistry::fields`] and [`InspectRegistry::set_field`] to
/// inspect and mutate entities without knowing their concrete type.
pub struct InspectRegistry {
    peek_fns: HashMap<TypeId, PeekFieldsFn>,
    set_fns: HashMap<TypeId, SetFieldFn>,
}

impl InspectRegistry {
    pub fn new() -> Self {
        Self {
            peek_fns: HashMap::new(),
            set_fns: HashMap::new(),
        }
    }

    /// Register a type for Facet-based inspection.
    ///
    /// After registration, any `AnyEntity` whose inner type is `T` can be
    /// inspected and mutated through this registry.
    pub fn register<T: Facet<'static> + 'static>(&mut self) {
        let type_id = TypeId::of::<T>();

        self.peek_fns.insert(
            type_id,
            Box::new(|any_entity, cx| {
                let entity = any_entity
                    .clone()
                    .downcast::<T>()
                    .expect("entity type mismatch in InspectRegistry::fields");
                let val = entity.read(cx);
                let peek = Peek::new(val);
                peek_to_fields(&peek)
            }),
        );

        self.set_fns.insert(
            type_id,
            Box::new(|any_entity, field_name, value, _window, cx| {
                let entity = any_entity
                    .clone()
                    .downcast::<T>()
                    .expect("entity type mismatch in InspectRegistry::set_field");
                entity.update(cx, |val, _cx| {
                    apply_field_by_name(val, field_name, value);
                });
            }),
        );
    }

    /// Read inspection fields from an entity.
    pub fn fields(&self, entity: &AnyEntity, cx: &App) -> Option<Vec<InspectionField>> {
        let peek_fn = self.peek_fns.get(&entity.entity_type())?;
        Some(peek_fn(entity, cx))
    }

    /// Apply a field change to an entity by field name.
    pub fn set_field(
        &self,
        entity: &AnyEntity,
        field_name: &str,
        value: InspectionValue,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        if let Some(set_fn) = self.set_fns.get(&entity.entity_type()) {
            set_fn(entity, field_name, value, window, cx);
            true
        } else {
            false
        }
    }
}

/// Walk a Peek's struct fields and produce [`InspectionField`] values.
///
/// Respects `#[facet(skip)]`, `inspect::label`, and `inspect::range` attributes.
/// Maps Facet scalar types to the appropriate [`InspectionValue`] variant.
pub fn peek_to_fields(peek: &Peek<'_, '_>) -> Vec<InspectionField> {
    let Ok(peek_struct) = peek.clone().into_struct() else {
        return vec![];
    };

    let struct_ty = peek_struct.ty();
    let mut fields = Vec::new();

    for (idx, field_def) in struct_ty.fields.iter().enumerate() {
        if field_def.flags.contains(facet::FieldFlags::SKIP) {
            continue;
        }

        let label: SharedString = field_def
            .attributes
            .iter()
            .find_map(|a| {
                if a.ns == Some("inspect") && a.key == "label" {
                    a.get_as::<&str>().map(|l| SharedString::from(*l))
                } else {
                    None
                }
            })
            .unwrap_or_else(|| SharedString::from(field_def.name));

        let range: Option<(f64, f64)> = field_def.attributes.iter().find_map(|a| {
            if a.ns == Some("inspect") && a.key == "range" {
                a.get_as::<inspect_attrs::Range>().and_then(|r| {
                    let min = r.min.parse::<f64>().ok()?;
                    let max = r.max.parse::<f64>().ok()?;
                    Some((min, max))
                })
            } else {
                None
            }
        });

        let Ok(field_peek) = peek_struct.field(idx) else {
            continue;
        };

        let Some(value) = peek_to_inspection_value(&field_peek) else {
            continue;
        };

        let mut field = InspectionField::new(label, FieldId(idx as u32), value);
        if let Some((min, max)) = range {
            field = field.with_range(min, max);
        }
        fields.push(field);
    }

    fields
}

/// Convert a Peek value to an InspectionValue based on its shape.
fn peek_to_inspection_value(peek: &Peek<'_, '_>) -> Option<InspectionValue> {
    let shape = peek.shape();

    if shape.id == <Hsla as Facet>::SHAPE.id {
        if let Ok(hsla) = peek.get::<Hsla>() {
            return Some(InspectionValue::Color(*hsla));
        }
    }

    if shape.id == <SharedString as Facet>::SHAPE.id {
        if let Ok(s) = peek.get::<SharedString>() {
            return Some(InspectionValue::String(s.to_string()));
        }
    }

    if shape.id == <bool as Facet>::SHAPE.id {
        if let Ok(b) = peek.get::<bool>() {
            return Some(InspectionValue::Bool(*b));
        }
    }

    if let Some(scalar) = peek.scalar_type() {
        let f64_val = match scalar {
            ScalarType::F32 => peek.get::<f32>().ok().map(|v| *v as f64),
            ScalarType::F64 => peek.get::<f64>().ok().copied(),
            ScalarType::I8 => peek.get::<i8>().ok().map(|v| *v as f64),
            ScalarType::I16 => peek.get::<i16>().ok().map(|v| *v as f64),
            ScalarType::I32 => peek.get::<i32>().ok().map(|v| *v as f64),
            ScalarType::I64 => peek.get::<i64>().ok().map(|v| *v as f64),
            ScalarType::U8 => peek.get::<u8>().ok().map(|v| *v as f64),
            ScalarType::U16 => peek.get::<u16>().ok().map(|v| *v as f64),
            ScalarType::U32 => peek.get::<u32>().ok().map(|v| *v as f64),
            ScalarType::U64 => peek.get::<u64>().ok().map(|v| *v as f64),
            _ => None,
        };
        if let Some(v) = f64_val {
            return Some(InspectionValue::F64(v));
        }
    }

    if shape.id == <String as Facet>::SHAPE.id {
        if let Ok(s) = peek.get::<String>() {
            return Some(InspectionValue::String(s.clone()));
        }
    }

    if let Ok(peek_enum) = peek.clone().into_enum() {
        let selected = peek_enum
            .variant_name_active()
            .unwrap_or("unknown")
            .to_string();
        let options: Vec<String> = peek_enum
            .variants()
            .iter()
            .map(|v| v.name.to_string())
            .collect();
        return Some(InspectionValue::Enum { selected, options });
    }

    None
}

/// Apply an [`InspectionValue`] to a field of a Facet-derived struct by name.
///
/// Uses `PokeStruct::set_field` which requires the type to be marked `#[facet(pod)]`.
fn apply_field_by_name<T: Facet<'static>>(target: &mut T, field_name: &str, value: InspectionValue) {
    use facet::Poke;

    let poke = Poke::new(target);
    let Ok(mut poke_struct) = poke.into_struct() else {
        return;
    };

    let struct_ty = poke_struct.ty();
    let Some((field_idx, field_def)) = struct_ty
        .fields
        .iter()
        .enumerate()
        .find(|(_, f)| f.name == field_name)
    else {
        return;
    };

    let field_shape = field_def.shape.get();

    match value {
        InspectionValue::F64(v) => {
            if field_shape.id == <f64 as Facet>::SHAPE.id {
                let _ = poke_struct.set_field(field_idx, v);
            } else if field_shape.id == <f32 as Facet>::SHAPE.id {
                let _ = poke_struct.set_field(field_idx, v as f32);
            }
        }
        InspectionValue::Bool(v) => {
            let _ = poke_struct.set_field(field_idx, v);
        }
        InspectionValue::String(ref s) => {
            if field_shape.id == <String as Facet>::SHAPE.id {
                let _ = poke_struct.set_field(field_idx, s.clone());
            } else if field_shape.id == <SharedString as Facet>::SHAPE.id {
                let owned = SharedString::from(s.to_string());
                let _ = poke_struct.set_field(field_idx, owned);
            }
        }
        InspectionValue::Color(hsla) => {
            if field_shape.id == <Hsla as Facet>::SHAPE.id {
                let _ = poke_struct.set_field(field_idx, hsla);
            }
        }
        _ => {}
    }
}
