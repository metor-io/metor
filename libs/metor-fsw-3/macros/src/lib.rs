//! Derive macros for metor-fsw-3.

use proc_macro::TokenStream;
use proc_macro_crate::{FoundCrate, crate_name};
use proc_macro2::Span;
use quote::quote;
use syn::Ident;

mod frame;
mod record;
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

/// Rewrites a free fn into a fn returning its system. Each parameter is a port,
/// named by its identifier; one `#[state] s: &mut S` parameter is the state,
/// and without one the state is `()`. The returned value derefs to a fn
/// pointer for direct calls. An `async fn` ending in `stop: Stop` becomes an
/// async system, with no fn pointer.
///
/// ```ignore
/// #[system]
/// fn nav(#[state] s: &mut NavState, now: Timestamp, est: &mut Output<Estimate>) { .. }
///
/// table.register("nav", nav(), NavState::new)?;
/// ```
#[proc_macro_attribute]
pub fn system(_attr: TokenStream, item: TokenStream) -> TokenStream {
    system_attr::system(item)
}

/// Rewrites an `async fn` into a fn returning its sequence, polled once per
/// cycle until it returns. Parameters are ports as on `#[system]`, plus a
/// `seq: &Seq` for the time, waits, and abort. It returns
/// `Result<(), SeqError>`.
///
/// ```ignore
/// #[sequence]
/// async fn arm(seq: &Seq, cmd: &mut Output<Arm>) -> Result<(), SeqError> { .. }
///
/// table.register_sequence("arm", arm(), Default::default)?;
/// ```
#[proc_macro_attribute]
pub fn sequence(_attr: TokenStream, item: TokenStream) -> TokenStream {
    system_attr::sequence(item)
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
