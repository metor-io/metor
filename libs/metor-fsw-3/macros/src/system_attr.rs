//! `#[system]` on a free fn: rewrites `fn nav(..)` into `fn nav() -> impl
//! SystemFn`, or `impl AsyncSystemFn` for an `async fn` ending in `stop: Stop`.
//! On an impl block, emits `SystemFn` from an `execute` method, or
//! `AsyncSystemFn` from an `async run(.., stop: Stop)` method.
//!
//! The attribute reads parameter identifiers and doc comments. It never
//! classifies a type; each parameter type's `Param` impl does that.

use convert_case::{Case, Casing};
use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{ToTokens, format_ident, quote, quote_spanned};
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::{
    Attribute, Expr, ExprLit, FnArg, Generics, Ident, ImplItem, ImplItemFn, Item, ItemFn, ItemImpl,
    Lifetime, Lit, Meta, Pat, ReturnType, Signature, Type, TypeImplTrait,
};

/// One `execute` parameter after the receiver.
struct Param {
    ident: Ident,
    key: Type,
}

/// Expands `#[system]`.
pub fn system(item: TokenStream) -> TokenStream {
    expand(item.into(), &crate::fsw_crate()).into()
}

fn expand(item: TokenStream2, fsw: &TokenStream2) -> TokenStream2 {
    match syn::parse2::<Item>(item) {
        Ok(Item::Fn(f)) => expand_fn(f, fsw),
        Ok(Item::Impl(block)) => match expand_impl(&block, fsw) {
            Ok(system_fn) => quote! { #block #system_fn },
            Err(e) => {
                let err = e.to_compile_error();
                quote! { #block #err }
            }
        },
        Ok(other) => {
            let err = syn::Error::new(other.span(), "`#[system]` goes on a fn").to_compile_error();
            quote! { #other #err }
        }
        Err(e) => e.to_compile_error(),
    }
}

/// One parameter of a `#[system]` fn.
struct Arg {
    ident: Ident,
    ty: Type,
}

/// A checked `#[system]` fn signature.
struct Sig {
    args: Vec<Arg>,
    /// The index of the `#[state]` parameter.
    state: Option<usize>,
    /// Whether the last parameter is the async `stop: Stop`.
    stop: bool,
}

impl Sig {
    fn parse(sig: &Signature, states: &[(usize, Span)]) -> syn::Result<Self> {
        check_generics(&sig.generics)?;
        check_output(&sig.output)?;
        let args = sig
            .inputs
            .iter()
            .map(arg)
            .collect::<syn::Result<Vec<_>>>()?;
        let state = state(&args, states)?;
        let stop = sig.asyncness.is_some();
        if stop && !ends_in_stop(&args, state) {
            return Err(syn::Error::new(
                sig.span(),
                "an async `#[system]` fn takes `stop: Stop` as its last parameter",
            ));
        }
        Ok(Self { args, state, stop })
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

/// Removes every `#[state]` attribute, returning each one's parameter index and span.
fn take_states(sig: &mut Signature) -> Vec<(usize, Span)> {
    let mut states = Vec::new();
    for (i, input) in sig.inputs.iter_mut().enumerate() {
        let FnArg::Typed(typed) = input else { continue };
        typed.attrs.retain(|attr| {
            let is_state = attr.path().is_ident("state");
            if is_state {
                states.push((i, attr.span()));
            }
            !is_state
        });
    }
    states
}

fn arg(input: &FnArg) -> syn::Result<Arg> {
    let FnArg::Typed(typed) = input else {
        return Err(syn::Error::new(
            input.span(),
            "`#[system]` goes on a free fn",
        ));
    };
    let Pat::Ident(pat) = &*typed.pat else {
        return Err(syn::Error::new(
            typed.pat.span(),
            "#[system] parameters need a plain name",
        ));
    };
    if pat.subpat.is_some() || pat.by_ref.is_some() {
        return Err(syn::Error::new(
            pat.span(),
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
fn state(args: &[Arg], states: &[(usize, Span)]) -> syn::Result<Option<usize>> {
    match states {
        [] => Ok(None),
        [(i, _)] => match &args[*i].ty {
            Type::Reference(r) if r.mutability.is_some() => Ok(Some(*i)),
            other => Err(syn::Error::new(other.span(), "`#[state]` takes `&mut S`")),
        },
        [_, (_, second), ..] => Err(syn::Error::new(
            *second,
            "a `#[system]` fn takes one `#[state]` parameter at most",
        )),
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
        Some(lt) => Err(syn::Error::new(lt.span(), NAMED_LIFETIME)),
        None => Ok(()),
    }
}

fn check_output(output: &ReturnType) -> syn::Result<()> {
    match output {
        ReturnType::Type(_, ty) if !matches!(&**ty, Type::Tuple(t) if t.elems.is_empty()) => {
            Err(syn::Error::new(ty.span(), "a `#[system]` fn returns `()`"))
        }
        _ => Ok(()),
    }
}

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
            self.0 = Some(syn::Error::new(lt.span(), NAMED_LIFETIME));
        }
    }

    fn visit_type_impl_trait(&mut self, ty: &'ast TypeImplTrait) {
        if self.0.is_none() {
            self.0 = Some(syn::Error::new(
                ty.span(),
                "`#[system]` parameters take no `impl Trait`",
            ));
        }
    }
}

/// Rewrites a fn, or emits it without its `#[state]` attributes plus the error.
fn expand_fn(mut f: ItemFn, fsw: &TokenStream2) -> TokenStream2 {
    let states = take_states(&mut f.sig);
    match Sig::parse(&f.sig, &states) {
        Ok(sig) => emit(f, &sig, fsw),
        Err(e) => {
            let err = e.to_compile_error();
            quote! { #f #err }
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
    let params: Vec<Ident> = generics.type_params().map(|p| p.ident.clone()).collect();
    let clause = generics.make_where_clause();
    for param in params {
        clause.predicates.push(syn::parse_quote!(#param: 'static));
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
    let (bound, call) = match sig.stop {
        true => (
            quote!(#fsw::AsyncSystemFn<State = #state_ty>),
            async_call(sig, &generics, fsw),
        ),
        false => (
            quote!(#fsw::SystemFn<State = #state_ty> + ::core::ops::Deref<Target = #fn_ptr>),
            sync_call(sig, &generics, fsw),
        ),
    };
    let ports = ports_impl(sig, &name.to_string(), &doc, &generics, fsw);
    let body = ItemFn {
        attrs: inner,
        vis: syn::Visibility::Inherited,
        sig: Signature {
            ident: format_ident!("__metor_body"),
            ..f.sig.clone()
        },
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
    let turbofish = generics.split_for_impl().1;
    let turbofish = turbofish.as_turbofish();
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

fn expand_impl(block: &ItemImpl, fsw: &TokenStream2) -> syn::Result<TokenStream2> {
    let name = type_name(&block.self_ty)?;
    let method = method(block)?;
    let is_async = method.sig.asyncness.is_some();
    let mut params = params(method)?;
    let stop = if is_async {
        Some(take_stop(method, &mut params)?)
    } else {
        None
    };
    let doc = doc(&method.attrs);
    let idents = params.iter().map(|p| &p.ident).collect::<Vec<_>>();
    let keys = params.iter().map(|p| &p.key);
    let names = params.iter().map(|p| p.ident.to_string());
    let self_ty = &block.self_ty;
    let (impl_generics, _, where_clause) = block.generics.split_for_impl();
    // Spanned at each parameter type, so a type that is no `Param` errors there.
    let asserts = params.iter().map(|p| {
        let key = &p.key;
        quote_spanned! { key.span() => #fsw::fn_system::assert_param::<#key>(); }
    });
    let call = match stop {
        Some(stop) => quote! {
            impl #impl_generics #fsw::AsyncSystemFn for #self_ty #where_clause {
                async fn call(
                    &mut self,
                    (#(#idents,)*): <Self::Params as #fsw::Param>::Item<'_, #fsw::ring::Notifier>,
                    #stop: #fsw::Stop,
                ) {
                    #(#asserts)*
                    self.run(#(#idents,)* #stop).await
                }
            }
        },
        None => quote! {
            impl #impl_generics #fsw::SystemFn for #self_ty #where_clause {
                fn call(
                    &mut self,
                    (#(#idents,)*): <Self::Params as #fsw::Param>::Item<'_, #fsw::ring::NoWake>,
                ) {
                    #(#asserts)*
                    self.execute(#(#idents),*)
                }
            }
        },
    };
    Ok(quote! {
        impl #impl_generics #fsw::Ports for #self_ty #where_clause {
            type Params = (#(#keys,)*);
            const NAME: &'static str = #name;
            const NAMES: &'static [&'static str] = &[#(#names),*];
            const DOC: &'static str = #doc;
        }
        #call
    })
}

/// Removes the trailing `stop: Stop` parameter, which is no port.
fn take_stop(method: &ImplItemFn, params: &mut Vec<Param>) -> syn::Result<Ident> {
    let is_stop = params.last().is_some_and(|p| {
        matches!(&p.key, Type::Path(path) if path.path.segments.last().is_some_and(|s| s.ident == "Stop"))
    });
    if !is_stop {
        return Err(syn::Error::new(
            method.sig.span(),
            "`run` takes `stop: Stop` as its last parameter",
        ));
    }
    // PANIC Safety: the check above found the parameter.
    Ok(params.pop().expect("a last parameter").ident)
}

/// Returns the snake-cased last path segment of the impl's type.
fn type_name(ty: &Type) -> syn::Result<String> {
    match ty {
        Type::Path(path) if path.path.segments.last().is_some() => {
            // PANIC Safety: the guard checked the segment exists.
            let ident = &path.path.segments.last().expect("non-empty path").ident;
            Ok(ident.to_string().to_case(Case::Snake))
        }
        other => Err(syn::Error::new(
            other.span(),
            "#[system] needs an impl block for a named type",
        )),
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

/// Finds the one `execute` or `async run` method and checks it takes `&mut self`.
fn method(block: &ItemImpl) -> syn::Result<&ImplItemFn> {
    let named = |name: &str| {
        block.items.iter().find_map(move |item| match item {
            ImplItem::Fn(f) if f.sig.ident == name => Some(f),
            _ => None,
        })
    };
    let method = match (named("execute"), named("run")) {
        (Some(_), Some(run)) => {
            return Err(syn::Error::new(
                run.sig.span(),
                "#[system] takes either `execute` or `run`, not both",
            ));
        }
        (Some(execute), None) => execute,
        (None, Some(run)) if run.sig.asyncness.is_some() => run,
        (None, Some(run)) => {
            return Err(syn::Error::new(run.sig.span(), "`run` must be `async`"));
        }
        (None, None) => {
            return Err(syn::Error::new(
                block.self_ty.span(),
                "#[system] needs an `execute` or `run` method in this impl block",
            ));
        }
    };
    let what = &method.sig.ident;
    match method.sig.inputs.first() {
        Some(FnArg::Receiver(recv)) if recv.reference.is_some() && recv.mutability.is_some() => {
            Ok(method)
        }
        Some(arg) => Err(syn::Error::new(
            arg.span(),
            format!("`{what}` takes `&mut self`"),
        )),
        None => Err(syn::Error::new(
            method.sig.span(),
            format!("`{what}` takes `&mut self`"),
        )),
    }
}

/// Collects each parameter's name and its `Param` key, the type without its outer `&mut`.
fn params(method: &ImplItemFn) -> syn::Result<Vec<Param>> {
    method
        .sig
        .inputs
        .iter()
        .skip(1)
        .map(|arg| {
            let FnArg::Typed(typed) = arg else {
                return Err(syn::Error::new(arg.span(), "one receiver only"));
            };
            let Pat::Ident(pat) = &*typed.pat else {
                return Err(syn::Error::new(
                    typed.pat.span(),
                    "#[system] parameters need a plain name",
                ));
            };
            Ok(Param {
                ident: pat.ident.clone(),
                key: key_type(&typed.ty),
            })
        })
        .collect()
}

/// Strips one outer `&mut`, so `&mut Input<T>` and `Input<T>` share a key.
fn key_type(ty: &Type) -> Type {
    match ty {
        Type::Reference(r) if r.mutability.is_some() => (*r.elem).clone(),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests;
