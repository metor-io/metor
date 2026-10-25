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

/// The struct being derived, with its fields, `#[frame(name = "..")]`,
/// `#[frame(depth = N)]`, and `#[frame(max_len = N)]`.
#[derive(FromDeriveInput)]
#[darling(attributes(frame), supports(struct_named))]
struct FrameInput {
    ident: Ident,
    generics: Generics,
    data: ast::Data<(), Field>,
    name: Option<String>,
    depth: Option<usize>,
    max_len: Option<syn::Expr>,
}

/// Expands `#[derive(Frame)]`.
///
/// The output implements the four component traits, `Payload`, `Record`, and
/// `Frame`.
pub fn frame(input: TokenStream) -> TokenStream {
    let parsed = parse_macro_input!(input as DeriveInput);
    let raw = match FrameInput::from_derive_input(&parsed) {
        Ok(raw) => raw,
        Err(e) => return e.write_errors().into(),
    };
    // PANIC Safety: darling's `supports(struct_named)` rejects every other
    // shape before this point.
    let fields = raw.data.take_struct().expect("named struct").fields;

    let fsw = match crate::fsw_crate() {
        Ok(fsw) => fsw,
        Err(e) => return e.to_compile_error().into(),
    };
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

    // A fixed frame encodes as its own bytes and needs no scratch; one with a
    // FrameMap is built into the port's scratch by `Output::write_with`.
    let (read_ty, decode_body, max_len, scratch_len) = match &raw.max_len {
        Some(max_len) => (
            quote! { #fsw::FrameRef<'a, Self> },
            quote! { #fsw::FrameRef::decode(bytes) },
            quote! {{
                let max_len: usize = #max_len;
                assert!(
                    max_len >= ::core::mem::size_of::<Self>(),
                    "#[frame(max_len)] is smaller than the frame"
                );
                max_len
            }},
            quote! { Self::MAX_LEN },
        ),
        None => (
            quote! { &'a Self },
            quote! { #fsw::record::fixed::decode(bytes) },
            quote! {{
                assert!(
                    !<Self as #fsw::AsVTable>::DYNAMIC,
                    "a frame holding a FrameMap needs #[frame(max_len = ..)]"
                );
                ::core::mem::size_of::<Self>()
            }},
            quote! { 0 },
        ),
    };

    let guard = raw.generics.params.is_empty().then(|| {
        let ident = &raw.ident;
        quote! { const _: usize = <#ident as #fsw::Record>::MAX_LEN; }
    });

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

        impl #impl_generics #fsw::Payload for #ident #ty_generics #where_clause {
            const ALIGN: usize = ::core::mem::align_of::<Self>();
            type Read<'a> = #read_ty where Self: 'a;
            fn timestamp(&self) -> Option<#proto::types::Timestamp> {
                Some(#timestamp_body)
            }
            fn encode<'a>(&'a self, _buf: &'a mut [u8]) -> Result<&'a [u8], #fsw::EncodeError> {
                Ok(#fsw::zerocopy::IntoBytes::as_bytes(self))
            }
            fn decode(bytes: &[u8]) -> Result<Self::Read<'_>, #fsw::DecodeError> {
                #decode_body
            }
        }

        impl #impl_generics #fsw::Record for #ident #ty_generics #where_clause {
            const NAME: &'static str = #frame_name;
            const MAX_LEN: usize = #max_len;
            const SCRATCH_LEN: usize = #scratch_len;
            #depth
            fn schema_with_root(root: &str) -> #fsw::record::RecordSchema {
                #fsw::record::RecordSchema::frame::<Self>(root)
            }
        }

        impl #impl_generics #fsw::Frame for #ident #ty_generics #where_clause {}

        #guard
    }
    .into()
}
