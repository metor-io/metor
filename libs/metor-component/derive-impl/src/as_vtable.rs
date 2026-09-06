use proc_macro2::TokenStream as TokenStream2;
use quote::quote;

use crate::input::{EnumInput, Input, StructInput};

/// Generates an `AsVTable` impl.
///
/// `frame_id` is `None` for the standalone `#[derive(AsVTable)]` and
/// `Some(<ComponentId expr>)` for `#[derive(Frame)]`, which wraps every
/// member's op chain in a `with_frame` op so each realized component
/// inherits the frame id. `crate_name` is the path prefix the generated code
/// uses to reach the framework's re-exports.
///
/// A struct field marked `#[fsw(timestamp)]` never becomes a component of
/// its own. It is captured as a raw table and attached to every other field
/// as the shared timestamp source.
pub fn as_vtable_impl(
    input: &Input,
    frame_id: Option<TokenStream2>,
    crate_name: &TokenStream2,
) -> TokenStream2 {
    let frame_map = frame_id.as_ref().map(|fid| {
        quote! { .map(move |field| field.with_frame(#fid)) }
    });
    let impeller = quote! { #crate_name::metor_proto };

    match input {
        Input::Enum(EnumInput {
            ident,
            generics,
            parent,
            repr_type,
            ..
        }) => {
            let where_clause = &generics.where_clause;
            let name = parent.clone().unwrap_or_else(|| ident.to_string());
            let Some(repr_type) = repr_type else {
                panic!("repr required for enum derive");
            };
            quote! {
                impl #crate_name::AsVTable for #ident #generics #where_clause {
                    fn vtable_fields(path: impl #crate_name::path::ComponentPath) -> impl Iterator<Item = #impeller::vtable::builder::FieldBuilder> {
                        let component = if path.is_empty() {
                            #impeller::vtable::builder::component(#name)
                        } else {
                            #impeller::vtable::builder::component(path.to_component_id())
                        };
                        [
                            #impeller::vtable::builder::raw_field(0, core::mem::size_of::<Self>() as u32, #impeller::vtable::builder::schema(
                                <#repr_type as #impeller::component::PrimTypeElem>::PRIM_TYPE,
                                &[],
                                component
                            ))
                        ].into_iter()
                        #frame_map
                    }
                }
            }
        }
        Input::Struct(StructInput {
            ident,
            generics,
            fields,
            parent,
            ..
        }) => {
            let where_clause = &generics.where_clause;
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
            let vtable_items = fields
                .iter()
                .filter(|f| !f.timestamp && !f.skipped())
                .map(|field| {
                    let ty = &field.ty;
                    let name = field.qualified_component_name(parent.as_deref());
                    let ident = &field.ident;
                    quote! {
                        .chain(<#ty as #crate_name::AsVTable>::vtable_fields(path.chain(#name))
                            .map(|field| field.offset_by(core::mem::offset_of!(Self, #ident) as u32))
                        )
                    }
                });
            // `element_fields` names members relative to a plain string
            // prefix instead of a component path, so a dynamic container can
            // stamp out copies under names chosen at runtime.
            let element_items = fields
                .iter()
                .filter(|f| !f.timestamp && !f.skipped())
                .map(|field| {
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
    }
}
