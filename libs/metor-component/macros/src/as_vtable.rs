use darling::FromDeriveInput;
use darling::ast;
use metor_component_derive_impl::{EnumInput, Field, Input, StructInput};
use proc_macro::TokenStream;
use syn::{Attribute, DeriveInput, Generics, Ident, Meta};

#[derive(Debug, FromDeriveInput)]
#[darling(
    attributes(fsw, metor_fsw),
    supports(struct_named, enum_unit),
    forward_attrs(allow, doc, cfg)
)]
struct AsVTable {
    ident: Ident,
    generics: Generics,
    data: ast::Data<(), Field>,
    parent: Option<String>,
    /// Frame name (frames.md §1.3); folds into `parent` as the component prefix.
    name: Option<String>,
    #[darling(default, rename = "group")]
    _group: darling::util::Ignored,
}

fn extract_repr_type(attrs: &[Attribute]) -> Option<Ident> {
    for attr in attrs {
        if attr.path().is_ident("repr")
            && let Meta::List(meta_list) = attr.meta.clone()
        {
            for token in meta_list.tokens {
                if let Ok(ident) = syn::parse2::<Ident>(token.into()) {
                    return Some(ident);
                }
            }
        }
    }
    None
}

/// `#[derive(AsVTable)]`: the standalone form, always expanded with no frame
/// id (see [`as_vtable_impl`](metor_component_derive_impl::as_vtable_impl)).
pub fn as_vtable(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as DeriveInput);
    let AsVTable {
        ident,
        generics,
        data,
        parent,
        name,
        _group,
    } = AsVTable::from_derive_input(&input).unwrap();
    let parent = parent.or(name);
    let model = match data {
        ast::Data::Enum(_) => Input::Enum(EnumInput {
            repr_type: extract_repr_type(&input.attrs),
            ident,
            generics,
            parent,
            variants: Vec::new(),
        }),
        ast::Data::Struct(fields) => Input::Struct(StructInput {
            ident,
            generics,
            fields: fields.fields,
            parent,
            group: None,
        }),
    };
    metor_component_derive_impl::as_vtable_impl(&model, None, &crate::metor_component_crate_name())
        .into()
}
