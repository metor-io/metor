use std::path::Component;

use impeller2_wkt::ComponentMetadata;
use nox::SpatialTransform;

pub trait Metadatatize {
    fn metadata() -> impl Iterator<Item = ComponentMetadata> {
        std::iter::empty()
    }
}

macro_rules! impl_metadatatize {
    ($($ty:tt),+) => {
        impl<$($ty),*> Metadatatize for ($($ty,)*)
        where
            $($ty: Metadatatize),+
        {
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
    fn metadata() -> impl Iterator<Item = ComponentMetadata> {
        (0..N).flat_map(|i| {
            let prefix = i.to_string();
            T::metadata().map(move |m| m.with_prefix(&prefix))
        })
    }
}
