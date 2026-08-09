//! `#[derive(ParamsDocs)]`: capture a `Params` struct's field doc comments.
//!
//! Each documented field contributes a `(name, doc)` pair to a
//! [`ParamsDocEntry`](metor_fsw_2::ParamsDocEntry) submitted into the
//! crate-local `inventory` collection, keyed by the struct name (which is its
//! postcard-schema name). The pack's own `.so` reads these back when it builds
//! its manifest, so pack module generation can render the units and prose. Docs
//! are optional: a field with no doc comment contributes nothing, and a params
//! type that skips the derive simply has no entry.

use proc_macro::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Expr, Fields, Lit, Meta, parse_macro_input};

/// Expands `#[derive(ParamsDocs)]` on a named struct.
pub fn params_docs(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let ident = &input.ident;
    let fsw2 = crate::metor_fsw_2_crate_name();

    let fields = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(named) => &named.named,
            // A tuple or unit params struct has no named fields to document.
            _ => {
                return submit(&fsw2, ident, Vec::new()).into();
            }
        },
        _ => {
            return syn::Error::new_spanned(
                ident,
                "#[derive(ParamsDocs)] applies to a struct with named fields",
            )
            .to_compile_error()
            .into();
        }
    };

    let pairs = fields
        .iter()
        .filter_map(|field| {
            let name = field.ident.as_ref()?.to_string();
            let doc = doc_text(&field.attrs)?;
            Some((name, doc))
        })
        .collect();

    submit(&fsw2, ident, pairs).into()
}

/// The submitted `inventory` entry for this params type.
fn submit(
    fsw2: &proc_macro2::TokenStream,
    ident: &syn::Ident,
    pairs: Vec<(String, String)>,
) -> proc_macro2::TokenStream {
    let schema = ident.to_string();
    let entries = pairs.iter().map(|(name, doc)| quote!((#name, #doc)));
    quote! {
        #fsw2::inventory::submit! {
            #fsw2::ParamsDocEntry {
                schema: #schema,
                fields: &[ #(#entries),* ],
            }
        }
    }
}

/// The joined text of a field's `#[doc]` comments, trimmed to one logical line,
/// or `None` when the field carries none.
fn doc_text(attrs: &[syn::Attribute]) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    for attr in attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        if let Meta::NameValue(nv) = &attr.meta
            && let Expr::Lit(expr) = &nv.value
            && let Lit::Str(s) = &expr.lit
        {
            lines.push(s.value().trim().to_string());
        }
    }
    let joined = lines.join(" ").split_whitespace().collect::<Vec<_>>().join(" ");
    (!joined.is_empty()).then_some(joined)
}
