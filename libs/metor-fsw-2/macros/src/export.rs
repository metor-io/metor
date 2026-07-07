//! Generates the C ABI a cyclic system exports when it is compiled as a
//! dynamically loaded `cdylib`.
//!
//! Each generated `#[unsafe(no_mangle)] pub extern "C" fn fsw_*` is a one-line
//! shim that delegates to the matching `metor_fsw_2::abi::run_*` helper, so
//! the real logic lives in code that can be tested without expanding the
//! macro. The function names are the string forms of the `abi::SYM_*`
//! constants the host resolves against, which keeps the two sides from
//! drifting apart.
//!
//! The system's `Params` type must implement `Serialize`, `Deserialize`, and
//! `postcard_schema::Schema`. The params blob crosses `fsw_create` as
//! canonical postcard bytes, and `fsw_describe` writes the schema into the
//! descriptor so a host can encode params without ever linking the `Params`
//! type.
//!
//! [`export_items`] is the shared generator. `export_system!` emits the items
//! ungated, while `#[system(export)]` and `#[system(export = "feature")]`
//! wrap each one in a `cfg` gate. Every item carries
//! `#[allow(clippy::not_unsafe_ptr_arg_deref)]` because raw-pointer
//! parameters are the ABI contract, sparing consuming crates a crate-level
//! allow.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{Type, parse_macro_input};

pub fn export_system(input: TokenStream) -> TokenStream {
    let ty = parse_macro_input!(input as Type);
    let fsw2 = crate::metor_fsw_2_crate_name();
    export_items(&quote! { #ty }, &fsw2, None).into()
}

/// Generates the `fsw_*` C-ABI items for the cyclic system `ty`, wrapping
/// each one in `gate` when a `cfg` gate is supplied and exporting
/// unconditionally when `gate` is `None`.
pub fn export_items(
    ty: &TokenStream2,
    fsw2: &TokenStream2,
    gate: Option<TokenStream2>,
) -> TokenStream2 {
    let gate = &gate;
    quote! {
        /// The ABI version word the host checks before making any other call.
        #gate
        #[unsafe(no_mangle)]
        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        pub extern "C" fn fsw_abi_version() -> u32 {
            #fsw2::abi::FSW_ABI_VERSION
        }

        /// Serialize this system's descriptor to the host-provided sink.
        #gate
        #[unsafe(no_mangle)]
        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        pub extern "C" fn fsw_describe(
            sink: #fsw2::abi::ByteSink,
            ctx: *mut ::core::ffi::c_void,
        ) -> i32 {
            // SAFETY: the host supplies a sink and the context pointer it expects.
            unsafe { #fsw2::abi::run_describe::<#ty>(sink, ctx) }
        }

        /// Decode the postcard `Params` blob, construct the system, and box its state.
        #gate
        #[unsafe(no_mangle)]
        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        pub extern "C" fn fsw_create(
            params: *const u8,
            params_len: usize,
        ) -> *mut ::core::ffi::c_void {
            // SAFETY: `params`/`params_len` name a readable byte range (or null/0).
            unsafe { #fsw2::abi::run_create::<#ty>(params, params_len) }
        }

        /// Rebuild the typed port bundles from the host's ring handles and run `init`.
        #gate
        #[unsafe(no_mangle)]
        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        pub extern "C" fn fsw_bind_init(
            state: *mut ::core::ffi::c_void,
            inputs: *const #fsw2::abi::FswRing,
            n_in: usize,
            outputs: *const #fsw2::abi::FswRing,
            n_out: usize,
        ) {
            // SAFETY: `state` came from `fsw_create`; the handles name live ring regions.
            unsafe { #fsw2::abi::run_bind_init::<#ty, _>(state, inputs, n_in, outputs, n_out) }
        }

        /// Run one cyclic `step`, returning an `FswStatus`.
        #gate
        #[unsafe(no_mangle)]
        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        pub extern "C" fn fsw_execute(
            state: *mut ::core::ffi::c_void,
            now: u64,
        ) -> #fsw2::abi::FswStatus {
            // SAFETY: `state` is a live pointer from `fsw_create`.
            unsafe { #fsw2::abi::run_execute::<#ty>(state, now) }
        }

        /// Run the system's `shutdown` hook once.
        #gate
        #[unsafe(no_mangle)]
        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        pub extern "C" fn fsw_shutdown(state: *mut ::core::ffi::c_void) {
            // SAFETY: `state` is a live pointer from `fsw_create`.
            unsafe { #fsw2::abi::run_shutdown::<#ty>(state) }
        }

        /// Drop the boxed state inside the library that allocated it.
        #gate
        #[unsafe(no_mangle)]
        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        pub extern "C" fn fsw_destroy(state: *mut ::core::ffi::c_void) {
            // SAFETY: `state` came from `fsw_create` and ownership transfers here exactly once.
            unsafe { #fsw2::abi::run_destroy::<#ty>(state) }
        }
    }
}
