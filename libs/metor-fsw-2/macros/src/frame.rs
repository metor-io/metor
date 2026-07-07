use convert_case::{Case, Casing};
use darling::FromDeriveInput;
use darling::ast;
use proc_macro::TokenStream;
use quote::quote;
use syn::{DeriveInput, Generics, Ident, parse_macro_input};

/// Struct-level attributes for `#[derive(Frame)]`.
///
/// The frame name (`name`, or its `parent` alias) becomes both the dotted
/// component-id prefix and the `FRAME_ID` tag. When absent, no prefix is
/// applied and `FRAME_ID` defaults to the snake_case struct ident.
#[derive(Debug, FromDeriveInput)]
#[darling(attributes(fsw, metor_fsw), supports(struct_named))]
struct Frame {
    ident: Ident,
    generics: Generics,
    data: ast::Data<(), crate::Field>,
    parent: Option<String>,
    name: Option<String>,
    /// Explicit opt-out of the shared timestamp. A frame with no
    /// `#[metor_fsw(timestamp)]` field must say `#[metor_fsw(no_timestamp)]`,
    /// so a forgotten timestamp is a derive error rather than every record
    /// silently stamping `Timestamp(0)`.
    #[darling(default)]
    no_timestamp: bool,
}

/// Expands `#[derive(Frame)]`.
///
/// Bundles the four sub-derives (`AsVTable`, `Metadatatize`, `Componentize`,
/// `Decomponentize`) and adds an `impl Frame`. The `AsVTable` half is generated
/// with a frame wrap so every member inherits the `FRAME_ID`.
pub fn frame(input: TokenStream) -> TokenStream {
    let parsed = parse_macro_input!(input as DeriveInput);
    let Frame {
        ident,
        generics,
        data,
        parent,
        name,
        no_timestamp,
    } = Frame::from_derive_input(&parsed).unwrap();

    // The sub-derives emit `::metor_fsw_2::…` re-export paths, so a crate that
    // depends only on metor_fsw_2 can use the derive without depending on the
    // protocol crate directly.
    let fsw2 = crate::metor_fsw_2_crate_name();
    let impeller = quote! { #fsw2::metor_proto };

    // Explicit `name`/`parent` wins, else snake_case of the ident. This is both
    // the dotted component prefix and the `FRAME_ID`.
    let frame_name = name
        .or(parent)
        .unwrap_or_else(|| ident.to_string().to_case(Case::Snake));
    let frame_id = quote! { #impeller::types::ComponentId::new(#frame_name) };

    // The shared timestamp accessor reads the `#[metor_fsw(timestamp)]` field.
    // A frame with neither the marker nor the `no_timestamp` opt-out is a
    // derive error; with the opt-out, the accessor returns the default stamp.
    let fields = data.take_struct().expect("Frame requires a named struct");
    let ts_field = fields.fields.iter().find(|f| f.timestamp);
    let timestamp_body = match (ts_field, no_timestamp) {
        (Some(_), true) => {
            return syn::Error::new_spanned(
                &parsed.ident,
                "#[metor_fsw(no_timestamp)] contradicts the #[metor_fsw(timestamp)] field; \
                 remove one",
            )
            .to_compile_error()
            .into();
        }
        (Some(f), false) => {
            let id = f.ident.as_ref().expect("named field");
            quote! { self.#id }
        }
        (None, true) => quote! { #impeller::types::Timestamp::default() },
        (None, false) => {
            return syn::Error::new_spanned(
                &parsed.ident,
                "#[derive(Frame)] requires a #[metor_fsw(timestamp)] field (the frame's \
                 shared timestamp); mark one, or opt out explicitly with \
                 #[metor_fsw(no_timestamp)] to stamp every record Timestamp(0)",
            )
            .to_compile_error()
            .into();
        }
    };

    // Each sub-derive reads the same `Field` attribute surface and takes the
    // frame name as its component prefix.
    let as_vtable = crate::as_vtable::as_vtable_impl(&parsed, Some(frame_id.clone()), &fsw2);
    let metadatatize = crate::metadatatize::metadatatize_impl(&parsed, &fsw2);
    let componentize = crate::componentize::componentize_impl(&parsed, &fsw2);
    let decomponentize = crate::decomponentize::decomponentize_impl(&parsed, &fsw2);

    let where_clause = &generics.where_clause;
    let frame_trait = quote! {
        impl #fsw2::Frame for #ident #generics #where_clause {
            const NAME: &'static str = #frame_name;
            const FRAME_ID: #impeller::types::ComponentId = #frame_id;
            fn timestamp(&self) -> #impeller::types::Timestamp {
                #timestamp_body
            }
        }
    };

    quote! {
        #as_vtable
        #metadatatize
        #componentize
        #decomponentize
        #frame_trait
    }
    .into()
}
