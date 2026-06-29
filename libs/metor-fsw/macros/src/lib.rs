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
mod from_kdl;
mod metadatatize;
mod sequence;
mod system;

#[derive(Debug, FromField)]
#[darling(attributes(metor_fsw))]
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
    /// explicitly `#[metor_fsw(nest)]`, or a dynamic `FrameList`/`FrameMap` whose
    /// slot carries no in-struct value.
    pub fn is_nested(&self) -> bool {
        self.nest || self.is_dynamic()
    }

    /// Whether the field type's outermost path segment is `FrameList`/`FrameMap`.
    /// Used to size the trailer in `Componentize::MAX_SIZE` and to skip the
    /// (slot-only) field on the scalar Componentize/Decomponentize paths.
    pub fn is_dynamic(&self) -> bool {
        if let syn::Type::Path(p) = &self.ty {
            if let Some(seg) = p.path.segments.last() {
                return seg.ident == "FrameList" || seg.ident == "FrameMap";
            }
        }
        false
    }
}

#[proc_macro_derive(Metadatatize, attributes(metor_fsw))]
pub fn metadatize(input: TokenStream) -> TokenStream {
    metadatatize::metadatatize(input)
}

#[proc_macro_derive(AsVTable, attributes(metor_fsw))]
pub fn as_vtable(input: TokenStream) -> TokenStream {
    as_vtable::as_vtable(input)
}

#[proc_macro_derive(Componentize, attributes(metor_fsw))]
pub fn componentize(input: TokenStream) -> TokenStream {
    componentize::componentize(input)
}

#[proc_macro_derive(Decomponentize, attributes(metor_fsw))]
pub fn decomponentize(input: TokenStream) -> TokenStream {
    decomponentize::decomponentize(input)
}

#[proc_macro_derive(Frame, attributes(metor_fsw))]
pub fn frame(input: TokenStream) -> TokenStream {
    frame::frame(input)
}

#[proc_macro_derive(SystemInput)]
pub fn system_input(input: TokenStream) -> TokenStream {
    system::system_input(input)
}

#[proc_macro_derive(SystemOutput)]
pub fn system_output(input: TokenStream) -> TokenStream {
    system::system_output(input)
}

/// `export_system!(MySystem);` — generates the `#[unsafe(no_mangle)] extern "C"`
/// `fsw_*` surface (dl-open.md §2/§3) of a `dlopen`-loadable system `cdylib`, each
/// body a one-liner delegating to a `metor_fsw_2::abi::run_*` helper. `MySystem`'s
/// `Params` must be `Serialize + Deserialize + Schema` (the postcard params contract).
#[proc_macro]
pub fn export_system(input: TokenStream) -> TokenStream {
    export::export_system(input)
}

/// `#[sequence]` / `#[sequence(name = "…")]` — turns an `async fn` whose parameters are
/// `Input<T, B>`/`Output<T, B>` ports into a complete dl-loadable sequence occupant
/// (sequences-slots.md §4): a future-driven state machine plus the `fsw_*` C-ABI
/// exports (delegating to `metor_fsw_2::abi::run_seq_*`, the sequence twins of the
/// `run_*` helpers `export_system!` uses). The ports are read off the signature and
/// **moved into the future**; the macro appends the implicit `SlotControlIn` input and
/// the `SequenceStatus` + health/log output tail. `name` defaults to the fn name. A
/// sequence may be paramless (`Params = ()`) or take one params parameter
/// (`Serialize + Deserialize + Schema`, the postcard contract).
#[proc_macro_attribute]
pub fn sequence(attr: TokenStream, item: TokenStream) -> TokenStream {
    sequence::sequence(attr, item)
}

/// Derives [`FromKdlNode`] for a system's params struct: a flat struct of
/// scalars/strings deserialized from a `system` node's KDL properties (wiring.md
/// §3). `Option<T>` fields are optional; `#[kdl(default = expr)]` supplies a
/// fallback; every other field is required.
#[proc_macro_derive(FromKdlNode, attributes(kdl))]
pub fn from_kdl_node(input: TokenStream) -> TokenStream {
    from_kdl::from_kdl_node(input)
}

pub(crate) fn metor_fsw_crate_name() -> proc_macro2::TokenStream {
    let name = crate_name("metor-fsw").expect("metor-fsw is present in `Cargo.toml`");

    match name {
        FoundCrate::Itself => quote!(crate),
        FoundCrate::Name(name) => {
            let ident = Ident::new(&name, Span::call_site());
            quote!( #ident )
        }
    }
}

/// Resolves the path to the `metor-fsw-2` framework crate (which defines the
/// `Frame` trait). Used only by the `Frame` derive so the macro crate needs no
/// Cargo dependency on `metor-fsw-2`.
pub(crate) fn metor_fsw_2_crate_name() -> proc_macro2::TokenStream {
    match crate_name("metor-fsw-2") {
        Ok(FoundCrate::Itself) => quote!(crate),
        Ok(FoundCrate::Name(name)) => {
            let ident = Ident::new(&name, Span::call_site());
            quote!( #ident )
        }
        Err(_) => quote!(metor_fsw_2),
    }
}
