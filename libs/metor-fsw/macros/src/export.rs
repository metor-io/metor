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

use proc_macro::TokenStream;
use quote::quote;
use syn::{Type, parse_macro_input};

pub fn export_system(input: TokenStream) -> TokenStream {
    let ty = parse_macro_input!(input as Type);
    let fsw2 = crate::metor_fsw_2_crate_name();

    quote! {
        /// The ABI word the host checks for equality before any other call.
        #[unsafe(no_mangle)]
        pub extern "C" fn fsw_abi_version() -> u32 {
            #fsw2::abi::FSW_ABI_VERSION
        }

        /// Serialize this system's descriptor (postcard) to the host sink.
        #[unsafe(no_mangle)]
        pub extern "C" fn fsw_describe(
            sink: #fsw2::abi::ByteSink,
            ctx: *mut ::core::ffi::c_void,
        ) -> i32 {
            // SAFETY: the host supplies a valid sink/ctx pair (dl-open.md §2.1).
            unsafe { #fsw2::abi::run_describe::<#ty>(sink, ctx) }
        }

        /// Decode the postcard `Params` blob, construct the system, box the state.
        #[unsafe(no_mangle)]
        pub extern "C" fn fsw_create(
            params: *const u8,
            params_len: usize,
        ) -> *mut ::core::ffi::c_void {
            // SAFETY: `params`/`params_len` name a readable byte range (or null/0).
            unsafe { #fsw2::abi::run_create::<#ty>(params, params_len) }
        }

        /// Reconstruct the typed bundles from the host's ring handles, run `init`.
        #[unsafe(no_mangle)]
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
        #[unsafe(no_mangle)]
        pub extern "C" fn fsw_execute(
            state: *mut ::core::ffi::c_void,
            now: u64,
        ) -> #fsw2::abi::FswStatus {
            // SAFETY: `state` is a live pointer from `fsw_create`.
            unsafe { #fsw2::abi::run_execute::<#ty>(state, now) }
        }

        /// Run `System::shutdown` once.
        #[unsafe(no_mangle)]
        pub extern "C" fn fsw_shutdown(state: *mut ::core::ffi::c_void) {
            // SAFETY: `state` is a live pointer from `fsw_create`.
            unsafe { #fsw2::abi::run_shutdown::<#ty>(state) }
        }

        /// Drop the boxed state inside this `.so`.
        #[unsafe(no_mangle)]
        pub extern "C" fn fsw_destroy(state: *mut ::core::ffi::c_void) {
            // SAFETY: `state` is from `fsw_create`, transferred here exactly once.
            unsafe { #fsw2::abi::run_destroy::<#ty>(state) }
        }
    }
    .into()
}
