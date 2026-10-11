//! `#[system]` on a free fn: rewrites `fn nav(..)` into `fn nav() -> impl
//! SystemFn`, or `impl AsyncSystemFn` for an `async fn` ending in `stop: Stop`.
//! `#[sequence]` on an `async fn` rewrites it into `impl SequenceFn`.
//!
//! The attribute reads parameter identifiers and doc comments. It classifies
//! no type beyond refusing the few that belong to the other kind of fn; each
//! parameter type's `Param` impl does the rest. A sequence's `Seq` and its
//! parameters' elided lifetimes are named `'__seq`, the brand's lifetime.

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{ToTokens, format_ident, quote, quote_spanned};
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::visit_mut::{self, VisitMut};
use syn::{
    Attribute, Expr, ExprLit, FnArg, GenericArgument, Generics, Ident, Item, ItemFn, Lifetime, Lit,
    Meta, Pat, PathArguments, ReturnType, Signature, Type, TypeImplTrait, TypeParamBound, TypePath,
};

/// Which attribute is expanding.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    System,
    Sequence,
}

/// Expands `#[system]`.
pub fn system(item: TokenStream) -> TokenStream {
    expand(item.into(), &crate::fsw_crate(), Kind::System).into()
}

/// Expands `#[sequence]`.
pub fn sequence(item: TokenStream) -> TokenStream {
    expand(item.into(), &crate::fsw_crate(), Kind::Sequence).into()
}

fn expand(item: TokenStream2, fsw: &TokenStream2, kind: Kind) -> TokenStream2 {
    match syn::parse2::<Item>(item) {
        Ok(Item::Fn(f)) => expand_fn(f, fsw, kind),
        Ok(Item::Impl(block)) => {
            let err = syn::Error::new_spanned(
                &block.self_ty,
                "`#[system]` goes on a free fn; see docs/plans/11-fn-systems.md",
            );
            with_error(block, err)
        }
        Ok(other) => {
            let err = syn::Error::new_spanned(&other, "`#[system]` goes on a fn");
            with_error(other, err)
        }
        Err(e) => e.to_compile_error(),
    }
}

/// Emits `item` unchanged plus `err`, so the IDE keeps its types.
fn with_error(item: impl ToTokens, err: syn::Error) -> TokenStream2 {
    let err = err.to_compile_error();
    quote! { #item #err }
}

/// One parameter of a `#[system]` fn.
struct Arg {
    ident: Ident,
    ty: Type,
}

/// A checked `#[system]` or `#[sequence]` fn signature.
struct Sig {
    kind: Kind,
    args: Vec<Arg>,
    /// The index of the `#[state]` parameter.
    state: Option<usize>,
    /// Whether the last parameter is the async `stop: Stop`.
    stop: bool,
}

impl Sig {
    fn parse(sig: &Signature, states: &[(usize, Attribute)], kind: Kind) -> syn::Result<Self> {
        check_generics(&sig.generics)?;
        match kind {
            Kind::System => check_output(&sig.output)?,
            Kind::Sequence => check_sequence_output(sig)?,
        }
        let args = sig
            .inputs
            .iter()
            .map(arg)
            .collect::<syn::Result<Vec<_>>>()?;
        check_kinds(&args, kind)?;
        let state = state(&args, states)?;
        let stop = match kind {
            Kind::System => system_stop(sig, &args, state)?,
            Kind::Sequence => sequence_stop(sig, &args, state)?,
        };
        Ok(Self {
            kind,
            args,
            state,
            stop,
        })
    }

    /// The parameters that are ports, in order.
    fn ports(&self) -> impl Iterator<Item = &Arg> {
        let end = self.args.len() - usize::from(self.stop);
        self.args[..end]
            .iter()
            .enumerate()
            .filter(|(i, _)| Some(*i) != self.state)
            .map(|(_, arg)| arg)
    }

    /// The state type, `S` from `&mut S`, or `()` without a state parameter.
    fn state_ty(&self) -> Type {
        match self.state {
            Some(i) => key_type(&self.args[i].ty),
            None => syn::parse_quote!(()),
        }
    }
}

/// Removes every `#[state]` attribute, returning each with its parameter index.
fn take_states(sig: &mut Signature) -> Vec<(usize, Attribute)> {
    let mut states = Vec::new();
    for (i, input) in sig.inputs.iter_mut().enumerate() {
        let FnArg::Typed(typed) = input else { continue };
        typed.attrs.retain(|attr| {
            let is_state = attr.path().is_ident("state");
            if is_state {
                states.push((i, attr.clone()));
            }
            !is_state
        });
    }
    states
}

fn arg(input: &FnArg) -> syn::Result<Arg> {
    let FnArg::Typed(typed) = input else {
        return Err(syn::Error::new_spanned(
            input,
            "`#[system]` goes on a free fn",
        ));
    };
    let Pat::Ident(pat) = &*typed.pat else {
        return Err(syn::Error::new_spanned(
            &typed.pat,
            "#[system] parameters need a plain name",
        ));
    };
    if pat.subpat.is_some() || pat.by_ref.is_some() {
        return Err(syn::Error::new_spanned(
            pat,
            "#[system] parameters need a plain name",
        ));
    }
    check_type(&typed.ty)?;
    Ok(Arg {
        ident: pat.ident.clone(),
        ty: (*typed.ty).clone(),
    })
}

/// Finds the one `#[state]` parameter and checks it is `&mut S`.
fn state(args: &[Arg], states: &[(usize, Attribute)]) -> syn::Result<Option<usize>> {
    match states {
        [] => Ok(None),
        [(i, _)] => match &args[*i].ty {
            Type::Reference(r) if r.mutability.is_some() => Ok(Some(*i)),
            other => Err(syn::Error::new_spanned(other, "`#[state]` takes `&mut S`")),
        },
        [_, (_, second), ..] => Err(syn::Error::new_spanned(
            second,
            "a `#[system]` fn takes one `#[state]` parameter at most",
        )),
    }
}

/// Whether an async `#[system]` fn ends in `stop`, which it must.
fn system_stop(sig: &Signature, args: &[Arg], state: Option<usize>) -> syn::Result<bool> {
    let stop = sig.asyncness.is_some();
    if stop && !ends_in_stop(args, state) {
        return Err(syn::Error::new_spanned(
            sig,
            "an async `#[system]` fn takes `stop: Stop` as its last parameter",
        ));
    }
    Ok(stop)
}

/// A `#[sequence]` fn is async and hears abort through its `Seq`, not `stop`.
fn sequence_stop(sig: &Signature, args: &[Arg], state: Option<usize>) -> syn::Result<bool> {
    if sig.asyncness.is_none() {
        return Err(syn::Error::new_spanned(
            sig.fn_token,
            "a `#[sequence]` fn is `async`",
        ));
    }
    if let Some(last) = args.last().filter(|_| ends_in_stop(args, state)) {
        return Err(syn::Error::new_spanned(
            &last.ty,
            "a `#[sequence]` fn takes no `stop`; `seq.aborted()` and its waits report an abort",
        ));
    }
    Ok(false)
}

/// Refuses the parameter types that belong to the other kind of fn.
fn check_kinds(args: &[Arg], kind: Kind) -> syn::Result<()> {
    let refused: &[(&str, &str)] = match kind {
        Kind::System => &[("Seq", "a `Seq` parameter belongs on a `#[sequence]` fn")],
        Kind::Sequence => &[
            (
                "Timestamp",
                "a `#[sequence]` fn reads the time with `seq.now()`",
            ),
            (
                "Abort",
                "a `#[sequence]` fn hears an abort through its `Seq`",
            ),
        ],
    };
    for arg in args {
        if let Some((_, message)) = refused.iter().find(|(name, _)| names(&arg.ty, name)) {
            return Err(syn::Error::new_spanned(&arg.ty, *message));
        }
    }
    Ok(())
}

/// Whether `ty`, behind any references, is a path ending in `name`.
fn names(ty: &Type, name: &str) -> bool {
    match ty {
        Type::Reference(r) => names(&r.elem, name),
        Type::Path(path) => path.path.segments.last().is_some_and(|s| s.ident == name),
        _ => false,
    }
}

fn ends_in_stop(args: &[Arg], state: Option<usize>) -> bool {
    let Some(last) = args.len().checked_sub(1) else {
        return false;
    };
    let is_stop = matches!(&args[last].ty, Type::Path(path)
        if path.path.segments.last().is_some_and(|s| s.ident == "Stop"));
    is_stop && state != Some(last)
}

fn check_generics(generics: &Generics) -> syn::Result<()> {
    match generics.lifetimes().next() {
        Some(lt) => Err(syn::Error::new_spanned(lt, NAMED_LIFETIME)),
        None => Ok(()),
    }
}

fn check_output(output: &ReturnType) -> syn::Result<()> {
    match output {
        ReturnType::Type(_, ty) if !matches!(&**ty, Type::Tuple(t) if t.elems.is_empty()) => {
            Err(syn::Error::new_spanned(ty, "a `#[system]` fn returns `()`"))
        }
        _ => Ok(()),
    }
}

/// A `#[sequence]` fn returns `Result<(), SeqError>`.
fn check_sequence_output(sig: &Signature) -> syn::Result<()> {
    match &sig.output {
        ReturnType::Type(_, ty) if names(ty, "Result") => Ok(()),
        ReturnType::Type(_, ty) => Err(syn::Error::new_spanned(ty, SEQUENCE_OUTPUT)),
        ReturnType::Default => Err(syn::Error::new_spanned(&sig.ident, SEQUENCE_OUTPUT)),
    }
}

const SEQUENCE_OUTPUT: &str = "a `#[sequence]` fn returns `Result<(), SeqError>`";

const NAMED_LIFETIME: &str = "`#[system]` parameters take no named lifetimes";

/// Rejects named lifetimes and `impl Trait`, which the fn pointer and the
/// `Params` tuple cannot spell.
fn check_type(ty: &Type) -> syn::Result<()> {
    let mut check = TypeCheck(None);
    check.visit_type(ty);
    check.0.map_or(Ok(()), Err)
}

struct TypeCheck(Option<syn::Error>);

impl<'ast> Visit<'ast> for TypeCheck {
    fn visit_lifetime(&mut self, lt: &'ast Lifetime) {
        if lt.ident != "_" && self.0.is_none() {
            self.0 = Some(syn::Error::new_spanned(lt, NAMED_LIFETIME));
        }
    }

    fn visit_type_impl_trait(&mut self, ty: &'ast TypeImplTrait) {
        if self.0.is_none() {
            self.0 = Some(syn::Error::new_spanned(
                ty,
                "`#[system]` parameters take no `impl Trait`",
            ));
        }
    }
}

/// Rewrites a fn, or emits it without its `#[state]` attributes plus the error.
fn expand_fn(mut f: ItemFn, fsw: &TokenStream2, kind: Kind) -> TokenStream2 {
    let states = take_states(&mut f.sig);
    match Sig::parse(&f.sig, &states, kind) {
        Ok(sig) => emit(f, &sig, fsw),
        Err(e) => {
            // An `async fn` cannot elide `Seq`'s lifetime.
            if kind == Kind::Sequence {
                brand(&mut f.sig);
            }
            with_error(f, e)
        }
    }
}

/// Whether an attribute belongs on the outer fn rather than the body.
fn is_outer(attr: &Attribute) -> bool {
    ["doc", "cfg", "deprecated", "must_use"]
        .iter()
        .any(|name| attr.path().is_ident(name))
}

/// Adds `T: 'static` for every type parameter, which `Ports: 'static` needs.
fn static_generics(generics: &Generics) -> Generics {
    let mut generics = generics.clone();
    let bound: TypeParamBound = syn::parse_quote!('static);
    for param in generics.type_params_mut() {
        if !param.bounds.iter().any(|b| *b == bound) {
            param.bounds.push(bound.clone());
        }
    }
    generics
}

fn emit(f: ItemFn, sig: &Sig, fsw: &TokenStream2) -> TokenStream2 {
    let (outer, inner): (Vec<_>, Vec<_>) = f.attrs.iter().cloned().partition(is_outer);
    let doc = doc(&outer);
    let generics = static_generics(&f.sig.generics);
    let (impl_generics, _, where_clause) = generics.split_for_impl();
    let (vis, name) = (&f.vis, &f.sig.ident);
    let type_params = f.sig.generics.type_params().map(|p| &p.ident);
    let state_ty = sig.state_ty();
    let fn_ptr = fn_ptr(sig);
    let (bound, call) = if sig.kind == Kind::Sequence {
        (
            quote!(#fsw::SequenceFn<State = #state_ty>),
            sequence_call(sig, &generics, fsw),
        )
    } else if sig.stop {
        (
            quote!(#fsw::AsyncSystemFn<State = #state_ty>),
            async_call(sig, &generics, fsw),
        )
    } else {
        (
            quote!(#fsw::SystemFn<State = #state_ty> + ::core::ops::Deref<Target = #fn_ptr>),
            sync_call(sig, &generics, fsw),
        )
    };
    let ports = ports_impl(sig, &name.to_string(), &doc, &generics, fsw);
    let mut body_sig = Signature {
        ident: format_ident!("__metor_body"),
        ..f.sig.clone()
    };
    if sig.kind == Kind::Sequence {
        brand(&mut body_sig);
    }
    let body = ItemFn {
        attrs: inner,
        vis: syn::Visibility::Inherited,
        sig: body_sig,
        block: f.block,
    };
    quote! {
        #(#outer)*
        #vis fn #name #impl_generics () -> impl #bound #where_clause {
            #body
            struct __MetorSystem #impl_generics (
                ::core::marker::PhantomData<fn() -> (#(#type_params,)*)>
            ) #where_clause;
            #ports
            #call
            __MetorSystem(::core::marker::PhantomData)
        }
    }
}

/// `fn(..)` over every parameter type, as the user wrote them.
fn fn_ptr(sig: &Sig) -> TokenStream2 {
    let tys = sig.args.iter().map(|arg| &arg.ty);
    quote!(fn(#(#tys),*))
}

fn ports_impl(
    sig: &Sig,
    name: &str,
    doc: &str,
    generics: &Generics,
    fsw: &TokenStream2,
) -> TokenStream2 {
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    let keys = sig.ports().map(|arg| key_type(&arg.ty));
    let names = sig.ports().map(|arg| arg.ident.to_string());
    let state_ty = sig.state_ty();
    quote! {
        impl #impl_generics #fsw::Ports for __MetorSystem #ty_generics #where_clause {
            type State = #state_ty;
            type Params = (#(#keys,)*);
            const NAME: &'static str = #name;
            const NAMES: &'static [&'static str] = &[#(#names),*];
            const DOC: &'static str = #doc;
        }
    }
}

/// The pieces every `call` shares: the state pattern, the items pattern, the
/// `Param` asserts, and the call into the body.
struct CallParts {
    state: TokenStream2,
    items: TokenStream2,
    asserts: TokenStream2,
    body: TokenStream2,
}

fn call_parts(sig: &Sig, generics: &Generics, fsw: &TokenStream2) -> CallParts {
    let state = match sig.state {
        Some(i) => sig.args[i].ident.to_token_stream(),
        None => quote!(_),
    };
    let idents = sig.ports().map(|arg| &arg.ident);
    // Spanned at each parameter type, so a type that is no `Param` errors there.
    let asserts = sig.ports().map(|arg| {
        let key = key_type(&arg.ty);
        quote_spanned! { arg.ty.span() => #fsw::fn_system::assert_param::<#key>(); }
    });
    let args = sig.args.iter().map(|arg| &arg.ident);
    let (_, ty_generics, _) = generics.split_for_impl();
    let turbofish = ty_generics.as_turbofish();
    CallParts {
        state,
        items: quote!((#(#idents,)*)),
        asserts: quote!(#(#asserts)*),
        body: quote!(__metor_body #turbofish (#(#args),*)),
    }
}

fn sync_call(sig: &Sig, generics: &Generics, fsw: &TokenStream2) -> TokenStream2 {
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    let CallParts {
        state,
        items,
        asserts,
        body,
    } = call_parts(sig, generics, fsw);
    let state_ty = sig.state_ty();
    let fn_ptr = fn_ptr(sig);
    let turbofish = ty_generics.as_turbofish();
    quote! {
        impl #impl_generics #fsw::SystemFn for __MetorSystem #ty_generics #where_clause {
            fn call(
                #state: &mut #state_ty,
                #items: <Self::Params as #fsw::Param>::Item<'_, #fsw::ring::NoWake>,
            ) {
                #asserts
                #body
            }
        }
        impl #impl_generics ::core::ops::Deref for __MetorSystem #ty_generics #where_clause {
            type Target = #fn_ptr;
            fn deref(&self) -> &Self::Target {
                &(__metor_body #turbofish as #fn_ptr)
            }
        }
    }
}

fn async_call(sig: &Sig, generics: &Generics, fsw: &TokenStream2) -> TokenStream2 {
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    let CallParts {
        state,
        items,
        asserts,
        body,
    } = call_parts(sig, generics, fsw);
    let state_ty = sig.state_ty();
    // PANIC Safety: `Sig::parse` checked an async fn ends in `stop`.
    let stop = &sig.args.last().expect("a stop parameter").ident;
    quote! {
        impl #impl_generics #fsw::AsyncSystemFn for __MetorSystem #ty_generics #where_clause {
            async fn call(
                #state: &mut #state_ty,
                #items: <Self::Params as #fsw::Param>::Item<'_, #fsw::ring::Notifier>,
                #stop: #fsw::Stop,
            ) {
                #asserts
                #body.await
            }
        }
    }
}

fn sequence_call(sig: &Sig, generics: &Generics, fsw: &TokenStream2) -> TokenStream2 {
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    let CallParts {
        state,
        items,
        asserts,
        body,
    } = call_parts(sig, generics, fsw);
    let state_ty = sig.state_ty();
    let brand = brand_lifetime();
    quote! {
        impl #impl_generics #fsw::SequenceFn for __MetorSystem #ty_generics #where_clause {
            async fn call<#brand>(
                #state: &#brand mut #state_ty,
                #items: <Self::Params as #fsw::Param>::Item<#brand, #fsw::ring::NoWake>,
            ) -> ::core::result::Result<(), #fsw::SeqError> {
                #asserts
                #body.await
            }
        }
    }
}

/// Joins the doc comment lines, each without its leading space.
fn doc(attrs: &[Attribute]) -> String {
    attrs
        .iter()
        .filter(|attr| attr.path().is_ident("doc"))
        .filter_map(|attr| match &attr.meta {
            Meta::NameValue(nv) => match &nv.value {
                Expr::Lit(ExprLit {
                    lit: Lit::Str(text),
                    ..
                }) => Some(text.value()),
                _ => None,
            },
            _ => None,
        })
        .map(|line| line.strip_prefix(' ').unwrap_or(&line).to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Strips one outer reference, so `&mut Input<T>`, `&Seq`, and `Input<T>`
/// name their `Param` alike. A `Seq` keys as `Seq<'static>`.
fn key_type(ty: &Type) -> Type {
    let key = match ty {
        Type::Reference(r) => (*r.elem).clone(),
        other => other.clone(),
    };
    match &key {
        Type::Path(path) if is_seq(path) => {
            let mut path = path.clone();
            set_seq_lifetime(&mut path, &Lifetime::new("'static", Span::call_site()));
            Type::Path(path)
        }
        _ => key,
    }
}

/// The lifetime a sequence body's elided lifetimes and its `Seq` share.
fn brand_lifetime() -> Lifetime {
    Lifetime::new("'__seq", Span::call_site())
}

/// Names every elided lifetime in a sequence body's parameters, and its
/// `Seq`'s, `'__seq`, so a redline's condition may borrow any parameter.
fn brand(sig: &mut Signature) {
    let lifetime = brand_lifetime();
    sig.generics.params.insert(
        0,
        syn::GenericParam::Lifetime(syn::LifetimeParam::new(lifetime.clone())),
    );
    let mut brander = Brander(lifetime);
    for input in &mut sig.inputs {
        if let FnArg::Typed(typed) = input {
            brander.visit_type_mut(&mut typed.ty);
        }
    }
}

struct Brander(Lifetime);

impl VisitMut for Brander {
    fn visit_type_reference_mut(&mut self, r: &mut syn::TypeReference) {
        if r.lifetime.is_none() {
            r.lifetime = Some(self.0.clone());
        }
        visit_mut::visit_type_reference_mut(self, r);
    }

    fn visit_lifetime_mut(&mut self, lt: &mut Lifetime) {
        if lt.ident == "_" {
            *lt = self.0.clone();
        }
    }

    fn visit_type_path_mut(&mut self, path: &mut TypePath) {
        if is_seq(path) {
            set_seq_lifetime(path, &self.0);
        }
        visit_mut::visit_type_path_mut(self, path);
    }
}

/// Whether `path` is `Seq` or `Seq<'_>`, whose lifetime the macro names.
fn is_seq(path: &TypePath) -> bool {
    let Some(last) = path.path.segments.last() else {
        return false;
    };
    if path.qself.is_some() || last.ident != "Seq" {
        return false;
    }
    match &last.arguments {
        PathArguments::None => true,
        PathArguments::AngleBracketed(args) => {
            args.args.len() == 1
                && matches!(args.args.first(), Some(GenericArgument::Lifetime(lt)) if lt.ident == "_")
        }
        PathArguments::Parenthesized(_) => false,
    }
}

fn set_seq_lifetime(path: &mut TypePath, lifetime: &Lifetime) {
    if let Some(last) = path.path.segments.last_mut() {
        last.arguments = PathArguments::AngleBracketed(syn::parse_quote!(<#lifetime>));
    }
}

#[cfg(test)]
mod tests;
