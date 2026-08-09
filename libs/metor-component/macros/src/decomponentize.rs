use convert_case::{Case, Casing};
use darling::FromDeriveInput;
use darling::ast::{self};
use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::quote;
use syn::{DeriveInput, Generics, Ident};

#[derive(Debug, FromDeriveInput)]
#[darling(attributes(fsw, metor_fsw), supports(struct_named))]
pub struct Decomponentize {
    ident: Ident,
    generics: Generics,
    data: ast::Data<(), crate::Field>,
    parent: Option<String>,
    name: Option<String>,
}

pub fn decomponentize(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as DeriveInput);
    decomponentize_impl(&input, &crate::metor_component_crate_name()).into()
}

/// See [`componentize_impl`](crate::componentize::componentize_impl) for the
/// `crate_name` root-path contract.
pub fn decomponentize_impl(input: &DeriveInput, crate_name: &TokenStream2) -> TokenStream2 {
    let Decomponentize {
        ident,
        generics,
        data,
        parent,
        name,
    } = Decomponentize::from_derive_input(input).unwrap();
    let parent = parent.or(name);
    let where_clause = &generics.where_clause;
    let impeller = quote! { #crate_name::metor_proto };
    let fields = data.take_struct().unwrap();
    let if_arms = fields.fields.iter().filter(|f| !f.timestamp && !f.skipped()).map(|field| {
        let ty = &field.ty;
        let ident = &field.ident;
        // Nested/dynamic fields forward every value (a `FrameList`/`FrameMap` slot
        // can't be reconstructed from individual scalar components, so its
        // `apply_value` is a no-op).
        if field.is_nested() {
            return quote! {
                self.#ident.apply_value(component_id, view.clone(), timestamp)?;
            };
        }
        let name = field
            .ident
            .as_ref()
            .expect("only named field allowed")
            .to_string()
            .to_case(Case::UpperSnake);
        let component_id = field.component_id();
        let component_id = match &parent {
            Some(parent) => format!("{parent}.{component_id}"),
            None => component_id,
        };
        let component_id = quote! { #impeller::types::ComponentId::new(#component_id) };
        let const_name = syn::Ident::new(&format!("{name}_ID"), Span::call_site());
        quote! {
            const #const_name: #impeller::types::ComponentId = #component_id;
            if component_id == #const_name {
                if let Ok(val) = <#ty as #impeller::com_de::FromComponentView>::from_component_view(view.clone()) {
                    self.#ident = val;
                }
            }
        }
    });
    quote! {
        impl #crate_name::Decomponentize for #ident #generics #where_clause {
            type Error = core::convert::Infallible;
            fn apply_value(&mut self,
                            component_id: #impeller::types::ComponentId,
                            view: #impeller::types::ComponentView<'_>,
                            timestamp: Option<#impeller::types::Timestamp>
            ) -> Result<(), Self::Error>{
                #(#if_arms)*
                Ok(())
            }
        }
    }
}
