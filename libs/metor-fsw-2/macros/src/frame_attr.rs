//! `#[frame(name = "...")]` — the one-line frame preamble.
//!
//! A frame needs five derives, a `#[repr(C)]`, and the name attribute:
//!
//! ```ignore
//! #[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
//! #[repr(C)]
//! #[metor_fsw(name = "imu")]
//! struct Imu { ... }
//! ```
//!
//! This attribute expands to exactly that stack (the zerocopy derives
//! resolved through the framework's re-export, so consumers need no direct
//! zerocopy dependency), leaving the struct and its field attributes
//! (`#[metor_fsw(timestamp)]` etc.) untouched. Extra derives ride an
//! ordinary `#[derive(...)]` under the attribute.

use proc_macro::TokenStream;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{ItemStruct, LitStr, Token, parse_macro_input};

struct FrameArgs {
    name: LitStr,
}

impl Parse for FrameArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let ident: syn::Ident = input.parse()?;
        if ident != "name" {
            return Err(syn::Error::new_spanned(
                ident,
                "expected `#[frame(name = \"...\")]`",
            ));
        }
        input.parse::<Token![=]>()?;
        let name: LitStr = input.parse()?;
        Ok(FrameArgs { name })
    }
}

pub fn frame_attr(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as FrameArgs);
    let item = parse_macro_input!(item as ItemStruct);
    let fsw2 = crate::metor_fsw_2_crate_name();
    let name = args.name;
    quote! {
        #[derive(
            #fsw2::Frame,
            #fsw2::zerocopy::IntoBytes,
            #fsw2::zerocopy::Immutable,
            #fsw2::zerocopy::KnownLayout,
            #fsw2::zerocopy::FromBytes,
        )]
        #[repr(C)]
        #[metor_fsw(name = #name)]
        #item
    }
    .into()
}
