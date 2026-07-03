//! `#[derive(SystemInput)]` / `#[derive(SystemOutput)]` (system.md §2, Q5).
//!
//! A system's input/output bundle is a named struct of `Input<F>` / `Output<F>` /
//! `MsgIn<M>` / `MsgOut<M>` ports. These derives generate the static `descriptors()`
//! (and, for inputs, `any_lapped()`; for outputs, `take_dropped()`) by delegating to
//! each port type's own `descriptor()`/`is_lapped()`/`take_dropped()` — so the macro
//! never has to parse `F` out of the field type.
//!
//! A `PhantomData` field is skipped everywhere (and default-constructed by `bind`):
//! `#[system]`'s generated bundles carry one to anchor the `__B: Backing` param when
//! a direction has no ports.

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

impl BundleField {
    /// Whether this field is a `PhantomData` anchor rather than a port.
    fn is_phantom(&self) -> bool {
        crate::sig::type_head(&self.ty).is_some_and(|h| h == "PhantomData")
    }
}

#[derive(Debug, FromDeriveInput)]
#[darling(supports(struct_named))]
struct Bundle {
    ident: Ident,
    generics: Generics,
    data: ast::Data<Ignored, BundleField>,
}

impl Bundle {
    /// The port fields, in declaration order (`PhantomData` anchors skipped).
    fn ports(&self) -> Vec<&BundleField> {
        self.data
            .as_ref()
            .take_struct()
            .expect("named struct")
            .into_iter()
            .filter(|f| !f.is_phantom())
            .collect()
    }
}

/// Emits the `descriptors()` body: each port type's `descriptor()`, in field order.
fn descriptors_body(bundle: &Bundle) -> TokenStream2 {
    let calls = bundle.ports().into_iter().map(|f| {
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
/// port up with the ring the coordinator reserved for it. `PhantomData` anchors are
/// default-constructed (they consume no ring).
fn bind_body(bundle: &Bundle) -> TokenStream2 {
    let fields = bundle.data.as_ref().take_struct().expect("named struct");
    let binds = fields.iter().map(|f| {
        let id = f.ident.as_ref().expect("named field");
        let ty = &f.ty;
        if f.is_phantom() {
            quote! { #id: ::core::marker::PhantomData }
        } else {
            quote! { #id: <#ty>::bind(src) }
        }
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

/// Emit the `BindPorts<B>` impl for a bundle: over the bundle's own `Backing` param if
/// it has one (detected by bound, `sig::backing_param`), else over `BoxBacking` (the
/// bundle's ports pin `BoxBacking`). Generics are default-stripped for the `impl<…>`
/// header (dl-open.md §1.2).
fn bind_ports_impl(bundle: &Bundle, fsw2: &TokenStream2, bind: &TokenStream2) -> TokenStream2 {
    let ident = &bundle.ident;
    let stripped = strip_defaults(&bundle.generics);
    let (impl_generics, ty_generics, where_clause) = stripped.split_for_impl();
    let backing = match crate::sig::backing_param(&bundle.generics) {
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
    let lapped = bundle.ports().into_iter().map(|f| {
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

    // take_dropped: sum-and-clear every output port's publish-drop counter (E6).
    let dropped = bundle.ports().into_iter().map(|f| {
        let id = f.ident.as_ref().expect("named field");
        quote! { + self.#id.take_dropped() }
    });

    quote! {
        impl #impl_generics #fsw2::SystemOutput for #ident #ty_generics #where_clause {
            fn descriptors() -> ::std::vec::Vec<#fsw2::PortDesc> {
                #descriptors
            }
            fn take_dropped(&mut self) -> u64 {
                0 #(#dropped)*
            }
        }

        #bind_ports
    }
    .into()
}
