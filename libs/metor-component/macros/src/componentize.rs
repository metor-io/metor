use darling::FromDeriveInput;
use darling::ast;
use metor_component_derive_impl::{Field, StructInput};
use proc_macro::TokenStream;
use syn::{DeriveInput, Generics, Ident};

#[derive(Debug, FromDeriveInput)]
#[darling(attributes(fsw, metor_fsw), supports(struct_named))]
struct Componentize {
    ident: Ident,
    generics: Generics,
    data: ast::Data<(), Field>,
    parent: Option<String>,
    name: Option<String>,
}

/// `#[derive(Componentize)]`.
pub fn componentize(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as DeriveInput);
    let Componentize {
        ident,
        generics,
        data,
        parent,
        name,
    } = Componentize::from_derive_input(&input).unwrap();
    let parent = parent.or(name);
    let fields = data.take_struct().unwrap().fields;
    let model = StructInput {
        ident,
        generics,
        fields,
        parent,
        group: None,
    };
    metor_component_derive_impl::componentize_impl(&model, &crate::metor_component_crate_name())
        .into()
}
