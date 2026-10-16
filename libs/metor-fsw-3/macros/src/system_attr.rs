//! Expands `#[system]` and `#[sequence]`.
//!
//! `#[system]` rewrites `fn nav(..)` into `fn nav() -> impl SystemFn`. For an
//! `async fn` whose last parameter is `stop: Stop`, it returns
//! `impl AsyncSystemFn` instead. `#[sequence]` rewrites an `async fn` into a fn
//! returning `impl SequenceFn`.
//!
//! The macro reads only parameter names and doc comments. It does not classify
//! parameter types, apart from rejecting a few that belong to the other kind of
//! fn. Each parameter type's `Param` impl handles the rest. In a sequence, the
//! `Seq` parameter and every elided lifetime in the parameters share one
//! lifetime, `'__seq`. That shared lifetime lets a redline's condition borrow
//! any parameter.

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
        Ok(other) => {
            let err = syn::Error::new(Span::call_site(), "`#[system]` goes on a free fn");
            with_error(other, err)
        }
        Err(e) => e.to_compile_error(),
    }
}

/// Emits `item` unchanged, followed by `err`.
///
/// Keeping the item lets IDEs still resolve its types.
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

    /// Returns the parameters that are ports, in order.
    fn ports(&self) -> impl Iterator<Item = &Arg> {
        let end = self.args.len() - usize::from(self.stop);
        self.args[..end]
            .iter()
            .enumerate()
            .filter(|(i, _)| Some(*i) != self.state)
            .map(|(_, arg)| arg)
    }

    /// Returns the state type.
    ///
    /// That is `S` from the `#[state] &mut S` parameter, or `()` without one.
    fn state_ty(&self) -> Type {
        match self.state {
            Some(i) => key_type(&self.args[i].ty),
            None => syn::parse_quote!(()),
        }
    }
}

/// Removes every `#[state]` attribute.
///
/// Returns each removed attribute with its parameter index.
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

/// Returns whether a `#[system]` fn is async.
///
/// An async `#[system]` fn must take `stop: Stop` as its last parameter.
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

/// Checks that a `#[sequence]` fn is async and takes no `stop`.
///
/// A sequence learns about an abort through its `Seq`, so this returns
/// `Ok(false)` on success.
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

/// Rejects parameter types that belong to the other kind of fn.
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

/// Returns whether `ty` is a path ending in `name`, ignoring references.
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

/// Checks that a `#[sequence]` fn returns a `Result`.
///
/// The expected type is `Result<(), SeqError>`.
fn check_sequence_output(sig: &Signature) -> syn::Result<()> {
    match &sig.output {
        ReturnType::Type(_, ty) if names(ty, "Result") => Ok(()),
        ReturnType::Type(_, ty) => Err(syn::Error::new_spanned(ty, SEQUENCE_OUTPUT)),
        ReturnType::Default => Err(syn::Error::new_spanned(&sig.ident, SEQUENCE_OUTPUT)),
    }
}

const SEQUENCE_OUTPUT: &str = "a `#[sequence]` fn returns `Result<(), SeqError>`";

const NAMED_LIFETIME: &str = "`#[system]` parameters take no named lifetimes";

/// Rejects named lifetimes and `impl Trait` in a parameter type.
///
/// Neither can be written in the fn pointer type or the `Params` tuple.
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

/// Rewrites a fn.
///
/// On error, emits the fn without its `#[state]` attributes, followed by the
/// error.
fn expand_fn(mut f: ItemFn, fsw: &TokenStream2, kind: Kind) -> TokenStream2 {
    let states = take_states(&mut f.sig);
    match Sig::parse(&f.sig, &states, kind) {
        Ok(sig) => emit(f, &sig, fsw),
        Err(e) => {
            // An `async fn` cannot elide `Seq`'s lifetime.
            if kind == Kind::Sequence {
                share_lifetime(&mut f.sig);
            }
            with_error(f, e)
        }
    }
}

/// Returns whether an attribute stays on the outer fn instead of the body.
fn is_outer(attr: &Attribute) -> bool {
    ["doc", "cfg", "deprecated", "must_use"]
        .iter()
        .any(|name| attr.path().is_ident(name))
}

/// Adds a `'static` bound to every type parameter.
///
/// `FnSignature` requires `'static`.
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
    let doc = extract_doc(&outer);
    let generics = static_generics(&f.sig.generics);
    let (impl_generics, _, where_clause) = generics.split_for_impl();
    let (vis, name) = (&f.vis, &f.sig.ident);
    let type_params = f.sig.generics.type_params().map(|p| &p.ident);
    let state_ty = sig.state_ty();
    // The trait bound the rewritten fn returns, plus the impls behind it.
    let (bound, call) = match (sig.kind, sig.args.last().filter(|_| sig.stop)) {
        (Kind::Sequence, _) => sequence_call(sig, &state_ty, &generics, fsw),
        (Kind::System, Some(stop)) => async_call(sig, &stop.ident, &state_ty, &generics, fsw),
        (Kind::System, None) => cyclic_call(sig, &state_ty, &generics, fsw),
    };
    let ports = ports_impl(sig, &name.to_string(), &doc, &state_ty, &generics, fsw);
    let mut body_sig = Signature {
        ident: format_ident!("__metor_body"),
        ..f.sig.clone()
    };
    if sig.kind == Kind::Sequence {
        share_lifetime(&mut body_sig);
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

fn ports_impl(
    sig: &Sig,
    name: &str,
    doc: &str,
    state_ty: &Type,
    generics: &Generics,
    fsw: &TokenStream2,
) -> TokenStream2 {
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    let keys = sig.ports().map(|arg| key_type(&arg.ty));
    let names = sig.ports().map(|arg| arg.ident.to_string());
    quote! {
        impl #impl_generics #fsw::FnSignature for __MetorSystem #ty_generics #where_clause {
            type State = #state_ty;
            type Params = (#(#keys,)*);
            const NAME: &'static str = #name;
            const NAMES: &'static [&'static str] = &[#(#names),*];
            const DOC: &'static str = #doc;
        }
    }
}

/// The parts every generated `call` shares.
///
/// That is the `#[state]` parameter's name (or `_`), the destructured ports,
/// and a body that asserts each port type implements `Param` and then calls
/// the original fn.
fn call_parts(
    sig: &Sig,
    generics: &Generics,
    fsw: &TokenStream2,
) -> (TokenStream2, TokenStream2, TokenStream2) {
    let state = match sig.state {
        Some(i) => sig.args[i].ident.to_token_stream(),
        None => quote!(_),
    };
    let idents = sig.ports().map(|arg| &arg.ident);
    let items = quote!((#(#idents,)*): <Self::Params as #fsw::Param>::Item);
    // Spanned at each parameter type, so a type that is no `Param` errors there.
    let asserts = sig.ports().map(|arg| {
        let key = key_type(&arg.ty);
        quote_spanned! { arg.ty.span() => #fsw::fn_system::assert_param::<#key>(); }
    });
    let args = sig.args.iter().map(|arg| &arg.ident);
    let turbofish = generics.split_for_impl().1.as_turbofish().to_token_stream();
    let body = quote!(#(#asserts)* __metor_body #turbofish (#(#args),*));
    (state, items, body)
}

/// Implements `fsw::trait_` for the generated struct, with `items` inside.
fn impl_for(
    trait_: TokenStream2,
    items: TokenStream2,
    generics: &Generics,
    fsw: &TokenStream2,
) -> TokenStream2 {
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    quote! {
        impl #impl_generics #fsw::#trait_ for __MetorSystem #ty_generics #where_clause {
            #items
        }
    }
}

fn sequence_call(
    sig: &Sig,
    state_ty: &Type,
    generics: &Generics,
    fsw: &TokenStream2,
) -> (TokenStream2, TokenStream2) {
    let (state, items, body) = call_parts(sig, generics, fsw);
    let lifetime = seq_lifetime();
    let call = quote! {
        async fn call<#lifetime>(
            #state: &#lifetime mut #state_ty,
            #items<#lifetime, #fsw::ring::NoWake>,
        ) -> ::core::result::Result<(), #fsw::SeqError> {
            #body.await
        }
    };
    (
        quote!(#fsw::SequenceFn<State = #state_ty>),
        impl_for(quote!(SequenceFn), call, generics, fsw),
    )
}

fn async_call(
    sig: &Sig,
    stop: &Ident,
    state_ty: &Type,
    generics: &Generics,
    fsw: &TokenStream2,
) -> (TokenStream2, TokenStream2) {
    let (state, items, body) = call_parts(sig, generics, fsw);
    let call = quote! {
        async fn call(
            #state: &mut #state_ty,
            #items<'_, #fsw::ring::Notifier>,
            #stop: #fsw::Stop,
        ) {
            #body.await
        }
    };
    (
        quote!(#fsw::AsyncSystemFn<State = #state_ty>),
        impl_for(quote!(AsyncSystemFn), call, generics, fsw),
    )
}

/// A cyclic system also derefs to a fn pointer to the original body.
fn cyclic_call(
    sig: &Sig,
    state_ty: &Type,
    generics: &Generics,
    fsw: &TokenStream2,
) -> (TokenStream2, TokenStream2) {
    let (state, items, body) = call_parts(sig, generics, fsw);
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    let turbofish = ty_generics.as_turbofish();
    let tys = sig.args.iter().map(|arg| &arg.ty);
    let fn_ptr = quote!(fn(#(#tys),*));
    let call = quote! {
        fn call(#state: &mut #state_ty, #items<'_, #fsw::ring::NoWake>) {
            #body
        }
    };
    let call = impl_for(quote!(SystemFn), call, generics, fsw);
    let impls = quote! {
        #call
        impl #impl_generics ::core::ops::Deref for __MetorSystem #ty_generics #where_clause {
            type Target = #fn_ptr;
            fn deref(&self) -> &Self::Target {
                &(__metor_body #turbofish as #fn_ptr)
            }
        }
    };
    (
        quote!(#fsw::SystemFn<State = #state_ty> + ::core::ops::Deref<Target = #fn_ptr>),
        impls,
    )
}

/// Joins the doc comment lines, stripping one leading space from each.
fn extract_doc(attrs: &[Attribute]) -> String {
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

/// Returns the type used to look up a parameter's `Param` impl.
///
/// One outer reference is stripped, so `&mut Input<T>` and `Input<T>` get the
/// same key. `Seq` is keyed as `Seq<'static>`.
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

/// Returns `'__seq`, the lifetime shared by a sequence's parameters.
fn seq_lifetime() -> Lifetime {
    Lifetime::new("'__seq", Span::call_site())
}

/// Names every elided lifetime in a sequence's parameters `'__seq`.
///
/// This includes the `Seq`'s lifetime. With one shared lifetime, a redline's
/// condition can borrow any parameter.
fn share_lifetime(sig: &mut Signature) {
    let lifetime = seq_lifetime();
    sig.generics.params.insert(
        0,
        syn::GenericParam::Lifetime(syn::LifetimeParam::new(lifetime.clone())),
    );
    let mut namer = ElidedLifetimes(lifetime);
    for input in &mut sig.inputs {
        if let FnArg::Typed(typed) = input {
            namer.visit_type_mut(&mut typed.ty);
        }
    }
}

/// Names every elided lifetime it visits with the lifetime it holds.
struct ElidedLifetimes(Lifetime);

impl VisitMut for ElidedLifetimes {
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

/// Returns whether `path` is `Seq` or `Seq<'_>`.
///
/// The macro fills in the lifetime of those two forms.
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
