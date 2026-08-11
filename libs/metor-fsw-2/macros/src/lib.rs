//! Proc macros behind the `metor_fsw_2_core` authoring surface.
//!
//! User code never depends on this crate directly. Every macro is re-exported
//! by the framework crate, and every expansion refers back to that crate
//! through a path resolved at expansion time (see
//! [`metor_fsw_2_crate_name`]), so the generated code works no matter what
//! the consumer renamed the dependency to.
//!
//! The surface is small:
//!
//! - [`#[derive(Frame)]`](derive@Frame) turns a struct into a frame in one
//!   annotation.
//! - [`#[derive(SystemInput)]`](derive@SystemInput) /
//!   [`#[derive(SystemOutput)]`](derive@SystemOutput) describe port bundles.
//! - [`#[system]`](macro@system) builds a whole system from an inherent impl
//!   block.
//!
//! Field and struct attributes live under `#[fsw(...)]`; the longer
//! `#[metor_fsw(...)]` spelling is accepted as an alias.

use darling::FromField;
use proc_macro::TokenStream;
use proc_macro_crate::{FoundCrate, crate_name};
use proc_macro2::Span;
use quote::quote;
use syn::Ident;

mod as_vtable;
mod componentize;
mod decomponentize;
mod frame;
mod metadatatize;
mod params_docs;
mod sig;
mod system;
mod system_attr;

/// A single struct field as parsed by the frame derives, pairing its ident
/// and type with the `#[fsw(...)]` attributes they all share.
#[derive(Debug, FromField)]
#[darling(attributes(fsw, metor_fsw))]
struct Field {
    ident: Option<syn::Ident>,
    ty: syn::Type,
    component_id: Option<String>,
    #[darling(default)]
    timestamp: bool,
    /// Descend into a sub-frame through the component traits instead of
    /// treating the field as a leaf scalar.
    #[darling(default)]
    nest: bool,
    /// `#[fsw(skip)]` force-hides a field from telemetry; `#[fsw(skip = false)]`
    /// opts a `_`-prefixed field back in. Absent, a field is skipped iff its
    /// name starts with `_` — the convention for `#[repr(C)]` padding.
    #[darling(default)]
    skip: Option<bool>,
}

impl Field {
    /// The field's component id, defaulting to the field name.
    pub fn component_name(&self) -> String {
        match &self.component_id {
            Some(c) => c.clone(),
            None => {
                let ident = self.ident.as_ref().expect("field must have ident");
                ident.to_string()
            }
        }
    }

    /// Whether the field is omitted from telemetry: never becomes a component
    /// and never round-trips through encode/decode. `_`-prefixed fields skip by
    /// default (padding), overridable in either direction with `#[fsw(skip)]`.
    pub fn skipped(&self) -> bool {
        self.skip.unwrap_or_else(|| {
            self.ident
                .as_ref()
                .is_some_and(|i| i.to_string().starts_with('_'))
        })
    }

    /// Whether the field recurses through the component traits rather than
    /// emitting a scalar leaf, either explicitly via `#[fsw(nest)]` or
    /// implicitly because it is dynamic.
    pub fn is_nested(&self) -> bool {
        self.nest || self.is_dynamic()
    }

    /// Whether the field type's outermost path segment is `FrameList` or
    /// `FrameMap`. Such fields carry no in-struct value, so the scalar
    /// encode/decode paths skip them and `MAX_SIZE` sizes their trailer
    /// instead.
    pub fn is_dynamic(&self) -> bool {
        if let syn::Type::Path(p) = &self.ty
            && let Some(seg) = p.path.segments.last()
        {
            return seg.ident == "FrameList" || seg.ident == "FrameMap";
        }
        false
    }
}

/// Derives the four component sub-traits and `Frame` itself, making a struct
/// a frame with a single annotation. Fields are configured with
/// `#[fsw(...)]`.
#[proc_macro_derive(Frame, attributes(fsw, metor_fsw))]
pub fn frame_derive(input: TokenStream) -> TokenStream {
    frame::frame(input)
}

/// Derives `ParamsDocs`: submits a system's `Params` field doc comments into
/// the crate-local params-docs collection, so pack module generation can render
/// them. Docs are optional; undocumented fields contribute nothing.
#[proc_macro_derive(ParamsDocs)]
pub fn params_docs_derive(input: TokenStream) -> TokenStream {
    params_docs::params_docs(input)
}

/// Derives `SystemInput` and `BindPorts` for a bundle of input ports.
#[proc_macro_derive(SystemInput, attributes(fsw))]
pub fn system_input(input: TokenStream) -> TokenStream {
    system::system_input(input)
}

/// Derives `SystemOutput` and `BindPorts` for a bundle of output ports.
///
/// `#[fsw(telemetered = false)]` keeps a field's output off the telemetry
/// tap; a `CommandOut<M>` field is recognized as the same opt-out.
#[proc_macro_derive(SystemOutput, attributes(fsw))]
pub fn system_output(input: TokenStream) -> TokenStream {
    system::system_output(input)
}

/// Builds a complete system from a type's inherent impl block, deriving
/// everything from the method signatures.
///
/// - `fn execute(&mut self, now: Timestamp, …ports)` produces a
///   `CyclicSystem`; `async fn run(&mut self, context: &AsyncContext, …ports)` produces an
///   `AsyncSystem`. Exactly one of the two must be present.
/// - Port parameters are `&mut Input<T>`, `&mut MsgIn<M>`, `&mut Output<T>`,
///   `&mut MsgOut<M>`, or `&mut CommandOut<M>`, plus at most one
///   `&mut HealthPort`. Descriptors keep signature order within each
///   direction.
/// - `fn new(params: P) -> Self`, `fn new() -> Self`, or, when absent, a
///   `Default` bound on the type drives the generated `BuildSystem` impl.
/// - Optional `fn init` and `fn shutdown` may take output ports (matched by
///   name) and/or `&mut HealthPort`.
///
/// `#[system(name = "…")]` overrides the wiring name, which defaults to the
/// snake_cased type ident with any `System` suffix stripped. It is the only
/// argument: there is no per-system export — a system reaches a cdylib
/// through its crate's pack (`Pack::system_type` + `export_pack!`).
#[proc_macro_attribute]
pub fn system(attr: TokenStream, item: TokenStream) -> TokenStream {
    system_attr::system_impl(attr.into(), item.into()).into()
}

/// The path generated code uses to reach the framework crate: the name the
/// consumer depends on it under, or `crate` when the framework crate is
/// compiling its own tests.
///
/// Everything these macros expand to lives in `metor-fsw-2-core`, so that is
/// the first name probed — a pack or contract crate depends on it directly.
/// A target crate depends only on the host `metor-fsw-2`, which re-exports
/// core whole, so the same paths resolve through it. A consumer with neither
/// cannot use any of these macros, so that case is a hard panic.
pub(crate) fn metor_fsw_2_crate_name() -> proc_macro2::TokenStream {
    for name in ["metor-fsw-2-core", "metor-fsw-2"] {
        match crate_name(name) {
            Ok(FoundCrate::Itself) => return quote!(crate),
            Ok(FoundCrate::Name(name)) => {
                let ident = Ident::new(&name, Span::call_site());
                return quote!( #ident );
            }
            Err(_) => continue,
        }
    }
    // This crate's own expansion unit tests run without a consumer manifest
    // to resolve against.
    if cfg!(test) {
        return quote!(metor_fsw_2_core);
    }
    panic!("metor-fsw-2 macros require `metor-fsw-2-core` (or `metor-fsw-2`) in [dependencies]")
}

/// The path to the *host* crate, for the one construct that only exists
/// there: an `AsyncSystem`, whose cancellation context is owned by the
/// coordinator's runtime. A crate that depends on `metor-fsw-2-core` alone can
/// author every other system form but not this one, which is the boundary
/// working as intended.
pub(crate) fn metor_fsw_2_host_crate_name() -> proc_macro2::TokenStream {
    match crate_name("metor-fsw-2") {
        Ok(FoundCrate::Itself) => quote!(crate),
        Ok(FoundCrate::Name(name)) => {
            let ident = Ident::new(&name, Span::call_site());
            quote!( #ident )
        }
        Err(_) if cfg!(test) => quote!(metor_fsw_2),
        Err(_) => panic!(
            "an `async fn run` system is driven by the coordinator's runtime, so it needs \
             `metor-fsw-2` in [dependencies] (`metor-fsw-2-core` alone carries the cyclic, \
             fn, and task forms)"
        ),
    }
}
