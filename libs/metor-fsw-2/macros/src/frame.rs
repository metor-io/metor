use convert_case::{Case, Casing};
use darling::FromDeriveInput;
use darling::ast;
use darling::util::Override;
use proc_macro::TokenStream;
use quote::quote;
use syn::{DeriveInput, Generics, Ident, parse_macro_input};

/// Everything `#[derive(Frame)]` reads from the input struct, parsed by
/// darling from the `fsw`/`metor_fsw` attributes.
///
/// The frame name (`name`, or its `parent` alias) becomes both the dotted
/// component-id prefix and the `FRAME_ID` tag. When absent, no prefix is
/// applied and `FRAME_ID` defaults to the snake_case struct ident.
#[derive(Debug, FromDeriveInput)]
#[darling(attributes(fsw, metor_fsw), supports(struct_named))]
struct FrameInput {
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
    /// Bare `#[fsw(group)]` emits a metadata-only parent entry named after the
    /// struct; `#[fsw(group = "Custom")]` supplies the name instead.
    #[darling(default)]
    group: Option<Override<String>>,
}

/// The frame derive input, parsed once and shared by the four sub-derive
/// emitters. `frame_name` is the explicit `name`/`parent` — the dotted
/// component-id prefix — and is `None` when neither was given, leaving fields
/// at the root.
pub(crate) struct FrameArgs {
    pub ident: Ident,
    pub generics: Generics,
    pub fields: Vec<crate::Field>,
    pub frame_name: Option<String>,
    pub no_timestamp: bool,
    pub group: Option<Override<String>>,
}

/// Expands `#[derive(Frame)]`.
///
/// Bundles the four sub-derives (`AsVTable`, `Metadatatize`, `Componentize`,
/// `Decomponentize`) and adds an `impl Frame`. The `AsVTable` half is generated
/// with a frame wrap so every member inherits the `FRAME_ID`.
pub fn frame(input: TokenStream) -> TokenStream {
    let parsed = parse_macro_input!(input as DeriveInput);
    let raw = match FrameInput::from_derive_input(&parsed) {
        Ok(raw) => raw,
        Err(e) => return e.write_errors().into(),
    };
    let fields = raw
        .data
        .take_struct()
        .expect("Frame requires a named struct")
        .fields;
    let args = FrameArgs {
        ident: raw.ident,
        generics: raw.generics,
        fields,
        frame_name: raw.name.or(raw.parent),
        no_timestamp: raw.no_timestamp,
        group: raw.group,
    };

    // The sub-derives emit `::metor_fsw_2_core::…` re-export paths, so a crate that
    // depends only on metor_fsw_2_core can use the derive without depending on the
    // protocol crate directly.
    let fsw2 = crate::metor_fsw_2_crate_name();
    let impeller = quote! { #fsw2::metor_proto };

    // Explicit `name`/`parent` wins, else snake_case of the ident. This is both
    // the dotted component prefix and the `FRAME_ID`.
    let frame_name = args
        .frame_name
        .clone()
        .unwrap_or_else(|| args.ident.to_string().to_case(Case::Snake));
    let frame_id = quote! { #impeller::types::ComponentId::new(#frame_name) };

    // The shared timestamp accessor reads the `#[metor_fsw(timestamp)]` field.
    // A frame with neither the marker nor the `no_timestamp` opt-out is a
    // derive error; with the opt-out, the accessor returns the default stamp.
    let ts_field = args.fields.iter().find(|f| f.timestamp);
    let timestamp_body = match (ts_field, args.no_timestamp) {
        (Some(_), true) => {
            return syn::Error::new_spanned(
                &args.ident,
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
                &args.ident,
                "#[derive(Frame)] requires a #[metor_fsw(timestamp)] field (the frame's \
                 shared timestamp); mark one, or opt out explicitly with \
                 #[metor_fsw(no_timestamp)] to stamp every record Timestamp(0)",
            )
            .to_compile_error()
            .into();
        }
    };

    // Each sub-derive reads the same parsed args and takes the frame name as
    // its component prefix.
    let as_vtable = crate::as_vtable::as_vtable_impl(&args, Some(frame_id.clone()), &fsw2);
    let metadatatize = crate::metadatatize::metadatatize_impl(&args, &fsw2);
    let componentize = crate::componentize::componentize_impl(&args, &fsw2);
    let decomponentize = crate::decomponentize::decomponentize_impl(&args, &fsw2);

    let ident = &args.ident;
    let generics = &args.generics;
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
