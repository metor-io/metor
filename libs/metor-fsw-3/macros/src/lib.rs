//! Derive and attribute macros for metor-fsw-3.

use proc_macro::TokenStream;
use proc_macro_crate::{FoundCrate, crate_name};
use proc_macro2::Span;
use quote::quote;
use syn::Ident;

mod frame;
mod record;
mod system_attr;

/// Derives `Frame` for a struct sent as its raw bytes.
///
/// The derive also implements `Record` and the four metor component traits.
/// Mark one field `#[frame(timestamp)]`.
/// `#[frame(name = "..")]` sets the record name, which defaults to the struct
/// name in snake_case.
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

/// Turns a free fn into a system.
///
/// `#[system] fn nav(..)` becomes `fn nav() -> impl SystemFn`, which you
/// register with a table. Each parameter is a port, named after the parameter.
/// Mark one `&mut S` parameter `#[state]` to give the system state. Without
/// one, the state is `()`. The value `nav()` returns derefs to a fn pointer to
/// the original body, so tests can call it directly. The fn's doc comment
/// becomes the system's documentation.
///
/// An `async fn` whose last parameter is `stop: Stop` becomes an async system
/// instead. It has no fn pointer.
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

/// Turns an `async fn` into a sequence.
///
/// A sequence is an async fn that the runtime polls once per cycle until it
/// returns. `#[sequence] async fn arm(..)` becomes `fn arm() -> impl SequenceFn`.
/// Parameters are ports, as with `#[system]`. A `seq: &Seq` parameter gives the
/// sequence the current time, waits, and aborts. The fn must return
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

/// Returns the path generated code uses to reach `metor-fsw-3`.
///
/// # Panics
/// Panics if the calling crate does not list `metor-fsw-3` in `[dependencies]`.
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
