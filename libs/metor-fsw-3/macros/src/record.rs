//! `#[derive(Record)]` for messages carried as postcard.

use convert_case::{Case, Casing};
use darling::ast;
use darling::{FromDeriveInput, FromField};
use proc_macro::TokenStream;
use quote::quote;
use syn::{DeriveInput, Generics, Ident, parse_macro_input};

/// One field, which may be marked `#[record(timestamp)]`.
#[derive(FromField)]
#[darling(attributes(record))]
struct RecordField {
    ident: Option<Ident>,
    #[darling(default)]
    timestamp: bool,
}

/// The struct being derived plus its `#[record(..)]` attribute.
#[derive(FromDeriveInput)]
#[darling(attributes(record), supports(struct_any))]
struct RecordInput {
    ident: Ident,
    generics: Generics,
    data: ast::Data<(), RecordField>,
    name: Option<String>,
    max_len: Option<usize>,
    depth: Option<usize>,
}

/// Expands `#[derive(Record)]`.
pub fn record(input: TokenStream) -> TokenStream {
    let parsed = parse_macro_input!(input as DeriveInput);
    let raw = match RecordInput::from_derive_input(&parsed) {
        Ok(raw) => raw,
        Err(e) => return e.write_errors().into(),
    };
    let fsw = crate::fsw_crate();
    let name = raw
        .name
        .unwrap_or_else(|| raw.ident.to_string().to_case(Case::Snake));
    let max_len = match raw.max_len {
        Some(len) => quote! { #len },
        None => quote! { <Self as #fsw::MaxSize>::POSTCARD_MAX_SIZE },
    };
    let depth = raw
        .depth
        .map(|depth| quote! { const DEPTH: usize = #depth; });
    let timestamp = raw.data.as_ref().take_struct().and_then(|fields| {
        fields
            .into_iter()
            .find(|f| f.timestamp)
            .and_then(|f| f.ident.as_ref())
            .map(|id| {
                quote! {
                    fn timestamp(&self) -> Option<#fsw::Timestamp> { Some(self.#id) }
                }
            })
    });
    let ident = &raw.ident;
    let (impl_generics, ty_generics, where_clause) = raw.generics.split_for_impl();
    quote! {
        impl #impl_generics #fsw::Record for #ident #ty_generics #where_clause {
            const NAME: &'static str = #name;
            const MAX_LEN: usize = #max_len;
            #depth
            #timestamp
            type Read<'a> = Self where Self: 'a;
            fn encode<'a>(&'a self, buf: &'a mut [u8])
                -> Result<&'a [u8], #fsw::EncodeError> {
                #fsw::record::postcard::encode(self, buf)
            }
            fn decode(bytes: &[u8]) -> Result<Self, #fsw::DecodeError> {
                #fsw::record::postcard::decode(bytes)
            }
            fn schema() -> #fsw::record::RecordSchema {
                #fsw::record::RecordSchema::postcard::<Self>(#name)
            }
        }
    }
    .into()
}
