use convert_case::{Case, Casing};
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::quote;

use crate::input::StructInput;

/// Generates the `Decomponentize` impl, which routes an incoming component
/// value to the matching field by comparing against per-field `ComponentId`
/// constants. Unmatched ids and values that fail conversion are silently
/// skipped. See [`componentize_impl`](crate::componentize::componentize_impl)
/// for the `crate_name` root-path contract.
pub fn decomponentize_impl(input: &StructInput, crate_name: &TokenStream2) -> TokenStream2 {
    let StructInput {
        ident,
        generics,
        fields,
        parent,
        ..
    } = input;
    let where_clause = &generics.where_clause;
    let impeller = quote! { #crate_name::metor_proto };
    let if_arms = fields.iter().filter(|f| !f.timestamp && !f.skipped()).map(|field| {
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
        let component_id = field.qualified_component_name(parent.as_deref());
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
