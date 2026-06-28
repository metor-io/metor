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

/// Emits the `BindPorts::bind` body: each port type's `bind(src)`, in field order —
/// symmetric to `descriptors()`, so the ring source's positional cursor lines each
/// port up with the ring the coordinator reserved for it.
fn bind_body(bundle: &Bundle) -> TokenStream2 {
    let fields = bundle.data.as_ref().take_struct().expect("named struct");
    let binds = fields.iter().map(|f| {
        let id = f.ident.as_ref().expect("named field");
        let ty = &f.ty;
        quote! { #id: <#ty>::bind(src) }
    });
    quote! {
        Self { #(#binds),* }
    }
}

/// A copy of `generics` with every default value stripped from its type/const params.
/// Rust forbids defaults in an `impl<…>` header, so a bundle author's
/// `struct PlantOut<B: Backing = BoxBacking>` must shed the `= BoxBacking` before the
/// generated impls reuse its generics (dl-open.md §1.2).
fn strip_defaults(generics: &Generics) -> Generics {
    let mut g = generics.clone();
    for p in g.params.iter_mut() {
        match p {
            syn::GenericParam::Type(t) => t.default = None,
            syn::GenericParam::Const(c) => c.default = None,
            syn::GenericParam::Lifetime(_) => {}
        }
    }
    g
}

/// Detect the bundle's ring-[`Backing`] type param (dl-open.md §1.2): the one whose
/// bounds name `Backing`. A `Backing`-generic bundle (`struct PlantOut<B: Backing>`)
/// returns `Some(B)`, so the generated `BindPorts<B>` impl is over *that* param; a
/// non-generic bundle (whose ports pin `BoxBacking`) returns `None`, so the impl is
/// over `BoxBacking`.
fn backing_param(generics: &Generics) -> Option<Ident> {
    for p in generics.params.iter() {
        if let syn::GenericParam::Type(t) = p {
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

/// Emit the `BindPorts<B>` impl for a bundle: over the bundle's own `Backing` param if
/// it has one, else over `BoxBacking` (the bundle's ports pin `BoxBacking`). Generics
/// are default-stripped for the `impl<…>` header (dl-open.md §1.2).
fn bind_ports_impl(
    bundle: &Bundle,
    fsw2: &TokenStream2,
    bind: &TokenStream2,
) -> TokenStream2 {
    let ident = &bundle.ident;
    let stripped = strip_defaults(&bundle.generics);
    let (impl_generics, ty_generics, where_clause) = stripped.split_for_impl();
    let backing = match backing_param(&bundle.generics) {
        Some(b) => quote! { #b },
        None => quote! { #fsw2::ring::BoxBacking },
    };
    quote! {
        impl #impl_generics #fsw2::BindPorts<#backing> for #ident #ty_generics #where_clause {
            fn bind<__S: #fsw2::RingSource<B = #backing>>(src: &mut __S) -> Self {
                #bind
            }
        }
    }
}

pub fn system_input(input: TokenStream) -> TokenStream {
    let parsed = syn::parse_macro_input!(input as DeriveInput);
    let bundle = Bundle::from_derive_input(&parsed).unwrap();
    let fsw2 = crate::metor_fsw_2_crate_name();
    let ident = &bundle.ident;
    // `SystemInput` is non-generic in `B`, but the bundle may carry a defaulted
    // `Backing` param: strip defaults for the impl header (dl-open.md §1.2).
    let stripped = strip_defaults(&bundle.generics);
    let (impl_generics, ty_generics, where_clause) = stripped.split_for_impl();
    let descriptors = descriptors_body(&bundle);

    // any_lapped: OR every input port's `is_lapped()`.
    let fields = bundle.data.as_ref().take_struct().expect("named struct");
    let lapped = fields.iter().map(|f| {
        let id = f.ident.as_ref().expect("named field");
        quote! { || self.#id.is_lapped() }
    });

    let bind = bind_body(&bundle);
    let bind_ports = bind_ports_impl(&bundle, &fsw2, &bind);

    quote! {
        impl #impl_generics #fsw2::SystemInput for #ident #ty_generics #where_clause {
            fn descriptors() -> ::std::vec::Vec<#fsw2::PortDesc> {
                #descriptors
            }
            fn any_lapped(&self) -> bool {
                false #(#lapped)*
            }
        }

        #bind_ports
    }
    .into()
}

pub fn system_output(input: TokenStream) -> TokenStream {
    let parsed = syn::parse_macro_input!(input as DeriveInput);
    let bundle = Bundle::from_derive_input(&parsed).unwrap();
    let fsw2 = crate::metor_fsw_2_crate_name();
    let ident = &bundle.ident;
    let stripped = strip_defaults(&bundle.generics);
    let (impl_generics, ty_generics, where_clause) = stripped.split_for_impl();
    let descriptors = descriptors_body(&bundle);
    let bind = bind_body(&bundle);
    let bind_ports = bind_ports_impl(&bundle, &fsw2, &bind);

    quote! {
        impl #impl_generics #fsw2::SystemOutput for #ident #ty_generics #where_clause {
            fn descriptors() -> ::std::vec::Vec<#fsw2::PortDesc> {
                #descriptors
            }
        }

        #bind_ports
    }
    .into()
}
