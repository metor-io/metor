//! Expands `#[derive(SystemParams)]`.
//!
//! The struct's fields are params. The derive implements `Param` for the
//! struct with its lifetime set to `'static`, and builds the struct each
//! cycle from its fields' items.

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::quote;
use syn::{Data, DeriveInput, Fields, GenericParam, Ident, Lifetime, LitBool, LitStr, Type};

use crate::key::key_type;

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

/// One field of the struct.
struct Field {
    ident: Ident,
    ty: Type,
    /// The segment the field adds to its ports' paths, or `None` when
    /// flattened.
    segment: Option<String>,
    skip: bool,
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
        self.fields.iter().filter(|field| !field.skip)
    }

    /// The struct literal, with param field `i` set to `value(key, i)` and
    /// skipped fields defaulted.
    fn build(&self, value: impl Fn(&Type, syn::Index) -> TokenStream2) -> TokenStream2 {
        let mut params = self.keys.iter().enumerate();
        let inits = self.fields.iter().map(|field| {
            let ident = &field.ident;
            match (!field.skip).then(|| params.next()).flatten() {
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
        .filter(|f| !f.skip)
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
        .map(|f| match &f.segment {
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

fn field(field: &syn::Field) -> syn::Result<Field> {
    let Some(ident) = field.ident.clone() else {
        return Err(syn::Error::new_spanned(field, "a field needs a name"));
    };
    let name = ident.to_string();
    let mut parsed = Field {
        skip: name.starts_with('_'),
        segment: Some(name),
        ident,
        ty: field.ty.clone(),
    };
    for attr in field.attrs.iter().filter(|a| a.path().is_ident("fsw")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("flatten") {
                parsed.segment = None;
            } else if meta.path.is_ident("rename") {
                parsed.segment = Some(meta.value()?.parse::<LitStr>()?.value());
            } else if meta.path.is_ident("skip") {
                parsed.skip =
                    !meta.input.peek(syn::Token![=]) || meta.value()?.parse::<LitBool>()?.value;
            } else {
                return Err(meta.error("expected `flatten`, `rename`, or `skip`"));
            }
            Ok(())
        })?;
    }
    Ok(parsed)
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
    fn test_rejects_bad_structs() {
        let cases = [
            quote! { struct S<'a, 'b> { x: &'a Input<A>, y: &'b Input<B> } },
            quote! { struct S<'a, T> { x: &'a Input<T> } },
            quote! { struct S<'a>(&'a Input<A>); },
            quote! { enum S { A } },
            quote! { #[system_params(sync)] struct S { x: Timestamp } },
            quote! { struct S { #[fsw(nest)] x: Timestamp } },
        ];
        for case in cases {
            assert!(expanded(case.clone()).is_err(), "{case}");
        }
    }
}
