use quote::{ToTokens, quote};
use syn::{ImplItem, Item, ItemFn, Stmt};

use super::{Kind, expand};

fn expand_fn(item: proc_macro2::TokenStream, kind: Kind) -> ItemFn {
    syn::parse2(expand(item, &quote!(::fsw), kind)).expect("one rewritten fn")
}

/// Returns `item` from the `name` impl inside the rewritten fn's body.
fn impl_item(f: &ItemFn, name: &str, item: &str) -> String {
    f.block
        .stmts
        .iter()
        .filter_map(|stmt| match stmt {
            Stmt::Item(Item::Impl(block)) => Some(block),
            _ => None,
        })
        .filter(|block| {
            let path = &block.trait_.as_ref().expect("a trait impl").1;
            path.segments.last().is_some_and(|s| s.ident == name)
        })
        .flat_map(|block| &block.items)
        .find_map(|found| match found {
            ImplItem::Const(c) if c.ident == item => Some(c.expr.to_token_stream()),
            ImplItem::Type(t) if t.ident == item => Some(t.ty.to_token_stream()),
            _ => None,
        })
        .expect("the impl item")
        .to_string()
}

fn has_impl(f: &ItemFn, name: &str) -> bool {
    f.block.stmts.iter().any(|stmt| {
        matches!(stmt, Stmt::Item(Item::Impl(block)) if block.trait_.as_ref()
            .is_some_and(|(_, path, _)| path.segments.last().is_some_and(|s| s.ident == name)))
    })
}

#[test]
fn test_sync_fn() {
    let f = expand_fn(
        quote! {
            /// One filter cycle.
            pub fn nav(
                #[state] s: &mut NavState,
                now: Timestamp,
                sensors: &mut Input<Sensors>,
                attitude_estimate: &mut Output<AttitudeEstimate>,
            ) {}
        },
        Kind::System,
    );
    assert!(f.sig.inputs.is_empty());
    assert_eq!(impl_item(&f, "Ports", "NAME"), quote!("nav").to_string());
    assert_eq!(
        impl_item(&f, "Ports", "NAMES"),
        quote!(&["now", "sensors", "attitude_estimate"]).to_string()
    );
    assert_eq!(
        impl_item(&f, "Ports", "State"),
        quote!(NavState).to_string()
    );
    assert_eq!(
        impl_item(&f, "Ports", "Params"),
        quote!((Timestamp, Input<Sensors>, Output<AttitudeEstimate>,)).to_string()
    );
    assert_eq!(
        impl_item(&f, "Ports", "DOC"),
        quote!("One filter cycle.").to_string()
    );
    assert!(has_impl(&f, "SystemFn") && has_impl(&f, "Deref"));
    assert!(!f.to_token_stream().to_string().contains("# [state]"));
}

#[test]
fn test_async_fn() {
    let f = expand_fn(
        quote! {
            async fn relay(gps: &mut Input<Gps, Notifier>, #[state] s: &mut Relay, stop: Stop) {}
        },
        Kind::System,
    );
    assert_eq!(
        impl_item(&f, "Ports", "NAMES"),
        quote!(&["gps"]).to_string()
    );
    assert_eq!(impl_item(&f, "Ports", "State"), quote!(Relay).to_string());
    assert!(has_impl(&f, "AsyncSystemFn"));
    assert!(!has_impl(&f, "Deref"));
    assert!(!f.to_token_stream().to_string().contains("# [state]"));
}

#[test]
fn test_generic_fn() {
    let f = expand_fn(
        quote! {
            fn probe<T: Frame>(#[state] seen: &mut Seen<T>, input: &mut Input<T>) {}
        },
        Kind::System,
    );
    assert_eq!(f.sig.generics.params.len(), 1);
    assert_eq!(impl_item(&f, "Ports", "NAME"), quote!("probe").to_string());
    assert_eq!(impl_item(&f, "Ports", "State"), quote!(Seen<T>).to_string());
    assert_eq!(
        impl_item(&f, "Ports", "Params"),
        quote!((Input<T>,)).to_string()
    );
}

#[test]
fn test_stateless_fn() {
    let f = expand_fn(
        quote! {
            #[allow(clippy::too_many_arguments)]
            fn mode(mode_cmd: &mut Output<ModeCmd>) {}
        },
        Kind::System,
    );
    assert_eq!(impl_item(&f, "Ports", "State"), quote!(()).to_string());
    assert_eq!(impl_item(&f, "Ports", "DOC"), quote!("").to_string());
    assert!(f.attrs.is_empty(), "lint attrs go on the body");
}

#[test]
fn test_error_keeps_fn() {
    let out = expand(
        quote! { fn two(#[state] a: &mut A, #[state] b: &mut B) {} },
        &quote!(::fsw),
        Kind::System,
    )
    .to_string();
    assert!(out.contains("fn two"));
    assert!(out.contains("compile_error"));
    assert!(!out.contains("# [state]"));
}

#[test]
fn test_rejects_bad_signatures() {
    let cases = [
        quote! { fn a<'a>(x: &'a mut Input<T>) {} },
        quote! { fn a(x: impl Param) {} },
        quote! { fn a((x, y): (A, B)) {} },
        quote! { fn a() -> u32 { 0 } },
    ];
    for case in cases {
        let out = expand(case.clone(), &quote!(::fsw), Kind::System).to_string();
        assert!(out.contains("compile_error"), "{case} expanded to {out}");
    }
}

#[test]
fn test_sequence_fn() {
    let f = expand_fn(
        quote! {
            /// Arms the motors.
            async fn arm(
                #[state] s: &mut Arm,
                seq: &Seq,
                cmd: &mut Output<Cmd>,
            ) -> Result<(), SeqError> {
                Ok(())
            }
        },
        Kind::Sequence,
    );
    assert_eq!(
        impl_item(&f, "Ports", "NAMES"),
        quote!(&["seq", "cmd"]).to_string()
    );
    assert_eq!(
        impl_item(&f, "Ports", "Params"),
        quote!((Seq<'static>, Output<Cmd>,)).to_string()
    );
    assert_eq!(impl_item(&f, "Ports", "State"), quote!(Arm).to_string());
    assert_eq!(
        impl_item(&f, "Ports", "DOC"),
        quote!("Arms the motors.").to_string()
    );
    assert!(has_impl(&f, "SequenceFn"));
    assert!(!has_impl(&f, "Deref") && !has_impl(&f, "SystemFn"));
}

/// Returns the signature of the rewritten fn's `__metor_body`.
fn body_sig(f: &ItemFn) -> String {
    f.block
        .stmts
        .iter()
        .find_map(|stmt| match stmt {
            Stmt::Item(Item::Fn(body)) if body.sig.ident == "__metor_body" => Some(body),
            _ => None,
        })
        .expect("the body")
        .sig
        .to_token_stream()
        .to_string()
}

#[test]
fn test_sequence_body_shares_the_brand() {
    let f = expand_fn(
        quote! {
            async fn arm(
                #[state] s: &mut Arm,
                seq: &Seq<'_>,
                pc: &Sample<Chamber>,
            ) -> Result<(), SeqError> {
                Ok(())
            }
        },
        Kind::Sequence,
    );
    assert_eq!(
        impl_item(&f, "Ports", "Params"),
        quote!((Seq<'static>, Sample<Chamber>,)).to_string()
    );
    let expected = quote! {
        async fn __metor_body<'__seq>(
            s: &'__seq mut Arm,
            seq: &'__seq Seq<'__seq>,
            pc: &'__seq Sample<Chamber>,
        ) -> Result<(), SeqError>
    };
    assert_eq!(body_sig(&f), expected.to_string());
}

#[test]
fn test_rejects_bad_sequences() {
    let cases = [
        quote! { async fn a(seq: &Seq) -> u32 { 0 } },
        quote! { async fn a(abort: &mut Abort) -> Result<(), SeqError> { Ok(()) } },
    ];
    for case in cases {
        let out = expand(case.clone(), &quote!(::fsw), Kind::Sequence).to_string();
        assert!(out.contains("compile_error"), "{case} expanded to {out}");
    }
}
