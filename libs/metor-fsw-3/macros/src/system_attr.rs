//! `#[system]` on an impl block: emits `SystemFn` from an `execute` method, or
//! `AsyncSystemFn` from an `async run(.., stop: Stop)` method.
//!
//! The attribute reads parameter identifiers and doc comments. It never
//! classifies a type; each parameter type's `Param` impl does that.

use convert_case::{Case, Casing};
use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{quote, quote_spanned};
use syn::spanned::Spanned;
use syn::{
    Expr, ExprLit, FnArg, Ident, ImplItem, ImplItemFn, ItemImpl, Lit, Meta, Pat, Type,
    parse_macro_input,
};

/// One `execute` parameter after the receiver.
struct Param {
    ident: Ident,
    key: Type,
}

/// Expands `#[system]`.
pub fn system(item: TokenStream) -> TokenStream {
    let block = parse_macro_input!(item as ItemImpl);
    match expand(&block) {
        Ok(system_fn) => quote! { #block #system_fn }.into(),
        Err(e) => {
            let err = e.to_compile_error();
            quote! { #block #err }.into()
        }
    }
}

fn expand(block: &ItemImpl) -> syn::Result<TokenStream2> {
    let fsw = crate::fsw_crate();
    let name = type_name(&block.self_ty)?;
    let method = method(block)?;
    let is_async = method.sig.asyncness.is_some();
    let mut params = params(method)?;
    let stop = if is_async { Some(take_stop(method, &mut params)?) } else { None };
    let doc = doc(method);
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
    let is_stop = params
        .last()
        .is_some_and(|p| matches!(&p.key, Type::Path(path) if path.path.segments.last().is_some_and(|s| s.ident == "Stop")));
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

/// Joins the method's doc comment lines, each without its leading space.
fn doc(method: &ImplItemFn) -> String {
    method
        .attrs
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
