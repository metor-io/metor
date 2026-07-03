use darling::FromDeriveInput;
use darling::ast::{self};
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{DeriveInput, Generics, Ident};

#[derive(Debug, FromDeriveInput)]
#[darling(attributes(fsw, metor_fsw), supports(struct_named))]
pub struct Componentize {
    ident: Ident,
    generics: Generics,
    data: ast::Data<(), crate::Field>,
    parent: Option<String>,
    name: Option<String>,
    /// Tolerated here; consumed by the bundling `Frame` derive (E4 opt-out).
    #[darling(default, rename = "no_timestamp")]
    _no_timestamp: darling::util::Ignored,
}

/// `crate_name` is the path to the crate that re-exports the trait surface the
/// generated code names — `metor-fsw` for the standalone derive, `metor-fsw-2`
/// when bundled by `#[derive(Frame)]` (so a metor-fsw-2-only consumer compiles).
pub fn componentize_impl(input: &DeriveInput, crate_name: &TokenStream2) -> TokenStream2 {
    let Componentize {
        ident,
        generics,
        data,
        parent,
        name,
        _no_timestamp: _,
    } = Componentize::from_derive_input(input).unwrap();
    let parent = parent.or(name);
    let where_clause = &generics.where_clause;
    let impeller = quote! { #crate_name::metor_proto };
    let fields = data.take_struct().unwrap();

    // sink_columns: scalar fields emit one component; nested/dynamic fields recurse
    // (a `FrameList`/`FrameMap` slot has no in-struct value, so its `sink_columns`
    // is a no-op). The timestamp field is the source only — never a component.
    let sink_calls = fields
        .fields
        .iter()
        .filter(|f| !f.timestamp)
        .map(|field| {
            let ident = field.ident.as_ref().expect("only named fields allowed");
            if field.is_nested() {
                quote! { self.#ident.sink_columns(output); }
            } else {
                let component_id = field.component_id();
                let component_id = match &parent {
                    Some(parent) => format!("{parent}.{component_id}"),
                    None => component_id,
                };
                quote! {
                    let _ = output.apply_value(
                        #impeller::types::ComponentId::new(#component_id),
                        self.#ident.as_component_view(),
                        None,
                    );
                }
            }
        });

    // MAX_SIZE (frames.md §3.4): the fixed region (`size_of::<Self>()`, which already
    // includes every 8-byte dynamic slot) plus each dynamic field's trailer budget,
    // plus an 8-byte alignment pad.
    let dyn_budgets = fields
        .fields
        .iter()
        .filter(|f| f.is_dynamic())
        .map(|field| {
            let ty = &field.ty;
            quote! { + <#ty as #crate_name::Componentize>::MAX_SIZE }
        });

    quote! {
        impl #crate_name::Componentize for #ident #generics #where_clause {
            fn sink_columns(&self, output: &mut impl #crate_name::Decomponentize) {
                use #impeller::com_de::AsComponentView;
                #(#sink_calls)*
            }

            const MAX_SIZE: usize = core::mem::size_of::<Self>() #(#dyn_budgets)* + 8;
        }
    }
}
