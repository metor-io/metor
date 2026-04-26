use darling::FromDeriveInput;
use darling::ast;
use darling::util::Override;
use proc_macro::TokenStream;
use quote::quote;
use syn::{DeriveInput, Generics, Ident, parse_macro_input};

#[derive(Debug, FromDeriveInput)]
#[darling(attributes(metor_fsw), supports(struct_named, enum_unit))]
pub struct Metadatatize {
    ident: Ident,
    generics: Generics,
    data: ast::Data<Ident, crate::Field>,
    parent: Option<String>,
    /// `#[metor_fsw(group)]` opts the struct into the data-table grouping
    /// model: emits a metadata-only parent entry at the struct's path with
    /// `group_name = <Ident>`. `#[metor_fsw(group = "Custom")]` overrides
    /// the label.
    #[darling(default)]
    group: Option<Override<String>>,
}

pub fn metadatatize(input: TokenStream) -> TokenStream {
    let crate_name = crate::metor_fsw_crate_name();
    let input = parse_macro_input!(input as DeriveInput);
    let Metadatatize {
        ident,
        generics,
        data,
        parent,
        group,
    } = Metadatatize::from_derive_input(&input).unwrap();
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
            .into()
        }
        ast::Data::Struct(fields) => {
            let metadata_items = fields.fields.iter().map(|field| {
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
            // Group-parent emission is opt-in. When the struct is tagged
            // `#[metor_fsw(group)]` (or `#[metor_fsw(group = "Name")]`),
            // emit a metadata-only entry at the struct's own path with
            // `group_name` set, so the data-table view can cluster each
            // instance of this struct into a group. Always skipped at the
            // empty root prefix — an empty name would collide across
            // roots.
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
            .into()
        }
    }
}
