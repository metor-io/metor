use darling::FromDeriveInput;
use darling::ast;
use proc_macro::TokenStream;
use quote::quote;
use syn::{Attribute, Meta};
use syn::{DeriveInput, Generics, Ident, parse_macro_input};

#[derive(Debug, FromDeriveInput)]
#[darling(
    attributes(metor_fsw),
    supports(struct_named, enum_unit),
    forward_attrs(allow, doc, cfg)
)]
pub struct AsVTable {
    ident: Ident,
    generics: Generics,
    data: ast::Data<(), crate::Field>,
    parent: Option<String>,
    #[darling(default, rename = "group")]
    _group: darling::util::Ignored,
}

fn extract_repr_type(attrs: &[Attribute]) -> Option<Ident> {
    for attr in attrs {
        if attr.path().is_ident("repr") {
            if let Meta::List(meta_list) = attr.meta.clone() {
                for token in meta_list.tokens {
                    if let Ok(ident) = syn::parse2::<Ident>(token.into()) {
                        return Some(ident);
                    }
                }
            }
        }
    }
    None
}

pub fn as_vtable(input: TokenStream) -> TokenStream {
    let crate_name = crate::metor_fsw_crate_name();
    let input = parse_macro_input!(input as DeriveInput);
    let AsVTable {
        ident,
        generics,
        data,
        parent,
        _group,
    } = AsVTable::from_derive_input(&input).unwrap();
    let where_clause = &generics.where_clause;
    let impeller = quote! { #crate_name::metor_proto };
    match data {
        ast::Data::Enum(_) => {
            let name = parent.unwrap_or_else(|| ident.to_string());
            let Some(repr_type) = extract_repr_type(&input.attrs) else {
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
            }.into()
        }
        ast::Data::Struct(fields) => {
            let mut timestamp_fields = fields.fields.iter().filter(|field| field.timestamp);
            let timestamp_field = timestamp_fields.next();
            if timestamp_fields.next().is_some() {
                panic!("only one field can be marked #[metor_fsw(timestamp)]");
            }
            let timestamp_source = timestamp_field.map(|field| {
                let ident = &field.ident;
                let ty = &field.ty;
                quote! {
                    let timestamp_source = #impeller::vtable::builder::raw_table(
                        core::mem::offset_of!(Self, #ident) as u32,
                        core::mem::size_of::<#ty>() as u32,
                    );
                }
            });
            let timestamp_map = timestamp_field.map(|_| {
                quote! {
                    .map(move |field| field.with_timestamp(timestamp_source.clone()))
                }
            });
            let vtable_items = fields.fields.iter().map(|field| {
                let ty = &field.ty;
                let name = field.component_name();
                let name = if let Some(parent) = &parent {
                    format!("{parent}.{name}")
                } else {
                    name
                };
                let ident = &field.ident;
                quote! {
                    .chain(<#ty as #crate_name::AsVTable>::vtable_fields(path.chain(#name))
                        .map(|field| field.offset_by(core::mem::offset_of!(Self, #ident) as u32))
                    )
                }
            });
            quote! {
        impl #crate_name::AsVTable for #ident #generics #where_clause {
            fn vtable_fields(path: impl #crate_name::path::ComponentPath) -> impl Iterator<Item = #impeller::vtable::builder::FieldBuilder> {
                use #crate_name::path::ComponentPath;
                let component = |name: &str| #impeller::vtable::builder::component(path.chain(name).to_component_id());
                #timestamp_source
                std::iter::empty()
                #(#vtable_items)*
                #timestamp_map
            }
        }
    }
    .into()
        }
    }
}
