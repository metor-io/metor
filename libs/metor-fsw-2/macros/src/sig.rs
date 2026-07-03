//! Shared signature → port classification for `#[system]` and `#[sequence]`
//! (`docs/design-system-macro.md` §5): both macros read their port set off an fn
//! signature by the **last path segment** of each parameter type, and both inject a
//! hidden `__B: Backing` generic by rewriting the port parameter types in place.

use proc_macro2::Span;
use syn::{GenericArgument, GenericParam, Generics, Ident, PathArguments, Type};

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
/// exactly one; an explicit backing `Input<T, RawBacking>` is rejected by `#[system]`).
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

/// The declared type param whose bounds name `Backing` (the same detection the
/// `SystemInput`/`SystemOutput` derives use on a bundle struct): `Some(B)` for a
/// hand-written `<B: Backing>`, `None` when the macro should inject `__B` itself.
pub fn backing_param(generics: &Generics) -> Option<Ident> {
    for p in generics.params.iter() {
        if let GenericParam::Type(t) = p {
            let is_backing = t.bounds.iter().any(|b| {
                matches!(b, syn::TypeParamBound::Trait(tb)
                    if tb.path.segments.last().is_some_and(|s| s.ident == "Backing"))
            });
            if is_backing {
                return Some(t.ident.clone());
            }
        }
    }
    None
}

/// Append generic type arguments to the **last path segment** of `ty` in place:
/// `Input<Sensors>` + `[__B]` → `Input<Sensors, __B>`. Keeps the user's own path
/// spelling (so their `use` imports stay used) while the macro supplies the backing
/// (and, for async systems, the wake endpoints).
pub fn append_type_args(ty: &mut Type, extra: &[Type]) {
    if extra.is_empty() {
        return;
    }
    let Type::Path(p) = ty else { return };
    let Some(seg) = p.path.segments.last_mut() else {
        return;
    };
    match &mut seg.arguments {
        PathArguments::AngleBracketed(args) => {
            for t in extra {
                args.args.push(GenericArgument::Type(t.clone()));
            }
        }
        PathArguments::None => {
            let mut args: syn::AngleBracketedGenericArguments = syn::AngleBracketedGenericArguments {
                colon2_token: None,
                lt_token: Default::default(),
                args: Default::default(),
                gt_token: Default::default(),
            };
            for t in extra {
                args.args.push(GenericArgument::Type(t.clone()));
            }
            seg.arguments = PathArguments::AngleBracketed(args);
        }
        PathArguments::Parenthesized(_) => {}
    }
}

/// A bare type naming `ident` (the injected `__B` as a `syn::Type`).
pub fn ident_type(ident: &Ident) -> Type {
    let mut segments = syn::punctuated::Punctuated::new();
    segments.push(syn::PathSegment {
        ident: ident.clone(),
        arguments: PathArguments::None,
    });
    Type::Path(syn::TypePath {
        qself: None,
        path: syn::Path {
            leading_colon: None,
            segments,
        },
    })
}

/// The conventional injected backing ident.
pub fn injected_backing_ident() -> Ident {
    Ident::new("__B", Span::call_site())
}
