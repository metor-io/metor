//! `#[derive(Record)]` for messages encoded with postcard.

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
    ty: syn::Type,
}

/// The struct being derived, with its `#[record(..)]` options. Only structs
/// can be records; darling rejects enums and unions.
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
    let fsw = match crate::fsw_crate() {
        Ok(fsw) => fsw,
        Err(e) => return e.to_compile_error().into(),
    };
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
    // PANIC Safety: darling's `supports(struct_any)` rejects every other
    // shape before this point.
    let fields = raw.data.as_ref().take_struct().expect("a struct").fields;
    let mut stamps = fields.into_iter().filter(|f| f.timestamp);
    let stamp = stamps.next();
    if let Some(extra) = stamps.next() {
        let message = "only one field can be marked #[record(timestamp)]";
        return syn::Error::new_spanned(&extra.ty, message)
            .to_compile_error()
            .into();
    }
    let timestamp = match stamp {
        None => None,
        Some(RecordField {
            ident: Some(id), ..
        }) => Some(quote! {
            fn timestamp(&self) -> Option<#fsw::Timestamp> { Some(self.#id) }
        }),
        Some(RecordField {
            ident: None, ty, ..
        }) => {
            let message = "#[record(timestamp)] needs a named field";
            return syn::Error::new_spanned(ty, message)
                .to_compile_error()
                .into();
        }
    };
    let ident = &raw.ident;
    let (impl_generics, ty_generics, where_clause) = raw.generics.split_for_impl();
    quote! {
        impl #impl_generics #fsw::Record for #ident #ty_generics #where_clause {
            const NAME: &'static str = #name;
            const MAX_LEN: usize = #max_len;
            #depth
            #timestamp
            type Read<'a> = Self where Self: 'a;
            fn encode<'a>(&'a self, buf: &'a mut [u8]) -> Result<&'a [u8], #fsw::EncodeError> {
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
