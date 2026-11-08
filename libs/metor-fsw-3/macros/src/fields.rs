//! `#[derive(Fields)]`, a group of components nested in frames.

use darling::util::Override;
use darling::{FromDeriveInput, ast};
use metor_component_derive_impl::{
    Field, Stamp, StructInput, struct_as_vtable_impl, struct_metadatatize_impl,
};
use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{DeriveInput, Generics, Ident, parse_macro_input};

/// The struct being derived, with `#[fsw(group)]`.
#[derive(FromDeriveInput)]
#[darling(attributes(fsw), supports(struct_named))]
struct FieldsInput {
    ident: Ident,
    generics: Generics,
    data: ast::Data<(), Field>,
    #[darling(default)]
    group: Option<Override<String>>,
}

/// Expands `#[derive(Fields)]`.
pub fn fields(input: TokenStream) -> TokenStream {
    let parsed = parse_macro_input!(input as DeriveInput);
    let raw = match FieldsInput::from_derive_input(&parsed) {
        Ok(raw) => raw,
        Err(e) => return e.write_errors().into(),
    };
    // PANIC Safety: darling's `supports(struct_named)` rejects every other
    // shape before this point.
    let fields = raw.data.take_struct().expect("named struct").fields;
    if let Some(stamp) = fields.iter().find(|f| f.timestamp) {
        let message = "#[fsw(timestamp)] is only for #[derive(Frame)]";
        return syn::Error::new_spanned(&stamp.ident, message)
            .to_compile_error()
            .into();
    }
    let fsw = match crate::fsw_crate() {
        Ok(fsw) => fsw,
        Err(e) => return e.to_compile_error().into(),
    };
    let input = StructInput {
        ident: raw.ident,
        generics: raw.generics,
        fields,
        parent: None,
        group: raw.group,
    };
    fields_impl(&input, &fsw).into()
}

/// The `AsVTable` and `Metadatatize` impls for a struct of components.
///
/// A `timestamp` field is left out of both.
pub(crate) fn fields_impl(input: &StructInput, fsw: &TokenStream2) -> TokenStream2 {
    if let Some(field) = duplicate_segment(&input.fields) {
        let message = format!("two fields are named `{}`", field.component_name());
        return syn::Error::new_spanned(&field.ident, message).to_compile_error();
    }
    let as_vtable = struct_as_vtable_impl(input, Stamp::Omit, fsw);
    let metadatatize = struct_metadatatize_impl(input, fsw);
    quote! {
        #as_vtable
        #metadatatize
    }
}

/// The first member whose segment an earlier member already uses.
fn duplicate_segment(fields: &[Field]) -> Option<&Field> {
    let mut seen = Vec::new();
    fields
        .iter()
        .filter(|f| f.is_member())
        .filter_map(|f| f.segment(None).map(|name| (f, name)))
        .find_map(|(f, name)| match seen.contains(&name) {
            true => Some(f),
            false => {
                seen.push(name);
                None
            }
        })
}
