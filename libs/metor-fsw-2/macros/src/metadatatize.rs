use darling::FromDeriveInput;
use darling::ast;
use darling::util::Override;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{DeriveInput, Generics, Ident};

/// The attribute arguments the derive understands, decoded by darling from
/// the input item together with its identity and fields.
#[derive(Debug, FromDeriveInput)]
#[darling(attributes(fsw, metor_fsw), supports(struct_named, enum_unit))]
pub struct Metadatatize {
    ident: Ident,
    generics: Generics,
    data: ast::Data<Ident, crate::Field>,
    parent: Option<String>,
    name: Option<String>,
    /// Accepted so the attribute parses when the derives are bundled; only the
    /// `Frame` derive acts on it.
    #[darling(default, rename = "no_timestamp")]
    _no_timestamp: darling::util::Ignored,
    /// Bare `#[fsw(group)]` emits a metadata-only parent entry whose
    /// `group_name` is the struct's identifier; `#[fsw(group = "Custom")]`
    /// supplies the name instead.
    #[darling(default)]
    group: Option<Override<String>>,
}

/// Generates the `Metadatatize` impl for a struct or unit enum.
///
/// `crate_name` follows the same root-path contract as
/// [`componentize_impl`](crate::componentize::componentize_impl).
pub fn metadatatize_impl(input: &DeriveInput, crate_name: &TokenStream2) -> TokenStream2 {
    let Metadatatize {
        ident,
        generics,
        data,
        parent,
        name,
        group,
        _no_timestamp: _,
    } = Metadatatize::from_derive_input(input).unwrap();
    let parent = parent.or(name);
    let where_clause = &generics.where_clause;
    let impeller_wkt = quote! { #crate_name::metor_proto_wkt };
    match data {
        ast::Data::Enum(variants) => {
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
        ast::Data::Struct(fields) => {
            let metadata_items = fields.fields.iter().filter(|f| !f.timestamp).map(|field| {
                let ty = &field.ty;

                let name = field.component_name();
                let name = if let Some(parent) = &parent {
                    format!("{parent}.{name}")
                } else {
                    name.to_string()
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
                        Override::Explicit(s) => s,
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
