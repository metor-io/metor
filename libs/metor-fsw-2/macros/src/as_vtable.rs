use darling::FromDeriveInput;
use darling::ast;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{Attribute, Meta};
use syn::{DeriveInput, Generics, Ident};

#[derive(Debug, FromDeriveInput)]
#[darling(
    attributes(fsw, metor_fsw),
    supports(struct_named, enum_unit),
    forward_attrs(allow, doc, cfg)
)]
pub struct AsVTable {
    ident: Ident,
    generics: Generics,
    data: ast::Data<(), crate::Field>,
    parent: Option<String>,
    /// Frame name (frames.md §1.3); folds into `parent` as the component prefix.
    name: Option<String>,
    #[darling(default, rename = "group")]
    _group: darling::util::Ignored,
    /// Tolerated here; consumed by the bundling `Frame` derive (E4 opt-out).
    #[darling(default, rename = "no_timestamp")]
    _no_timestamp: darling::util::Ignored,
}

fn extract_repr_type(attrs: &[Attribute]) -> Option<Ident> {
    for attr in attrs {
        if attr.path().is_ident("repr")
            && let Meta::List(meta_list) = attr.meta.clone() {
                for token in meta_list.tokens {
                    if let Ok(ident) = syn::parse2::<Ident>(token.into()) {
                        return Some(ident);
                    }
                }
            }
    }
    None
}

/// Shared `AsVTable` code generator.
///
/// `frame_id` is `None` for the standalone `#[derive(AsVTable)]` and
/// `Some(<ComponentId expr>)` for `#[derive(Frame)]`, which wraps every member's
/// op chain in a `frame(...)` op so each realized component inherits the frame id.
///
/// The `#[metor_fsw(timestamp)]` field is suppressed as a standalone component on
/// both paths (frames.md Q1) — it contributes only the shared timestamp source.
///
/// `crate_name` is the root path to the crate re-exporting the named trait surface
/// (`metor-fsw` standalone, `metor-fsw-2` when bundled by `#[derive(Frame)]`).
pub fn as_vtable_impl(
    input: &DeriveInput,
    frame_id: Option<TokenStream2>,
    crate_name: &TokenStream2,
) -> TokenStream2 {
    let AsVTable {
        ident,
        generics,
        data,
        parent,
        name,
        _group,
        _no_timestamp: _,
    } = AsVTable::from_derive_input(input).unwrap();
    let parent = parent.or(name);
    let where_clause = &generics.where_clause;
    let impeller = quote! { #crate_name::metor_proto };

    let frame_map = frame_id.as_ref().map(|fid| {
        quote! { .map(move |field| field.with_frame(#fid)) }
    });

    match data {
        ast::Data::Enum(_) => {
            let name = parent.unwrap_or_else(|| ident.to_string());
            let Some(repr_type) = extract_repr_type(&input.attrs) else {
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
        ast::Data::Struct(fields) => {
            let mut timestamp_fields = fields.fields.iter().filter(|field| field.timestamp);
            let timestamp_field = timestamp_fields.next();
            if timestamp_fields.next().is_some() {
                panic!("only one field can be marked #[metor_fsw(timestamp)]");
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
            // The timestamp field is the source only; never emitted as a component.
            let vtable_items = fields.fields.iter().filter(|f| !f.timestamp).map(|field| {
                let ty = &field.ty;
                let name = field.component_name();
                let name = if let Some(parent) = &parent {
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
            // Dynamic member-template form (frames.md §4): leaves are
            // `path_component`, names are relative to the element base.
            let element_items = fields.fields.iter().filter(|f| !f.timestamp).map(|field| {
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
