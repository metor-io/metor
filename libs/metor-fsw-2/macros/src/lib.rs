//! Proc macros behind the `metor_fsw_2` authoring surface.
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
//! - [`#[sequence]`](macro@sequence) turns an `async fn` into a loadable
//!   sequence.
//! - [`export_system!`] hand-writes the loadable-system C entry points.
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
mod export;
mod frame;
mod metadatatize;
mod sequence;
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

    /// Alias for [`component_name`](Field::component_name).
    pub fn component_id(&self) -> String {
        self.component_name()
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
pub fn frame(input: TokenStream) -> TokenStream {
    frame::frame(input)
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

/// `export_system!(MySystem);` emits the `extern "C"` `fsw_*` entry points
/// that make a system loadable at runtime from a `cdylib`, each body a
/// one-liner delegating to an `abi::run_*` helper. The system's `Params`
/// must implement `Serialize`, `Deserialize`, and `Schema` so they can cross
/// the boundary as postcard bytes.
///
/// This is the hand-written form; [`#[system(export)]`](macro@system) emits
/// the same surface for macro-authored systems.
#[proc_macro]
pub fn export_system(input: TokenStream) -> TokenStream {
    export::export_system(input)
}

/// Turns an `async fn` whose parameters are `Input<T>`/`Output<T>` ports into
/// a complete sequence, loadable at runtime from a `cdylib`.
///
/// The macro reads the ports off the signature and moves them into the
/// generated future, appending the implicit `SlotControlIn` input and the
/// `SequenceStatus`, health, and log output tail. It then emits the `fsw_*`
/// C entry points, each delegating to an `abi::run_seq_*` helper. Because
/// rings erase their backing type, the fn body is emitted verbatim with no
/// injected generics.
///
/// `#[sequence(name = "…")]` overrides the sequence name, which defaults to
/// the fn name. The fn may take no params, or one params argument
/// implementing `Serialize`, `Deserialize`, and `Schema`.
#[proc_macro_attribute]
pub fn sequence(attr: TokenStream, item: TokenStream) -> TokenStream {
    sequence::sequence(attr, item)
}

/// Builds a complete system from a type's inherent impl block, deriving
/// everything from the method signatures.
///
/// - `fn execute(&mut self, now: Timestamp, …ports)` produces a
///   `CyclicSystem`; `async fn run(&mut self, …ports)` produces an
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
/// snake_cased type ident with any `System` suffix stripped.
/// `#[system(export)]` and `#[system(export = "feature")]` additionally emit
/// the same `fsw_*` entry points as [`export_system!`], compiled out under
/// `cfg(test)` and, in the second form, gated on the named cargo feature.
#[proc_macro_attribute]
pub fn system(attr: TokenStream, item: TokenStream) -> TokenStream {
    system_attr::system_impl(attr.into(), item.into()).into()
}

/// The path generated code uses to reach the framework crate: the name the
/// consumer depends on it under, or `crate` when the framework crate is
/// compiling its own tests. A consumer without the dependency cannot use any
/// of these macros, so that case is a hard panic.
pub(crate) fn metor_fsw_2_crate_name() -> proc_macro2::TokenStream {
    match crate_name("metor-fsw-2") {
        Ok(FoundCrate::Itself) => quote!(crate),
        Ok(FoundCrate::Name(name)) => {
            let ident = Ident::new(&name, Span::call_site());
            quote!( #ident )
        }
        // This crate's own expansion unit tests run without a consumer
        // manifest to resolve against.
        Err(_) if cfg!(test) => quote!(metor_fsw_2),
        Err(e) => panic!("metor-fsw-2 macros require `metor-fsw-2` in [dependencies]: {e}"),
    }
}
