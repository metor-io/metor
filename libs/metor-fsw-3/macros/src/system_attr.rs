//! `#[system]` on an impl block: emits `SystemFn` for the type from its `execute` method.
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
    let execute = execute_method(block)?;
    let params = params(execute)?;
    let doc = doc(execute);
    let idents = params.iter().map(|p| &p.ident);
    let keys = params.iter().map(|p| &p.key);
    let names = params.iter().map(|p| p.ident.to_string());
    let self_ty = &block.self_ty;
    let (impl_generics, _, where_clause) = block.generics.split_for_impl();
    let idents2 = idents.clone();
    // Spanned at each parameter type, so a type that is no `Param` errors there.
    let asserts = params.iter().map(|p| {
        let key = &p.key;
        quote_spanned! { key.span() => #fsw::fn_system::assert_param::<#key>(); }
    });
    Ok(quote! {
        impl #impl_generics #fsw::SystemFn for #self_ty #where_clause {
            type Params = (#(#keys,)*);
            const NAME: &'static str = #name;
            const NAMES: &'static [&'static str] = &[#(#names),*];
            const DOC: &'static str = #doc;
            fn call(&mut self, (#(#idents,)*): <Self::Params as #fsw::Param>::Item<'_>) {
                #(#asserts)*
                self.execute(#(#idents2),*)
            }
        }
    })
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

/// Joins `execute`'s doc comment lines, each without its leading space.
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

/// Finds `execute` and checks it takes `&mut self`.
fn execute_method(block: &ItemImpl) -> syn::Result<&ImplItemFn> {
    let found = block.items.iter().find_map(|item| match item {
        ImplItem::Fn(f) if f.sig.ident == "execute" => Some(f),
        _ => None,
    });
    let Some(method) = found else {
        return Err(syn::Error::new(
            block.self_ty.span(),
            "#[system] needs an `execute` method in this impl block",
        ));
    };
    match method.sig.inputs.first() {
        Some(FnArg::Receiver(recv)) if recv.reference.is_some() && recv.mutability.is_some() => {
            Ok(method)
        }
        Some(arg) => Err(syn::Error::new(arg.span(), "`execute` takes `&mut self`")),
        None => Err(syn::Error::new(
            method.sig.span(),
            "`execute` takes `&mut self`",
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
