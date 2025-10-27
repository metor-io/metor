use darling::FromDeriveInput;
use darling::ast;
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
}

pub fn metadatatize(input: TokenStream) -> TokenStream {
    let crate_name = crate::metor_fsw_crate_name();
    let input = parse_macro_input!(input as DeriveInput);
    let Metadatatize {
        ident,
        generics,
        data,
        parent,
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
            quote! {
                impl #crate_name::Metadatatize for #ident #generics #where_clause {
                    fn metadata(prefix: impl #crate_name::path::ComponentPath) -> impl Iterator<Item = #impeller_wkt::ComponentMetadata> {
                        core::iter::empty()
                        #(#metadata_items)*
                    }
                }
            }
            .into()
        }
    }
}
