use quote::{ToTokens, quote};
use syn::{ImplItem, Item, ItemFn, Stmt};

use super::{Kind, expand};

fn expand_fn(item: proc_macro2::TokenStream) -> ItemFn {
    let out = expand(item, &quote!(::fsw), Kind::System);
    syn::parse2(out).expect("one rewritten fn")
}

/// The item of the impl for `name` inside the rewritten fn's body.
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

fn tokens(t: proc_macro2::TokenStream) -> String {
    t.to_string()
}

#[test]
fn test_sync_fn() {
    let f = expand_fn(quote! {
        /// One filter cycle.
        pub fn nav(
            #[state] s: &mut NavState,
            now: Timestamp,
            sensors: &mut Input<Sensors>,
            attitude_estimate: &mut Output<AttitudeEstimate>,
        ) {}
    });
    assert!(f.sig.inputs.is_empty());
    assert_eq!(impl_item(&f, "Ports", "NAME"), tokens(quote!("nav")));
    assert_eq!(
        impl_item(&f, "Ports", "NAMES"),
        tokens(quote!(&["now", "sensors", "attitude_estimate"]))
    );
    assert_eq!(impl_item(&f, "Ports", "State"), tokens(quote!(NavState)));
    assert_eq!(
        impl_item(&f, "Ports", "Params"),
        tokens(quote!((
            Timestamp,
            Input<Sensors>,
            Output<AttitudeEstimate>,
        )))
    );
    assert_eq!(
        impl_item(&f, "Ports", "DOC"),
        tokens(quote!("One filter cycle."))
    );
    assert!(has_impl(&f, "SystemFn") && has_impl(&f, "Deref"));
    assert!(!f.to_token_stream().to_string().contains("# [state]"));
}

#[test]
fn test_async_fn() {
    let f = expand_fn(quote! {
        async fn relay(gps: &mut Input<Gps, Notifier>, #[state] s: &mut Relay, stop: Stop) {}
    });
    assert_eq!(impl_item(&f, "Ports", "NAMES"), tokens(quote!(&["gps"])));
    assert_eq!(impl_item(&f, "Ports", "State"), tokens(quote!(Relay)));
    assert!(has_impl(&f, "AsyncSystemFn"));
    assert!(!has_impl(&f, "Deref"));
    assert!(!f.to_token_stream().to_string().contains("# [state]"));
}

#[test]
fn test_generic_fn() {
    let f = expand_fn(quote! {
        fn probe<T: Frame>(#[state] seen: &mut Seen<T>, input: &mut Input<T>) {}
    });
    assert_eq!(f.sig.generics.params.len(), 1);
    assert_eq!(impl_item(&f, "Ports", "NAME"), tokens(quote!("probe")));
    assert_eq!(impl_item(&f, "Ports", "State"), tokens(quote!(Seen<T>)));
    assert_eq!(
        impl_item(&f, "Ports", "Params"),
        tokens(quote!((Input<T>,)))
    );
}

#[test]
fn test_stateless_fn() {
    let f = expand_fn(quote! {
        #[allow(clippy::too_many_arguments)]
        fn mode(mode_cmd: &mut Output<ModeCmd>) {}
    });
    assert_eq!(impl_item(&f, "Ports", "State"), tokens(quote!(())));
    assert_eq!(impl_item(&f, "Ports", "DOC"), tokens(quote!("")));
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
        quote! { fn a(#[state] s: S) {} },
        quote! { fn a<'a>(x: &'a mut Input<T>) {} },
        quote! { fn a(x: &'static str) {} },
        quote! { fn a(x: impl Param) {} },
        quote! { fn a((x, y): (A, B)) {} },
        quote! { fn a(&mut self) {} },
        quote! { fn a() -> u32 { 0 } },
        quote! { async fn a(x: &mut Input<T>) {} },
    ];
    for case in cases {
        let out = expand(case.clone(), &quote!(::fsw), Kind::System).to_string();
        assert!(out.contains("compile_error"), "{case} expanded to {out}");
    }
}

#[test]
fn test_sequence_fn() {
    let out = expand(
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
        &quote!(::fsw),
        Kind::Sequence,
    );
    let f: ItemFn = syn::parse2(out).expect("one rewritten fn");
    assert_eq!(
        impl_item(&f, "Ports", "NAMES"),
        tokens(quote!(&["seq", "cmd"]))
    );
    assert_eq!(
        impl_item(&f, "Ports", "Params"),
        tokens(quote!((Seq, Output<Cmd>,)))
    );
    assert_eq!(impl_item(&f, "Ports", "State"), tokens(quote!(Arm)));
    assert_eq!(
        impl_item(&f, "Ports", "DOC"),
        tokens(quote!("Arms the motors."))
    );
    assert!(has_impl(&f, "SequenceFn"));
    assert!(!has_impl(&f, "Deref") && !has_impl(&f, "SystemFn"));
}

#[test]
fn test_rejects_bad_sequences() {
    let cases = [
        quote! { fn a(seq: &Seq) -> Result<(), SeqError> { Ok(()) } },
        quote! { async fn a(seq: &Seq, stop: Stop) -> Result<(), SeqError> { Ok(()) } },
        quote! { async fn a(seq: &Seq) {} },
        quote! { async fn a(seq: &Seq) -> u32 { 0 } },
        quote! { async fn a(seq: &Seq, now: Timestamp) -> Result<(), SeqError> { Ok(()) } },
        quote! { async fn a(abort: &mut Abort) -> Result<(), SeqError> { Ok(()) } },
    ];
    for case in cases {
        let out = expand(case.clone(), &quote!(::fsw), Kind::Sequence).to_string();
        assert!(out.contains("compile_error"), "{case} expanded to {out}");
    }
}

#[test]
fn test_system_rejects_seq() {
    let out = expand(quote! { fn a(seq: &Seq) {} }, &quote!(::fsw), Kind::System).to_string();
    assert!(out.contains("compile_error"), "{out}");
}
