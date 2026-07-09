use darling::FromDeriveInput;
use darling::ast;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{Attribute, Meta};
use syn::{DeriveInput, Generics, Ident};

/// The parsed shape of a deriving type, carrying its fields and any
/// `#[fsw(...)]`/`#[metor_fsw(...)]` arguments for [`as_vtable_impl`].
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
    /// Alternative spelling of `parent`; a frame's name doubles as the
    /// component prefix of its fields.
    name: Option<String>,
    #[darling(default, rename = "group")]
    _group: darling::util::Ignored,
    /// Accepted so the shared attribute parses; only the `Frame` derive
    /// acts on it.
    #[darling(default, rename = "no_timestamp")]
    _no_timestamp: darling::util::Ignored,
}

/// Returns the first identifier inside a `#[repr(...)]` attribute, if any.
fn extract_repr_type(attrs: &[Attribute]) -> Option<Ident> {
    for attr in attrs {
        if attr.path().is_ident("repr")
            && let Meta::List(meta_list) = attr.meta.clone()
        {
            for token in meta_list.tokens {
                if let Ok(ident) = syn::parse2::<Ident>(token.into()) {
                    return Some(ident);
                }
            }
        }
    }
    None
}

/// Generates an `AsVTable` impl for the derive input.
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
    } = match AsVTable::from_derive_input(input) {
        Ok(parsed) => parsed,
        Err(e) => return e.write_errors(),
    };
    let parent = parent.or(name);
    let where_clause = &generics.where_clause;
    let impeller = quote! { #crate_name::metor_proto };

    let frame_map = frame_id.as_ref().map(|fid| {
        quote! { .map(move |field| field.with_frame(#fid)) }
    });

    match data {
        // A unit enum is a single raw field typed after its `#[repr(...)]`.
        ast::Data::Enum(_) => {
            let name = parent.unwrap_or_else(|| ident.to_string());
            let Some(repr_type) = extract_repr_type(&input.attrs) else {
                return syn::Error::new_spanned(
                    &input.ident,
                    "deriving AsVTable on an enum requires a primitive #[repr(...)] \
                     (the enum is carried as that scalar)",
                )
                .to_compile_error();
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
            // of the component fields.
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
            // `element_fields` names members relative to a plain string prefix
            // instead of a component path, so a dynamic container can stamp
            // out copies under names chosen at runtime.
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
