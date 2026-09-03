use darling::util::Override;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;

use crate::input::{EnumInput, Input, StructInput};

/// Generates the `Metadatatize` impl.
///
/// `crate_name` follows the same root-path contract as
/// [`componentize_impl`](crate::componentize::componentize_impl).
pub fn metadatatize_impl(input: &Input, crate_name: &TokenStream2) -> TokenStream2 {
    let impeller_wkt = quote! { #crate_name::metor_proto_wkt };

    match input {
        Input::Enum(EnumInput {
            ident,
            generics,
            variants,
            ..
        }) => {
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
        Input::Struct(StructInput {
            ident,
            generics,
            fields,
            parent,
            group,
        }) => {
            let where_clause = &generics.where_clause;
            let metadata_items = fields
                .iter()
                .filter(|f| !f.timestamp && !f.skipped())
                .map(|field| {
                    let ty = &field.ty;
                    let name = field.qualified_component_name(parent.as_deref());
                    quote! {
                        .chain(<#ty>::metadata(prefix.clone().chain(#name)))
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
    }
}
