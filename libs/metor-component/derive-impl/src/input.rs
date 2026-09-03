use darling::util::Override;
use syn::{Generics, Ident};

use crate::Field;

/// A named struct's fields, as consumed by every emitter: `AsVTable`,
/// `Metadatatize`, `Componentize`, and `Decomponentize` all key off the same
/// ident/generics/fields/parent-prefix quadruple, plus `group` for
/// `metadatatize_impl`'s metadata-only parent entry.
pub struct StructInput {
    pub ident: Ident,
    pub generics: Generics,
    pub fields: Vec<Field>,
    /// The dotted component-id prefix; `None` leaves fields at the root.
    pub parent: Option<String>,
    /// `#[fsw(group)]`/`#[fsw(group = "Custom")]`, read only by
    /// `metadatatize_impl`.
    pub group: Option<Override<String>>,
}

/// A fieldless (unit) enum, as consumed by `as_vtable_impl` (needs the
/// `#[repr(_)]` type) and `metadatatize_impl` (needs the variant names).
pub struct EnumInput {
    pub ident: Ident,
    pub generics: Generics,
    pub parent: Option<String>,
    /// Variant names, used by `metadatatize_impl`'s enum branch.
    pub variants: Vec<Ident>,
    /// The `#[repr(_)]` integer type, required by `as_vtable_impl`'s enum
    /// branch and unused by `metadatatize_impl`.
    pub repr_type: Option<Ident>,
}

/// Either shape a `#[derive(AsVTable)]`/`#[derive(Metadatatize)]` input can
/// take. `Componentize`/`Decomponentize` and `#[derive(Frame)]` only ever
/// produce [`StructInput`], so they skip this wrapper.
pub enum Input {
    Struct(StructInput),
    Enum(EnumInput),
}
