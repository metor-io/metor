use darling::FromDeriveInput;
use darling::ast::{self};
use proc_macro::TokenStream;
use quote::quote;
use syn::{DeriveInput, Generics, Ident, parse_macro_input};

#[derive(Debug, FromDeriveInput)]
#[darling(attributes(roci), supports(struct_named))]
pub struct AsVTable {
    ident: Ident,
    generics: Generics,
    data: ast::Data<(), crate::Field>,
    parent: Option<String>,
}

pub fn as_vtable(input: TokenStream) -> TokenStream {
    let crate_name = crate::roci_crate_name();
    let input = parse_macro_input!(input as DeriveInput);
    let AsVTable {
        ident,
        generics,
        data,
        parent,
    } = AsVTable::from_derive_input(&input).unwrap();
    let where_clause = &generics.where_clause;
    let impeller = quote! { #crate_name::impeller2 };
    let fields = data.take_struct().unwrap();
    let vtable_items = fields.fields.iter().map(|field| {
        let ty = &field.ty;
        let name = field.component_name();
        let name = if let Some(parent) = &parent {
            format!("{parent}.{name}")
        } else {
            name
        };
        let ident = &field.ident;
        if !field.nest {
            quote! {
                .chain({
                    let schema = <#ty as #impeller::component::Component>::schema();
                    assert_eq!(schema.size(), #impeller::vtable::builder::field_size!(Self, #ident), "to cast to a vtable each field must be the same size as the component");
                    [#impeller::vtable::builder::field!(
                        Self::#ident,
                        #impeller::vtable::builder::schema(
                            schema.prim_type(),
                            schema.dim(),
                            component(#name)
                        )
                    )]
                })
            }
        } else {
            quote! {
                .chain(<#ty as #crate_name::AsVTable>::vtable_fields(
                    if let Some(prefix) = &prefix {
                        Some(std::borrow::Cow::Owned(format!("{}.{}", prefix, #name)))
                    } else {
                        Some(std::borrow::Cow::Borrowed(#name))
                    }
                )
                    .map(|field| field.offset_by(core::mem::offset_of!(Self, #ident) as u16))
                )
            }
        }
    });
    quote! {
        impl #crate_name::AsVTable for #ident #generics #where_clause {
            fn vtable_fields(prefix: Option<std::borrow::Cow<'_, str>>) -> impl Iterator<Item = #impeller::vtable::builder::FieldBuilder> {
                let component = |name: &str| {
                    if let Some(prefix) = &prefix {
                        #impeller::vtable::builder::component(dbg!(format!("{}.{}", prefix, name).as_str()))
                    } else {
                        #impeller::vtable::builder::component(dbg!(name))
                    }
                };
                std::iter::empty()
                #(#vtable_items)*
            }
        }
    }
    .into()
}
