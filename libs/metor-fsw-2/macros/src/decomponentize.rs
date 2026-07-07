use convert_case::{Case, Casing};
use darling::FromDeriveInput;
use darling::ast::{self};
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
    /// Accepted so the attribute parses; only the `Frame` derive acts on it.
    #[darling(default, rename = "no_timestamp")]
    _no_timestamp: darling::util::Ignored,
}

/// Generates the `Decomponentize` impl, which routes an incoming component
/// value to the matching field by comparing against per-field `ComponentId`
/// constants. Unmatched ids and values that fail conversion are silently
/// skipped. See [`componentize_impl`](crate::componentize::componentize_impl)
/// for the `crate_name` root-path contract.
pub fn decomponentize_impl(input: &DeriveInput, crate_name: &TokenStream2) -> TokenStream2 {
    let Decomponentize {
        ident,
        generics,
        data,
        parent,
        name,
        _no_timestamp: _,
    } = Decomponentize::from_derive_input(input).unwrap();
    let parent = parent.or(name);
    let where_clause = &generics.where_clause;
    let impeller = quote! { #crate_name::metor_proto };
    let fields = data.take_struct().unwrap();
    let if_arms = fields.fields.iter().filter(|f| !f.timestamp).map(|field| {
        let ty = &field.ty;
        let ident = &field.ident;
        // Nested and dynamic fields have no single id to match, so every value
        // is forwarded and the field's own `apply_value` decides what applies.
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
