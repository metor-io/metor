//! Derive macros for metor-fsw-3.

use proc_macro::TokenStream;
use proc_macro_crate::{FoundCrate, crate_name};
use proc_macro2::Span;
use quote::quote;
use syn::Ident;

mod frame;
mod record;
mod system;
mod system_attr;

/// Derives the four component sub-traits and `Frame`. Configured with
/// `#[frame(name = "..")]` and one `#[frame(timestamp)]` field.
#[proc_macro_derive(Frame, attributes(frame))]
pub fn frame_derive(input: TokenStream) -> TokenStream {
    frame::frame(input)
}

/// Derives `Record` over postcard, configured with `#[record(name, max_len, depth)]`.
#[proc_macro_derive(Record, attributes(record))]
pub fn record_derive(input: TokenStream) -> TokenStream {
    record::record(input)
}

/// Emits `SystemFn` for an impl block from its `execute(&mut self, ..)` method.
#[proc_macro_attribute]
pub fn system(_attr: TokenStream, item: TokenStream) -> TokenStream {
    system_attr::system(item)
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
        Ok(FoundCrate::Itself) => quote!(::metor_fsw_3),
        Ok(FoundCrate::Name(name)) => {
            let ident = Ident::new(&name, Span::call_site());
            quote!( #ident )
        }
        // PANIC Safety: expansion has no crate to name; a compile error here
        // would point at the derive rather than the missing dependency.
        Err(_) => panic!("metor-fsw-3 macros require `metor-fsw-3` in [dependencies]"),
    }
}
