//! Derive macros for frames, port bundles, and parameter documentation.
//!
//! The framework re-exports these macros. Expansions resolve its dependency
//! name so renamed dependencies work. Frame attributes accept both `fsw`
//! and `metor_fsw`; port bundle attributes use `fsw`.

use proc_macro::TokenStream;
use proc_macro_crate::{FoundCrate, crate_name};
use proc_macro2::Span;
use quote::quote;
use syn::Ident;

mod frame;
mod params_docs;
mod system;

/// Derives the four component sub-traits and `Frame` itself, making a struct
/// a frame with a single annotation. Fields are configured with
/// `#[fsw(...)]`.
#[proc_macro_derive(Frame, attributes(fsw, metor_fsw))]
pub fn frame_derive(input: TokenStream) -> TokenStream {
    frame::frame(input)
}

/// Derives `ParamsDocs`: submits a system's `Params` field doc comments into
/// the crate-local params-docs collection, so pack module generation can render
/// them. Docs are optional; undocumented fields contribute nothing.
#[proc_macro_derive(ParamsDocs)]
pub fn params_docs_derive(input: TokenStream) -> TokenStream {
    params_docs::params_docs(input)
}

/// Derives `SystemInput` and `BindPorts` for a bundle of input ports.
#[proc_macro_derive(SystemInput, attributes(fsw))]
pub fn system_input(input: TokenStream) -> TokenStream {
    system::system_input(input)
}

/// Derives `SystemOutput` and `BindPorts` for a bundle of output ports.
///
/// `#[fsw(telemetered = false)]` keeps a field's output off the telemetry
/// tap; a `CommandOut<M>` field is recognized as the same opt-out.
#[proc_macro_derive(SystemOutput, attributes(fsw))]
pub fn system_output(input: TokenStream) -> TokenStream {
    system::system_output(input)
}

/// The path generated code uses to reach the framework crate: the name the
/// consumer depends on it under, or `crate` when the framework crate is
/// compiling its own tests.
///
/// Everything these macros expand to lives in `metor-fsw-2-core`, so that is
/// the first name probed — a pack or contract crate depends on it directly.
/// A target crate depends only on the host `metor-fsw-2`, which re-exports
/// core whole, so the same paths resolve through it. A consumer with neither
/// cannot use any of these macros, so that case is a hard panic.
pub(crate) fn metor_fsw_2_crate_name() -> proc_macro2::TokenStream {
    for name in ["metor-fsw-2-core", "metor-fsw-2"] {
        match crate_name(name) {
            Ok(FoundCrate::Itself) => return quote!(crate),
            Ok(FoundCrate::Name(name)) => {
                let ident = Ident::new(&name, Span::call_site());
                return quote!( #ident );
            }
            Err(_) => continue,
        }
    }
    // This crate's own expansion unit tests run without a consumer manifest
    // to resolve against.
    if cfg!(test) {
        return quote!(metor_fsw_2_core);
    }
    panic!("metor-fsw-2 macros require `metor-fsw-2-core` (or `metor-fsw-2`) in [dependencies]")
}
