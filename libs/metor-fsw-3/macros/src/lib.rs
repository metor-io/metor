//! Derive macros for metor-fsw-3.

use proc_macro::TokenStream;
use proc_macro_crate::{FoundCrate, crate_name};
use proc_macro2::Span;
use quote::quote;
use syn::Ident;

mod frame;
mod record;

/// Derives `Frame` for a struct sent as its raw bytes.
///
/// The derive also implements `Record` and the four metor component traits.
/// Mark one field `#[frame(timestamp)]`.
/// `#[frame(name = "..")]` sets the record name, which defaults to the struct
/// name in snake_case. `#[frame(depth = N)]` sets `Record::DEPTH`.
#[proc_macro_derive(Frame, attributes(frame))]
pub fn frame_derive(input: TokenStream) -> TokenStream {
    frame::frame(input)
}

/// Derives `Record` for a type encoded with postcard.
///
/// `#[record(name = "..")]` sets the record name, which defaults to the type
/// name in snake_case. `max_len` sets the largest encoded size, which defaults
/// to the type's `MaxSize` bound. `depth` sets `Record::DEPTH`. A field marked
/// `#[record(timestamp)]` becomes the record's timestamp.
#[proc_macro_derive(Record, attributes(record))]
pub fn record_derive(input: TokenStream) -> TokenStream {
    record::record(input)
}

/// Returns the path generated code uses to reach `metor-fsw-3`.
///
/// Fails if the calling crate does not list `metor-fsw-3` in `[dependencies]`.
pub(crate) fn fsw_crate() -> syn::Result<proc_macro2::TokenStream> {
    match crate_name("metor-fsw-3") {
        Ok(FoundCrate::Itself) => Ok(quote!(::metor_fsw_3)),
        Ok(FoundCrate::Name(name)) => {
            let ident = Ident::new(&name, Span::call_site());
            Ok(quote!( #ident ))
        }
        Err(_) => Err(syn::Error::new(
            Span::call_site(),
            "metor-fsw-3 macros require `metor-fsw-3` in [dependencies]",
        )),
    }
}
