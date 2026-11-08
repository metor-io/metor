//! Maps a parameter's type to the type that keys its `Param` impl.

use proc_macro2::Span;
use syn::visit_mut::{self, VisitMut};
use syn::{GenericArgument, Lifetime, PathArguments, Type, TypePath};

/// Returns the type used to look up a parameter's `Param` impl.
///
/// One outer reference is stripped, so `&mut Input<T>` and `Input<T>` get the
/// same key. An array is keyed by its element's key, so `[&mut Input<T>; N]`
/// is `[Input<T>; N]`. Every lifetime in the key becomes `'static`, so
/// `Sensors<'a>` is `Sensors<'static>`, and a bare `Seq` is `Seq<'static>`.
pub(crate) fn key_type(ty: &Type) -> Type {
    let mut key = match ty {
        Type::Reference(r) => (*r.elem).clone(),
        Type::Array(array) => {
            let mut array = array.clone();
            *array.elem = key_type(&array.elem);
            return Type::Array(array);
        }
        other => other.clone(),
    };
    Static.visit_type_mut(&mut key);
    key
}

/// Names every lifetime `'static`.
struct Static;

impl VisitMut for Static {
    fn visit_lifetime_mut(&mut self, lt: &mut Lifetime) {
        *lt = static_lifetime();
    }

    fn visit_type_path_mut(&mut self, path: &mut TypePath) {
        if is_seq(path) {
            set_seq_lifetime(path, &static_lifetime());
        }
        visit_mut::visit_type_path_mut(self, path);
    }
}

fn static_lifetime() -> Lifetime {
    Lifetime::new("'static", Span::call_site())
}

/// Returns whether `path` is `Seq` or `Seq<'_>`.
///
/// The macro fills in the lifetime of those two forms.
pub(crate) fn is_seq(path: &TypePath) -> bool {
    let Some(last) = path.path.segments.last() else {
        return false;
    };
    if path.qself.is_some() || last.ident != "Seq" {
        return false;
    }
    match &last.arguments {
        PathArguments::None => true,
        PathArguments::AngleBracketed(args) => {
            args.args.len() == 1
                && matches!(args.args.first(), Some(GenericArgument::Lifetime(lt)) if lt.ident == "_")
        }
        PathArguments::Parenthesized(_) => false,
    }
}

pub(crate) fn set_seq_lifetime(path: &mut TypePath, lifetime: &Lifetime) {
    if let Some(last) = path.path.segments.last_mut() {
        last.arguments = PathArguments::AngleBracketed(syn::parse_quote!(<#lifetime>));
    }
}
