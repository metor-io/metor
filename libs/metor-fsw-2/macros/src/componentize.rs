use proc_macro2::TokenStream as TokenStream2;
use quote::quote;

use crate::frame::FrameArgs;

/// Generates the `Componentize` impl. `crate_name` is the path prefix the
/// generated code uses to name the trait surface, so the same expansion works
/// from whichever crate re-exports it.
pub fn componentize_impl(args: &FrameArgs, crate_name: &TokenStream2) -> TokenStream2 {
    let FrameArgs {
        ident,
        generics,
        fields,
        frame_name: parent,
        ..
    } = args;
    let where_clause = &generics.where_clause;
    let impeller = quote! { #crate_name::metor_proto };

    // Scalar fields emit one component apiece. Nested and dynamic fields
    // recurse through their own `sink_columns`; a `FrameList`/`FrameMap` slot
    // holds no in-struct value, so its call is a no-op. The timestamp field
    // supplies the frame's time and is never emitted as a component.
    let sink_calls = fields
        .iter()
        .filter(|f| !f.timestamp && !f.skipped())
        .map(|field| {
            let ident = field.ident.as_ref().expect("only named fields allowed");
            if field.is_nested() {
                quote! { self.#ident.sink_columns(output); }
            } else {
                let component_id = field.qualified_component_name(parent.as_deref());
                quote! {
                    let _ = output.apply_value(
                        #impeller::types::ComponentId::new(#component_id),
                        self.#ident.as_component_view(),
                        None,
                    );
                }
            }
        });

    // MAX_SIZE covers the fixed region (`size_of::<Self>()` already counts
    // each 8-byte dynamic slot), every dynamic field's trailer budget, and an
    // 8-byte alignment pad.
    let dyn_budgets = fields.iter().filter(|f| f.is_dynamic()).map(|field| {
        let ty = &field.ty;
        quote! { + <#ty as #crate_name::Componentize>::MAX_SIZE }
    });

    quote! {
        impl #crate_name::Componentize for #ident #generics #where_clause {
            fn sink_columns(&self, output: &mut impl #crate_name::Decomponentize) {
                use #impeller::com_de::AsComponentView;
                #(#sink_calls)*
            }

            const MAX_SIZE: usize = core::mem::size_of::<Self>() #(#dyn_budgets)* + 8;
        }
    }
}
