use convert_case::{Case, Casing};
use darling::ast;
use darling::{FromDeriveInput, FromField};
use metor_component_derive_impl::{
    Field, Input, StructInput, as_vtable_impl, componentize_impl, decomponentize_impl,
    metadatatize_impl,
};
use proc_macro::TokenStream;
use quote::quote;
use syn::{DeriveInput, Generics, Ident, parse_macro_input};

/// One field of the frame struct, read from `#[frame(..)]` attributes.
///
/// It converts into the shared derive-impl `Field`, which drives the four
/// component derives.
#[derive(FromField)]
#[darling(attributes(frame))]
struct FrameField {
    ident: Option<Ident>,
    ty: syn::Type,
    component_id: Option<String>,
    #[darling(default)]
    timestamp: bool,
    #[darling(default)]
    nest: bool,
    #[darling(default)]
    skip: Option<bool>,
}

impl From<FrameField> for Field {
    fn from(f: FrameField) -> Self {
        Field {
            ident: f.ident,
            ty: f.ty,
            component_id: f.component_id,
            timestamp: f.timestamp,
            nest: f.nest,
            max: None,
            skip: f.skip,
        }
    }
}

/// The struct being derived, with its fields, `#[frame(name = "..")]`, and
/// `#[frame(depth = N)]`.
#[derive(FromDeriveInput)]
#[darling(attributes(frame), supports(struct_named))]
struct FrameInput {
    ident: Ident,
    generics: Generics,
    data: ast::Data<(), FrameField>,
    name: Option<String>,
    depth: Option<usize>,
}

/// Expands `#[derive(Frame)]`.
///
/// The output implements the four component traits, `Record`, and `Frame`.
pub fn frame(input: TokenStream) -> TokenStream {
    let parsed = parse_macro_input!(input as DeriveInput);
    let raw = match FrameInput::from_derive_input(&parsed) {
        Ok(raw) => raw,
        Err(e) => return e.write_errors().into(),
    };
    // PANIC Safety: darling's `supports(struct_named)` rejects every other
    // shape before this point.
    let fields = raw
        .data
        .take_struct()
        .expect("named struct")
        .fields
        .into_iter()
        .map(Field::from)
        .collect::<Vec<_>>();

    let fsw = crate::fsw_crate();
    let proto = quote! { #fsw::metor_proto };
    let frame_name = raw
        .name
        .unwrap_or_else(|| raw.ident.to_string().to_case(Case::Snake));
    let frame_id = quote! { #proto::types::ComponentId::new(#frame_name) };
    let depth = raw
        .depth
        .map(|depth| quote! { const DEPTH: usize = #depth; });

    let timestamp_body = match fields.iter().find(|f| f.timestamp) {
        Some(f) => {
            // PANIC Safety: a named struct's fields all have idents.
            let id = f.ident.as_ref().expect("named field");
            quote! { self.#id }
        }
        None => {
            return syn::Error::new_spanned(
                &raw.ident,
                "#[derive(Frame)] requires a #[frame(timestamp)] field",
            )
            .to_compile_error()
            .into();
        }
    };

    let (ident, generics) = (raw.ident.clone(), raw.generics.clone());
    let struct_input = StructInput {
        ident: raw.ident,
        generics: raw.generics,
        fields,
        parent: None,
        group: None,
    };
    let componentize = componentize_impl(&struct_input, &fsw);
    let decomponentize = decomponentize_impl(&struct_input, &fsw);
    let input = Input::Struct(struct_input);
    let as_vtable = as_vtable_impl(&input, Some(frame_id), &fsw);
    let metadatatize = metadatatize_impl(&input, &fsw);

    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    quote! {
        #as_vtable
        #metadatatize
        #componentize
        #decomponentize

        impl #impl_generics #fsw::Record for #ident #ty_generics #where_clause {
            const NAME: &'static str = #frame_name;
            const MAX_LEN: usize = ::core::mem::size_of::<Self>();
            const ALIGN: usize = ::core::mem::align_of::<Self>();
            #depth
            type Read<'a> = &'a Self where Self: 'a;
            fn encode<'a>(&'a self, _buf: &'a mut [u8])
                -> Result<&'a [u8], #fsw::EncodeError> {
                Ok(#fsw::zerocopy::IntoBytes::as_bytes(self))
            }
            fn decode(bytes: &[u8]) -> Result<&Self, #fsw::DecodeError> {
                #fsw::record::fixed::decode(bytes)
            }
            fn timestamp(&self) -> Option<#proto::types::Timestamp> {
                Some(#timestamp_body)
            }
            fn schema() -> #fsw::record::RecordSchema {
                #fsw::record::RecordSchema::frame::<Self>(Self::NAME)
            }
            fn schema_at(root: &str) -> #fsw::record::RecordSchema {
                #fsw::record::RecordSchema::frame::<Self>(root)
            }
        }

        impl #impl_generics #fsw::Frame for #ident #ty_generics #where_clause {}
    }
    .into()
}
