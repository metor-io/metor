use convert_case::{Case, Casing};
use darling::FromDeriveInput;
use darling::ast;
use proc_macro::TokenStream;
use quote::quote;
use syn::{DeriveInput, Generics, Ident, parse_macro_input};

/// Frame-level attributes. The frame name (`name`/`parent`) doubles as both the
/// component-id prefix and the `FRAME_ID` tag (frames.md §1.3). When absent, no
/// prefix is applied and `FRAME_ID` defaults to `snake_case(ident)`.
#[derive(Debug, FromDeriveInput)]
#[darling(attributes(metor_fsw), supports(struct_named))]
struct Frame {
    ident: Ident,
    generics: Generics,
    data: ast::Data<(), crate::Field>,
    parent: Option<String>,
    name: Option<String>,
}

/// `#[derive(Frame)]` — the one-annotation entry point (frames.md §2.4).
///
/// Bundles the four sub-derives (`AsVTable` + `Metadatatize` + `Componentize` +
/// `Decomponentize`) and adds `impl Frame`. The `AsVTable` half is generated with a
/// frame wrap so every member inherits the `FRAME_ID`.
pub fn frame(input: TokenStream) -> TokenStream {
    let parsed = parse_macro_input!(input as DeriveInput);
    let Frame {
        ident,
        generics,
        data,
        parent,
        name,
    } = Frame::from_derive_input(&parsed).unwrap();

    // `#[derive(Frame)]` is a metor-fsw-2 concept, so the bundled sub-derives emit
    // `::metor_fsw_2::…` re-export paths — a crate depending only on metor-fsw-2 can
    // then use the derive without a direct metor-fsw / metor-proto dependency.
    let fsw2 = crate::metor_fsw_2_crate_name();
    let impeller = quote! { #fsw2::metor_proto };

    // The frame name: explicit `name`/`parent`, else snake_case of the ident. This
    // is both the dotted component prefix and the `FRAME_ID`.
    let frame_name = name
        .or(parent)
        .unwrap_or_else(|| ident.to_string().to_case(Case::Snake));
    let frame_id = quote! { #impeller::types::ComponentId::new(#frame_name) };

    // The shared timestamp accessor reads the `#[metor_fsw(timestamp)]` field.
    let fields = data.take_struct().expect("Frame requires a named struct");
    let ts_field = fields.fields.iter().find(|f| f.timestamp);
    let timestamp_body = match ts_field {
        Some(f) => {
            let id = f.ident.as_ref().expect("named field");
            quote! { self.#id }
        }
        None => quote! { #impeller::types::Timestamp::default() },
    };

    // The four sub-derives share the same `Field`/attribute surface; each reads the
    // frame `name` as its component prefix.
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
