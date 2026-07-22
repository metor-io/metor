use darling::util::Override;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;

use crate::frame::FrameArgs;

/// Generates the `Metadatatize` impl for a frame struct.
///
/// `crate_name` follows the same root-path contract as
/// [`componentize_impl`](crate::componentize::componentize_impl).
pub fn metadatatize_impl(args: &FrameArgs, crate_name: &TokenStream2) -> TokenStream2 {
    let FrameArgs {
        ident,
        generics,
        fields,
        frame_name: parent,
        group,
        ..
    } = args;
    let where_clause = &generics.where_clause;
    let impeller_wkt = quote! { #crate_name::metor_proto_wkt };

    let metadata_items = fields.iter().filter(|f| !f.timestamp && !f.skipped()).map(|field| {
        let ty = &field.ty;

        let name = field.component_name();
        let name = if let Some(parent) = parent {
            format!("{parent}.{name}")
        } else {
            name
        };
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
