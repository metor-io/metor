//! `#[derive(SystemInput)]` / `#[derive(SystemOutput)]` (system.md §2, Q5).
//!
//! A system's input/output bundle is a named struct of `Input<F>` / `Output<F>` /
//! `MsgIn<M>` / `MsgOut<M>` ports (plus capability fields like `AllOutputs`).
//! These derives generate the static `decls()` (and, for outputs,
//! `take_dropped()`) by delegating to each field type's own
//! `decl()`/`take_dropped()` — so the macro never has to parse `F` out of the
//! field type, and a capability field rides the same walk (its `decl()` is a
//! `PortDecl::Capability`; its `bind` consumes no ring).
//!
//! ## `#[fsw(...)]` field attributes
//!
//! - `#[fsw(telemetered = false)]` (outputs) — the downlink / `AllOutputs` opt-out
//!   (A6); lowers to `.untelemetered()` on the descriptor. The `CommandOut<M>` type
//!   token is recognized as sugar for exactly this on a `MsgOut<M>`.
//!
//! A `PhantomData` field is skipped everywhere (and default-constructed by `bind`),
//! so a hand-written bundle may carry one for its own generics.

use darling::FromDeriveInput;
use darling::ast;
use darling::util::Ignored;
use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{DeriveInput, Generics, Ident};

/// Which derive is running — the two directions accept different `#[fsw(...)]`
/// keys (`telemetered` is output-only).
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

    /// Validate the attributes against the derive direction.
    fn validate(&self, dir: Dir) -> Result<(), syn::Error> {
        if dir == Dir::Input && self.telemetered.is_some() {
            return Err(syn::Error::new_spanned(
                &self.ty,
                "`#[fsw(telemetered = ..)]` applies to output ports only \
                 (an input is never downlinked)",
            ));
        }
        Ok(())
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
fn decls_body(bundle: &Bundle, _fsw2: &TokenStream2) -> TokenStream2 {
    let calls = bundle.ports().into_iter().map(|f| {
        let ty = &f.ty;
        let mut decl = quote! { <#ty>::decl() };
        if f.is_command_out() || f.telemetered == Some(false) {
            decl = quote! { #decl.untelemetered() };
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
/// port up with the ring the coordinator reserved for it. `PhantomData` anchors
/// are default-constructed (they consume no ring).
fn bind_body(bundle: &Bundle, _fsw2: &TokenStream2) -> TokenStream2 {
    let fields = bundle.data.as_ref().take_struct().expect("named struct");
    let binds = fields.iter().map(|f| {
        let id = f.ident.as_ref().expect("named field");
        let ty = &f.ty;
        if f.is_phantom() {
            quote! { #id: ::core::marker::PhantomData }
        } else {
            quote! { #id: <#ty>::bind(src) }
        }
    });
    quote! {
        Self { #(#binds),* }
    }
}

/// Emit the `BindPorts` impl for a bundle. Rings are backing-erased, so the one
/// impl serves the host binder and a dlopen'd system's raw binder alike.
fn bind_ports_impl(bundle: &Bundle, fsw2: &TokenStream2, bind: &TokenStream2) -> TokenStream2 {
    let ident = &bundle.ident;
    let (impl_generics, ty_generics, where_clause) = bundle.generics.split_for_impl();
    quote! {
        impl #impl_generics #fsw2::BindPorts for #ident #ty_generics #where_clause {
            fn bind<__S: #fsw2::RingSource>(src: &mut __S) -> Self {
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
    let (impl_generics, ty_generics, where_clause) = bundle.generics.split_for_impl();
    let decls = decls_body(&bundle, &fsw2);
    let bind = bind_body(&bundle, &fsw2);
    let bind_ports = bind_ports_impl(&bundle, &fsw2, &bind);

    quote! {
        impl #impl_generics #fsw2::SystemInput for #ident #ty_generics #where_clause {
            fn decls() -> ::std::vec::Vec<#fsw2::PortDecl> {
                #decls
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
    let (impl_generics, ty_generics, where_clause) = bundle.generics.split_for_impl();
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

    /// The input derive delegates every field to its type's `decl()`/`bind(src)`,
    /// in field order.
    #[test]
    fn input_delegates_decl_and_bind() {
        let out = input_ok(quote! {
            struct GuardIn {
                cmds: MsgIn<GuardCmd>,
                imu: Input<Imu>,
            }
        });
        assert!(out.contains("decls.push(<MsgIn<GuardCmd>>::decl());"), "{out}");
        assert!(out.contains("decls.push(<Input<Imu>>::decl());"), "{out}");
        assert!(out.contains("cmds: <MsgIn<GuardCmd>>::bind(src)"), "{out}");
        assert!(out.contains("imu: <Input<Imu>>::bind(src)"), "{out}");
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

    /// Direction misuse is a compile error, not a silent no-op; an unknown
    /// `#[fsw(...)]` key errs via darling.
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

        // Unknown key (darling), surfaced as a compile error — never a macro panic.
        let out = err(false, quote! {
            struct A { #[fsw(telemetred = false)] x: MsgOut<M> }
        });
        assert!(out.contains("compile_error"), "{out}");
    }
}
