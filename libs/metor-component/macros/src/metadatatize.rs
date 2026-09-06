use darling::FromDeriveInput;
use darling::ast;
use darling::util::Override;
use metor_component_derive_impl::{EnumInput, Field, Input, StructInput};
use proc_macro::TokenStream;
use syn::{DeriveInput, Generics, Ident, parse_macro_input};

#[derive(Debug, FromDeriveInput)]
#[darling(attributes(fsw, metor_fsw), supports(struct_named, enum_unit))]
struct Metadatatize {
    ident: Ident,
    generics: Generics,
    data: ast::Data<Ident, Field>,
    parent: Option<String>,
    name: Option<String>,
    /// `#[metor_fsw(group)]` emits a metadata-only parent entry with
    /// `group_name = <Ident>`. `#[metor_fsw(group = "Custom")]` overrides.
    #[darling(default)]
    group: Option<Override<String>>,
}

/// `#[derive(Metadatatize)]`.
pub fn metadatatize(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let Metadatatize {
        ident,
        generics,
        data,
        parent,
        name,
        group,
    } = Metadatatize::from_derive_input(&input).unwrap();
    let parent = parent.or(name);
    let model = match data {
        ast::Data::Enum(variants) => Input::Enum(EnumInput {
            ident,
            generics,
            parent,
            variants,
            repr_type: None,
        }),
        ast::Data::Struct(fields) => Input::Struct(StructInput {
            ident,
            generics,
            fields: fields.fields,
            parent,
            group,
        }),
    };
    metor_component_derive_impl::metadatatize_impl(&model, &crate::metor_component_crate_name())
        .into()
}
