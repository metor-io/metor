//! Implementation of the `#[system]` attribute.
//!
//! The attribute annotates a type's inherent impl block and derives the whole
//! system surface from the method signatures found there. A system's ports
//! are declared once, as parameters of its `execute` (cyclic) or `run`
//! (async) method, and the macro reads them off that signature to emit:
//!
//! 1. the inherent impl itself, with the recognized methods renamed to hidden
//!    delegation targets and everything else passed through verbatim,
//! 2. two hidden port-bundle structs, one per direction, carrying
//!    [`#[derive(SystemInput)]`](derive@crate::SystemInput) and
//!    [`#[derive(SystemOutput)]`](derive@crate::SystemOutput) so that all
//!    descriptor and binding knowledge stays in those derives,
//! 3. `impl System` plus `impl CyclicSystem` or `impl AsyncSystem`, which
//!    split the output bundle and lend each port back to the user's method in
//!    its original parameter order,
//! 4. `impl BuildSystem`, delegating to `fn new` when the impl has one and to
//!    `Default` otherwise.
//!
//! There is no per-system export: a system reaches a cdylib through its
//! crate's pack (`Pack::system_type` + `export_pack!`), so the only accepted
//! argument is `name = "…"`.
//!
//! Every validation failure is a `syn::Error` on the narrowest offending
//! token, and independent failures are combined so a broken signature reports
//! all of its problems in one compile. On error the original impl block is
//! re-emitted untouched, which keeps the type and its methods resolving and
//! avoids cascading diagnostics.

use convert_case::{Case, Casing};
use proc_macro2::{Span, TokenStream};
use quote::{ToTokens, format_ident, quote, quote_spanned};
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{
    Expr, ExprLit, FnArg, Ident, ImplItem, ImplItemFn, ItemImpl, Lit, Meta, Pat, ReturnType, Token,
    Type,
};

use crate::sig;

// ---------------------------------------------------------------------------
// Attribute arguments
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Args {
    name: Option<String>,
}

fn parse_args(attr: TokenStream) -> Result<Args, syn::Error> {
    let mut args = Args::default();
    if attr.is_empty() {
        return Ok(args);
    }
    let metas = Punctuated::<Meta, Token![,]>::parse_terminated.parse2(attr)?;
    for meta in metas {
        match &meta {
            Meta::NameValue(nv) if nv.path.is_ident("name") => {
                if let Expr::Lit(ExprLit {
                    lit: Lit::Str(s), ..
                }) = &nv.value
                {
                    args.name = Some(s.value());
                } else {
                    return Err(syn::Error::new_spanned(
                        &nv.value,
                        "`name` must be a string literal",
                    ));
                }
            }
            other => {
                let name = other.path().to_token_stream().to_string();
                return Err(syn::Error::new_spanned(
                    other,
                    format!(
                        "unknown #[system] argument `{name}`; expected `name = \"…\"` \
                         (a system reaches a cdylib through its crate's pack — \
                         `Pack::system_type` + `export_pack!` — not a per-system export)"
                    ),
                ));
            }
        }
    }
    Ok(args)
}

/// Derive the default system name from the type ident by stripping a trailing
/// `System` and snake_casing the rest, so `NavSystem` becomes `"nav"` and
/// `GpsDriver` becomes `"gps_driver"`.
fn default_name(ty_ident: &Ident) -> String {
    let s = ty_ident.to_string();
    let base = match s.strip_suffix("System") {
        Some(b) if !b.is_empty() => b,
        _ => s.as_str(),
    };
    base.to_case(Case::Snake)
}

// ---------------------------------------------------------------------------
// Signature classification
// ---------------------------------------------------------------------------

/// Identifies which recognized port type a parameter named, and with it
/// whether the field lands in the input or the output bundle.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PortKind {
    Input,
    MsgIn,
    Output,
    MsgOut,
    CommandOut,
}

impl PortKind {
    fn from_head(head: &str) -> Option<Self> {
        Some(match head {
            "Input" => PortKind::Input,
            "MsgIn" => PortKind::MsgIn,
            "Output" => PortKind::Output,
            "MsgOut" => PortKind::MsgOut,
            "CommandOut" => PortKind::CommandOut,
            _ => None?,
        })
    }

    fn is_input(self) -> bool {
        matches!(self, PortKind::Input | PortKind::MsgIn)
    }

    fn ident(self) -> Ident {
        let s = match self {
            PortKind::Input => "Input",
            PortKind::MsgIn => "MsgIn",
            PortKind::Output => "Output",
            PortKind::MsgOut => "MsgOut",
            PortKind::CommandOut => "CommandOut",
        };
        Ident::new(s, Span::call_site())
    }
}

/// What one parameter of a recognized method turned out to be. The element
/// type is boxed because a `syn::Type` dwarfs the dataless variants.
enum ParamKind {
    /// `now: Timestamp`, the cycle timestamp.
    Now,
    /// `context: &AsyncContext`, the async cancellation context.
    Context,
    /// `x: &mut <Port><T>`, a wired port.
    Port { kind: PortKind, elem: Box<Type> },
    /// `health: &mut HealthPort`, the opt-in health handle.
    Health,
}

/// One accepted parameter, pairing its name with its [`ParamKind`] so the
/// generated impls can rebuild the user's argument list in order.
struct ExecParam {
    ident: Ident,
    kind: ParamKind,
}

/// The catch-all error for a parameter that is neither a port, `now`, nor the
/// health handle.
fn unrecognized(ty: &Type) -> syn::Error {
    let found = ty.to_token_stream().to_string().replace(' ', "");
    syn::Error::new_spanned(
        ty,
        format!(
            "expected a port (`&mut Input<T>`, `&mut Output<T>`, `&mut MsgIn<T>`, \
             `&mut MsgOut<T>`), `&mut HealthPort`, `&AsyncContext`, or `now: Timestamp`; \
             found `{found}` \
             — non-port state belongs in fields of the system struct"
        ),
    )
}

/// Classify every parameter of `f`, pushing an error for each independent
/// problem and returning the accepted parameters in signature order.
fn classify_params(
    f: &mut ImplItemFn,
    method: &str,
    errors: &mut Vec<syn::Error>,
) -> Vec<ExecParam> {
    let mut out = Vec::new();
    let mut saw_health = false;

    let recv_msg = format!("`{method}` takes `&mut self` (the system state lives in the struct)");
    match f.sig.inputs.first() {
        Some(FnArg::Receiver(r)) if r.reference.is_some() && r.mutability.is_some() => {}
        Some(FnArg::Receiver(r)) => errors.push(syn::Error::new_spanned(r, recv_msg)),
        _ => errors.push(syn::Error::new(f.sig.ident.span(), recv_msg)),
    }

    for arg in f.sig.inputs.iter_mut() {
        let FnArg::Typed(pt) = arg else { continue };
        let ident = match &*pt.pat {
            Pat::Ident(p) => p.ident.clone(),
            other => {
                errors.push(syn::Error::new_spanned(
                    other,
                    "parameters need a plain name",
                ));
                continue;
            }
        };
        match &mut *pt.ty {
            // By value only `Timestamp` is accepted. A by-value port gets a
            // dedicated error since ports are runner-owned and lent per cycle.
            Type::Path(_) => {
                let head = sig::type_head(&pt.ty)
                    .map(|i| i.to_string())
                    .unwrap_or_default();
                if head == "Timestamp" {
                    out.push(ExecParam {
                        ident,
                        kind: ParamKind::Now,
                    });
                } else if PortKind::from_head(&head).is_some() || head == "HealthPort" {
                    errors.push(syn::Error::new_spanned(
                        &pt.ty,
                        format!(
                            "system ports are owned by the runner and lent per cycle: write \
                             `{ident}: &mut {head}<…>` (only task ports are moved by value)"
                        ),
                    ));
                } else {
                    errors.push(unrecognized(&pt.ty));
                }
            }
            Type::Reference(r) => {
                let head = sig::type_head(&r.elem)
                    .map(|i| i.to_string())
                    .unwrap_or_default();
                if head == "AsyncContext" {
                    if r.mutability.is_some() {
                        errors.push(syn::Error::new_spanned(
                            &pt.ty,
                            "the async context is read-only: write `&AsyncContext`",
                        ));
                        continue;
                    }
                    if !sig::type_args(&r.elem).is_empty() {
                        errors.push(syn::Error::new_spanned(
                            &r.elem,
                            "`AsyncContext` takes no type parameters",
                        ));
                        continue;
                    }
                    out.push(ExecParam {
                        ident,
                        kind: ParamKind::Context,
                    });
                } else if head == "HealthPort" {
                    if r.mutability.is_none() {
                        errors.push(syn::Error::new_spanned(
                            &pt.ty,
                            "the health handle is written through: write `&mut HealthPort`",
                        ));
                        continue;
                    }
                    if saw_health {
                        errors.push(syn::Error::new_spanned(
                            &pt.ty,
                            "at most one `&mut HealthPort` parameter",
                        ));
                        continue;
                    }
                    saw_health = true;
                    if let Some(extra) = sig::type_args(&r.elem).first() {
                        errors.push(syn::Error::new_spanned(
                            extra,
                            "#[system] supplies the wake endpoints itself; write a bare `&mut HealthPort`",
                        ));
                        continue;
                    }
                    out.push(ExecParam {
                        ident,
                        kind: ParamKind::Health,
                    });
                } else if let Some(kind) = PortKind::from_head(&head) {
                    if r.mutability.is_none() {
                        errors.push(syn::Error::new_spanned(
                            &pt.ty,
                            format!(
                                "system ports are lent mutably: write `{ident}: &mut {head}<…>`"
                            ),
                        ));
                        continue;
                    }
                    let (elem, extra_span) = {
                        let targs = sig::type_args(&r.elem);
                        if targs.is_empty() {
                            errors.push(syn::Error::new_spanned(
                                &r.elem,
                                format!("`{head}` needs an element type: `{head}<MyFrame>`"),
                            ));
                            continue;
                        }
                        let extra = targs.get(1).map(|t| t.span());
                        (Box::new(targs[0].clone()), extra)
                    };
                    if let Some(span) = extra_span {
                        errors.push(syn::Error::new(
                            span,
                            "#[system] ports take a single element type (the wake endpoints \
                             are macro-supplied); drop the second type parameter",
                        ));
                        continue;
                    }
                    out.push(ExecParam {
                        ident,
                        kind: ParamKind::Port { kind, elem },
                    });
                } else {
                    errors.push(unrecognized(&pt.ty));
                }
            }
            other => errors.push(unrecognized(other)),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Expansion
// ---------------------------------------------------------------------------

/// The body of `#[system]`, over `proc_macro2` streams so the unit tests
/// below can drive it directly.
pub fn system_impl(attr: TokenStream, item: TokenStream) -> TokenStream {
    match expand(attr, item.clone()) {
        Ok(ts) => ts,
        Err(e) => {
            // Re-emit the original impl so the type and its inherent methods
            // keep resolving, then append the diagnostics.
            let err = e.to_compile_error();
            quote! {
                #item
                #err
            }
        }
    }
}

/// Fold a non-empty error list into one combined `syn::Error`.
fn combined(mut errors: Vec<syn::Error>) -> syn::Error {
    let mut first = errors.remove(0);
    for e in errors {
        first.combine(e);
    }
    first
}

fn expand(attr: TokenStream, item: TokenStream) -> Result<TokenStream, syn::Error> {
    let args = parse_args(attr)?;
    let mut imp: ItemImpl = syn::parse2(item)?;
    let mut errors: Vec<syn::Error> = Vec::new();
    let fsw2 = crate::metor_fsw_2_crate_name();

    if let Some((_, path, _)) = &imp.trait_ {
        return Err(syn::Error::new_spanned(
            path,
            "#[system] annotates the type's *inherent* impl block, not a trait impl",
        ));
    }
    if !imp.generics.params.is_empty() {
        errors.push(syn::Error::new_spanned(
            &imp.generics,
            "#[system] does not support generic impls; implement `System`/`CyclicSystem` \
             by hand for this type",
        ));
    }
    let self_ty = (*imp.self_ty).clone();
    let Some(ty_ident) = sig::type_head(&self_ty).cloned() else {
        return Err(syn::Error::new_spanned(
            &self_ty,
            "#[system] needs a plain type name to implement the system traits for",
        ));
    };

    // Locate the recognized methods; everything else passes through verbatim.
    let mut execute_idx = None;
    let mut run_idx = None;
    let mut new_idx = None;
    let mut init_idx = None;
    let mut shutdown_idx = None;
    for (i, it) in imp.items.iter().enumerate() {
        let ImplItem::Fn(f) = it else { continue };
        match f.sig.ident.to_string().as_str() {
            "execute" => {
                if run_idx.is_some() {
                    errors.push(syn::Error::new(
                        f.sig.ident.span(),
                        "a system is cyclic or async, not both: remove `execute` or `run`",
                    ));
                }
                execute_idx = Some(i);
            }
            "run" => {
                if execute_idx.is_some() {
                    errors.push(syn::Error::new(
                        f.sig.ident.span(),
                        "a system is cyclic or async, not both: remove `execute` or `run`",
                    ));
                }
                run_idx = Some(i);
            }
            "new" => new_idx = Some(i),
            "init" => init_idx = Some(i),
            "shutdown" => shutdown_idx = Some(i),
            _ => {}
        }
    }

    let is_cyclic = execute_idx.is_some();
    let main_idx = match (execute_idx, run_idx) {
        (Some(i), _) => i,
        (None, Some(i)) => i,
        (None, None) => {
            errors.push(syn::Error::new_spanned(
                &self_ty,
                "#[system] needs a `fn execute(&mut self, now: Timestamp, …ports)` (cyclic) \
                 or an `async fn run(&mut self, …ports)` (async) in this impl",
            ));
            return Err(combined(errors));
        }
    };

    // Classify the main method's parameters, then hide it behind a rename.
    let main_params;
    let main_hidden;
    {
        let ImplItem::Fn(f) = &mut imp.items[main_idx] else {
            unreachable!()
        };
        if is_cyclic {
            if let Some(asyncness) = &f.sig.asyncness {
                errors.push(syn::Error::new_spanned(
                    asyncness,
                    "`execute` is called synchronously once per cycle; for a self-driven \
                     loop write `async fn run`",
                ));
            }
        } else if f.sig.asyncness.is_none() {
            errors.push(syn::Error::new(
                f.sig.ident.span(),
                "`run` must be `async` (a cyclic system's per-cycle entry point is `fn execute`)",
            ));
        }
        let method = if is_cyclic { "execute" } else { "run" };
        let params = classify_params(f, method, &mut errors);

        // `now: Timestamp` is required exactly once on execute and rejected
        // on run.
        let mut now_seen = false;
        let mut context_seen = false;
        for (p, arg) in params.iter().zip(non_receiver_args(&f.sig)) {
            match p.kind {
                ParamKind::Now => {
                    if !is_cyclic {
                        errors.push(syn::Error::new_spanned(
                            arg,
                            "async systems have no coordinator `now`; remove this parameter",
                        ));
                    } else if now_seen {
                        errors.push(syn::Error::new_spanned(
                            arg,
                            "at most one `now: Timestamp` parameter",
                        ));
                    }
                    now_seen = true;
                }
                ParamKind::Context => {
                    if is_cyclic {
                        errors.push(syn::Error::new_spanned(
                            arg,
                            "cyclic systems have no async context; remove this parameter",
                        ));
                    } else if context_seen {
                        errors.push(syn::Error::new_spanned(
                            arg,
                            "at most one `context: &AsyncContext` parameter",
                        ));
                    }
                    context_seen = true;
                }
                _ => {}
            }
        }
        if is_cyclic && !now_seen {
            errors.push(syn::Error::new(
                f.sig.ident.span(),
                "cyclic `execute` needs the cycle timestamp: add `now: Timestamp` (systems \
                 stamp outputs with the coordinator's `now`, not wall time)",
            ));
        }

        main_hidden = format_ident!("__fsw_{}", method);
        hide(f, &main_hidden);
        main_params = params;
    }

    // Classify optional init/shutdown and validate them against the main
    // method's outputs.
    let mut lifecycle = [(init_idx, "init", None), (shutdown_idx, "shutdown", None)];
    for (idx, method, params_slot) in lifecycle.iter_mut() {
        let Some(i) = *idx else { continue };
        let ImplItem::Fn(f) = &mut imp.items[i] else {
            unreachable!()
        };
        if let Some(asyncness) = &f.sig.asyncness {
            errors.push(syn::Error::new_spanned(
                asyncness,
                format!("`{method}` is a synchronous lifecycle hook"),
            ));
        }
        let params = classify_params(f, method, &mut errors);
        validate_lifecycle(&params, method, &main_params, &mut errors);
        let hidden = format_ident!("__fsw_{}", *method);
        hide(f, &hidden);
        *params_slot = Some(params);
    }
    let init_params = lifecycle[0].2.take();
    let shutdown_params = lifecycle[1].2.take();

    // Optional `fn new`, kept verbatim; `BuildSystem` delegates to it.
    // `None` means no `new` at all, `Some(None)` a paramless `fn new()`, and
    // `Some(Some(p))` one taking `p`.
    let mut new_params: Option<Option<Type>> = None;
    if let Some(i) = new_idx {
        let ImplItem::Fn(f) = &imp.items[i] else {
            unreachable!()
        };
        match validate_new(f, &ty_ident) {
            Ok(p) => new_params = Some(p),
            Err(e) => errors.push(e),
        }
    }

    if !errors.is_empty() {
        return Err(combined(errors));
    }

    // ------------------------------------------------------------------
    // Emission
    // ------------------------------------------------------------------
    let name = args.name.unwrap_or_else(|| default_name(&ty_ident));
    let in_ident = format_ident!("__{}In", ty_ident);
    let out_ident = format_ident!("__{}Out", ty_ident);

    // Hidden bundles. Field name is the parameter ident and field order the
    // signature order within each direction. A direction with no ports is a
    // genuinely empty struct, which the derives accept.
    let bundle = |want_input: bool| -> TokenStream {
        let fields: Vec<TokenStream> = main_params
            .iter()
            .filter_map(|p| match &p.kind {
                ParamKind::Port { kind, elem } if kind.is_input() == want_input => {
                    let id = &p.ident;
                    let port = kind.ident();
                    Some(quote! { pub #id: #fsw2::#port<#elem>, })
                }
                _ => None,
            })
            .collect();
        quote! { #(#fields)* }
    };
    let in_fields = bundle(true);
    let out_fields = bundle(false);

    let bundles = quote! {
        #[derive(#fsw2::SystemInput)]
        #[doc(hidden)]
        pub struct #in_ident {
            #in_fields
        }

        #[derive(#fsw2::SystemOutput)]
        #[doc(hidden)]
        pub struct #out_ident {
            #out_fields
        }
    };

    // Delegation arguments, in the user's original signature order.
    let delegate_args = |params: &[ExecParam]| -> Vec<TokenStream> {
        params
            .iter()
            .map(|p| {
                let id = &p.ident;
                match &p.kind {
                    ParamKind::Now => quote! { __now },
                    ParamKind::Context => quote! { __context },
                    ParamKind::Health => quote! { __health },
                    ParamKind::Port { kind, .. } if kind.is_input() => {
                        quote! { &mut __input.#id }
                    }
                    ParamKind::Port { .. } => quote! { &mut __ports.#id },
                }
            })
            .collect()
    };

    let lifecycle_fn = |trait_fn: &str, params: &Option<Vec<ExecParam>>| -> TokenStream {
        let Some(params) = params else {
            return quote!();
        };
        let trait_ident = Ident::new(trait_fn, Span::call_site());
        let hidden = format_ident!("__fsw_{}", trait_fn);
        let args = delegate_args(params);
        quote! {
            fn #trait_ident(&mut self, __output: &mut Self::Output) {
                let (__ports, __health) = #fsw2::Out::split(__output);
                let _ = (&__ports, &__health);
                self.#hidden(#(#args),*)
            }
        }
    };
    let init_fn = lifecycle_fn("init", &init_params);
    let shutdown_fn = lifecycle_fn("shutdown", &shutdown_params);

    let system_impl = quote! {
        impl #fsw2::System for #self_ty {
            type Input = #in_ident;
            type Output = #fsw2::Out<#out_ident>;
            const NAME: &'static str = #name;
            #init_fn
            #shutdown_fn
        }
    };

    let main_args = delegate_args(&main_params);
    let leaf_impl = if is_cyclic {
        quote! {
            impl #fsw2::CyclicSystem for #self_ty {
                fn execute(
                    &mut self,
                    __now: #fsw2::Timestamp,
                    __input: &mut Self::Input,
                    __output: &mut Self::Output,
                ) {
                    // Splitting the output yields disjoint borrows of the user
                    // ports and the health pair, so both can be lent to the
                    // user's method at once.
                    let (__ports, __health) = #fsw2::Out::split(__output);
                    let _ = (&__input, &__ports, &__health);
                    self.#main_hidden(#(#main_args),*)
                }
            }
        }
    } else {
        quote! {
            impl #fsw2::AsyncSystem for #self_ty {
                async fn run(
                    &mut self,
                    __context: &#fsw2::AsyncContext,
                    __input: &mut Self::Input,
                    __output: &mut Self::Output,
                ) {
                    let (__ports, __health) = #fsw2::Out::split(__output);
                    let _ = (&__context, &__input, &__ports, &__health);
                    self.#main_hidden(#(#main_args),*).await
                }
            }
        }
    };

    let build_impl = match new_params {
        // With a concrete params type in hand, the impl also answers whether
        // that type carries defaults, via autoref specialization: the probe
        // method resolves to the inherent `P: Default + Serialize` impl when
        // the bounds hold and to the blanket `None` fallback when they don't.
        // The probing happens here, at the expansion site, because a generic
        // context would resolve against unsubstituted parameters and always
        // fall back — which is why hand-written `BuildSystem` impls declare
        // defaults through `Pack::system_type_with_defaults` instead.
        Some(Some(p)) => quote! {
            impl #fsw2::BuildSystem for #self_ty {
                type Params = #p;
                fn new(params: #p) -> Self {
                    <#self_ty>::new(params)
                }
                fn params_default_blob() -> ::core::option::Option<::std::vec::Vec<u8>> {
                    use #fsw2::NoParamsDefault as _;
                    (&#fsw2::ParamsDefaultProbe::<#p>(::core::marker::PhantomData))
                        .probe_params_default_blob()
                }
            }
        },
        Some(None) => quote! {
            impl #fsw2::BuildSystem for #self_ty {
                type Params = ();
                fn new(_params: ()) -> Self {
                    <#self_ty>::new()
                }
            }
        },
        None => {
            // Without `fn new`, construction goes through `Default`. The
            // spanned call surfaces a missing impl as an error on the
            // annotated type name.
            let default_call = quote_spanned! {ty_ident.span()=>
                <#self_ty as ::core::default::Default>::default()
            };
            quote! {
                impl #fsw2::BuildSystem for #self_ty {
                    type Params = ();
                    /// `#[system]`: no `fn new` found, so construction requires `Default`.
                    fn new(_params: ()) -> Self {
                        #default_call
                    }
                }
            }
        }
    };

    Ok(quote! {
        #imp
        #bundles
        #system_impl
        #leaf_impl
        #build_impl
    })
}

/// The non-receiver arguments of `sig`, aligned with `classify_params`'s
/// output order.
fn non_receiver_args(sig: &syn::Signature) -> impl Iterator<Item = &FnArg> {
    sig.inputs.iter().filter(|a| matches!(a, FnArg::Typed(_)))
}

/// Turn a recognized method into its hidden twin by renaming it and marking
/// it `#[doc(hidden)]`. The body tokens are untouched, so spans survive.
fn hide(f: &mut ImplItemFn, hidden: &Ident) {
    f.sig.ident = hidden.clone();
    f.attrs.push(syn::parse_quote!(#[doc(hidden)]));
    f.attrs
        .push(syn::parse_quote!(#[allow(clippy::too_many_arguments)]));
}

/// Check that `init`/`shutdown` only take output ports the main method also
/// takes (matched by ident, with the same port type) plus `&mut HealthPort`.
fn validate_lifecycle(
    params: &[ExecParam],
    method: &str,
    exec_params: &[ExecParam],
    errors: &mut Vec<syn::Error>,
) {
    for p in params {
        match &p.kind {
            ParamKind::Health => {}
            ParamKind::Context => errors.push(syn::Error::new(
                p.ident.span(),
                format!(
                    "`{method}` may only take execute's output ports (by name) and \
                     `&mut HealthPort`; it has no async context"
                ),
            )),
            ParamKind::Now => errors.push(syn::Error::new(
                p.ident.span(),
                format!(
                    "`{method}` may only take execute's output ports (by name) and \
                     `&mut HealthPort`; it has no cycle timestamp"
                ),
            )),
            ParamKind::Port { kind, elem } => {
                let ident = &p.ident;
                let exec = exec_params.iter().find(|e| e.ident == *ident);
                match exec {
                    None => errors.push(syn::Error::new(
                        ident.span(),
                        format!(
                            "`{method}` may only take execute's output ports (by name) and \
                             `&mut HealthPort`; `{ident}` is not an execute port"
                        ),
                    )),
                    Some(e) => match &e.kind {
                        ParamKind::Port { kind: ek, elem: ee } => {
                            if ek.is_input() {
                                errors.push(syn::Error::new(
                                    ident.span(),
                                    format!(
                                        "`{method}` may only take execute's output ports (by \
                                         name) and `&mut HealthPort`; `{ident}` is an input"
                                    ),
                                ));
                            } else if ek != kind
                                || ee.to_token_stream().to_string()
                                    != elem.to_token_stream().to_string()
                            {
                                let exec_ty = format!(
                                    "{}<{}>",
                                    ek.ident(),
                                    ee.to_token_stream().to_string().replace(' ', "")
                                );
                                let here_ty = format!(
                                    "{}<{}>",
                                    kind.ident(),
                                    elem.to_token_stream().to_string().replace(' ', "")
                                );
                                errors.push(syn::Error::new(
                                    ident.span(),
                                    format!(
                                        "`{ident}` has type `{exec_ty}` in `execute` but \
                                         `{here_ty}` here"
                                    ),
                                ));
                            }
                        }
                        _ => errors.push(syn::Error::new(
                            ident.span(),
                            format!("`{ident}` does not name an execute output port"),
                        )),
                    },
                }
            }
        }
    }
}

/// Check the shape of `fn new`, which must be `fn new(params: P) -> Self` or
/// `fn new() -> Self` with no receiver. Returns the params type, `None` when
/// paramless.
fn validate_new(f: &ImplItemFn, ty_ident: &Ident) -> Result<Option<Type>, syn::Error> {
    const MSG: &str = "`new` must be `fn new(params: P) -> Self` or `fn new() -> Self`";
    let mut params = Vec::new();
    for arg in &f.sig.inputs {
        match arg {
            FnArg::Receiver(r) => return Err(syn::Error::new_spanned(r, MSG)),
            FnArg::Typed(pt) => params.push((*pt.ty).clone()),
        }
    }
    if params.len() > 1 {
        return Err(syn::Error::new_spanned(&f.sig.inputs, MSG));
    }
    let ret_ok = match &f.sig.output {
        ReturnType::Type(_, ty) => sig::type_head(ty).is_some_and(|i| i == "Self" || i == ty_ident),
        ReturnType::Default => false,
    };
    if !ret_ok {
        return Err(syn::Error::new_spanned(&f.sig.output, MSG));
    }
    Ok(params.pop())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Expand and pretty-print (panicking on error) for substring assertions.
    fn expand_ok(attr: TokenStream, item: TokenStream) -> String {
        let out = expand(attr, item).expect("expansion succeeds");
        let file: syn::File = syn::parse2(out).expect("expansion parses");
        prettyplease::unparse(&file)
    }

    /// Expand, expecting failure; returns every diagnostic message.
    fn expand_err(attr: TokenStream, item: TokenStream) -> Vec<String> {
        let err = expand(attr, item).expect_err("expansion fails");
        err.into_iter().map(|e| e.to_string()).collect()
    }

    fn minimal() -> TokenStream {
        quote! {
            impl EchoSystem {
                fn execute(&mut self, now: Timestamp, ping: &mut Input<Ping>, pong: &mut Output<Pong>) {
                    let _ = (now, ping, pong);
                }
            }
        }
    }

    #[test]
    fn minimal_cyclic_expansion() {
        let out = expand_ok(quote!(), minimal());
        // Default name: `System` suffix stripped, then snake_case.
        assert!(
            out.contains(r#"const NAME: &'static str = "echo";"#),
            "{out}"
        );
        // The hidden bundles carry the real derives.
        assert!(out.contains("struct __EchoSystemIn"), "{out}");
        assert!(out.contains("struct __EchoSystemOut"), "{out}");
        assert!(out.contains("pub ping: metor_fsw_2::Input<Ping>"), "{out}");
        assert!(out.contains("pub pong: metor_fsw_2::Output<Pong>"), "{out}");
        // `Out<>` appears only in generated code, delegation goes through
        // `Out::split`, and construction falls back to `Default`.
        assert!(
            out.contains("type Output = metor_fsw_2::Out<__EchoSystemOut>"),
            "{out}"
        );
        assert!(out.contains("Out::split"), "{out}");
        assert!(out.contains("__fsw_execute"), "{out}");
        assert!(
            out.contains("as ::core::default::Default>::default()"),
            "{out}"
        );
        // No export arg, no ABI surface.
        assert!(!out.contains("fsw_abi_version"), "{out}");
    }

    #[test]
    fn name_arg_and_no_export_surface() {
        let out = expand_ok(
            quote!(name = "navigator"),
            quote! {
                impl NavSystem {
                    pub fn new(p: NavParams) -> Self { Self }
                    fn execute(&mut self, now: Timestamp, gps: &mut Input<Gps>) {}
                }
            },
        );
        assert!(
            out.contains(r#"const NAME: &'static str = "navigator";"#),
            "{out}"
        );
        // The per-system C exports are gone with the pack ABI; a crate
        // exports its whole pack through `export_pack!` instead.
        assert!(!out.contains("fsw_abi_version"), "{out}");
        assert!(out.contains("type Params = NavParams;"), "{out}");
        // A port-less direction is a genuinely empty bundle.
        assert!(out.contains("pub struct __NavSystemOut {}"), "{out}");
    }

    #[test]
    fn params_default_probe_emission() {
        // A typed `fn new` gets the defaults probe over its concrete params
        // type, feeding `Pack::system_type`'s automatic entry defaults.
        let out = expand_ok(
            quote!(),
            quote! {
                impl Nav {
                    fn new(p: NavParams) -> Self { Self }
                    fn execute(&mut self, now: Timestamp) {}
                }
            },
        );
        assert!(out.contains("fn params_default_blob"), "{out}");
        assert!(out.contains("ParamsDefaultProbe::<NavParams>"), "{out}");
        assert!(
            out.contains("use metor_fsw_2::NoParamsDefault as _;"),
            "{out}"
        );

        // Unit-params systems (paramless `new`, or no `new` at all) keep the
        // trait's `None` default rather than probing `()`.
        let paramless = expand_ok(
            quote!(),
            quote! {
                impl A {
                    fn new() -> Self { Self }
                    fn execute(&mut self, now: Timestamp) {}
                }
            },
        );
        assert!(!paramless.contains("params_default_blob"), "{paramless}");
        let via_default = expand_ok(quote!(), minimal());
        assert!(
            !via_default.contains("params_default_blob"),
            "{via_default}"
        );
    }

    #[test]
    fn async_run_expansion() {
        let out = expand_ok(
            quote!(),
            quote! {
                impl Radio {
                    async fn run(
                        &mut self,
                        context: &AsyncContext,
                        cmds: &mut MsgIn<GroundCmd>,
                        tm: &mut MsgOut<RadioTm>,
                    ) {}
                }
            },
        );
        assert!(out.contains("AsyncSystem for Radio"), "{out}");
        assert!(out.contains("__fsw_run"), "{out}");
        assert!(out.contains("__fsw_run(__context"), "{out}");
        assert!(
            out.contains(r#"const NAME: &'static str = "radio";"#),
            "{out}"
        );
        assert!(
            out.contains("pub cmds: metor_fsw_2::MsgIn<GroundCmd>"),
            "{out}"
        );
    }

    #[test]
    fn health_param_and_init() {
        let out = expand_ok(
            quote!(),
            quote! {
                impl Watchdog {
                    fn init(&mut self, beat: &mut Output<Heartbeat>, health: &mut HealthPort) {}
                    fn execute(&mut self, now: Timestamp, beat: &mut Output<Heartbeat>, health: &mut HealthPort) {}
                }
            },
        );
        assert!(
            out.contains("fn init(&mut self, __output: &mut Self::Output)"),
            "{out}"
        );
        assert!(out.contains("__fsw_init"), "{out}");
        assert!(out.contains("&mut __ports.beat, __health"), "{out}");
    }

    #[test]
    fn errors_are_combined_and_spanned() {
        // Three independent mistakes in one signature report together.
        let msgs = expand_err(
            quote!(),
            quote! {
                impl Bad {
                    fn execute(&mut self, sensors: Input<Sensors>, count: u32) {}
                }
            },
        );
        assert!(
            msgs.iter()
                .any(|m| m.contains("only task ports are moved by value")),
            "{msgs:?}"
        );
        assert!(
            msgs.iter()
                .any(|m| m.contains("non-port state belongs in fields")),
            "{msgs:?}"
        );
        assert!(
            msgs.iter().any(|m| m.contains("needs the cycle timestamp")),
            "{msgs:?}"
        );
    }

    #[test]
    fn error_table_rows() {
        // no execute/run
        let msgs = expand_err(quote!(), quote! { impl Empty { fn helper(&self) {} } });
        assert!(
            msgs[0].contains("#[system] needs a `fn execute"),
            "{msgs:?}"
        );

        // both
        let msgs = expand_err(
            quote!(),
            quote! { impl Both {
                fn execute(&mut self, now: Timestamp) {}
                async fn run(&mut self) {}
            } },
        );
        assert!(
            msgs.iter().any(|m| m.contains("cyclic or async, not both")),
            "{msgs:?}"
        );

        // async execute
        let msgs = expand_err(
            quote!(),
            quote! { impl A { async fn execute(&mut self, now: Timestamp) {} } },
        );
        assert!(
            msgs.iter().any(|m| m.contains("write `async fn run`")),
            "{msgs:?}"
        );

        // sync run
        let msgs = expand_err(quote!(), quote! { impl A { fn run(&mut self) {} } });
        assert!(
            msgs.iter().any(|m| m.contains("`run` must be `async`")),
            "{msgs:?}"
        );

        // now on run
        let msgs = expand_err(
            quote!(),
            quote! { impl A { async fn run(&mut self, now: Timestamp) {} } },
        );
        assert!(
            msgs.iter().any(|m| m.contains("no coordinator `now`")),
            "{msgs:?}"
        );

        // missing &mut self
        let msgs = expand_err(
            quote!(),
            quote! { impl A { fn execute(&self, now: Timestamp) {} } },
        );
        assert!(
            msgs.iter().any(|m| m.contains("takes `&mut self`")),
            "{msgs:?}"
        );

        // extra type parameter (the wake endpoints are macro-supplied)
        let msgs = expand_err(
            quote!(),
            quote! { impl A { fn execute(&mut self, now: Timestamp, x: &mut Input<T, NoWake>) {} } },
        );
        assert!(
            msgs.iter()
                .any(|m| m.contains("drop the second type parameter")),
            "{msgs:?}"
        );

        // missing element type
        let msgs = expand_err(
            quote!(),
            quote! { impl A { fn execute(&mut self, now: Timestamp, x: &mut Input) {} } },
        );
        assert!(
            msgs.iter().any(|m| m.contains("needs an element type")),
            "{msgs:?}"
        );

        // two HealthPorts
        let msgs = expand_err(
            quote!(),
            quote! { impl A { fn execute(&mut self, now: Timestamp, h1: &mut HealthPort, h2: &mut HealthPort) {} } },
        );
        assert!(
            msgs.iter()
                .any(|m| m.contains("at most one `&mut HealthPort`")),
            "{msgs:?}"
        );

        // bad new
        let msgs = expand_err(
            quote!(),
            quote! { impl A {
                fn new(a: u32, b: u32) -> Self { Self }
                fn execute(&mut self, now: Timestamp) {}
            } },
        );
        assert!(msgs.iter().any(|m| m.contains("`new` must be")), "{msgs:?}");

        // init naming an input
        let msgs = expand_err(
            quote!(),
            quote! { impl A {
                fn init(&mut self, imu: &mut Input<Imu>) {}
                fn execute(&mut self, now: Timestamp, imu: &mut Input<Imu>) {}
            } },
        );
        assert!(
            msgs.iter().any(|m| m.contains("`imu` is an input")),
            "{msgs:?}"
        );

        // init type mismatch
        let msgs = expand_err(
            quote!(),
            quote! { impl A {
                fn init(&mut self, est: &mut Output<B>) {}
                fn execute(&mut self, now: Timestamp, est: &mut Output<A>) {}
            } },
        );
        assert!(
            msgs.iter()
                .any(|m| m.contains("has type `Output<A>` in `execute` but `Output<B>` here")),
            "{msgs:?}"
        );

        // generic impl
        let msgs = expand_err(
            quote!(),
            quote! { impl<T> A<T> { fn execute(&mut self, now: Timestamp) {} } },
        );
        assert!(
            msgs.iter()
                .any(|m| m.contains("does not support generic impls")),
            "{msgs:?}"
        );

        // the retired `export` arg points at the pack surface
        let msgs = expand_err(
            quote!(export),
            quote! { impl A { async fn run(&mut self) {} } },
        );
        assert!(
            msgs.iter().any(|m| m.contains("export_pack!")),
            "{msgs:?}"
        );

        // unknown arg
        let msgs = expand_err(quote!(frobnicate), minimal());
        assert!(
            msgs[0].contains("unknown #[system] argument `frobnicate`"),
            "{msgs:?}"
        );
    }
}
