//! Expands `#[derive(SystemParams)]`.
//!
//! The struct's fields are params. The derive implements `Param` for the
//! struct with its lifetime set to `'static`, and builds the struct each
//! cycle from its fields' items.

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::quote;
use darling::FromField;
use metor_component_derive_impl::Field;
use syn::{Data, DeriveInput, Fields, GenericParam, Ident, Lifetime, Type};

use crate::key::{element, element_name, key_type};

/// Expands `#[derive(SystemParams)]`.
pub fn system_params(input: TokenStream) -> TokenStream {
    let expanded = crate::fsw_crate().and_then(|fsw| {
        let input = syn::parse::<DeriveInput>(input)?;
        expand(&input, &fsw)
    });
    match expanded {
        Ok(tokens) => tokens.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

/// What the generated impl is built from.
struct Parts<'a> {
    name: &'a Ident,
    fields: Vec<Field>,
    /// Each param field's `Param` key, in field order.
    keys: Vec<Type>,
    /// `fsw::Param<W>`.
    param: TokenStream2,
}

impl Parts<'_> {
    fn params(&self) -> impl Iterator<Item = &Field> {
        self.fields.iter().filter(|field| !field.skipped())
    }

    /// The struct literal, with param field `i` set to `value(key, i)` and
    /// skipped fields defaulted.
    fn build(&self, value: impl Fn(&Type, syn::Index) -> TokenStream2) -> TokenStream2 {
        let mut params = self.keys.iter().enumerate();
        let inits = self.fields.iter().map(|field| {
            let ident = &field.ident;
            match (!field.skipped()).then(|| params.next()).flatten() {
                Some((i, key)) => {
                    let value = value(key, syn::Index::from(i));
                    quote!(#ident: #value)
                }
                None => quote!(#ident: ::core::default::Default::default()),
            }
        });
        let inits: Vec<_> = inits.collect();
        let name = self.name;
        quote!(#name { #(#inits,)* })
    }
}

fn expand(input: &DeriveInput, fsw: &TokenStream2) -> syn::Result<TokenStream2> {
    let lifetime = lifetime(input)?;
    let wake = wake(input, fsw)?;
    let fields = fields(input)?;
    let keys = fields
        .iter()
        .filter(|f| !f.skipped())
        .map(|f| key_type(&f.ty))
        .collect();
    let parts = Parts {
        name: &input.ident,
        fields,
        keys,
        param: quote!(#fsw::Param<#wake>),
    };
    let target = with_lifetime(&input.ident, lifetime.is_some(), "'static");
    let item = with_lifetime(&input.ident, lifetime.is_some(), "'__p");
    let body = impl_body(&parts, &item, &wake, fsw);
    Ok(quote! {
        impl #fsw::Param<#wake> for #target {
            #body
        }

        impl #fsw::ArrayParam for #target {}
    })
}

fn impl_body(
    parts: &Parts<'_>,
    item: &TokenStream2,
    wake: &TokenStream2,
    fsw: &TokenStream2,
) -> TokenStream2 {
    let Parts { keys, param, .. } = parts;
    let at: Vec<_> = parts
        .params()
        .map(|f| match f.segment(None) {
            Some(segment) => quote!(&at.child(#segment)),
            None => quote!(at),
        })
        .collect();
    let index: Vec<_> = (0..keys.len()).map(syn::Index::from).collect();
    let get =
        parts.build(|key, i| quote!(<#key as #param>::get(&mut input.#i, &mut output.#i, cx)));
    let sampled = parts.build(|_, i| quote!(parts.#i.0));
    quote! {
        type In = (#(<#keys as #param>::In,)*);
        type Out = (#(<#keys as #param>::Out,)*);
        type Item<'__p> = #item;
        const FLATTEN: bool = true;

        #[allow(unused_variables)]
        fn append_defs(
            at: &#fsw::ParamPath<'_>,
            cx: &#fsw::DefCx<'_>,
            defs: &mut #fsw::fn_system::Defs,
        ) -> ::core::result::Result<(), #fsw::DefError> {
            #(<#keys as #param>::append_defs(#at, cx, defs)?;)*
            ::core::result::Result::Ok(())
        }

        #[allow(unused_variables, clippy::unused_unit)]
        fn bind_in(views: &mut #fsw::fn_system::Views<#wake>) -> Self::In {
            (#(<#keys as #param>::bind_in(views),)*)
        }

        #[allow(unused_variables, clippy::unused_unit)]
        fn bind_out(
            at: &#fsw::ParamPath<'_>,
            writers: &mut #fsw::fn_system::Writers,
        ) -> ::core::result::Result<Self::Out, #fsw::MakeError> {
            ::core::result::Result::Ok((#(<#keys as #param>::bind_out(#at, writers)?,)*))
        }

        #[allow(unused_variables)]
        fn get<'__p>(
            input: &'__p mut Self::In,
            output: &'__p mut Self::Out,
            cx: &mut #fsw::Cycle<'__p>,
        ) -> #item {
            #get
        }

        #[allow(unused_variables)]
        fn refresh(input: &mut Self::In) {
            #(<#keys as #param>::refresh(&mut input.#index);)*
        }

        #[allow(unused_variables, clippy::unused_unit)]
        fn get_sampled<'__p>(
            input: &'__p mut Self::In,
            output: &'__p mut Self::Out,
            cx: &mut #fsw::Cycle<'__p>,
        ) -> (#item, impl #fsw::fn_system::Refresh + use<'__p>) {
            let parts = (#(<#keys as #param>::get_sampled(&mut input.#index, &mut output.#index, cx),)*);
            (#sampled, (#(parts.#index.1,)*))
        }
    }
}

/// The struct's one lifetime, if it has one.
fn lifetime(input: &DeriveInput) -> syn::Result<Option<Lifetime>> {
    let mut found = None;
    for param in &input.generics.params {
        match param {
            GenericParam::Lifetime(lt) if found.is_none() => found = Some(lt.lifetime.clone()),
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    "a `SystemParams` struct takes one lifetime and no other generics",
                ));
            }
        }
    }
    Ok(found)
}

/// `NoWake`, or `Notifier` under `#[system_params(async)]`.
fn wake(input: &DeriveInput, fsw: &TokenStream2) -> syn::Result<TokenStream2> {
    let mut is_async = false;
    for attr in input
        .attrs
        .iter()
        .filter(|a| a.path().is_ident("system_params"))
    {
        attr.parse_nested_meta(|meta| match meta.path.is_ident("async") {
            true => {
                is_async = true;
                Ok(())
            }
            false => Err(meta.error("expected `async`")),
        })?;
    }
    Ok(match is_async {
        true => quote!(#fsw::ring::Notifier),
        false => quote!(#fsw::ring::NoWake),
    })
}

/// The struct's named fields, with their `#[fsw(..)]` attributes read.
fn fields(input: &DeriveInput) -> syn::Result<Vec<Field>> {
    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "`SystemParams` derives for a struct",
        ));
    };
    let Fields::Named(named) = &data.fields else {
        return Err(syn::Error::new_spanned(
            &data.fields,
            "a `SystemParams` struct has named fields",
        ));
    };
    named.named.iter().map(field).collect()
}

/// Reads one field's `#[fsw(..)]` attributes, which share the parser of
/// `#[derive(Fields)]` but take only `flatten`, `rename`, and `skip`.
fn field(field: &syn::Field) -> syn::Result<Field> {
    let parsed = Field::from_field(field)?;
    let component = parsed.unit.is_some() || !parsed.metadata.is_empty() || parsed.max.is_some();
    if parsed.timestamp || component {
        let message = "a `SystemParams` field takes only `flatten`, `rename`, and `skip`";
        return Err(syn::Error::new_spanned(field, message));
    }
    if !parsed.skipped() {
        check_field(field, parsed.flatten)?;
    }
    Ok(parsed)
}

/// Params that only a fn argument may be.
const ARGUMENT_ONLY: [&str; 3] = ["Seq", "Timestamp", "Abort"];

/// Params a struct flattens into nothing: each would take its parent's path.
const LEAVES: [&str; 13] = [
    "Input",
    "Output",
    "Sample",
    "Rng",
    "Cap",
    "Alarms",
    "Log",
    "Records",
    "DynInputs",
    "DynOutputs",
    "Timestamp",
    "Abort",
    "Seq",
];

/// Refuses a field type that a `SystemParams` struct cannot hold.
fn check_field(field: &syn::Field, flatten: bool) -> syn::Result<()> {
    let ty = &field.ty;
    let name = element_name(ty);
    let error = |message: String| Err(syn::Error::new_spanned(ty, message));
    if let Some(name) = name.filter(|name| ARGUMENT_ONLY.iter().any(|n| *name == n)) {
        return error(format!(
            "`{name}` is a fn argument, not a `SystemParams` field"
        ));
    }
    if matches!(element(ty), Type::Tuple(_)) {
        return error("a `SystemParams` field cannot be a tuple; use a nested struct".into());
    }
    let leaf = name.is_some_and(|name| LEAVES.iter().any(|n| *name == n));
    if flatten && (leaf || matches!(key_type(ty), Type::Array(_))) {
        return error("`flatten` goes on a field holding a `SystemParams` struct".into());
    }
    Ok(())
}

/// `name<'lt>`, or `name` for a struct without a lifetime.
fn with_lifetime(name: &Ident, has_lifetime: bool, lt: &str) -> TokenStream2 {
    let lt = Lifetime::new(lt, Span::call_site());
    match has_lifetime {
        true => quote!(#name<#lt>),
        false => quote!(#name),
    }
}

#[cfg(test)]
mod tests {
    use quote::quote;

    use super::*;

    /// `text` without whitespace, since token spacing differs.
    fn squash(text: &str) -> String {
        text.split_whitespace().collect()
    }

    fn expanded(item: TokenStream2) -> syn::Result<String> {
        let input = syn::parse2::<DeriveInput>(item)?;
        expand(&input, &quote!(::fsw)).map(|tokens| tokens.to_string())
    }

    #[test]
    fn test_keys_and_paths() {
        let text = expanded(quote! {
            struct Sensors<'a> {
                fss: [&'a mut Input<Fss>; 8],
                #[fsw(rename = "rate")]
                gyro: &'a Sample<Gyro>,
                #[fsw(flatten)]
                more: More<'a>,
                _marker: PhantomData<&'a ()>,
            }
        })
        .expect("expands");
        let has = |tokens: TokenStream2| squash(&text).contains(&squash(&tokens.to_string()));
        assert!(has(
            quote!(impl ::fsw::Param<::fsw::ring::NoWake> for Sensors<'static>)
        ));
        assert!(has(
            quote!(<[Input<Fss>; 8] as ::fsw::Param<::fsw::ring::NoWake>>::append_defs(&at.child("fss"), cx, defs)?;)
        ));
        assert!(has(quote!(&at.child("rate"))));
        assert!(has(
            quote!(<More<'static> as ::fsw::Param<::fsw::ring::NoWake>>::append_defs(at, cx, defs)?;)
        ));
        assert!(has(quote!(_marker: ::core::default::Default::default())));
        assert!(!has(quote!(PhantomData<&'static ()>)), "{text}");
        assert!(has(quote!(impl ::fsw::ArrayParam for Sensors<'static> {})));
    }

    #[test]
    fn test_async_takes_notifier() {
        let text = expanded(quote! {
            #[system_params(async)]
            struct Feeds<'a> { fss: [&'a mut Input<Fss, Notifier>; 8] }
        })
        .expect("expands");
        let impl_ = quote!(Param<::fsw::ring::Notifier> for Feeds<'static>).to_string();
        assert!(squash(&text).contains(&squash(&impl_)), "{text}");
    }

    #[test]
    fn test_skip_opts_back_in() {
        let text = expanded(quote! {
            struct S<'a> { #[fsw(skip = false)] _imu: &'a mut Input<Imu>, #[fsw(skip)] other: u8 }
        })
        .expect("expands");
        assert!(text.contains(&quote!(&at.child("_imu")).to_string()));
        assert!(text.contains(&quote!(other: ::core::default::Default::default()).to_string()));
    }

    #[test]
    fn test_flatten_takes_a_struct() {
        let text = expanded(quote! {
            struct S<'a> { #[fsw(flatten)] inner: Inner<'a>, #[fsw(skip, flatten)] _x: u8 }
        });
        assert!(text.is_ok(), "{text:?}");
    }

    #[test]
    fn test_skipped_fields_are_not_checked() {
        let text = expanded(quote! { struct S { #[fsw(skip)] now: Timestamp } });
        assert!(text.is_ok(), "{text:?}");
    }

    #[test]
    fn test_rejects_bad_structs() {
        let cases = [
            quote! { struct S<'a, 'b> { x: &'a Input<A>, y: &'b Input<B> } },
            quote! { struct S<'a, T> { x: &'a Input<T> } },
            quote! { struct S<'a>(&'a Input<A>); },
            quote! { enum S { A } },
            quote! { #[system_params(sync)] struct S { x: Timestamp } },
            quote! { struct S { #[fsw(nest)] x: Timestamp } },
            quote! { struct S<'a> { seq: &'a Seq } },
            quote! { struct S<'a> { seq: &'a Seq<'a> } },
            quote! { struct S { now: Timestamp } },
            quote! { struct S<'a> { abort: [&'a mut Abort; 2] } },
            quote! { struct S<'a> { pair: (&'a mut Input<A>, &'a mut Input<B>) } },
            quote! { struct S<'a> { #[fsw(flatten)] imu: &'a mut Input<Imu> } },
            quote! { struct S<'a> { #[fsw(flatten)] all: [Inner<'a>; 2] } },
        ];
        for case in cases {
            assert!(expanded(case.clone()).is_err(), "{case}");
        }
    }
}
