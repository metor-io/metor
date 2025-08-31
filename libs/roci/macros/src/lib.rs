use darling::FromField;
use proc_macro::TokenStream;
use proc_macro_crate::{FoundCrate, crate_name};
use proc_macro2::Span;
use quote::quote;
use syn::Ident;

mod as_vtable;
mod metadatatize;

#[derive(Debug, FromField)]
#[darling(attributes(roci))]
struct Field {
    ident: Option<syn::Ident>,
    ty: syn::Type,
    component_id: Option<String>,
}

impl Field {
    pub fn component_name(&self) -> String {
        match &self.component_id {
            Some(c) => c.clone(),
            None => {
                let ident = self.ident.as_ref().expect("field must have ident");
                ident.to_string()
            }
        }
    }
}

#[proc_macro_derive(Metadatatize, attributes(roci))]
pub fn metadatize(input: TokenStream) -> TokenStream {
    metadatatize::metadatatize(input)
}

#[proc_macro_derive(AsVTable, attributes(roci))]
pub fn as_vtable(input: TokenStream) -> TokenStream {
    as_vtable::as_vtable(input)
}

pub(crate) fn roci_crate_name() -> proc_macro2::TokenStream {
    let name = crate_name("roci").expect("roci is present in `Cargo.toml`");

    match name {
        FoundCrate::Itself => quote!(crate),
        FoundCrate::Name(name) => {
            let ident = Ident::new(&name, Span::call_site());
            quote!( #ident )
        }
    }
}
