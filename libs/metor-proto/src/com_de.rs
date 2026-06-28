//! Contains traits for converting Rust types to and form tables
//!
//! Componentize and Decomponentize are impeller's conceptual equivalent to [`serde::Serialize`] and [`serde::Deserialize`].
//! They allow you to read data from a table into a type, and write data from a type into a table.
//! They are designed to be chained together. So you can use `sink_columns` to write data into a type that implements [`Decomponentize`].

use crate::{
    error::Error,
    types::{ComponentId, ComponentView, Timestamp},
};
use core::{convert::Infallible, slice};
use nox_array::ArrayView;

pub trait Componentize {
    fn sink_columns(&self, output: &mut impl Decomponentize);

    const MAX_SIZE: usize = usize::MAX;
}

impl Componentize for () {
    fn sink_columns(&self, _output: &mut impl Decomponentize) {}

    const MAX_SIZE: usize = 0;
}

macro_rules! impl_componentize {
    ($($ty:tt),+) => {
        impl<$($ty),*> Componentize for ($($ty,)*)
        where
            $($ty: Componentize),+
        {
            #[allow(unused_parens, non_snake_case)]
            fn sink_columns(&self, output: &mut impl Decomponentize) {
                let ($($ty,)*) = self;
                $($ty.sink_columns(output);)*
            }

            const MAX_SIZE: usize = 0 $(+ $ty::MAX_SIZE)*;
        }
    };
}

impl_componentize!(T1);
impl_componentize!(T1, T2);
impl_componentize!(T1, T2, T3);
impl_componentize!(T1, T2, T3, T4);
impl_componentize!(T1, T2, T3, T4, T5);
impl_componentize!(T1, T2, T3, T4, T5, T6);
impl_componentize!(T1, T2, T3, T4, T5, T6, T7);
impl_componentize!(T1, T2, T3, T4, T5, T6, T7, T8);
impl_componentize!(T1, T2, T3, T4, T5, T6, T7, T9, T10);
impl_componentize!(T1, T2, T3, T4, T5, T6, T7, T9, T10, T11);
impl_componentize!(T1, T2, T3, T4, T5, T6, T7, T9, T10, T11, T12);
impl_componentize!(T1, T2, T3, T4, T5, T6, T7, T9, T10, T11, T12, T13);
impl_componentize!(T1, T2, T3, T4, T5, T6, T7, T9, T10, T11, T12, T13, T14);
impl_componentize!(T1, T2, T3, T4, T5, T6, T7, T9, T10, T11, T12, T13, T14, T15);
impl_componentize!(
    T1, T2, T3, T4, T5, T6, T7, T9, T10, T11, T12, T13, T14, T15, T16
);
impl_componentize!(
    T1, T2, T3, T4, T5, T6, T7, T9, T10, T11, T12, T13, T14, T15, T16, T17
);
impl_componentize!(
    T1, T2, T3, T4, T5, T6, T7, T9, T10, T11, T12, T13, T14, T15, T16, T17, T18
);

pub trait Decomponentize {
    type Error;
    fn apply_value(
        &mut self,
        component_id: ComponentId,
        value: ComponentView<'_>,
        timestamp: Option<Timestamp>,
    ) -> Result<(), Self::Error>;
}

impl Decomponentize for () {
    type Error = Infallible;
    fn apply_value(
        &mut self,
        _component_id: ComponentId,
        _value: ComponentView<'_>,
        _timestamp: Option<Timestamp>,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl<F> Decomponentize for F
where
    F: for<'a> FnMut(ComponentId, ComponentView<'_>, Option<Timestamp>),
{
    type Error = Infallible;
    fn apply_value(
        &mut self,
        component_id: ComponentId,
        value: ComponentView<'_>,
        timestamp: Option<Timestamp>,
    ) -> Result<(), Self::Error> {
        (self)(component_id, value, timestamp);
        Ok(())
    }
}

macro_rules! impl_decomponentize {
    ($($ty:tt),+) => {
        impl<E, $($ty),*> Decomponentize for ($($ty,)*)
        where
            $($ty: Decomponentize<Error = E>),+
        {
            type Error = E;
            #[allow(unused_parens, non_snake_case)]
            fn apply_value(
                &mut self,
                component_id: ComponentId,
                value: ComponentView<'_>,
                timestamp: Option<Timestamp>
            ) -> Result<(), Self::Error>{
                let ($($ty,)*) = self;
                $(
                    $ty.apply_value(component_id, value.clone(), timestamp)?;
                )*
                Ok(())
            }
        }
    };
}

impl_decomponentize!(T1);
impl_decomponentize!(T1, T2);
impl_decomponentize!(T1, T2, T3);
impl_decomponentize!(T1, T2, T3, T4);
impl_decomponentize!(T1, T2, T3, T4, T5);
impl_decomponentize!(T1, T2, T3, T4, T5, T6);
impl_decomponentize!(T1, T2, T3, T4, T5, T6, T7);
impl_decomponentize!(T1, T2, T3, T4, T5, T6, T7, T8);
impl_decomponentize!(T1, T2, T3, T4, T5, T6, T7, T9, T10);
impl_decomponentize!(T1, T2, T3, T4, T5, T6, T7, T9, T10, T11);
impl_decomponentize!(T1, T2, T3, T4, T5, T6, T7, T9, T10, T11, T12);
impl_decomponentize!(T1, T2, T3, T4, T5, T6, T7, T9, T10, T11, T12, T13);
impl_decomponentize!(T1, T2, T3, T4, T5, T6, T7, T9, T10, T11, T12, T13, T14);
impl_decomponentize!(T1, T2, T3, T4, T5, T6, T7, T9, T10, T11, T12, T13, T14, T15);
impl_decomponentize!(
    T1, T2, T3, T4, T5, T6, T7, T9, T10, T11, T12, T13, T14, T15, T16
);
impl_decomponentize!(
    T1, T2, T3, T4, T5, T6, T7, T9, T10, T11, T12, T13, T14, T15, T16, T17
);
impl_decomponentize!(
    T1, T2, T3, T4, T5, T6, T7, T9, T10, T11, T12, T13, T14, T15, T16, T17, T18
);

pub trait FromComponentView: Sized {
    fn from_component_view(view: ComponentView<'_>) -> Result<Self, Error>;
}

pub trait AsComponentView {
    fn as_component_view(&self) -> ComponentView<'_>;
}

macro_rules! impl_component_view {
    ($ty:tt, $prim:tt) => {
        impl FromComponentView for $ty {
            fn from_component_view(view: ComponentView<'_>) -> Result<Self, Error> {
                match view {
                    ComponentView::$prim(view) => {
                        view.buf().first().ok_or(Error::BufferUnderflow).copied()
                    }
                    _ => Err(Error::InvalidComponentData),
                }
            }
        }

        impl AsComponentView for $ty {
            fn as_component_view(&self) -> ComponentView<'_> {
                ComponentView::$prim(ArrayView::from_buf_shape_unchecked(
                    slice::from_ref(self),
                    &[],
                ))
            }
        }
    };
}

impl_component_view!(u64, U64);
impl_component_view!(u32, U32);
impl_component_view!(u16, U16);
impl_component_view!(u8, U8);
impl_component_view!(i64, I64);
impl_component_view!(i32, I32);
impl_component_view!(i16, I16);
impl_component_view!(i8, I8);
impl_component_view!(f64, F64);
impl_component_view!(f32, F32);
impl_component_view!(bool, Bool);

// A fixed-size array `[T; N]` is one component of prim type `T` and shape `[N]`,
// matching how `AsVTable`/`schema(ty, &[N], ..)` already represents arrays. This is
// what lets a `#[derive(Frame)]` field of type `[f64; 3]` (and friends) round-trip
// the same way nox `Vector`/`Quaternion` do (see `nox_impls.rs`).
macro_rules! impl_array_component_view {
    ($ty:tt, $prim:tt) => {
        impl<const N: usize> FromComponentView for [$ty; N] {
            fn from_component_view(view: ComponentView<'_>) -> Result<Self, Error> {
                match view {
                    ComponentView::$prim(view) => {
                        <[$ty; N]>::try_from(view.buf()).map_err(|_| Error::InvalidComponentData)
                    }
                    _ => Err(Error::InvalidComponentData),
                }
            }
        }

        impl<const N: usize> AsComponentView for [$ty; N] {
            fn as_component_view(&self) -> ComponentView<'_> {
                // `&[N]` is rvalue-static-promoted, so the shape outlives the view.
                ComponentView::$prim(ArrayView::from_buf_shape_unchecked(self.as_slice(), &[N]))
            }
        }
    };
}

impl_array_component_view!(u64, U64);
impl_array_component_view!(u32, U32);
impl_array_component_view!(u16, U16);
impl_array_component_view!(u8, U8);
impl_array_component_view!(i64, I64);
impl_array_component_view!(i32, I32);
impl_array_component_view!(i16, I16);
impl_array_component_view!(i8, I8);
impl_array_component_view!(f64, F64);
impl_array_component_view!(f32, F32);
impl_array_component_view!(bool, Bool);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn array_component_view_round_trip() {
        let src: [f64; 3] = [1.5, -2.5, 3.25];
        let view = src.as_component_view();
        assert!(matches!(view, ComponentView::F64(_)));
        assert_eq!(view.shape(), &[3]);
        let dst = <[f64; 3]>::from_component_view(view).unwrap();
        assert_eq!(dst, src);
    }

    #[test]
    fn array_component_view_wrong_prim_type_errors() {
        let src: [f64; 3] = [0.0; 3];
        let view = src.as_component_view();
        assert!(<[i32; 3]>::from_component_view(view).is_err());
    }
}
