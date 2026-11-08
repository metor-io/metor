use darling::util::Override;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;

use crate::input::{EnumInput, Input, StructInput};

/// Generates the `Metadatatize` impl.
///
/// `crate_name` follows the same root-path contract as
/// [`componentize_impl`](crate::componentize::componentize_impl).
pub fn metadatatize_impl(input: &Input, crate_name: &TokenStream2) -> TokenStream2 {
    match input {
        Input::Enum(input) => enum_metadatatize_impl(input, crate_name),
        Input::Struct(input) => struct_metadatatize_impl(input, crate_name),
    }
}

/// Generates the `Metadatatize` impl for an enum.
pub fn enum_metadatatize_impl(input: &EnumInput, crate_name: &TokenStream2) -> TokenStream2 {
    let impeller_wkt = quote! { #crate_name::metor_proto_wkt };
    let EnumInput {
        ident,
        generics,
        variants,
        ..
    } = input;
    let where_clause = &generics.where_clause;
    let variants = variants.iter().map(|v| v.to_string()).collect::<Vec<_>>();
    quote! {
        impl #crate_name::Metadatatize for #ident #generics #where_clause {
            fn metadata(prefix: impl #crate_name::path::ComponentPath) -> impl Iterator<Item = #impeller_wkt::ComponentMetadata> {
                std::iter::once(prefix.to_metadata().with_enum([
                    #(#variants),*
                ]))
            }
        }
    }
}

/// Generates the `Metadatatize` impl for a struct.
pub fn struct_metadatatize_impl(input: &StructInput, crate_name: &TokenStream2) -> TokenStream2 {
    let impeller_wkt = quote! { #crate_name::metor_proto_wkt };
    let StructInput {
        ident,
        generics,
        fields,
        parent,
        group,
        ..
    } = input;
    let where_clause = &generics.where_clause;
    let metadata_items = fields.iter().filter(|f| f.is_member()).map(|field| {
        let ty = &field.ty;
        let prefix = match field.segment(parent.as_deref()) {
            Some(name) => quote! { prefix.clone().chain(#name) },
            None => quote! { prefix.clone() },
        };
        let entries: Vec<(&str, &str)> = field
            .unit
            .iter()
            .map(|unit| ("unit", unit.as_str()))
            .chain(field.metadata.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .collect();
        let set = (!entries.is_empty()).then(|| {
            let keys = entries.iter().map(|(k, _)| k);
            let values = entries.iter().map(|(_, v)| v);
            quote! {
                .map(|mut m| {
                    use #impeller_wkt::MetadataExt as _;
                    #(m.set(#keys, #values);)*
                    m
                })
            }
        });
        quote! {
            .chain(<#ty>::metadata(#prefix) #set)
        }
    });
    // No group entry at an empty root prefix, since the empty name
    // would collide across roots.
    let group_emit = match group {
        None => quote! {
            let group_parent: Option<#impeller_wkt::ComponentMetadata> = None;
        },
        Some(override_name) => {
            let label = match override_name {
                Override::Inherit => ident.to_string(),
                Override::Explicit(s) => s.clone(),
            };
            quote! {
                let group_parent: Option<#impeller_wkt::ComponentMetadata> =
                    if prefix.is_empty() {
                        None
                    } else {
                        use #impeller_wkt::MetadataExt as _;
                        let mut m = prefix.to_metadata();
                        m.set("group_name", #label);
                        Some(m)
                    };
            }
        }
    };
    quote! {
        impl #crate_name::Metadatatize for #ident #generics #where_clause {
            fn metadata(prefix: impl #crate_name::path::ComponentPath) -> impl Iterator<Item = #impeller_wkt::ComponentMetadata> {
                #group_emit
                group_parent.into_iter()
                #(#metadata_items)*
            }
        }
    }
}
