//! Proc macros for the `metor-fsw-2` flight-software framework.
//!
//! Everything here emits `::metor_fsw_2::…` paths (resolved via `proc-macro-crate`),
//! and everything is consumed through `metor_fsw_2`'s re-exports — user code never
//! depends on this crate directly. The old framework's derives
//! (`AsVTable`/`Componentize`/…) stay in `metor-fsw-macros`; this crate owns the
//! fsw-2 authoring surface: `#[derive(Frame)]`, the `SystemInput`/`SystemOutput`
//! bundle derives, `export_system!`, `#[sequence]`, and `#[system]`.
//!
//! Helper-attribute namespace: **`#[fsw(...)]`**, with the legacy `#[metor_fsw(...)]`
//! spelling accepted during the transition (`docs/design-system-macro.md` §4).

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

    /// Whether this field should recurse rather than emit a scalar leaf: either
    /// explicitly `#[fsw(nest)]`, or a dynamic `FrameList`/`FrameMap` whose
    /// slot carries no in-struct value.
    pub fn is_nested(&self) -> bool {
        self.nest || self.is_dynamic()
    }

    /// Whether the field type's outermost path segment is `FrameList`/`FrameMap`.
    /// Used to size the trailer in `Componentize::MAX_SIZE` and to skip the
    /// (slot-only) field on the scalar Componentize/Decomponentize paths.
    pub fn is_dynamic(&self) -> bool {
        if let syn::Type::Path(p) = &self.ty
            && let Some(seg) = p.path.segments.last() {
                return seg.ident == "FrameList" || seg.ident == "FrameMap";
            }
        false
    }
}

/// `#[derive(Frame)]` — the one-annotation frame entry point (frames.md §2.4):
/// bundles the four component sub-derives and adds `impl Frame`, all emitting
/// `::metor_fsw_2::…` re-export paths. Field/struct attributes: `#[fsw(...)]`
/// (legacy `#[metor_fsw(...)]` accepted).
#[proc_macro_derive(Frame, attributes(fsw, metor_fsw))]
pub fn frame(input: TokenStream) -> TokenStream {
    frame::frame(input)
}

/// `#[derive(SystemInput)]` — generates `SystemInput` (`descriptors`) and
/// `BindPorts` from the bundle's port fields.
#[proc_macro_derive(SystemInput, attributes(fsw))]
pub fn system_input(input: TokenStream) -> TokenStream {
    system::system_input(input)
}

/// `#[derive(SystemOutput)]` — generates `SystemOutput` (`descriptors`/
/// `take_dropped`) and `BindPorts` from the bundle's port fields. Field attribute:
/// `#[fsw(telemetered = false)]` opts the output out of the downlink/`AllOutputs`
/// tap (the `CommandOut<M>` type token is recognized as the same opt-out).
#[proc_macro_derive(SystemOutput, attributes(fsw))]
pub fn system_output(input: TokenStream) -> TokenStream {
    system::system_output(input)
}

/// `export_system!(MySystem);` — generates the `#[unsafe(no_mangle)] extern "C"`
/// `fsw_*` surface (dl-open.md §2/§3) of a `dlopen`-loadable system `cdylib`, each
/// body a one-liner delegating to a `metor_fsw_2::abi::run_*` helper. `MySystem`'s
/// `Params` must be `Serialize + Deserialize + Schema` (the postcard params contract).
///
/// This is the hand-written escape hatch; `#[system(export)]` emits the same surface
/// (gated) for macro-authored systems.
#[proc_macro]
pub fn export_system(input: TokenStream) -> TokenStream {
    export::export_system(input)
}

/// `#[sequence]` / `#[sequence(name = "…")]` — turns an `async fn` whose parameters are
/// `Input<T>`/`Output<T>` ports into a complete dl-loadable sequence occupant
/// (sequences-slots.md §4): a future-driven state machine plus the `fsw_*` C-ABI
/// exports (delegating to `metor_fsw_2::abi::run_seq_*`). The ports are read off the
/// signature and **moved into the future**; the macro appends the implicit
/// `SlotControlIn` input and the `SequenceStatus` + health/log output tail. `name`
/// defaults to the fn name.
///
/// The ring-backing generic is **injected** (review E7): a plain
/// `async fn seq(mut x: Input<T>, …)` gets a hidden `__B: Backing` parameter and each
/// port type is rewritten to `Input<T, __B>`; a hand-written `<B: Backing>` is still
/// accepted. A sequence may be paramless (`Params = ()`) or take one params parameter
/// (`Serialize + Deserialize + Schema`, the postcard contract).
#[proc_macro_attribute]
pub fn sequence(attr: TokenStream, item: TokenStream) -> TokenStream {
    sequence::sequence(attr, item)
}

/// `#[system]` — the one-annotation system author surface
/// (`docs/design-system-macro.md`). Annotates a type's **inherent impl block** and
/// derives everything else from the method signatures:
///
/// - `fn execute(&mut self, now: Timestamp, …ports)` ⇒ a [`CyclicSystem`]; an
///   `async fn run(&mut self, …ports)` ⇒ an [`AsyncSystem`] (exactly one of the two).
/// - Port parameters are `&mut Input<T>` / `&mut MsgIn<M>` / `&mut Output<T>` /
///   `&mut MsgOut<M>` / `&mut CommandOut<M>`, plus at most one `&mut HealthPort`.
///   Descriptor order = signature order within each direction.
/// - `fn new(params: P) -> Self` (or `fn new() -> Self`, or absent ⇒ `Self: Default`)
///   drives `BuildSystem`.
/// - Optional `fn init`/`fn shutdown` taking output ports (by name) and/or
///   `&mut HealthPort`.
///
/// Arguments: `#[system(name = "…")]` overrides the wiring name (default: the type
/// ident, `System` suffix stripped, snake_cased); `#[system(export)]` /
/// `#[system(export = "feature")]` emit the `fsw_*` dl ABI (gated on `not(test)`,
/// plus the named cargo feature).
#[proc_macro_attribute]
pub fn system(attr: TokenStream, item: TokenStream) -> TokenStream {
    system_attr::system_impl(attr.into(), item.into()).into()
}

/// Resolves the root path of the `metor-fsw-2` framework crate in the consumer's
/// namespace (`crate` when metor-fsw-2 compiles its own tests). Hard error when the
/// consumer does not depend on `metor-fsw-2` — every macro here emits its paths.
pub(crate) fn metor_fsw_2_crate_name() -> proc_macro2::TokenStream {
    match crate_name("metor-fsw-2") {
        Ok(FoundCrate::Itself) => quote!(crate),
        Ok(FoundCrate::Name(name)) => {
            let ident = Ident::new(&name, Span::call_site());
            quote!( #ident )
        }
        // In-crate unit tests of the expansion functions run without a consumer
        // manifest; outside tests a missing dep is a real authoring error.
        Err(_) if cfg!(test) => quote!(metor_fsw_2),
        Err(e) => panic!("metor-fsw-2 macros require `metor-fsw-2` in [dependencies]: {e}"),
    }
}
