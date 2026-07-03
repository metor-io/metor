//! `#[system]` — the one-annotation system author surface
//! (`docs/design-system-macro.md`). Annotates a type's **inherent impl block**, reads
//! the ports off the `execute` (cyclic) / `run` (async) method signature, and emits
//! everything a hand-written system spells out today:
//!
//! 1. the inherent impl re-emitted (recognized methods hidden + made `Backing`-generic,
//!    `new` and unrecognized methods verbatim),
//! 2. hidden `__<Ty>In`/`__<Ty>Out` port bundles carrying the *existing*
//!    `#[derive(SystemInput)]`/`#[derive(SystemOutput)]` (the port-unification
//!    insulation point, §9 — this macro owns no descriptor/bind knowledge),
//! 3. `impl System` + `impl CyclicSystem`/`impl AsyncSystem` (delegating through
//!    `Out::split`, so `Out<>` never appears in user code — E8a),
//! 4. `impl BuildSystem` from `fn new` (or `Default` when absent),
//! 5. optionally the `fsw_*` dl C-ABI exports (`export`/`export = "feature"`),
//!    byte-identical to `export_system!`'s but `cfg`-gated.
//!
//! All errors are `syn::Error`s on the narrowest offending token, combined so a wrong
//! signature reports everything at once (§10). On error the original impl is re-emitted
//! untouched so downstream name resolution stays alive.

use convert_case::{Case, Casing};
use proc_macro2::{Span, TokenStream};
use quote::{ToTokens, format_ident, quote, quote_spanned};
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{
    Expr, ExprLit, FnArg, Ident, ImplItem, ImplItemFn, ItemImpl, Lit, Meta, Pat, ReturnType,
    Token, Type,
};

use crate::sig;

// ---------------------------------------------------------------------------
// Attribute arguments (§4)
// ---------------------------------------------------------------------------

/// How the `fsw_*` dl ABI is gated: `export` ⇒ `not(test)` only; `export = "feat"` ⇒
/// additionally on that cargo feature. Absent ⇒ no exports (static-link-only crate).
enum Export {
    Bare(Span),
    Feature(String, Span),
}

impl Export {
    fn span(&self) -> Span {
        match self {
            Export::Bare(s) | Export::Feature(_, s) => *s,
        }
    }
}

#[derive(Default)]
struct Args {
    name: Option<String>,
    export: Option<Export>,
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
                if let Expr::Lit(ExprLit { lit: Lit::Str(s), .. }) = &nv.value {
                    args.name = Some(s.value());
                } else {
                    return Err(syn::Error::new_spanned(
                        &nv.value,
                        "`name` must be a string literal",
                    ));
                }
            }
            Meta::Path(p) if p.is_ident("export") => {
                args.export = Some(Export::Bare(p.span()));
            }
            Meta::NameValue(nv) if nv.path.is_ident("export") => {
                if let Expr::Lit(ExprLit { lit: Lit::Str(s), .. }) = &nv.value {
                    args.export = Some(Export::Feature(s.value(), nv.span()));
                } else {
                    return Err(syn::Error::new_spanned(
                        &nv.value,
                        "`export` takes a cargo feature name: `export = \"export\"` (or bare `export`)",
                    ));
                }
            }
            other => {
                let name = other.path().to_token_stream().to_string();
                return Err(syn::Error::new_spanned(
                    other,
                    format!(
                        "unknown #[system] argument `{name}`; expected `name = \"…\"`, \
                         `export`, `export = \"feature\"`"
                    ),
                ));
            }
        }
    }
    Ok(args)
}

/// The default wiring name: the self-type ident with a trailing `System` stripped,
/// snake_cased (`NavSystem` → `"nav"`, `GpsDriver` → `"gps_driver"`).
fn default_name(ty_ident: &Ident) -> String {
    let s = ty_ident.to_string();
    let base = match s.strip_suffix("System") {
        Some(b) if !b.is_empty() => b,
        _ => s.as_str(),
    };
    base.to_case(Case::Snake)
}

// ---------------------------------------------------------------------------
// Signature classification (§5)
// ---------------------------------------------------------------------------

/// The accepted port type heads and which bundle they land in.
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

/// One classified `execute`/`run`/`init`/`shutdown` parameter, in signature order.
/// The element type is boxed: a `syn::Type` dwarfs the dataless variants.
enum ParamKind {
    /// `now: Timestamp` — the cycle timestamp.
    Now,
    /// `x: &mut <Port><T>` — a wired port.
    Port { kind: PortKind, elem: Box<Type> },
    /// `health: &mut HealthPort` — the opt-in health handle.
    Health,
}

struct ExecParam {
    ident: Ident,
    kind: ParamKind,
}

/// The §10 catch-all for a parameter that is neither a port nor `now`/health.
fn unrecognized(ty: &Type) -> syn::Error {
    let found = ty.to_token_stream().to_string().replace(' ', "");
    syn::Error::new_spanned(
        ty,
        format!(
            "expected a port (`&mut Input<T>`, `&mut Output<T>`, `&mut MsgIn<T>`, \
             `&mut MsgOut<T>`), `&mut HealthPort`, or `now: Timestamp`; found `{found}` \
             — non-port state belongs in fields of the system struct"
        ),
    )
}

/// Classify (and rewrite, in place) the parameters of `f`: every port parameter type
/// gains the injected `__B` backing argument. Pushes every independent error; returns
/// the classification in signature order.
fn classify_params(
    f: &mut ImplItemFn,
    method: &str,
    backing_ty: &Type,
    errors: &mut Vec<syn::Error>,
) -> Vec<ExecParam> {
    let mut out = Vec::new();
    let mut saw_health = false;

    // `&mut self` receiver (the system state lives in the struct).
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
                errors.push(syn::Error::new_spanned(other, "parameters need a plain name"));
                continue;
            }
        };
        match &mut *pt.ty {
            // By-value parameter: only `Timestamp` is accepted; a by-value port gets
            // the dedicated "&mut" error (ports are runner-owned, lent per cycle).
            Type::Path(_) => {
                let head = sig::type_head(&pt.ty)
                    .map(|i| i.to_string())
                    .unwrap_or_default();
                if head == "Timestamp" {
                    out.push(ExecParam { ident, kind: ParamKind::Now });
                } else if PortKind::from_head(&head).is_some() || head == "HealthPort" {
                    errors.push(syn::Error::new_spanned(
                        &pt.ty,
                        format!(
                            "system ports are owned by the runner and lent per cycle: write \
                             `{ident}: &mut {head}<…>` (only #[sequence] ports are moved by value)"
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
                if head == "HealthPort" {
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
                            "#[system] supplies the ring backing itself; write a bare `&mut HealthPort`",
                        ));
                        continue;
                    }
                    sig::append_type_args(&mut r.elem, std::slice::from_ref(backing_ty));
                    out.push(ExecParam { ident, kind: ParamKind::Health });
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
                            "#[system] supplies the ring backing itself; drop the second type parameter",
                        ));
                        continue;
                    }
                    sig::append_type_args(&mut r.elem, std::slice::from_ref(backing_ty));
                    out.push(ExecParam { ident, kind: ParamKind::Port { kind, elem } });
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

/// The proc-macro2 body of `#[system]` (`TokenStream2`-shaped so the in-crate unit
/// tests can drive it directly).
pub fn system_impl(attr: TokenStream, item: TokenStream) -> TokenStream {
    match expand(attr, item.clone()) {
        Ok(ts) => ts,
        Err(e) => {
            // Re-emit the original impl so the type and its inherent methods keep
            // resolving (no cascading unknown-type errors), then the diagnostics.
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

    // ---- locate the recognized methods (everything else passes through verbatim).
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

    let backing = sig::injected_backing_ident();
    let backing_ty = sig::ident_type(&backing);
    let backing_param: syn::GenericParam = syn::parse_quote!(#backing: #fsw2::ring::Backing);

    // ---- the main method: classify + hide (rename, inject __B, rewrite port types).
    let main_params;
    let main_hidden;
    {
        let ImplItem::Fn(f) = &mut imp.items[main_idx] else { unreachable!() };
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
        let params = classify_params(f, method, &backing_ty, &mut errors);

        // `now: Timestamp` — required (exactly once) on execute, rejected on run.
        let mut now_seen = false;
        for (p, arg) in params.iter().zip(non_receiver_args(&f.sig)) {
            if matches!(p.kind, ParamKind::Now) {
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
        }
        if is_cyclic && !now_seen {
            errors.push(syn::Error::new(
                f.sig.ident.span(),
                "cyclic `execute` needs the cycle timestamp: add `now: Timestamp` (systems \
                 stamp outputs with the coordinator's `now`, not wall time)",
            ));
        }

        main_hidden = format_ident!("__fsw_{}", method);
        hide(f, &main_hidden, &backing_param);
        main_params = params;
    }

    // ---- optional init/shutdown: classify + validate against execute's outputs.
    let mut lifecycle = [(init_idx, "init", None), (shutdown_idx, "shutdown", None)];
    for (idx, method, params_slot) in lifecycle.iter_mut() {
        let Some(i) = *idx else { continue };
        let ImplItem::Fn(f) = &mut imp.items[i] else { unreachable!() };
        if let Some(asyncness) = &f.sig.asyncness {
            errors.push(syn::Error::new_spanned(
                asyncness,
                format!("`{method}` is a synchronous lifecycle hook"),
            ));
        }
        let params = classify_params(f, method, &backing_ty, &mut errors);
        validate_lifecycle(&params, method, &main_params, &mut errors);
        let hidden = format_ident!("__fsw_{}", *method);
        hide(f, &hidden, &backing_param);
        *params_slot = Some(params);
    }
    let init_params = lifecycle[0].2.take();
    let shutdown_params = lifecycle[1].2.take();

    // ---- optional `fn new` (kept verbatim; BuildSystem delegates to it).
    // `None` ⇒ no `new` at all; `Some(None)` ⇒ `fn new()`; `Some(Some(P))` ⇒ params P.
    let mut new_params: Option<Option<Type>> = None;
    if let Some(i) = new_idx {
        let ImplItem::Fn(f) = &imp.items[i] else { unreachable!() };
        match validate_new(f, &ty_ident) {
            Ok(p) => new_params = Some(p),
            Err(e) => errors.push(e),
        }
    }

    // ---- `export` is a dl (cyclic) surface; the ABI cannot drive `async fn run`.
    if let Some(export) = &args.export
        && !is_cyclic
    {
        errors.push(syn::Error::new(
            export.span(),
            "`export` needs a cyclic system (`fn execute`): the dl ABI's `fsw_execute` \
             drives one step per call and cannot host an `async fn run` loop",
        ));
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

    // Hidden bundles: field name = param ident, field order = signature order within
    // each direction; a `PhantomData` anchors `__B` when a direction has no ports
    // (the derives skip it and `bind` default-constructs it).
    let bundle = |want_input: bool| -> TokenStream {
        let fields: Vec<TokenStream> = main_params
            .iter()
            .filter_map(|p| match &p.kind {
                ParamKind::Port { kind, elem } if kind.is_input() == want_input => {
                    let id = &p.ident;
                    let port = kind.ident();
                    Some(quote! { pub #id: #fsw2::#port<#elem, #backing>, })
                }
                _ => None,
            })
            .collect();
        if fields.is_empty() {
            quote! { pub __b: ::core::marker::PhantomData<fn() -> #backing>, }
        } else {
            quote! { #(#fields)* }
        }
    };
    let in_fields = bundle(true);
    let out_fields = bundle(false);

    let bundles = quote! {
        #[derive(#fsw2::SystemInput)]
        #[doc(hidden)]
        pub struct #in_ident<#backing: #fsw2::ring::Backing = #fsw2::ring::BoxBacking> {
            #in_fields
        }

        #[derive(#fsw2::SystemOutput)]
        #[doc(hidden)]
        pub struct #out_ident<#backing: #fsw2::ring::Backing = #fsw2::ring::BoxBacking> {
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
                self.#hidden::<#backing>(#(#args),*)
            }
        }
    };
    let init_fn = lifecycle_fn("init", &init_params);
    let shutdown_fn = lifecycle_fn("shutdown", &shutdown_params);

    let system_impl = quote! {
        impl<#backing: #fsw2::ring::Backing> #fsw2::System<#backing> for #self_ty {
            type Input = #in_ident<#backing>;
            type Output = #fsw2::Out<#out_ident<#backing>, #backing>;
            const NAME: &'static str = #name;
            #init_fn
            #shutdown_fn
        }
    };

    let main_args = delegate_args(&main_params);
    let leaf_impl = if is_cyclic {
        quote! {
            impl<#backing: #fsw2::ring::Backing> #fsw2::CyclicSystem<#backing> for #self_ty {
                fn execute(
                    &mut self,
                    __now: #fsw2::Timestamp,
                    __input: &mut Self::Input,
                    __output: &mut Self::Output,
                ) {
                    // `Out::split` (E8a): disjoint borrows of the user ports and the
                    // health pair, so both can be lent to the user fn at once.
                    let (__ports, __health) = #fsw2::Out::split(__output);
                    let _ = (&__input, &__ports, &__health);
                    self.#main_hidden::<#backing>(#(#main_args),*)
                }
            }
        }
    } else {
        quote! {
            impl<#backing: #fsw2::ring::Backing> #fsw2::AsyncSystem<#backing> for #self_ty {
                async fn run(&mut self, __input: &mut Self::Input, __output: &mut Self::Output) {
                    let (__ports, __health) = #fsw2::Out::split(__output);
                    let _ = (&__input, &__ports, &__health);
                    self.#main_hidden::<#backing>(#(#main_args),*).await
                }
            }
        }
    };

    let build_impl = match new_params {
        Some(Some(p)) => quote! {
            impl #fsw2::BuildSystem for #self_ty {
                type Params = #p;
                fn new(params: #p) -> Self {
                    <#self_ty>::new(params)
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
            // No `fn new`: construction requires `Default`. The spanned call surfaces
            // a missing impl as an error on the annotated type name.
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

    let exports = match &args.export {
        None => quote!(),
        Some(Export::Bare(_)) => {
            let gate = quote! { #[cfg(not(test))] };
            crate::export::export_items(&self_ty.to_token_stream(), &fsw2, Some(gate))
        }
        Some(Export::Feature(feat, _)) => {
            let gate = quote! { #[cfg(all(feature = #feat, not(test)))] };
            crate::export::export_items(&self_ty.to_token_stream(), &fsw2, Some(gate))
        }
    };

    Ok(quote! {
        #imp
        #bundles
        #system_impl
        #leaf_impl
        #build_impl
        #exports
    })
}

/// The non-receiver arguments of `sig`, aligned with `classify_params`'s output order.
fn non_receiver_args(sig: &syn::Signature) -> impl Iterator<Item = &FnArg> {
    sig.inputs
        .iter()
        .filter(|a| matches!(a, FnArg::Typed(_)))
}

/// Turn a recognized method into its hidden generic twin: rename, `#[doc(hidden)]`,
/// and the injected `__B: Backing` generic (port parameter types were already
/// rewritten by `classify_params`). The body tokens are untouched (spans preserved).
fn hide(f: &mut ImplItemFn, hidden: &Ident, backing_param: &syn::GenericParam) {
    f.sig.ident = hidden.clone();
    f.sig.generics.params.push(backing_param.clone());
    f.attrs.push(syn::parse_quote!(#[doc(hidden)]));
    f.attrs.push(syn::parse_quote!(#[allow(clippy::too_many_arguments)]));
}

/// `init`/`shutdown` may only take execute's *output* ports (matched by ident, same
/// port type) and `&mut HealthPort` (§5, §10).
fn validate_lifecycle(
    params: &[ExecParam],
    method: &str,
    exec_params: &[ExecParam],
    errors: &mut Vec<syn::Error>,
) {
    for p in params {
        match &p.kind {
            ParamKind::Health => {}
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

/// `fn new` drives `BuildSystem`: `fn new(params: P) -> Self` or `fn new() -> Self`,
/// no receiver. Returns the params type (`None` for paramless).
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
        ReturnType::Type(_, ty) => {
            sig::type_head(ty).is_some_and(|i| i == "Self" || i == ty_ident)
        }
        ReturnType::Default => false,
    };
    if !ret_ok {
        return Err(syn::Error::new_spanned(&f.sig.output, MSG));
    }
    Ok(params.pop())
}

// ---------------------------------------------------------------------------
// Unit tests: expansion shape + the §10 error table (in-crate, over TokenStream2)
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
        // Default name: `System` suffix stripped + snake_case.
        assert!(out.contains(r#"const NAME: &'static str = "echo";"#), "{out}");
        // Hidden bundles carry the real derives (the insulation point).
        assert!(out.contains("struct __EchoSystemIn"), "{out}");
        assert!(out.contains("struct __EchoSystemOut"), "{out}");
        assert!(out.contains("pub ping: metor_fsw_2::Input<Ping, __B>"), "{out}");
        assert!(out.contains("pub pong: metor_fsw_2::Output<Pong, __B>"), "{out}");
        // Out<> only in generated code; split-based delegation; Default-built params.
        assert!(out.contains("type Output = metor_fsw_2::Out<__EchoSystemOut<__B>, __B>"), "{out}");
        assert!(out.contains("Out::split"), "{out}");
        assert!(out.contains("__fsw_execute"), "{out}");
        assert!(out.contains("as ::core::default::Default>::default()"), "{out}");
        // No export arg: no ABI surface.
        assert!(!out.contains("fsw_abi_version"), "{out}");
    }

    #[test]
    fn name_and_export_args() {
        let out = expand_ok(
            quote!(name = "navigator", export = "export"),
            quote! {
                impl NavSystem {
                    pub fn new(p: NavParams) -> Self { Self }
                    fn execute(&mut self, now: Timestamp, gps: &mut Input<Gps>) {}
                }
            },
        );
        assert!(out.contains(r#"const NAME: &'static str = "navigator";"#), "{out}");
        assert!(out.contains(r#"#[cfg(all(feature = "export", not(test)))]"#), "{out}");
        assert!(out.contains("fsw_abi_version"), "{out}");
        assert!(out.contains("type Params = NavParams;"), "{out}");
        // Empty output direction is anchored by a skipped PhantomData field.
        assert!(out.contains("PhantomData<fn() -> __B>"), "{out}");
    }

    #[test]
    fn async_run_expansion() {
        let out = expand_ok(
            quote!(),
            quote! {
                impl Radio {
                    async fn run(&mut self, cmds: &mut MsgIn<GroundCmd>, tm: &mut MsgOut<RadioTm>) {}
                }
            },
        );
        assert!(out.contains("AsyncSystem<__B> for Radio"), "{out}");
        assert!(out.contains("__fsw_run"), "{out}");
        assert!(out.contains(r#"const NAME: &'static str = "radio";"#), "{out}");
        assert!(out.contains("pub cmds: metor_fsw_2::MsgIn<GroundCmd, __B>"), "{out}");
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
        assert!(out.contains("fn init(&mut self, __output: &mut Self::Output)"), "{out}");
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
            msgs.iter().any(|m| m.contains("only #[sequence] ports are moved by value")),
            "{msgs:?}"
        );
        assert!(
            msgs.iter().any(|m| m.contains("non-port state belongs in fields")),
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
        assert!(msgs[0].contains("#[system] needs a `fn execute"), "{msgs:?}");

        // both
        let msgs = expand_err(
            quote!(),
            quote! { impl Both {
                fn execute(&mut self, now: Timestamp) {}
                async fn run(&mut self) {}
            } },
        );
        assert!(msgs.iter().any(|m| m.contains("cyclic or async, not both")), "{msgs:?}");

        // async execute
        let msgs = expand_err(
            quote!(),
            quote! { impl A { async fn execute(&mut self, now: Timestamp) {} } },
        );
        assert!(msgs.iter().any(|m| m.contains("write `async fn run`")), "{msgs:?}");

        // sync run
        let msgs = expand_err(quote!(), quote! { impl A { fn run(&mut self) {} } });
        assert!(msgs.iter().any(|m| m.contains("`run` must be `async`")), "{msgs:?}");

        // now on run
        let msgs = expand_err(
            quote!(),
            quote! { impl A { async fn run(&mut self, now: Timestamp) {} } },
        );
        assert!(msgs.iter().any(|m| m.contains("no coordinator `now`")), "{msgs:?}");

        // missing &mut self
        let msgs = expand_err(
            quote!(),
            quote! { impl A { fn execute(&self, now: Timestamp) {} } },
        );
        assert!(msgs.iter().any(|m| m.contains("takes `&mut self`")), "{msgs:?}");

        // explicit backing
        let msgs = expand_err(
            quote!(),
            quote! { impl A { fn execute(&mut self, now: Timestamp, x: &mut Input<T, RawBacking>) {} } },
        );
        assert!(msgs.iter().any(|m| m.contains("drop the second type parameter")), "{msgs:?}");

        // missing element type
        let msgs = expand_err(
            quote!(),
            quote! { impl A { fn execute(&mut self, now: Timestamp, x: &mut Input) {} } },
        );
        assert!(msgs.iter().any(|m| m.contains("needs an element type")), "{msgs:?}");

        // two HealthPorts
        let msgs = expand_err(
            quote!(),
            quote! { impl A { fn execute(&mut self, now: Timestamp, h1: &mut HealthPort, h2: &mut HealthPort) {} } },
        );
        assert!(msgs.iter().any(|m| m.contains("at most one `&mut HealthPort`")), "{msgs:?}");

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
        assert!(msgs.iter().any(|m| m.contains("`imu` is an input")), "{msgs:?}");

        // init type mismatch
        let msgs = expand_err(
            quote!(),
            quote! { impl A {
                fn init(&mut self, est: &mut Output<B>) {}
                fn execute(&mut self, now: Timestamp, est: &mut Output<A>) {}
            } },
        );
        assert!(
            msgs.iter().any(|m| m.contains("has type `Output<A>` in `execute` but `Output<B>` here")),
            "{msgs:?}"
        );

        // generic impl
        let msgs = expand_err(
            quote!(),
            quote! { impl<T> A<T> { fn execute(&mut self, now: Timestamp) {} } },
        );
        assert!(msgs.iter().any(|m| m.contains("does not support generic impls")), "{msgs:?}");

        // export on an async system
        let msgs = expand_err(
            quote!(export),
            quote! { impl A { async fn run(&mut self) {} } },
        );
        assert!(msgs.iter().any(|m| m.contains("`export` needs a cyclic system")), "{msgs:?}");

        // unknown arg
        let msgs = expand_err(quote!(frobnicate), minimal());
        assert!(msgs[0].contains("unknown #[system] argument `frobnicate`"), "{msgs:?}");
    }
}
