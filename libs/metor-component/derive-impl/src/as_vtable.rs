use proc_macro2::TokenStream as TokenStream2;
use quote::quote;

use crate::Field;
use crate::input::{EnumInput, Input, StructInput};

/// What a struct's `timestamp` field does in its vtable.
///
/// The field never becomes a component of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stamp {
    /// Every other field reads its time from the marked field.
    Attach,
    /// The field is left out; the caller attaches a time.
    Omit,
}

/// Generates an `AsVTable` impl.
///
/// `crate_name` is the path prefix the generated code uses to reach the
/// framework's re-exports.
pub fn as_vtable_impl(input: &Input, stamp: Stamp, crate_name: &TokenStream2) -> TokenStream2 {
    match input {
        Input::Enum(input) => enum_as_vtable_impl(input, crate_name),
        Input::Struct(input) => struct_as_vtable_impl(input, stamp, crate_name),
    }
}

/// Generates an `AsVTable` impl for an enum. See [`as_vtable_impl`].
pub fn enum_as_vtable_impl(input: &EnumInput, crate_name: &TokenStream2) -> TokenStream2 {
    let impeller = quote! { #crate_name::metor_proto };
    let EnumInput {
        ident,
        generics,
        parent,
        repr_type,
        ..
    } = input;
    let where_clause = &generics.where_clause;
    let name = parent.clone().unwrap_or_else(|| ident.to_string());
    let Some(repr_type) = repr_type else {
        panic!("repr required for enum derive");
    };
    quote! {
        impl #crate_name::AsVTable for #ident #generics #where_clause {
            fn vtable_fields(path: impl #crate_name::path::ComponentPath) -> impl Iterator<Item = #impeller::vtable::builder::FieldBuilder> {
                let component = if path.is_empty() {
                    #impeller::vtable::builder::component(#name)
                } else {
                    #impeller::vtable::builder::component(path.to_component_id())
                };
                [
                    #impeller::vtable::builder::raw_field(0, core::mem::size_of::<Self>() as u32, #impeller::vtable::builder::schema(
                        <#repr_type as #impeller::component::PrimTypeElem>::PRIM_TYPE,
                        &[],
                        component
                    ))
                ].into_iter()
            }
        }
    }
}

/// Generates an `AsVTable` impl for a struct. See [`as_vtable_impl`].
pub fn struct_as_vtable_impl(
    input: &StructInput,
    stamp: Stamp,
    crate_name: &TokenStream2,
) -> TokenStream2 {
    let impeller = quote! { #crate_name::metor_proto };
    let StructInput {
        ident,
        generics,
        fields,
        parent,
        ..
    } = input;
    let where_clause = &generics.where_clause;
    let (timestamp_source, timestamp_map) = match stamp {
        Stamp::Attach => match stamp_source(fields, &impeller) {
            Ok(source) => source,
            Err(e) => return e.to_compile_error(),
        },
        Stamp::Omit => (None, None),
    };
    let members = || fields.iter().filter(|f| f.is_member());
    let vtable_items = members().map(|field| {
        let ty = &field.ty;
        let ident = &field.ident;
        let path = match field.segment(parent.as_deref()) {
            Some(name) => quote! { path.chain(#name) },
            None => quote! { path.clone() },
        };
        quote! {
            .chain(<#ty as #crate_name::AsVTable>::vtable_fields(#path)
                .map(|field| field.offset_by(core::mem::offset_of!(Self, #ident) as u32))
            )
        }
    });
    // `element_fields` names members relative to a plain string prefix
    // instead of a component path, so a dynamic container can stamp out
    // copies under names chosen at runtime.
    let element_items = members().map(|field| {
        let ty = &field.ty;
        let ident = &field.ident;
        let prefix = match field.segment(None) {
            Some(name) => quote! { child(#name) },
            None => quote! { prefix.clone() },
        };
        quote! {
            .chain(<#ty as #crate_name::AsVTable>::element_fields(#prefix)
                .map(|field| field.offset_by(core::mem::offset_of!(Self, #ident) as u32))
            )
        }
    });
    let dynamic_items = members().map(|field| {
        let ty = &field.ty;
        quote! { || <#ty as #crate_name::AsVTable>::DYNAMIC }
    });
    let element_flags = members().map(|field| {
        let ty = &field.ty;
        quote! { && <#ty as #crate_name::AsVTable>::ELEMENT }
    });
    quote! {
        impl #crate_name::AsVTable for #ident #generics #where_clause {
            const DYNAMIC: bool = false #(#dynamic_items)*;
            const ELEMENT: bool = true #(#element_flags)*;

            fn vtable_fields(path: impl #crate_name::path::ComponentPath) -> impl Iterator<Item = #impeller::vtable::builder::FieldBuilder> {
                use #crate_name::path::ComponentPath;
                #timestamp_source
                std::iter::empty()
                #(#vtable_items)*
                #timestamp_map
            }

            fn element_fields(prefix: String) -> impl Iterator<Item = #impeller::vtable::builder::FieldBuilder> {
                let child = |name: &str| if prefix.is_empty() {
                    name.to_string()
                } else {
                    format!("{prefix}.{name}")
                };
                std::iter::empty()
                #(#element_items)*
            }
        }
    }
}

/// The statement that captures the `timestamp` field as a raw table, and the
/// map that attaches it to every member.
fn stamp_source(
    fields: &[Field],
    impeller: &TokenStream2,
) -> syn::Result<(Option<TokenStream2>, Option<TokenStream2>)> {
    let mut stamps = fields.iter().filter(|field| field.timestamp);
    let Some(field) = stamps.next() else {
        return Ok((None, None));
    };
    if let Some(extra) = stamps.next() {
        let message = "only one field can be marked #[fsw(timestamp)]";
        return Err(syn::Error::new_spanned(&extra.ident, message));
    }
    let ident = &field.ident;
    let ty = &field.ty;
    let source = quote! {
        let timestamp_source = #impeller::vtable::builder::raw_table(
            core::mem::offset_of!(Self, #ident) as u32,
            core::mem::size_of::<#ty>() as u32,
        );
    };
    let map = quote! {
        .map(move |field| field.with_timestamp(timestamp_source.clone()))
    };
    Ok((Some(source), Some(map)))
}
