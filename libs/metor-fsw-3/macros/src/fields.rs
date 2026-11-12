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
/// A `timestamp` field is left out of both. A misused attribute adds a
/// compile error beside the impls, so the error is the only one reported.
pub(crate) fn fields_impl(input: &StructInput, fsw: &TokenStream2) -> TokenStream2 {
    let error = check(&input.fields).err().map(|e| e.to_compile_error());
    let as_vtable = struct_as_vtable_impl(input, Stamp::Omit, fsw);
    let metadatatize = struct_metadatatize_impl(input, fsw);
    quote! {
        #error
        #as_vtable
        #metadatatize
    }
}

/// Refuses `nest`, a flattened scalar or array, and two members of one name.
fn check(fields: &[Field]) -> syn::Result<()> {
    let error = |tokens: &dyn quote::ToTokens, message: String| {
        Err(syn::Error::new_spanned(tokens, message))
    };
    if let Some(field) = fields.iter().find(|f| f.nest) {
        let message = "`nest` is not an attribute; derive `Fields` on the field's type instead";
        return error(&field.ident, message.into());
    }
    if let Some(field) = flattened_leaf(fields) {
        let message = "`flatten` goes on a field holding a `Fields` struct";
        return error(&field.ty, message.into());
    }
    match duplicate_segment(fields) {
        Some(field) => error(
            &field.ident,
            format!("two fields are named `{}`", field.component_name()),
        ),
        None => Ok(()),
    }
}

/// Scalar types, which hold one component and so cannot be flattened.
const SCALARS: [&str; 14] = [
    "bool", "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128", "f32",
    "f64",
];

/// The first flattened member whose type is a scalar or an array.
fn flattened_leaf(fields: &[Field]) -> Option<&Field> {
    fields
        .iter()
        .filter(|f| f.is_member() && f.flatten)
        .find(|f| match &f.ty {
            syn::Type::Array(_) => true,
            syn::Type::Path(path) => path
                .path
                .get_ident()
                .is_some_and(|i| SCALARS.iter().any(|s| i == s)),
            _ => false,
        })
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
