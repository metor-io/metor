//! `#[derive(FromKdlNode)]` — generates the per-field `node.get(..)` walk that
//! turns a system's KDL config properties into its `Params` struct (wiring.md §3).
//!
//! Mirrors the hand-written `parse_*` functions in `metor-proto-kdl/src/de.rs`:
//! a required scalar/string field reads `node.get("f")` and errors if missing or
//! the wrong type, an `Option<T>` field is optional, and `#[kdl(default = expr)]`
//! falls back to `expr`. The generated code targets the `FromKdlNode` trait and
//! `LoadError`/`kdl_*` helpers in the `metor-fsw-2` `wiring` module, so it is
//! retargetable onto `facet-kdl` later without touching any system's declaration.

use proc_macro::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Expr, Fields, Type, parse_macro_input};

use crate::metor_fsw_2_crate_name;

pub fn from_kdl_node(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let ident = &input.ident;
    let struct_name = ident.to_string();
    let fsw2 = metor_fsw_2_crate_name();

    let fields = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(named) => &named.named,
            Fields::Unit => {
                // A no-field params struct: an empty walk (every system gets one).
                return quote! {
                    impl #fsw2::wiring::FromKdlNode for #ident {
                        fn from_kdl_node(
                            _node: &#fsw2::kdl::KdlNode,
                            _src: &str,
                        ) -> ::core::result::Result<Self, #fsw2::wiring::LoadError> {
                            ::core::result::Result::Ok(#ident)
                        }
                    }
                }
                .into();
            }
            Fields::Unnamed(_) => {
                return syn::Error::new_spanned(
                    ident,
                    "FromKdlNode only supports structs with named fields",
                )
                .to_compile_error()
                .into();
            }
        },
        _ => {
            return syn::Error::new_spanned(ident, "FromKdlNode only supports structs")
                .to_compile_error()
                .into();
        }
    };

    let mut inits = Vec::new();
    for field in fields {
        let fname = field.ident.as_ref().expect("named field");
        let key = fname.to_string();
        let default = parse_default(&field.attrs);

        let init = if let Some(inner) = option_inner(&field.ty) {
            // `Option<T>` → optional property; absent ⇒ `None`.
            quote! {
                #fname: #fsw2::wiring::kdl_optional::<#inner>(node, #key, #struct_name, src)?
            }
        } else if let Some(default) = default {
            // `#[kdl(default = expr)]` → optional property; absent ⇒ the default.
            let ty = &field.ty;
            quote! {
                #fname: #fsw2::wiring::kdl_optional::<#ty>(node, #key, #struct_name, src)?
                    .unwrap_or(#default)
            }
        } else {
            // A required scalar/string property.
            let ty = &field.ty;
            quote! {
                #fname: #fsw2::wiring::kdl_required::<#ty>(node, #key, #struct_name, src)?
            }
        };
        inits.push(init);
    }

    quote! {
        impl #fsw2::wiring::FromKdlNode for #ident {
            fn from_kdl_node(
                node: &#fsw2::kdl::KdlNode,
                src: &str,
            ) -> ::core::result::Result<Self, #fsw2::wiring::LoadError> {
                ::core::result::Result::Ok(#ident {
                    #( #inits ),*
                })
            }
        }
    }
    .into()
}

/// The `T` in `Option<T>`, or `None` for any other type.
fn option_inner(ty: &Type) -> Option<&Type> {
    let Type::Path(p) = ty else { return None };
    let seg = p.path.segments.last()?;
    if seg.ident != "Option" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(args) = &seg.arguments else {
        return None;
    };
    args.args.iter().find_map(|a| match a {
        syn::GenericArgument::Type(t) => Some(t),
        _ => None,
    })
}

/// The expression in `#[kdl(default = expr)]`, if present.
fn parse_default(attrs: &[syn::Attribute]) -> Option<Expr> {
    let mut found = None;
    for attr in attrs {
        if !attr.path().is_ident("kdl") {
            continue;
        }
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("default") {
                found = Some(meta.value()?.parse::<Expr>()?);
            }
            Ok(())
        });
    }
    found
}
