//! Derives for system port bundles: `#[derive(SystemInput)]` and
//! `#[derive(SystemOutput)]`.
//!
//! A bundle is a named struct whose fields are port types (`Input<F>`,
//! `Output<F>`, `MsgIn<M>`, `MsgOut<M>`) or capability fields such as
//! `AllOutputs`. The derives never parse a payload type out of a field.
//! Instead they delegate to the field type itself: the generated `decls()`
//! pushes each field's `decl()` and the generated `bind` calls each field's
//! `bind(src)`, both in declaration order. That symmetry is what makes
//! positional binding work, since the ring source hands out rings by cursor
//! and each `bind(src)` call lands on the ring reserved for the matching
//! declaration. Capability fields ride the same walk without disturbing it;
//! their `decl()` contributes to `Declarations::capabilities` and their `bind`
//! consumes no ring.
//!
//! A `PhantomData` field is skipped by both walks and default-constructed by
//! `bind`, so a hand-written bundle can carry one for its own generics.
//!
//! Two field attributes are recognized, both output-only.
//! `#[fsw(telemetered = false)]` excludes an output port from telemetry,
//! lowering to `.untelemetered()` on the port's descriptor; the
//! `CommandOut<M>` type alias is sugar for exactly this on a `MsgOut<M>`
//! (the alias carries no flag of its own, so the derive spots the type
//! token and applies the override). `#[fsw(snapshot)]` marks a message
//! channel latest-wins, lowering to `.with_delivery(Delivery::Snapshot)` —
//! the downlink retains such a channel's newest record for late-joining
//! link connections instead of streaming it as an event log.

use darling::FromDeriveInput;
use darling::ast;
use darling::util::Ignored;
use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{DeriveInput, Generics, Ident};

/// Which derive is running. The two directions accept different `#[fsw(...)]`
/// keys; `telemetered` is output-only.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dir {
    Input,
    Output,
}

/// One field of the struct being derived, carrying its type and any parsed
/// `#[fsw(...)]` attribute values.
#[derive(Debug, darling::FromField)]
#[darling(attributes(fsw))]
struct BundleField {
    ident: Option<Ident>,
    ty: syn::Type,
    /// `#[fsw(telemetered = false)]`, the output-only telemetry opt-out.
    #[darling(default)]
    telemetered: Option<bool>,
    /// `#[fsw(snapshot)]`, the output-only latest-wins delivery marker.
    #[darling(default)]
    snapshot: darling::util::Flag,
}

impl BundleField {
    /// Whether this field is a `PhantomData` anchor rather than a port.
    fn is_phantom(&self) -> bool {
        type_head(&self.ty).is_some_and(|h| h == "PhantomData")
    }

    /// Whether the field's type token is the `CommandOut` sugar for an
    /// untelemetered `MsgOut`.
    fn is_command_out(&self) -> bool {
        type_head(&self.ty).is_some_and(|h| h == "CommandOut")
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
        if dir == Dir::Input && self.snapshot.is_present() {
            return Err(syn::Error::new_spanned(
                &self.ty,
                "`#[fsw(snapshot)]` applies to output ports only",
            ));
        }
        Ok(())
    }
}

fn type_head(ty: &syn::Type) -> Option<&Ident> {
    let syn::Type::Path(path) = ty else {
        return None;
    };
    path.path.segments.last().map(|segment| &segment.ident)
}

/// The struct being derived, reduced to its name, generics, and
/// [`BundleField`] list. Both derives share this one receiver; what differs
/// per direction is checked by [`Bundle::validate`].
#[derive(Debug, FromDeriveInput)]
#[darling(supports(struct_named), attributes(fsw))]
struct Bundle {
    ident: Ident,
    generics: Generics,
    data: ast::Data<Ignored, BundleField>,
}

impl Bundle {
    /// The port fields in declaration order, with `PhantomData` anchors
    /// skipped.
    fn ports(&self) -> impl Iterator<Item = &BundleField> {
        self.data
            .as_ref()
            .take_struct()
            .expect("named struct")
            .into_iter()
            .filter(|f| !f.is_phantom())
    }

    /// Validate every field's attributes for this direction, folding all
    /// errors into one so the caller reports them together.
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

/// Emits the `decls()` body: each field type's `decl()` in field order, with
/// the telemetry override chained on where the attribute or the `CommandOut`
/// sugar asks for it.
fn decls_body(bundle: &Bundle, fsw2: &TokenStream2) -> TokenStream2 {
    let calls = bundle.ports().map(|f| {
        let ty = &f.ty;
        let mut decl = quote! { <#ty>::decl() };
        if f.is_command_out() || f.telemetered == Some(false) {
            decl = quote! { #decl.untelemetered() };
        }
        if f.snapshot.is_present() {
            decl = quote! { #decl.with_delivery(#fsw2::Delivery::Snapshot) };
        }
        quote! { declarations.push(#decl); }
    });
    quote! {
        let mut declarations = #fsw2::Declarations::default();
        #(#calls)*
        declarations
    }
}

/// Emits the `BindPorts::bind` body: each port type's `bind(src)` in field
/// order, mirroring [`decls_body`] so positional binding lines up (see the
/// module doc). `PhantomData` anchors are default-constructed and consume no
/// ring.
fn bind_body(bundle: &Bundle) -> TokenStream2 {
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

/// Emit the `BindPorts` impl for a bundle. The impl is generic over the ring
/// source, so it serves any binder that can hand out rings.
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

/// The `TokenStream2` body of `#[derive(SystemInput)]`, split out so the unit
/// tests can call it directly.
fn expand_input(parsed: DeriveInput) -> TokenStream2 {
    // A darling parse failure (say, an unknown `#[fsw(...)]` key) becomes a
    // compile error on the offending tokens rather than a proc-macro panic.
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
    let bind = bind_body(&bundle);
    let bind_ports = bind_ports_impl(&bundle, &fsw2, &bind);

    quote! {
        impl #impl_generics #fsw2::SystemInput for #ident #ty_generics #where_clause {
            fn decls() -> #fsw2::Declarations {
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

/// The `TokenStream2` body of `#[derive(SystemOutput)]`, split out so the unit
/// tests can call it directly.
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
    let bind = bind_body(&bundle);
    let bind_ports = bind_ports_impl(&bundle, &fsw2, &bind);

    // take_dropped sums and clears every output port's publish-drop counter.
    let dropped = bundle.ports().map(|f| {
        let id = f.ident.as_ref().expect("named field");
        quote! { + self.#id.take_dropped() }
    });

    quote! {
        impl #impl_generics #fsw2::SystemOutput for #ident #ty_generics #where_clause {
            fn decls() -> #fsw2::Declarations {
                #decls
            }
            fn take_dropped(&mut self) -> u64 {
                0 #(#dropped)*
            }
        }

        #bind_ports
    }
}

// --- Unit tests: attribute lowering and the direction errors, checked over
// `TokenStream2` without compiling the expansion ---

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

    /// The input derive delegates every field to its type's `decl()` and
    /// `bind(src)`, in field order.
    #[test]
    fn input_delegates_decl_and_bind() {
        let out = input_ok(quote! {
            struct GuardIn {
                cmds: MsgIn<GuardCmd>,
                imu: Input<Imu>,
            }
        });
        assert!(
            out.contains("declarations.push(<MsgIn<GuardCmd>>::decl());"),
            "{out}"
        );
        assert!(
            out.contains("declarations.push(<Input<Imu>>::decl());"),
            "{out}"
        );
        assert!(out.contains("cmds: <MsgIn<GuardCmd>>::bind(src)"), "{out}");
        assert!(out.contains("imu: <Input<Imu>>::bind(src)"), "{out}");
    }

    /// `#[fsw(telemetered = false)]` and the `CommandOut` type token both
    /// lower to `.untelemetered()` on the descriptor, and only there; bind is
    /// untouched.
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
        assert!(
            out.contains("<Output<Nav>>::decl().untelemetered()"),
            "{out}"
        );
        assert!(
            out.contains("<CommandOut<SequenceCommand>>::decl().untelemetered()"),
            "{out}"
        );
        assert!(
            out.contains("declarations.push(<Output<Heartbeat>>::decl());"),
            "{out}"
        );
        // Bind never chains a telemetry override.
        assert!(out.contains("nav: <Output<Nav>>::bind(src)"), "{out}");
        assert!(!out.contains("bind(src).untelemetered"), "{out}");
    }

    /// Direction misuse is a compile error, not a silent no-op, and an
    /// unknown `#[fsw(...)]` key errs via darling.
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
        let out = err(
            true,
            quote! {
                struct A { #[fsw(telemetered = false)] x: MsgIn<M> }
            },
        );
        assert!(out.contains("applies to output ports only"), "{out}");

        // An unknown key surfaces as a compile error, never a macro panic.
        let out = err(
            false,
            quote! {
                struct A { #[fsw(telemetred = false)] x: MsgOut<M> }
            },
        );
        assert!(out.contains("compile_error"), "{out}");
    }
}
