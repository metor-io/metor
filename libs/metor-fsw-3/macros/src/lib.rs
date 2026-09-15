//! Derive macros for metor-fsw-3.

use proc_macro::TokenStream;
use proc_macro_crate::{FoundCrate, crate_name};
use proc_macro2::Span;
use quote::quote;
use syn::Ident;

mod frame;
mod system;

/// Derives the four component sub-traits and `Frame`. Configured with
/// `#[frame(name = "..")]` and one `#[frame(timestamp)]` field.
#[proc_macro_derive(Frame, attributes(frame))]
pub fn frame_derive(input: TokenStream) -> TokenStream {
    frame::frame(input)
}

/// Derives `SystemInputs` for a struct of `Input<F>` fields.
#[proc_macro_derive(SystemInputs)]
pub fn system_inputs(input: TokenStream) -> TokenStream {
    system::system_inputs(input)
}

/// Derives `SystemOutputs` for a struct of `Output<F>` fields.
#[proc_macro_derive(SystemOutputs)]
pub fn system_outputs(input: TokenStream) -> TokenStream {
    system::system_outputs(input)
}

/// The path generated code uses to reach `metor-fsw-3`.
///
/// # Panics
/// A consumer without the crate in `[dependencies]` cannot use these macros.
pub(crate) fn fsw_crate() -> proc_macro2::TokenStream {
    match crate_name("metor-fsw-3") {
        Ok(FoundCrate::Itself) => quote!(crate),
        Ok(FoundCrate::Name(name)) => {
            let ident = Ident::new(&name, Span::call_site());
            quote!( #ident )
        }
        // PANIC Safety: expansion has no crate to name; a compile error here
        // would point at the derive rather than the missing dependency.
        Err(_) => panic!("metor-fsw-3 macros require `metor-fsw-3` in [dependencies]"),
    }
}
