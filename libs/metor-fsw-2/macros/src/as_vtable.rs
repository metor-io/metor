use proc_macro2::TokenStream as TokenStream2;
use quote::quote;

use crate::frame::FrameArgs;

/// Generates an `AsVTable` impl for the frame.
///
/// When `frame_id` is given, every emitted field builder is wrapped in a
/// `with_frame` op so the realized components inherit that frame id; when it
/// is `None` the fields stand on their own. `crate_name` is the path prefix
/// the generated code uses to reach the framework's re-exports.
///
/// A struct field marked `#[fsw(timestamp)]` never becomes a component of its
/// own. It is captured as a raw table and attached to every other field as
/// the shared timestamp source.
pub fn as_vtable_impl(
    args: &FrameArgs,
    frame_id: Option<TokenStream2>,
    crate_name: &TokenStream2,
) -> TokenStream2 {
    let FrameArgs {
        ident,
        generics,
        fields,
        frame_name: parent,
        ..
    } = args;
    let where_clause = &generics.where_clause;
    let impeller = quote! { #crate_name::metor_proto };

    let frame_map = frame_id.as_ref().map(|fid| {
        quote! { .map(move |field| field.with_frame(#fid)) }
    });

    let mut timestamp_fields = fields.iter().filter(|field| field.timestamp);
    let timestamp_field = timestamp_fields.next();
    if let Some(extra) = timestamp_fields.next() {
        return syn::Error::new_spanned(
            &extra.ident,
            "only one field can be marked #[metor_fsw(timestamp)]",
        )
        .to_compile_error();
    }
    let timestamp_source = timestamp_field.map(|field| {
        let ident = &field.ident;
        let ty = &field.ty;
        quote! {
            let timestamp_source = #impeller::vtable::builder::raw_table(
                core::mem::offset_of!(Self, #ident) as u32,
                core::mem::size_of::<#ty>() as u32,
            );
        }
    });
    let timestamp_map = timestamp_field.map(|_| {
        quote! {
            .map(move |field| field.with_timestamp(timestamp_source.clone()))
        }
    });
    // The timestamp field feeds the source above and is filtered out
    // of the component fields, as are `skip`ped fields.
    let vtable_items = fields.iter().filter(|f| !f.timestamp && !f.skipped()).map(|field| {
        let ty = &field.ty;
        let name = field.component_name();
        let name = if let Some(parent) = parent {
            format!("{parent}.{name}")
        } else {
            name
        };
        let ident = &field.ident;
        quote! {
            .chain(<#ty as #crate_name::AsVTable>::vtable_fields(path.chain(#name))
                .map(|field| field.offset_by(core::mem::offset_of!(Self, #ident) as u32))
            )
        }
    });
    // `element_fields` names members relative to a plain string prefix
    // instead of a component path, so a dynamic container can stamp
    // out copies under names chosen at runtime.
    let element_items = fields.iter().filter(|f| !f.timestamp && !f.skipped()).map(|field| {
        let ty = &field.ty;
        let name = field.component_name();
        let ident = &field.ident;
        quote! {
            .chain(<#ty as #crate_name::AsVTable>::element_fields(child(#name))
                .map(|field| field.offset_by(core::mem::offset_of!(Self, #ident) as u32))
            )
        }
    });
    quote! {
        impl #crate_name::AsVTable for #ident #generics #where_clause {
            fn vtable_fields(path: impl #crate_name::path::ComponentPath) -> impl Iterator<Item = #impeller::vtable::builder::FieldBuilder> {
                use #crate_name::path::ComponentPath;
                #timestamp_source
                std::iter::empty()
                #(#vtable_items)*
                #timestamp_map
                #frame_map
            }

            fn element_fields(prefix: String) -> impl Iterator<Item = #impeller::vtable::builder::FieldBuilder> {
                let child = |name: &str| if prefix.is_empty() {
                    name.to_string()
                } else {
                    format!("{prefix}.{name}")
                };
                std::iter::empty()
                #(#element_items)*
            }
        }
    }
}
