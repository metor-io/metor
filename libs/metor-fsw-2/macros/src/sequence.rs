//! Implementation of `#[sequence]`, which expands an `async fn` into a
//! complete sequence system. The expansion has three parts: the fn itself,
//! emitted verbatim; a hidden wrapper type implementing `SeqSystem`; and the
//! `fsw_*` C-ABI exports, which delegate to the `abi::run_seq_*` helpers so
//! the host can drive the sequence like any other cyclic system.
//!
//! The port set is read off the signature. Every `Input<T>` parameter becomes
//! an input port and every `Output<T>` an output port, in signature order.
//! The macro appends an implicit `SlotControlIn` input and the status output
//! tail, then emits a `descriptor()` and a `build()` that agree on that
//! order. The user ports move into the future when it is built; the control
//! and status ports stay in the wrapper state so the host can command the
//! sequence and read its status on every poll.
//!
//! Rings are backing-erased, so the fn needs no injected generics and no
//! port-type rewriting. The port types named in the signature are exactly the
//! ones `descriptor()` and `build()` use.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::{FnArg, ItemFn, Lit, Meta, Pat, Token, Type, parse_macro_input};

use crate::sig;

/// One `Input<T>` or `Output<T>` parameter, reduced to its binding ident and
/// the element type `T`. This is all the expansion needs to emit the port's
/// descriptor and bind call; see [`Param`] for the other parameter kinds.
struct Port {
    ident: syn::Ident,
    elem: Type,
}

/// The role one fn parameter plays in the expansion, decided by the last
/// path segment of its type. Port parameters carry a [`Port`].
enum Param {
    /// A user input port, `Input<T>`.
    Input(Port),
    /// A user output port, `Output<T>`.
    Output(Port),
    /// The opt-in `Seq` handle.
    Seq(syn::Ident),
    /// The at-most-one params parameter. Only its type is used; the build
    /// argument is always named `params`.
    Params { ty: Type },
}

/// The binding ident of a `pat: ty` parameter (ignoring any `mut`).
fn pat_ident(pat: &Pat) -> Option<syn::Ident> {
    match pat {
        Pat::Ident(p) => Some(p.ident.clone()),
        _ => None,
    }
}

/// Parse the optional `name = "…"` attribute argument, defaulting to the fn
/// ident's string.
fn parse_name(attr: TokenStream, default: &syn::Ident) -> Result<String, syn::Error> {
    if attr.is_empty() {
        return Ok(default.to_string());
    }
    let metas = Punctuated::<Meta, Token![,]>::parse_terminated.parse(attr)?;
    for meta in metas {
        if let Meta::NameValue(nv) = meta
            && nv.path.is_ident("name")
        {
            if let syn::Expr::Lit(syn::ExprLit {
                lit: Lit::Str(s), ..
            }) = nv.value
            {
                return Ok(s.value());
            }
            return Err(syn::Error::new_spanned(
                nv.value,
                "`name` must be a string literal",
            ));
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

    // Rejecting generics here gives a clear error instead of an
    // unresolved-trait cascade from the generated impl.
    if !func.sig.generics.params.is_empty() {
        return syn::Error::new_spanned(
            &func.sig.generics,
            "#[sequence] fns take no generic parameters (rings are backing-erased)",
        )
        .to_compile_error()
        .into();
    }

    // Classify every parameter by the last segment of its type.
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
                    return syn::Error::new_spanned(&pt.ty, msg)
                        .to_compile_error()
                        .into();
                };
                let args = sig::type_args(&pt.ty);
                if args.len() > 1 {
                    return syn::Error::new_spanned(
                        args[1],
                        "#[sequence] ports take a single element type (the wake \
                         endpoints are macro-supplied); drop the second type parameter",
                    )
                    .to_compile_error()
                    .into();
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
                params.push(Param::Params {
                    ty: (*pt.ty).clone(),
                });
            }
        }
    }

    // Split the classified parameters into user ports (in signature order
    // within each kind), the optional params type, and the optional Seq handle.
    let inputs: Vec<&Port> = params
        .iter()
        .filter_map(|p| {
            if let Param::Input(p) = p {
                Some(p)
            } else {
                None
            }
        })
        .collect();
    let outputs: Vec<&Port> = params
        .iter()
        .filter_map(|p| {
            if let Param::Output(p) = p {
                Some(p)
            } else {
                None
            }
        })
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

    // ---- descriptor(): user ports first, then the control input and the
    // status output tail.
    let input_descs = inputs.iter().map(|p| {
        let elem = &p.elem;
        quote! { <#fsw2::Input<#elem>>::descriptor() }
    });
    let output_descs = outputs.iter().map(|p| {
        let elem = &p.elem;
        quote! { <#fsw2::Output<#elem>>::descriptor() }
    });

    // ---- build(): bind in descriptor order; user ports move into the future.
    let bind_inputs = inputs.iter().map(|p| {
        let id = &p.ident;
        let elem = &p.elem;
        quote! { let #id = <#fsw2::Input<#elem>>::bind(binder); }
    });
    let bind_outputs = outputs.iter().map(|p| {
        let id = &p.ident;
        let elem = &p.elem;
        quote! { let #id = <#fsw2::Output<#elem>>::bind(binder); }
    });
    let bind_seq = seq_ident.map(|id| {
        quote! { let #id = #fsw2::sequence::Seq::new(clock.clone()); }
    });

    // The future's arguments, in original signature order. A params parameter
    // maps to the build argument, which is always named `params`.
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
        // No params parameter, so name the unused trait argument `_params`.
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
                inputs.push(<#fsw2::Input<#fsw2::sequence::SlotControlIn>>::descriptor());
                let mut outputs = ::std::vec![ #(#output_descs,)* ];
                outputs.extend(
                    <#fsw2::Out<#fsw2::sequence::SeqStatusOut> as #fsw2::SystemOutput>::port_descs(),
                );
                #fsw2::SystemDescriptor {
                    name: #name,
                    kind: #fsw2::SystemKind::Cyclic,
                    inputs,
                    outputs,
                    // A sequence declares wired ports only, no host capabilities.
                    capabilities: ::std::vec::Vec::new(),
                }
            }

            fn build(
                #build_params_arg,
                binder: &mut #fsw2::abi::RawBinder,
                clock: &::std::rc::Rc<#fsw2::sequence::SeqClock>,
            ) -> #fsw2::sequence::SeqBound {
                #(#bind_inputs)*
                let __control = <#fsw2::Input<#fsw2::sequence::SlotControlIn>>::bind(binder);
                #(#bind_outputs)*
                let __status = <#fsw2::Out<#fsw2::sequence::SeqStatusOut> as #fsw2::BindPorts>::bind(binder);
                #bind_seq
                let __fut: ::core::pin::Pin<::std::boxed::Box<dyn ::core::future::Future<Output = #fsw2::sequence::Outcome>>> =
                    ::std::boxed::Box::pin(#fn_ident( #(#call_args),* ));
                #fsw2::sequence::SeqBound {
                    future: __fut,
                    status: __status,
                    control: __control,
                }
            }
        }
    };

    quote! {
        #func
        #impl_block
    }
    .into()
}
