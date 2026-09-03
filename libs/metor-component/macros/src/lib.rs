//! Proc macros behind the [`metor_component`] derive surface.
//!
//! Every macro is re-exported by the component crate, and every expansion
//! reaches back to it through a path resolved at expansion time (see
//! [`metor_component_crate_name`]), so the generated code works no matter what
//! the consumer renamed the dependency to.
//!
//! These are the *standalone* derives: they turn any `#[repr(C)]` struct into
//! a component group, with `#[metor_fsw(parent = "…")]` setting the dotted
//! prefix. `metor-fsw-2` bundles equivalent expansions into
//! `#[derive(Frame)]`; reach for these when a struct is a component group but
//! not a frame in its own right — a nested sub-struct, say.
//!
//! Field and struct attributes live under `#[fsw(...)]`; the longer
//! `#[metor_fsw(...)]` spelling is accepted as an alias.
//!
//! [`metor_component`]: ../metor_component/index.html

use proc_macro::TokenStream;
use proc_macro_crate::{FoundCrate, crate_name};
use proc_macro2::Span;
use quote::quote;
use syn::Ident;

mod as_vtable;
mod componentize;
mod decomponentize;
mod metadatatize;

#[proc_macro_derive(Metadatatize, attributes(fsw, metor_fsw))]
pub fn metadatize(input: TokenStream) -> TokenStream {
    metadatatize::metadatatize(input)
}

#[proc_macro_derive(AsVTable, attributes(fsw, metor_fsw))]
pub fn as_vtable(input: TokenStream) -> TokenStream {
    as_vtable::as_vtable(input)
}

#[proc_macro_derive(Componentize, attributes(fsw, metor_fsw))]
pub fn componentize(input: TokenStream) -> TokenStream {
    componentize::componentize(input)
}

#[proc_macro_derive(Decomponentize, attributes(fsw, metor_fsw))]
pub fn decomponentize(input: TokenStream) -> TokenStream {
    decomponentize::decomponentize(input)
}

/// The path generated code uses to reach the component crate: the name the
/// consumer depends on it under, or `crate` when the component crate is
/// compiling itself. A consumer without the dependency cannot use any of these
/// derives, so that case is a hard panic.
pub(crate) fn metor_component_crate_name() -> proc_macro2::TokenStream {
    match crate_name("metor-component") {
        Ok(FoundCrate::Itself) => quote!(crate),
        Ok(FoundCrate::Name(name)) => {
            let ident = Ident::new(&name, Span::call_site());
            quote!( #ident )
        }
        // This crate's own expansion tests run without a consumer manifest to
        // resolve against.
        Err(_) if cfg!(test) => quote!(metor_component),
        Err(e) => panic!("the component derives require `metor-component` in [dependencies]: {e}"),
    }
}

