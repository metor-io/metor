//! Shared signature → port classification for `#[system]` and `#[sequence]`
//! (`docs/design-system-macro.md` §5): both macros read their port set off an fn
//! signature by the **last path segment** of each parameter type.

use syn::{GenericArgument, Ident, PathArguments, Type};

/// The last path segment ident of a type (`Input<…>` → `Input`).
pub fn type_head(ty: &Type) -> Option<&Ident> {
    if let Type::Path(p) = ty {
        p.path.segments.last().map(|s| &s.ident)
    } else {
        None
    }
}

/// The first generic *type* argument of a `Foo<T, …>` type (the element `T`).
pub fn first_type_arg(ty: &Type) -> Option<Type> {
    if let Type::Path(p) = ty
        && let Some(seg) = p.path.segments.last()
        && let PathArguments::AngleBracketed(args) = &seg.arguments
    {
        for a in &args.args {
            if let GenericArgument::Type(t) = a {
                return Some(t.clone());
            }
        }
    }
    None
}

/// The generic *type* arguments of a `Foo<…>` type, for arity checks (`Input<T>` has
/// exactly one; an extra type argument is rejected by `#[system]`/`#[sequence]`).
pub fn type_args(ty: &Type) -> Vec<&Type> {
    let mut out = Vec::new();
    if let Type::Path(p) = ty
        && let Some(seg) = p.path.segments.last()
        && let PathArguments::AngleBracketed(args) = &seg.arguments
    {
        for a in &args.args {
            if let GenericArgument::Type(t) = a {
                out.push(t);
            }
        }
    }
    out
}

