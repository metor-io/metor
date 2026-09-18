//! Derives for port bundles: `#[derive(SystemInputs)]` and
//! `#[derive(SystemOutputs)]`.
//!
//! A bundle is a named struct of `Input<F>` (or `Output<F>`) fields. `defs`
//! emits one `PortDef` per field and `bind` consumes one entry per field, both
//! in declaration order, which is what makes positional binding work.

use darling::FromDeriveInput;
use darling::ast;
use darling::util::Ignored;
use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{DeriveInput, Generics, Ident};

#[derive(darling::FromField)]
struct BundleField {
    ident: Option<Ident>,
    ty: syn::Type,
}

#[derive(FromDeriveInput)]
#[darling(supports(struct_named))]
struct Bundle {
    ident: Ident,
    generics: Generics,
    data: ast::Data<Ignored, BundleField>,
}

impl Bundle {
    fn fields(&self) -> Vec<&BundleField> {
        // PANIC Safety: `supports(struct_named)` rejects every other shape.
        self.data
            .as_ref()
            .take_struct()
            .expect("named struct")
            .into_iter()
            .collect()
    }
}

/// `defs` pushes each field's frame id and size, in declaration order.
fn defs_body(bundle: &Bundle) -> TokenStream2 {
    let pushes = bundle.fields().into_iter().map(|f| {
        // PANIC Safety: a named struct's fields all have idents.
        let name = f.ident.as_ref().expect("named field").to_string();
        let ty = &f.ty;
        quote! { defs.push(<#ty>::def(#name)); }
    });
    quote! {
        let mut defs = Vec::new();
        #(#pushes)*
        defs
    }
}

/// `bind` takes one entry per field, in declaration order.
fn bind_body(bundle: &Bundle, arg: &Ident, dir: Dir) -> TokenStream2 {
    let ident = &bundle.ident;
    let name = ident.to_string();
    let fields = bundle.fields();
    let count = fields.len();
    let binds = fields.into_iter().map(|f| {
        // PANIC Safety: a named struct's fields all have idents.
        let id = f.ident.as_ref().expect("named field");
        let ty = &f.ty;
        let handles = dir.handles();
        // PANIC Safety: length checked above; the coordinator validates frame alignment.
        quote! {
            #id: <#ty>::try_new(#arg.next().expect("checked length").#handles)
                .expect("unsupported frame alignment"),
        }
    });
    quote! {
        // PANIC Safety: the coordinator binds one entry per `defs()` entry;
        // a mismatch is a coordinator bug, never a config error.
        assert_eq!(
            #arg.len(),
            #count,
            concat!("bind list length does not match ", #name, "::defs()")
        );
        let mut #arg = #arg.into_iter();
        #ident { #(#binds)* }
    }
}

pub fn system_inputs(input: TokenStream) -> TokenStream {
    let parsed = syn::parse_macro_input!(input as DeriveInput);
    expand(parsed, Dir::Inputs).into()
}

pub fn system_outputs(input: TokenStream) -> TokenStream {
    let parsed = syn::parse_macro_input!(input as DeriveInput);
    expand(parsed, Dir::Outputs).into()
}

#[derive(Clone, Copy)]
enum Dir {
    Inputs,
    Outputs,
}

impl Dir {
    /// The binding field holding this direction's ring handles.
    fn handles(self) -> TokenStream2 {
        match self {
            Dir::Inputs => quote!(views),
            Dir::Outputs => quote!(writer),
        }
    }
}

fn expand(parsed: DeriveInput, dir: Dir) -> TokenStream2 {
    let bundle = match Bundle::from_derive_input(&parsed) {
        Ok(b) => b,
        Err(e) => return e.write_errors(),
    };
    let fsw = crate::fsw_crate();
    let ident = &bundle.ident;
    let (impl_generics, ty_generics, where_clause) = bundle.generics.split_for_impl();
    let defs = defs_body(&bundle);
    let arg = Ident::new("bound", proc_macro2::Span::call_site());
    let bind = bind_body(&bundle, &arg, dir);
    let (trait_name, arg_ty) = match dir {
        Dir::Inputs => (
            quote!(SystemInputs),
            quote!(Vec<#fsw::system::InputBinding>),
        ),
        Dir::Outputs => (
            quote!(SystemOutputs),
            quote!(Vec<#fsw::system::OutputBinding>),
        ),
    };
    quote! {
        impl #impl_generics #fsw::#trait_name for #ident #ty_generics #where_clause {
            fn defs() -> Vec<#fsw::PortDef> {
                #defs
            }
            fn bind(#arg: #arg_ty) -> Self {
                #bind
            }
        }
    }
}
