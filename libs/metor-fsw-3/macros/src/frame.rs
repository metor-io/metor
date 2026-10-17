use convert_case::{Case, Casing};
use darling::FromDeriveInput;
use darling::ast;
use metor_component_derive_impl::{
    Field, StructInput, componentize_impl, decomponentize_impl, struct_as_vtable_impl,
    struct_metadatatize_impl,
};
use proc_macro::TokenStream;
use quote::quote;
use syn::{DeriveInput, Generics, Ident, parse_macro_input};

/// The struct being derived, with its fields, `#[frame(name = "..")]`, and
/// `#[frame(depth = N)]`.
#[derive(FromDeriveInput)]
#[darling(attributes(frame), supports(struct_named))]
struct FrameInput {
    ident: Ident,
    generics: Generics,
    data: ast::Data<(), Field>,
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
    let fields = raw.data.take_struct().expect("named struct").fields;

    let fsw = crate::fsw_crate();
    let proto = quote! { #fsw::metor_proto };
    let frame_name = raw
        .name
        .unwrap_or_else(|| raw.ident.to_string().to_case(Case::Snake));
    let frame_id = quote! { #proto::types::ComponentId::new(#frame_name) };
    let depth = raw
        .depth
        .map(|depth| quote! { const DEPTH: usize = #depth; });

    let mut stamps = fields.iter().filter(|f| f.timestamp);
    let Some(stamp) = stamps.next() else {
        let message = "#[derive(Frame)] requires a #[frame(timestamp)] field";
        return syn::Error::new_spanned(&raw.ident, message)
            .to_compile_error()
            .into();
    };
    if let Some(extra) = stamps.next() {
        let message = "only one field can be marked #[frame(timestamp)]";
        return syn::Error::new_spanned(&extra.ident, message)
            .to_compile_error()
            .into();
    }
    // PANIC Safety: a named struct's fields all have idents.
    let stamp = stamp.ident.as_ref().expect("named field");
    let timestamp_body = quote! { self.#stamp };

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
    let as_vtable = struct_as_vtable_impl(&struct_input, Some(frame_id), &fsw);
    let metadatatize = struct_metadatatize_impl(&struct_input, &fsw);

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
            fn encode<'a>(&'a self, _buf: &'a mut [u8]) -> Result<&'a [u8], #fsw::EncodeError> {
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
            fn schema_with_root(root: &str) -> #fsw::record::RecordSchema {
                #fsw::record::RecordSchema::frame::<Self>(root)
            }
        }

        impl #impl_generics #fsw::Frame for #ident #ty_generics #where_clause {}
    }
    .into()
}
