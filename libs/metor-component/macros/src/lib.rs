//! Proc macros behind the [`metor_component`] derive surface.
//!
//! Every macro is re-exported by the component crate, and every expansion
//! reaches back to it through a path resolved at expansion time (see
//! [`metor_component_crate_name`]), so the generated code works no matter what
//! the consumer renamed the dependency to.
//!
//! These are the *standalone* derives: they turn any `#[repr(C)]` struct into
//! a component group, with `#[metor_fsw(parent = "…")]` setting the dotted
//! prefix. `metor-fsw-2` bundles equivalent expansions into
//! `#[derive(Frame)]`; reach for these when a struct is a component group but
//! not a frame in its own right — a nested sub-struct, say.
//!
//! Field and struct attributes live under `#[fsw(...)]`; the longer
//! `#[metor_fsw(...)]` spelling is accepted as an alias.
//!
//! [`metor_component`]: ../metor_component/index.html

use darling::FromField;
use proc_macro::TokenStream;
use proc_macro_crate::{FoundCrate, crate_name};
use proc_macro2::Span;
use quote::quote;
use syn::Ident;

mod as_vtable;
mod componentize;
mod decomponentize;
mod metadatatize;

#[derive(Debug, FromField)]
#[darling(attributes(fsw, metor_fsw))]
struct Field {
    ident: Option<syn::Ident>,
    ty: syn::Type,
    component_id: Option<String>,
    #[darling(default)]
    timestamp: bool,
    /// Descend into a sub-frame/struct instead of treating the field as a leaf
    /// scalar (Componentize/Decomponentize recurse through the trait).
    #[darling(default)]
    nest: bool,
    /// Max cardinality for a `FrameList`/`FrameMap` field (frames.md §3.4). The
    /// const-generic on the type is the source of truth; this is accepted for
    /// forward-compat but unused by the derives.
    #[darling(default)]
    #[allow(dead_code)]
    max: Option<usize>,
    /// `#[metor_fsw(skip)]` force-hides a field from telemetry;
    /// `#[metor_fsw(skip = false)]` opts a `_`-prefixed field back in. Absent,
    /// a field is skipped iff its name starts with `_` — the convention for
    /// `#[repr(C)]` padding.
    #[darling(default)]
    skip: Option<bool>,
}

impl Field {
    pub fn component_name(&self) -> String {
        match &self.component_id {
            Some(c) => c.clone(),
            None => {
                let ident = self.ident.as_ref().expect("field must have ident");
                ident.to_string()
            }
        }
    }

    /// Alias for [`component_name`](Field::component_name) used by the
    /// Componentize/Decomponentize derives.
    pub fn component_id(&self) -> String {
        self.component_name()
    }

    /// Whether the field is omitted from telemetry: never becomes a component
    /// and never round-trips through encode/decode. `_`-prefixed fields skip by
    /// default (padding), overridable in either direction with
    /// `#[metor_fsw(skip)]`.
    pub fn skipped(&self) -> bool {
        self.skip.unwrap_or_else(|| {
            self.ident
                .as_ref()
                .is_some_and(|i| i.to_string().starts_with('_'))
        })
    }

    /// Whether this field should recurse rather than emit a scalar leaf: either
    /// explicitly `#[metor_fsw(nest)]`, or a dynamic `FrameList`/`FrameMap` whose
    /// slot carries no in-struct value.
    pub fn is_nested(&self) -> bool {
        self.nest || self.is_dynamic()
    }

    /// Whether the field type's outermost path segment is `FrameList`/`FrameMap`.
    /// Used to size the trailer in `Componentize::MAX_SIZE` and to skip the
    /// (slot-only) field on the scalar Componentize/Decomponentize paths.
    pub fn is_dynamic(&self) -> bool {
        if let syn::Type::Path(p) = &self.ty
            && let Some(seg) = p.path.segments.last()
        {
            return seg.ident == "FrameList" || seg.ident == "FrameMap";
        }
        false
    }
}

#[proc_macro_derive(Metadatatize, attributes(fsw, metor_fsw))]
pub fn metadatize(input: TokenStream) -> TokenStream {
    metadatatize::metadatatize(input)
}

#[proc_macro_derive(AsVTable, attributes(fsw, metor_fsw))]
pub fn as_vtable(input: TokenStream) -> TokenStream {
    as_vtable::as_vtable(input)
}

#[proc_macro_derive(Componentize, attributes(fsw, metor_fsw))]
pub fn componentize(input: TokenStream) -> TokenStream {
    componentize::componentize(input)
}

#[proc_macro_derive(Decomponentize, attributes(fsw, metor_fsw))]
pub fn decomponentize(input: TokenStream) -> TokenStream {
    decomponentize::decomponentize(input)
}

/// The path generated code uses to reach the component crate: the name the
/// consumer depends on it under, or `crate` when the component crate is
/// compiling itself. A consumer without the dependency cannot use any of these
/// derives, so that case is a hard panic.
pub(crate) fn metor_component_crate_name() -> proc_macro2::TokenStream {
    match crate_name("metor-component") {
        Ok(FoundCrate::Itself) => quote!(crate),
        Ok(FoundCrate::Name(name)) => {
            let ident = Ident::new(&name, Span::call_site());
            quote!( #ident )
        }
        // This crate's own expansion tests run without a consumer manifest to
        // resolve against.
        Err(_) if cfg!(test) => quote!(metor_component),
        Err(e) => panic!("the component derives require `metor-component` in [dependencies]: {e}"),
    }
}

