//! Type-shape helpers shared by `#[system]` and `#[sequence]`.
//!
//! Both macros classify each fn parameter as a port by the last path segment
//! of its type, so `fsw::Input<T>`, `crate::Input<T>`, and `Input<T>` all read
//! the same. These helpers pull that segment and its generic arguments apart.

use syn::{GenericArgument, Ident, PathArguments, Type};

/// The last path segment ident of a type, so `fsw::Input<T>` yields `Input`.
pub fn type_head(ty: &Type) -> Option<&Ident> {
    if let Type::Path(p) = ty {
        p.path.segments.last().map(|s| &s.ident)
    } else {
        None
    }
}


/// All generic type arguments of the last path segment. Callers use the
/// count to reject ports with the wrong arity.
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
