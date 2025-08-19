use std::borrow::Cow;

use impeller2_wkt::ComponentMetadata;

use crate::path::ComponentPath;

pub trait Metadatatize {
    fn metadata(prefix: impl ComponentPath) -> impl Iterator<Item = ComponentMetadata> {
        std::iter::once(prefix.to_metadata())
    }
}

macro_rules! impl_metadatatize {
    ($($ty:tt),+) => {
        impl<$($ty),*> Metadatatize for ($($ty,)*)
        where
            $($ty: Metadatatize),+
        {
            fn metadata(p: impl ComponentPath) -> impl Iterator<Item = ComponentMetadata> {
                std::iter::empty()
                $(
                .chain(<$ty>::metadata(p.clone()))
                )*
            }
        }
    };
}

impl_metadatatize!(T1);
impl_metadatatize!(T1, T2);
impl_metadatatize!(T1, T2, T3);
impl_metadatatize!(T1, T2, T3, T4);
impl_metadatatize!(T1, T2, T3, T4, T5);
impl_metadatatize!(T1, T2, T3, T4, T5, T6);
impl_metadatatize!(T1, T2, T3, T4, T5, T6, T7);
impl_metadatatize!(T1, T2, T3, T4, T5, T6, T7, T8);
impl_metadatatize!(T1, T2, T3, T4, T5, T6, T7, T9, T10);
impl_metadatatize!(T1, T2, T3, T4, T5, T6, T7, T9, T10, T11);
impl_metadatatize!(T1, T2, T3, T4, T5, T6, T7, T9, T10, T11, T12);
impl_metadatatize!(T1, T2, T3, T4, T5, T6, T7, T9, T10, T11, T12, T13);
impl_metadatatize!(T1, T2, T3, T4, T5, T6, T7, T9, T10, T11, T12, T13, T14);
impl_metadatatize!(T1, T2, T3, T4, T5, T6, T7, T9, T10, T11, T12, T13, T14, T15);
impl_metadatatize!(
    T1, T2, T3, T4, T5, T6, T7, T9, T10, T11, T12, T13, T14, T15, T16
);
impl_metadatatize!(
    T1, T2, T3, T4, T5, T6, T7, T9, T10, T11, T12, T13, T14, T15, T16, T17
);
impl_metadatatize!(
    T1, T2, T3, T4, T5, T6, T7, T9, T10, T11, T12, T13, T14, T15, T16, T17, T18
);

impl<const N: usize, T> Metadatatize for [T; N]
where
    T: Metadatatize,
{
    fn metadata(path: impl ComponentPath) -> impl Iterator<Item = ComponentMetadata> {
        (0..N).flat_map(move |i| {
            let prefix = i.to_string();
            let path = path.clone().chain(prefix);
            T::metadata(path)
        })
    }
}
