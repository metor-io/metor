//! `#[derive(SystemInput)]` / `#[derive(SystemOutput)]` (system.md §2, Q5).
//!
//! A system's input/output bundle is a named struct of `Input<F>` / `Output<F>` /
//! `MsgIn<M>` / `MsgOut<M>` ports (plus capability fields like `AllOutputs`).
//! These derives generate the static `decls()` (and, for inputs, `any_lapped()`;
//! for outputs, `take_dropped()`) by delegating to each field type's own
//! `decl()`/`lap_fault()`/`take_dropped()` — so the macro never has to parse `F`
//! out of the field type, and a capability field rides the same walk (its
//! `decl()` is a `PortDecl::Capability`; its `bind` consumes no ring).
//!
//! ## `#[fsw(...)]` field attributes
//!
//! Axis overrides lower onto **both** generated fns (descriptor + bind), so the
//! static shape and the runtime port can never disagree:
//!
//! - `#[fsw(telemetered = false)]` (outputs) — the downlink / `AllOutputs` opt-out
//!   (A6); lowers to `.untelemetered()` on the descriptor. The `CommandOut<M>` type
//!   token is recognized as sugar for exactly this on a `MsgOut<M>`.
//! - `#[fsw(on_lap = "stop" | "resync")]` (inputs) — the lap policy (axis 4);
//!   lowers to `.with_on_lap(..)` on the descriptor *and* the bound port.
//!
//! A `PhantomData` field is skipped everywhere (and default-constructed by `bind`):
//! `#[system]`'s generated bundles carry one to anchor the `__B: Backing` param when
//! a direction has no ports.

use darling::FromDeriveInput;
use darling::ast;
use darling::util::Ignored;
use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{DeriveInput, Generics, Ident};

/// Which derive is running — the two directions accept different `#[fsw(...)]`
/// keys (`telemetered` is output-only, `on_lap` input-only).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dir {
    Input,
    Output,
}

#[derive(Debug, darling::FromField)]
#[darling(attributes(fsw))]
struct BundleField {
    ident: Option<Ident>,
    ty: syn::Type,
    /// `#[fsw(telemetered = false)]` — output-only downlink opt-out (A6).
    #[darling(default)]
    telemetered: Option<bool>,
    /// `#[fsw(on_lap = "stop" | "resync")]` — input-only lap policy (axis 4).
    #[darling(default)]
    on_lap: Option<syn::LitStr>,
}

impl BundleField {
    /// Whether this field is a `PhantomData` anchor rather than a port.
    fn is_phantom(&self) -> bool {
        crate::sig::type_head(&self.ty).is_some_and(|h| h == "PhantomData")
    }

    /// Whether the field's type token is the `CommandOut` sugar (an untelemetered
    /// `MsgOut` — the alias carries no flag of its own, the macros apply it).
    fn is_command_out(&self) -> bool {
        crate::sig::type_head(&self.ty).is_some_and(|h| h == "CommandOut")
    }

    /// The parsed `on_lap` policy variant ident (`Stop`/`Resync`), or an error on an
    /// unknown value.
    fn on_lap_variant(&self) -> Result<Option<Ident>, syn::Error> {
        let Some(lit) = &self.on_lap else {
            return Ok(None);
        };
        match lit.value().as_str() {
            "stop" => Ok(Some(Ident::new("Stop", lit.span()))),
            "resync" => Ok(Some(Ident::new("Resync", lit.span()))),
            other => Err(syn::Error::new(
                lit.span(),
                format!(r#"unknown on_lap policy `{other}`; expected "stop" or "resync""#),
            )),
        }
    }

    /// Validate the attributes against the derive direction.
    fn validate(&self, dir: Dir) -> Result<(), syn::Error> {
        if dir == Dir::Input && self.telemetered.is_some() {
            return Err(syn::Error::new_spanned(
                &self.ty,
                "`#[fsw(telemetered = ..)]` applies to output ports only \
                 (an input is never downlinked)",
            ));
        }
        if dir == Dir::Output && self.on_lap.is_some() {
            return Err(syn::Error::new_spanned(
                &self.ty,
                "`#[fsw(on_lap = ..)]` applies to input ports only \
                 (the lap policy is a reader concern)",
            ));
        }
        self.on_lap_variant().map(|_| ())
    }
}

#[derive(Debug, FromDeriveInput)]
#[darling(supports(struct_named), attributes(fsw))]
struct Bundle {
    ident: Ident,
    generics: Generics,
    data: ast::Data<Ignored, BundleField>,
}

impl Bundle {
    /// The port fields, in declaration order (`PhantomData` anchors skipped).
    fn ports(&self) -> Vec<&BundleField> {
        self.data
            .as_ref()
            .take_struct()
            .expect("named struct")
            .into_iter()
            .filter(|f| !f.is_phantom())
            .collect()
    }

    /// Validate every field's attributes for this direction, folding all errors.
    fn validate(&self, dir: Dir) -> Result<(), syn::Error> {
        let mut err: Option<syn::Error> = None;
        for f in self.ports() {
            if let Err(e) = f.validate(dir) {
                match &mut err {
                    Some(first) => first.combine(e),
                    None => err = Some(e),
                }
            }
        }
        match err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

/// Emits the `decls()` body: each field type's `decl()` (a wired port, or a
/// capability like `AllOutputs`), in field order, with the `#[fsw(...)]` axis
/// overrides (and the `CommandOut` sugar) chained on. The overrides are
/// `PortDecl`-level so the walk stays type-blind; they only touch `Port` decls.
fn decls_body(bundle: &Bundle, fsw2: &TokenStream2) -> TokenStream2 {
    let calls = bundle.ports().into_iter().map(|f| {
        let ty = &f.ty;
        let mut decl = quote! { <#ty>::decl() };
        if f.is_command_out() || f.telemetered == Some(false) {
            decl = quote! { #decl.untelemetered() };
        }
        if let Ok(Some(v)) = f.on_lap_variant() {
            decl = quote! { #decl.with_on_lap(#fsw2::OnLap::#v) };
        }
        quote! { decls.push(#decl); }
    });
    quote! {
        let mut decls = ::std::vec::Vec::new();
        #(#calls)*
        decls
    }
}

/// Emits the `BindPorts::bind` body: each port type's `bind(src)`, in field order —
/// symmetric to `decls()`, so the ring source's positional cursor lines each
/// port up with the ring the coordinator reserved for it. An `on_lap` override is
/// chained onto the bound port too (descriptor and runtime can never disagree).
/// `PhantomData` anchors are default-constructed (they consume no ring).
fn bind_body(bundle: &Bundle, fsw2: &TokenStream2) -> TokenStream2 {
    let fields = bundle.data.as_ref().take_struct().expect("named struct");
    let binds = fields.iter().map(|f| {
        let id = f.ident.as_ref().expect("named field");
        let ty = &f.ty;
        if f.is_phantom() {
            quote! { #id: ::core::marker::PhantomData }
        } else {
            let mut bind = quote! { <#ty>::bind(src) };
            if let Ok(Some(v)) = f.on_lap_variant() {
                bind = quote! { #bind.with_on_lap(#fsw2::OnLap::#v) };
            }
            quote! { #id: #bind }
        }
    });
    quote! {
        Self { #(#binds),* }
    }
}

/// A copy of `generics` with every default value stripped from its type/const params.
/// Rust forbids defaults in an `impl<…>` header, so a bundle author's
/// `struct PlantOut<B: Backing = BoxBacking>` must shed the `= BoxBacking` before the
/// generated impls reuse its generics (dl-open.md §1.2).
fn strip_defaults(generics: &Generics) -> Generics {
    let mut g = generics.clone();
    for p in g.params.iter_mut() {
        match p {
            syn::GenericParam::Type(t) => t.default = None,
            syn::GenericParam::Const(c) => c.default = None,
            syn::GenericParam::Lifetime(_) => {}
        }
    }
    g
}

/// Emit the `BindPorts<B>` impl for a bundle: over the bundle's own `Backing` param if
/// it has one (detected by bound, `sig::backing_param`), else over `BoxBacking` (the
/// bundle's ports pin `BoxBacking`). Generics are default-stripped for the `impl<…>`
/// header (dl-open.md §1.2).
fn bind_ports_impl(bundle: &Bundle, fsw2: &TokenStream2, bind: &TokenStream2) -> TokenStream2 {
    let ident = &bundle.ident;
    let stripped = strip_defaults(&bundle.generics);
    let (impl_generics, ty_generics, where_clause) = stripped.split_for_impl();
    let backing = match crate::sig::backing_param(&bundle.generics) {
        Some(b) => quote! { #b },
        None => quote! { #fsw2::ring::BoxBacking },
    };
    quote! {
        impl #impl_generics #fsw2::BindPorts<#backing> for #ident #ty_generics #where_clause {
            fn bind<__S: #fsw2::RingSource<B = #backing>>(src: &mut __S) -> Self {
                #bind
            }
        }
    }
}

pub fn system_input(input: TokenStream) -> TokenStream {
    let parsed = syn::parse_macro_input!(input as DeriveInput);
    expand_input(parsed).into()
}

/// The `TokenStream2` body of `#[derive(SystemInput)]` (unit-testable in-crate).
fn expand_input(parsed: DeriveInput) -> TokenStream2 {
    // A darling parse failure (e.g. an unknown `#[fsw(...)]` key) is a compile
    // error on the offending tokens, not a proc-macro panic.
    let bundle = match Bundle::from_derive_input(&parsed) {
        Ok(b) => b,
        Err(e) => return e.write_errors(),
    };
    if let Err(e) = bundle.validate(Dir::Input) {
        return e.to_compile_error();
    }
    let fsw2 = crate::metor_fsw_2_crate_name();
    let ident = &bundle.ident;
    // `SystemInput` is non-generic in `B`, but the bundle may carry a defaulted
    // `Backing` param: strip defaults for the impl header (dl-open.md §1.2).
    let stripped = strip_defaults(&bundle.generics);
    let (impl_generics, ty_generics, where_clause) = stripped.split_for_impl();
    let decls = decls_body(&bundle, &fsw2);

    // any_lapped: OR every input port's policy-derived `lap_fault()` (A5).
    let lapped = bundle.ports().into_iter().map(|f| {
        let id = f.ident.as_ref().expect("named field");
        quote! { || self.#id.lap_fault() }
    });

    let bind = bind_body(&bundle, &fsw2);
    let bind_ports = bind_ports_impl(&bundle, &fsw2, &bind);

    quote! {
        impl #impl_generics #fsw2::SystemInput for #ident #ty_generics #where_clause {
            fn decls() -> ::std::vec::Vec<#fsw2::PortDecl> {
                #decls
            }
            fn any_lapped(&self) -> bool {
                false #(#lapped)*
            }
        }

        #bind_ports
    }
}

pub fn system_output(input: TokenStream) -> TokenStream {
    let parsed = syn::parse_macro_input!(input as DeriveInput);
    expand_output(parsed).into()
}

/// The `TokenStream2` body of `#[derive(SystemOutput)]` (unit-testable in-crate).
fn expand_output(parsed: DeriveInput) -> TokenStream2 {
    let bundle = match Bundle::from_derive_input(&parsed) {
        Ok(b) => b,
        Err(e) => return e.write_errors(),
    };
    if let Err(e) = bundle.validate(Dir::Output) {
        return e.to_compile_error();
    }
    let fsw2 = crate::metor_fsw_2_crate_name();
    let ident = &bundle.ident;
    let stripped = strip_defaults(&bundle.generics);
    let (impl_generics, ty_generics, where_clause) = stripped.split_for_impl();
    let decls = decls_body(&bundle, &fsw2);
    let bind = bind_body(&bundle, &fsw2);
    let bind_ports = bind_ports_impl(&bundle, &fsw2, &bind);

    // take_dropped: sum-and-clear every output port's publish-drop counter (E6).
    let dropped = bundle.ports().into_iter().map(|f| {
        let id = f.ident.as_ref().expect("named field");
        quote! { + self.#id.take_dropped() }
    });

    quote! {
        impl #impl_generics #fsw2::SystemOutput for #ident #ty_generics #where_clause {
            fn decls() -> ::std::vec::Vec<#fsw2::PortDecl> {
                #decls
            }
            fn take_dropped(&mut self) -> u64 {
                0 #(#dropped)*
            }
        }

        #bind_ports
    }
}

// ---------------------------------------------------------------------------
// Unit tests: `#[fsw(...)]` attribute lowering + the direction/value errors
// (in-crate, over `TokenStream2` — the compile-and-run half lives in
// metor-fsw-2's own test suites, which exercise the generated impls).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use quote::quote;

    use super::*;

    fn input_ok(item: TokenStream2) -> String {
        let parsed: DeriveInput = syn::parse2(item).expect("test bundle parses");
        let out = expand_input(parsed);
        let file: syn::File = syn::parse2(out).expect("expansion parses");
        prettyplease::unparse(&file)
    }

    fn output_ok(item: TokenStream2) -> String {
        let parsed: DeriveInput = syn::parse2(item).expect("test bundle parses");
        let out = expand_output(parsed);
        let file: syn::File = syn::parse2(out).expect("expansion parses");
        prettyplease::unparse(&file)
    }

    /// `#[fsw(on_lap = "…")]` lowers onto BOTH generated fns — the descriptor and
    /// the bound port — so the static shape and the runtime policy cannot disagree.
    #[test]
    fn on_lap_lowers_onto_descriptor_and_bind() {
        let out = input_ok(quote! {
            struct GuardIn {
                #[fsw(on_lap = "stop")]
                cmds: MsgIn<GuardCmd>,
                imu: Input<Imu>,
            }
        });
        assert!(
            out.contains("<MsgIn<GuardCmd>>::decl().with_on_lap(metor_fsw_2::OnLap::Stop)"),
            "{out}"
        );
        assert!(
            out.contains("<MsgIn<GuardCmd>>::bind(src).with_on_lap(metor_fsw_2::OnLap::Stop)"),
            "{out}"
        );
        // The unattributed field expands exactly as before (no chained override).
        assert!(out.contains("decls.push(<Input<Imu>>::decl());"), "{out}");
        assert!(out.contains("imu: <Input<Imu>>::bind(src)"), "{out}");
        // any_lapped ORs the policy-derived lap_fault (A5).
        assert!(out.contains("false || self.cmds.lap_fault() || self.imu.lap_fault()"), "{out}");
    }

    /// `#[fsw(telemetered = false)]` and the `CommandOut` type token both lower to
    /// `.untelemetered()` on the descriptor (bind is untouched — the flag is
    /// descriptor-only).
    #[test]
    fn telemetered_and_command_out_lower_to_untelemetered() {
        let out = output_ok(quote! {
            struct QuietOut {
                #[fsw(telemetered = false)]
                nav: Output<Nav>,
                cmds: CommandOut<SequenceCommand>,
                beat: Output<Heartbeat>,
            }
        });
        assert!(out.contains("<Output<Nav>>::decl().untelemetered()"), "{out}");
        assert!(
            out.contains("<CommandOut<SequenceCommand>>::decl().untelemetered()"),
            "{out}"
        );
        assert!(out.contains("decls.push(<Output<Heartbeat>>::decl());"), "{out}");
        // Bind never chains a telemetry override.
        assert!(out.contains("nav: <Output<Nav>>::bind(src)"), "{out}");
        assert!(!out.contains("bind(src).untelemetered"), "{out}");
    }

    /// Direction misuse and a bad `on_lap` value are compile errors, not silent
    /// no-ops; an unknown `#[fsw(...)]` key errs via darling.
    #[test]
    fn attribute_errors() {
        let err = |dir_input: bool, item: TokenStream2| -> String {
            let parsed: DeriveInput = syn::parse2(item).expect("parses");
            let out = if dir_input {
                expand_input(parsed)
            } else {
                expand_output(parsed)
            };
            out.to_string()
        };

        // telemetered on an input.
        let out = err(true, quote! {
            struct A { #[fsw(telemetered = false)] x: MsgIn<M> }
        });
        assert!(out.contains("applies to output ports only"), "{out}");

        // on_lap on an output.
        let out = err(false, quote! {
            struct A { #[fsw(on_lap = "stop")] x: MsgOut<M> }
        });
        assert!(out.contains("applies to input ports only"), "{out}");

        // Unknown policy value.
        let out = err(true, quote! {
            struct A { #[fsw(on_lap = "panic")] x: MsgIn<M> }
        });
        assert!(out.contains("unknown on_lap policy"), "{out}");

        // Unknown key (darling), surfaced as a compile error — never a macro panic.
        let out = err(false, quote! {
            struct A { #[fsw(telemetred = false)] x: MsgOut<M> }
        });
        assert!(out.contains("compile_error"), "{out}");
    }
}
