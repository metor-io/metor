//! `#[derive(SystemInput)]` / `#[derive(SystemOutput)]` (system.md §2, Q5).
//!
//! A system's input/output bundle is a named struct of `Input<F>` / `Output<F>`
//! ports. These derives generate the static `descriptors()` (and, for inputs,
//! `any_lapped()`) by delegating to each port type's own `descriptor()`/
//! `is_lapped()` — so the macro never has to parse `F` out of the field type.

use darling::FromDeriveInput;
use darling::ast;
use darling::util::Ignored;
use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{DeriveInput, Generics, Ident};

#[derive(Debug, darling::FromField)]
struct BundleField {
    ident: Option<Ident>,
    ty: syn::Type,
}

#[derive(Debug, FromDeriveInput)]
#[darling(supports(struct_named))]
struct Bundle {
    ident: Ident,
    generics: Generics,
    data: ast::Data<Ignored, BundleField>,
}

/// Emits the `descriptors()` body: each port type's `descriptor()`, in field order.
fn descriptors_body(bundle: &Bundle) -> TokenStream2 {
    let fields = bundle.data.as_ref().take_struct().expect("named struct");
    let calls = fields.iter().map(|f| {
        let ty = &f.ty;
        quote! { descs.push(<#ty>::descriptor()); }
    });
    quote! {
        let mut descs = ::std::vec::Vec::new();
        #(#calls)*
        descs
    }
}

pub fn system_input(input: TokenStream) -> TokenStream {
    let parsed = syn::parse_macro_input!(input as DeriveInput);
    let bundle = Bundle::from_derive_input(&parsed).unwrap();
    let fsw2 = crate::metor_fsw_2_crate_name();
    let ident = &bundle.ident;
    let generics = &bundle.generics;
    let where_clause = &generics.where_clause;
    let descriptors = descriptors_body(&bundle);

    // any_lapped: OR every input port's `is_lapped()`.
    let fields = bundle.data.as_ref().take_struct().expect("named struct");
    let lapped = fields.iter().map(|f| {
        let id = f.ident.as_ref().expect("named field");
        quote! { || self.#id.is_lapped() }
    });

    quote! {
        impl #fsw2::SystemInput for #ident #generics #where_clause {
            fn descriptors() -> ::std::vec::Vec<#fsw2::PortDesc> {
                #descriptors
            }
            fn any_lapped(&self) -> bool {
                false #(#lapped)*
            }
        }
    }
    .into()
}

pub fn system_output(input: TokenStream) -> TokenStream {
    let parsed = syn::parse_macro_input!(input as DeriveInput);
    let bundle = Bundle::from_derive_input(&parsed).unwrap();
    let fsw2 = crate::metor_fsw_2_crate_name();
    let ident = &bundle.ident;
    let generics = &bundle.generics;
    let where_clause = &generics.where_clause;
    let descriptors = descriptors_body(&bundle);

    quote! {
        impl #fsw2::SystemOutput for #ident #generics #where_clause {
            fn descriptors() -> ::std::vec::Vec<#fsw2::PortDesc> {
                #descriptors
            }
        }
    }
    .into()
}
