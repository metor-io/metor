//! `export_system!(MySystem);` — the system-author surface that turns an ordinary
//! `impl CyclicSystem` into a `dlopen`-loadable `cdylib`'s C-ABI (dl-open.md §3).
//!
//! Each generated `#[unsafe(no_mangle)] pub extern "C" fn fsw_*` is a one-liner that
//! delegates to the matching `metor_fsw_2::abi::run_*` helper, so the real logic
//! lives in `abi.rs` (testable without the macro) and the macro stays thin. The
//! function names are the string forms of the `abi::SYM_*` constants — one source of
//! truth the host resolves by.
//!
//! `MySystem::Params` must be `Serialize + Deserialize + Schema` (postcard): the
//! params blob crosses `fsw_create` as canonical postcard bytes, and `fsw_describe`
//! exports `<Params as postcard_schema::Schema>::SCHEMA` so the host can encode params
//! from KDL without linking the `Params` type (dl-open.md §6.3).
//!
//! [`export_items`] is the shared generator: `export_system!` emits it ungated;
//! `#[system(export)]`/`#[system(export = "feature")]` emit it behind a `cfg` gate.
//! Every item carries `#[allow(clippy::not_unsafe_ptr_arg_deref)]` (the raw-pointer
//! params are the ABI contract), so consuming crates need no crate-level allow.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{Type, parse_macro_input};

pub fn export_system(input: TokenStream) -> TokenStream {
    let ty = parse_macro_input!(input as Type);
    let fsw2 = crate::metor_fsw_2_crate_name();
    export_items(&quote! { #ty }, &fsw2, None).into()
}

/// The `fsw_*` C-ABI items for cyclic system `ty`, each optionally wrapped in the
/// `cfg` `gate` (`#[system]`'s `not(test)` / feature gating; `export_system!` passes
/// `None` — a pure-cdylib crate exports unconditionally).
pub fn export_items(
    ty: &TokenStream2,
    fsw2: &TokenStream2,
    gate: Option<TokenStream2>,
) -> TokenStream2 {
    let gate = &gate;
    quote! {
        /// The ABI word the host checks for equality before any other call.
        #gate
        #[unsafe(no_mangle)]
        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        pub extern "C" fn fsw_abi_version() -> u32 {
            #fsw2::abi::FSW_ABI_VERSION
        }

        /// Serialize this system's descriptor (postcard) to the host sink.
        #gate
        #[unsafe(no_mangle)]
        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        pub extern "C" fn fsw_describe(
            sink: #fsw2::abi::ByteSink,
            ctx: *mut ::core::ffi::c_void,
        ) -> i32 {
            // SAFETY: the host supplies a valid sink/ctx pair (dl-open.md §2.1).
            unsafe { #fsw2::abi::run_describe::<#ty>(sink, ctx) }
        }

        /// Decode the postcard `Params` blob, construct the system, box the state.
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

        /// Reconstruct the typed bundles from the host's ring handles, run `init`.
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
            // SAFETY: `state` is from `fsw_create`; the handles name live regions.
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

        /// Run `System::shutdown` once.
        #gate
        #[unsafe(no_mangle)]
        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        pub extern "C" fn fsw_shutdown(state: *mut ::core::ffi::c_void) {
            // SAFETY: `state` is a live pointer from `fsw_create`.
            unsafe { #fsw2::abi::run_shutdown::<#ty>(state) }
        }

        /// Drop the boxed state inside this `.so`.
        #gate
        #[unsafe(no_mangle)]
        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        pub extern "C" fn fsw_destroy(state: *mut ::core::ffi::c_void) {
            // SAFETY: `state` is from `fsw_create`, transferred here exactly once.
            unsafe { #fsw2::abi::run_destroy::<#ty>(state) }
        }
    }
}
