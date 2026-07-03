//! `#[sequence]` — the ergonomic author surface (sequences-slots.md §4) that turns an
//! `async fn` into a complete, dl-loadable occupant: a future-driven state machine
//! **plus** the `fsw_*` C-ABI exports, driven by the host exactly like any cyclic
//! `.so` (it delegates to the `abi::run_seq_*` helpers, the sequence twins of the
//! `run_*` helpers `export_system!` uses).
//!
//! The port set is read straight off the signature: every `Input<T>` parameter is
//! an input port and every `Output<T>` an output port (in signature order), the
//! macro appends the implicit `SlotControlIn` input and the `SequenceStatus` +
//! health/log output tail, and emits a `descriptor()` + a `build()` that binds those
//! ports in lockstep. The user ports **move into the future**; the tail stays in the
//! wrapper state for per-cycle telemetry (§4.2).
//!
//! The ring-backing generic is **injected** (review E7, `docs/design-system-macro.md`
//! §3.5): a plain `async fn seq(mut x: Input<T>, …)` gains a hidden `__B: Backing`
//! parameter and each port parameter type is rewritten to `Input<T, __B>`; a
//! hand-written `<B: Backing>` is still accepted (the macro uses it instead).

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::{FnArg, ItemFn, Lit, Meta, Pat, Token, Type, parse_macro_input};

use crate::sig;

/// One user port the signature declares: its binding ident and its element frame type
/// `T` (extracted from `Input<T>` / `Output<T>`).
struct Port {
    ident: syn::Ident,
    elem: Type,
}

/// How a fn parameter is classified by the last path segment of its type.
enum Param {
    /// `Input<T>` — a user input port.
    Input(Port),
    /// `Output<T>` — a user output port.
    Output(Port),
    /// A `Seq` — the opt-in explicit handle.
    Seq(syn::Ident),
    /// Anything else — the (at most one) `Params` parameter (only its type is used;
    /// the build arg is always named `params`).
    Params { ty: Type },
}

/// The binding ident of a `pat: ty` parameter (ignoring any `mut`).
fn pat_ident(pat: &Pat) -> Option<syn::Ident> {
    match pat {
        Pat::Ident(p) => Some(p.ident.clone()),
        _ => None,
    }
}

/// Parse the optional `name = "…"` from the attribute meta list, defaulting to the
/// fn ident's string.
fn parse_name(attr: TokenStream, default: &syn::Ident) -> Result<String, syn::Error> {
    if attr.is_empty() {
        return Ok(default.to_string());
    }
    let metas = Punctuated::<Meta, Token![,]>::parse_terminated.parse(attr)?;
    for meta in metas {
        if let Meta::NameValue(nv) = meta
            && nv.path.is_ident("name")
        {
            if let syn::Expr::Lit(syn::ExprLit { lit: Lit::Str(s), .. }) = nv.value {
                return Ok(s.value());
            }
            return Err(syn::Error::new_spanned(nv.value, "`name` must be a string literal"));
        }
    }
    Ok(default.to_string())
}

pub fn sequence(attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut func = parse_macro_input!(item as ItemFn);

    if func.sig.asyncness.is_none() {
        return syn::Error::new_spanned(func.sig.fn_token, "`#[sequence]` requires an `async fn`")
            .to_compile_error()
            .into();
    }

    let fn_ident = func.sig.ident.clone();
    let name = match parse_name(attr, &fn_ident) {
        Ok(n) => n,
        Err(e) => return e.to_compile_error().into(),
    };

    let fsw2 = crate::metor_fsw_2_crate_name();

    // The ring-backing generic: a hand-written `<B: Backing>` is used as-is;
    // otherwise `__B: Backing` is injected and every port parameter type below is
    // rewritten to carry it (design-system-macro.md §3.5).
    let (backing, injected) = match sig::backing_param(&func.sig.generics) {
        Some(b) => (b, false),
        None => (sig::injected_backing_ident(), true),
    };
    if injected {
        let b = &backing;
        func.sig
            .generics
            .params
            .push(syn::parse_quote!(#b: #fsw2::ring::Backing));
    }
    let backing_ty = sig::ident_type(&backing);

    // Classify every parameter by the last segment of its type, rewriting port
    // parameter types in place when the backing is injected.
    let mut params: Vec<Param> = Vec::new();
    let mut params_seen = false;
    for arg in &mut func.sig.inputs {
        let FnArg::Typed(pt) = arg else {
            return syn::Error::new_spanned(arg, "`#[sequence]` does not take a `self` receiver")
                .to_compile_error()
                .into();
        };
        let Some(ident) = pat_ident(&pt.pat) else {
            return syn::Error::new_spanned(&pt.pat, "sequence parameters need a plain name")
                .to_compile_error()
                .into();
        };
        let head = sig::type_head(&pt.ty).map(|i| i.to_string());
        match head.as_deref() {
            Some(h @ ("Input" | "Output")) => {
                let Some(elem) = sig::first_type_arg(&pt.ty) else {
                    let msg = format!("`{h}` needs an element type: `{h}<MyFrame>`");
                    return syn::Error::new_spanned(&pt.ty, msg).to_compile_error().into();
                };
                if injected {
                    let args = sig::type_args(&pt.ty);
                    if args.len() > 1 {
                        return syn::Error::new_spanned(
                            args[1],
                            "#[sequence] supplies the ring backing itself; \
                             drop the second type parameter (or declare a `<B: Backing>` generic)",
                        )
                        .to_compile_error()
                        .into();
                    }
                    sig::append_type_args(&mut pt.ty, std::slice::from_ref(&backing_ty));
                }
                let port = Port { ident, elem };
                if h == "Input" {
                    params.push(Param::Input(port));
                } else {
                    params.push(Param::Output(port));
                }
            }
            Some("Seq") => params.push(Param::Seq(ident)),
            _ => {
                if params_seen {
                    return syn::Error::new_spanned(
                        &pt.ty,
                        "a sequence takes at most one params parameter",
                    )
                    .to_compile_error()
                    .into();
                }
                params_seen = true;
                let _ = ident;
                params.push(Param::Params { ty: (*pt.ty).clone() });
            }
        }
    }

    // Collect, in signature order within each kind, the user ports + the optional
    // params type + whether a Seq handle is wanted.
    let inputs: Vec<&Port> = params
        .iter()
        .filter_map(|p| if let Param::Input(p) = p { Some(p) } else { None })
        .collect();
    let outputs: Vec<&Port> = params
        .iter()
        .filter_map(|p| if let Param::Output(p) = p { Some(p) } else { None })
        .collect();
    let params_ty: Option<&Type> = params.iter().find_map(|p| {
        if let Param::Params { ty } = p {
            Some(ty)
        } else {
            None
        }
    });
    let seq_ident: Option<&syn::Ident> = params.iter().find_map(|p| {
        if let Param::Seq(id) = p {
            Some(id)
        } else {
            None
        }
    });

    // ---- descriptor(): inputs = [user inputs…, SlotControlIn];
    //                    outputs = [user outputs…, then the Out<SeqStatusOut> tail].
    let input_descs = inputs.iter().map(|p| {
        let elem = &p.elem;
        quote! { <#fsw2::Input<#elem, #fsw2::ring::BoxBacking>>::descriptor() }
    });
    let output_descs = outputs.iter().map(|p| {
        let elem = &p.elem;
        quote! { <#fsw2::Output<#elem, #fsw2::ring::BoxBacking>>::descriptor() }
    });

    // ---- build(): bind in the same order; user ports MOVE INTO the future.
    let bind_inputs = inputs.iter().map(|p| {
        let id = &p.ident;
        let elem = &p.elem;
        quote! { let #id = <#fsw2::Input<#elem, #fsw2::ring::RawBacking>>::bind(binder); }
    });
    let bind_outputs = outputs.iter().map(|p| {
        let id = &p.ident;
        let elem = &p.elem;
        quote! { let #id = <#fsw2::Output<#elem, #fsw2::ring::RawBacking>>::bind(binder); }
    });
    let bind_seq = seq_ident.map(|id| {
        quote! { let #id = #fsw2::sequence::Seq::new(clock.clone()); }
    });

    // The future args, in ORIGINAL signature order (params? mapped to the build arg).
    let call_args = params.iter().map(|p| match p {
        Param::Input(p) => {
            let id = &p.ident;
            quote! { #id }
        }
        Param::Output(p) => {
            let id = &p.ident;
            quote! { #id }
        }
        Param::Seq(id) => quote! { #id },
        Param::Params { .. } => quote! { params },
    });

    let params_assoc = match params_ty {
        Some(ty) => quote! { type Params = #ty; },
        None => quote! { type Params = (); },
    };
    let build_params_arg = match params_ty {
        Some(ty) => quote! { params: #ty },
        // No params parameter: name it `_params` so the unused trait arg is silent.
        None => quote! { _params: () },
    };

    let wrapper = format_ident!("__Seq_{}", fn_ident);

    let impl_block = quote! {
        #[allow(non_camel_case_types)]
        #[doc(hidden)]
        pub struct #wrapper;

        impl #fsw2::SeqSystem for #wrapper {
            #params_assoc

            fn descriptor() -> #fsw2::SystemDescriptor {
                let mut inputs = ::std::vec![ #(#input_descs,)* ];
                inputs.push(<#fsw2::Input<#fsw2::sequence::SlotControlIn, #fsw2::ring::BoxBacking>>::descriptor());
                let mut outputs = ::std::vec![ #(#output_descs,)* ];
                outputs.extend(
                    <#fsw2::Out<#fsw2::sequence::SeqStatusOut<#fsw2::ring::BoxBacking>, #fsw2::ring::BoxBacking> as #fsw2::SystemOutput>::descriptors(),
                );
                #fsw2::SystemDescriptor {
                    name: #name,
                    kind: #fsw2::SystemKind::Cyclic,
                    inputs,
                    outputs,
                }
            }

            fn build(
                #build_params_arg,
                binder: &mut #fsw2::abi::RawBinder,
                clock: &::std::rc::Rc<#fsw2::sequence::SeqClock>,
            ) -> #fsw2::sequence::SeqBound {
                #(#bind_inputs)*
                let __control = <#fsw2::Input<#fsw2::sequence::SlotControlIn, #fsw2::ring::RawBacking>>::bind(binder);
                #(#bind_outputs)*
                let __status = <#fsw2::Out<#fsw2::sequence::SeqStatusOut<#fsw2::ring::RawBacking>, #fsw2::ring::RawBacking> as #fsw2::BindPorts<#fsw2::ring::RawBacking>>::bind(binder);
                #bind_seq
                let __fut: ::core::pin::Pin<::std::boxed::Box<dyn ::core::future::Future<Output = #fsw2::sequence::Outcome>>> =
                    ::std::boxed::Box::pin(#fn_ident::<#fsw2::ring::RawBacking>( #(#call_args),* ));
                #fsw2::sequence::SeqBound {
                    future: __fut,
                    status: __status,
                    control: __control,
                }
            }
        }
    };

    // ---- The C-ABI exports: same `fsw_*` symbols as export.rs, delegating to the
    // `run_seq_*` helpers. Gated on `not(test)` so an in-crate macro test can coexist
    // with another crate-unique `fsw_*` export (a real `.so` builds without `--test`).
    // Each item carries the clippy allow (raw-pointer params are the ABI contract), so
    // consuming crates need no crate-level allow (design-system-macro.md §3.5).
    let exports = quote! {
        /// The ABI word the host checks for equality before any other call.
        #[cfg(not(test))]
        #[unsafe(no_mangle)]
        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        pub extern "C" fn fsw_abi_version() -> u32 {
            #fsw2::abi::FSW_ABI_VERSION
        }

        /// Serialize this sequence's descriptor (postcard) to the host sink.
        #[cfg(not(test))]
        #[unsafe(no_mangle)]
        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        pub extern "C" fn fsw_describe(
            sink: #fsw2::abi::ByteSink,
            ctx: *mut ::core::ffi::c_void,
        ) -> i32 {
            // SAFETY: the host supplies a valid sink/ctx pair.
            unsafe { #fsw2::abi::run_seq_describe::<#wrapper>(sink, ctx) }
        }

        /// Decode the postcard `Params` blob and box the (unbound) sequence state.
        #[cfg(not(test))]
        #[unsafe(no_mangle)]
        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        pub extern "C" fn fsw_create(
            params: *const u8,
            params_len: usize,
        ) -> *mut ::core::ffi::c_void {
            // SAFETY: `params`/`params_len` name a readable byte range (or null/0).
            unsafe { #fsw2::abi::run_seq_create::<#wrapper>(params, params_len) }
        }

        /// Bind the ports off the host's ring handles and build the future.
        #[cfg(not(test))]
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
            unsafe { #fsw2::abi::run_seq_bind_init::<#wrapper>(state, inputs, n_in, outputs, n_out) }
        }

        /// Poll the future once, returning an `FswStatus` (`Done` when terminal).
        #[cfg(not(test))]
        #[unsafe(no_mangle)]
        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        pub extern "C" fn fsw_execute(
            state: *mut ::core::ffi::c_void,
            now: u64,
        ) -> #fsw2::abi::FswStatus {
            // SAFETY: `state` is a live pointer from `fsw_create`.
            unsafe { #fsw2::abi::run_seq_execute::<#wrapper>(state, now) }
        }

        /// No-op for v1 sequences (kept for symbol uniformity).
        #[cfg(not(test))]
        #[unsafe(no_mangle)]
        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        pub extern "C" fn fsw_shutdown(state: *mut ::core::ffi::c_void) {
            // SAFETY: `state` is a live pointer from `fsw_create`.
            unsafe { #fsw2::abi::run_seq_shutdown::<#wrapper>(state) }
        }

        /// Drop the boxed sequence state inside this `.so`.
        #[cfg(not(test))]
        #[unsafe(no_mangle)]
        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        pub extern "C" fn fsw_destroy(state: *mut ::core::ffi::c_void) {
            // SAFETY: `state` is from `fsw_create`, transferred here exactly once.
            unsafe { #fsw2::abi::run_seq_destroy::<#wrapper>(state) }
        }
    };

    quote! {
        #func
        #impl_block
        #exports
    }
    .into()
}
