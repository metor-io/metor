use impeller2::{
    component::PrimTypeElem,
    types::Timestamp,
    vtable::builder::{self, FieldBuilder, raw_field, schema},
};
use impeller2_wkt::ComponentMetadata;
use nox::{
    Body, ConstDim, Dim, Field, SpatialForce, SpatialInertia, SpatialMotion, SpatialTransform,
};
use std::{iter, mem::offset_of};

use crate::{AsVTable, Metadatatize, path::ComponentPath};

impl AsVTable for Body {
    fn vtable_fields(path: impl ComponentPath) -> impl Iterator<Item = FieldBuilder> {
        SpatialTransform::<f64>::vtable_fields(path.chain("pos"))
            .chain(
                SpatialMotion::<f64>::vtable_fields(path.chain("vel"))
                    .map(|field| field.offset_by(offset_of!(Body, vel) as u16)),
            )
            .chain(
                SpatialMotion::<f64>::vtable_fields(path.chain("accel"))
                    .map(|field| field.offset_by(offset_of!(Body, accel) as u16)),
            )
            .chain(
                SpatialInertia::<f64>::vtable_fields(path.chain("inertia"))
                    .map(|field| field.offset_by(offset_of!(Body, inertia) as u16)),
            )
            .chain(
                SpatialForce::<f64>::vtable_fields(path.chain("force"))
                    .map(|field| field.offset_by(offset_of!(Body, force) as u16)),
            )
    }
}

impl Metadatatize for nox::Body {
    fn metadata(prefix: impl ComponentPath) -> impl Iterator<Item = ComponentMetadata> {
        SpatialTransform::<f64>::metadata(prefix.chain("pos"))
            .chain(SpatialMotion::<f64>::metadata(prefix.chain("vel")))
            .chain(SpatialMotion::<f64>::metadata(prefix.chain("accel")))
            .chain(SpatialInertia::<f64>::metadata(prefix.chain("inertia")))
            .chain(SpatialForce::<f64>::metadata(prefix.chain("force")))
    }
}

impl<T: PrimTypeElem + Field> AsVTable for SpatialTransform<T> {
    fn vtable_fields(path: impl ComponentPath) -> impl Iterator<Item = FieldBuilder> {
        let component = |name| builder::component(path.chain(name).to_component_id());
        [
            raw_field(
                0,
                (4 * size_of::<T>()) as u16,
                schema(T::PRIM_TYPE, &[4], component("angular")),
            ),
            raw_field(
                4 * size_of::<T>() as u16,
                (3 * size_of::<T>()) as u16,
                schema(T::PRIM_TYPE, &[3], component("linear")),
            ),
        ]
        .into_iter()
    }
}

impl<T: PrimTypeElem + Field> AsVTable for SpatialMotion<T> {
    fn vtable_fields(path: impl ComponentPath) -> impl Iterator<Item = FieldBuilder> {
        let component = |name| builder::component(path.chain(name).to_component_id());
        [
            raw_field(
                0,
                (3 * size_of::<T>()) as u16,
                schema(T::PRIM_TYPE, &[3], component("angular")),
            ),
            raw_field(
                3 * size_of::<T>() as u16,
                (3 * size_of::<T>()) as u16,
                schema(T::PRIM_TYPE, &[3], component("linear")),
            ),
        ]
        .into_iter()
    }
}

impl<T: PrimTypeElem + Field> AsVTable for SpatialInertia<T> {
    fn vtable_fields(path: impl ComponentPath) -> impl Iterator<Item = FieldBuilder> {
        let component = |name| builder::component(path.chain(name).to_component_id());
        [
            raw_field(
                0,
                (3 * size_of::<T>()) as u16,
                schema(T::PRIM_TYPE, &[3], component("moment_of_inertia")),
            ),
            raw_field(
                3 * size_of::<T>() as u16,
                (3 * size_of::<T>()) as u16,
                schema(T::PRIM_TYPE, &[3], component("momentum")),
            ),
            raw_field(
                6 * size_of::<T>() as u16,
                (size_of::<T>()) as u16,
                schema(T::PRIM_TYPE, &[1], component("mass")),
            ),
        ]
        .into_iter()
    }
}

impl<T: PrimTypeElem + Field> AsVTable for SpatialForce<T> {
    fn vtable_fields(path: impl ComponentPath) -> impl Iterator<Item = FieldBuilder> {
        let component = |name| builder::component(path.chain(name).to_component_id());
        [
            raw_field(
                0,
                (3 * size_of::<T>()) as u16,
                schema(T::PRIM_TYPE, &[3], component("angular")),
            ),
            raw_field(
                3 * size_of::<T>() as u16,
                (3 * size_of::<T>()) as u16,
                schema(T::PRIM_TYPE, &[3], component("linear")),
            ),
        ]
        .into_iter()
    }
}

macro_rules! impl_spatial_metadata {
    ($ty:ty, $ang_elem_names:expr, $lin_elem_names:expr) => {
        impl<T: Field> Metadatatize for $ty {
            fn metadata(prefix: impl ComponentPath) -> impl Iterator<Item = ComponentMetadata> {
                [
                    prefix
                        .clone()
                        .chain("angular")
                        .to_metadata()
                        .with_element_names($ang_elem_names),
                    prefix
                        .clone()
                        .chain("linear")
                        .to_metadata()
                        .with_element_names($lin_elem_names),
                ]
                .into_iter()
            }
        }
    };
}

impl_spatial_metadata!(
    SpatialTransform<T>,
    ["q0", "q1", "q2", "q3"],
    ["x", "y", "z"]
);
impl_spatial_metadata!(SpatialMotion<T>, ["ωx", "ωy", "ωz"], ["x", "y", "z"]);
impl_spatial_metadata!(SpatialForce<T>, ["𝛕x", "𝛕y", "𝛕z"], ["x", "y", "z"]);
impl<T: Field> Metadatatize for SpatialInertia<T> {
    fn metadata(prefix: impl ComponentPath) -> impl Iterator<Item = ComponentMetadata> {
        ["moment_of_inertia", "momentum", "mass"]
            .map(|name| prefix.clone().chain(name))
            .map(|path| path.to_metadata())
            .into_iter()
    }
}

macro_rules! impl_prim_component {
    ($ty:ty) => {
        impl AsVTable for $ty {
            fn vtable_fields(path: impl ComponentPath) -> impl Iterator<Item = FieldBuilder> {
                let component = if path.is_empty() {
                    builder::component(std::stringify!($ty))
                } else {
                    builder::component(path.to_component_id())
                };
                std::iter::once(raw_field(
                    0,
                    size_of::<$ty>() as u16,
                    schema(<$ty>::PRIM_TYPE, &[], component),
                ))
            }
        }

        impl Metadatatize for $ty {}
    };
}

impl_prim_component!(f32);
impl_prim_component!(f64);
impl_prim_component!(i8);
impl_prim_component!(i16);
impl_prim_component!(i32);
impl_prim_component!(i64);
impl_prim_component!(u8);
impl_prim_component!(u16);
impl_prim_component!(u32);
impl_prim_component!(u64);
impl_prim_component!(bool);

impl<T: Field, D: Dim> Metadatatize for nox::Tensor<T, D> {}
impl<T: Field> Metadatatize for nox::Quaternion<T> {}

impl<T: Field + PrimTypeElem, D: ConstDim + Dim> AsVTable for nox::Tensor<T, D> {
    fn vtable_fields(path: impl ComponentPath) -> impl Iterator<Item = FieldBuilder> {
        let component = if path.is_empty() {
            builder::component("tensor")
        } else {
            builder::component(path.to_component_id())
        };
        let dim = D::DIM.into_iter().map(|d| *d as u64).collect::<Vec<_>>();
        let size = D::DIM.iter().product::<usize>() * size_of::<T>();
        iter::once(raw_field(
            0,
            size as u16,
            builder::schema(T::PRIM_TYPE, &dim, component),
        ))
    }
}

impl<T: Field + PrimTypeElem> AsVTable for nox::Quaternion<T> {
    fn vtable_fields(path: impl ComponentPath) -> impl Iterator<Item = FieldBuilder> {
        let component = if path.is_empty() {
            builder::component("quaternion")
        } else {
            builder::component(path.to_component_id())
        };
        let size = size_of::<T>() * 4;
        iter::once(raw_field(
            0,
            size as u16,
            builder::schema(T::PRIM_TYPE, &[4], component),
        ))
    }
}

impl AsVTable for Timestamp {
    fn vtable_fields(path: impl ComponentPath) -> impl Iterator<Item = FieldBuilder> {
        let component = if path.is_empty() {
            builder::component("timestamp")
        } else {
            builder::component(path.to_component_id())
        };
        iter::once(raw_field(
            0,
            size_of::<Timestamp>() as u16,
            builder::schema(impeller2::types::PrimType::I64, &[], component),
        ))
    }
}

impl Metadatatize for Timestamp {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_body() {
        let metadata = nox::Body::metadata("foo").collect::<Vec<_>>();
        assert_eq!(&metadata[0].name, "foo.pos.angular");
        assert_eq!(&metadata[1].name, "foo.pos.linear");
        assert_eq!(&metadata[2].name, "foo.pos.angular");
        assert_eq!(&metadata[3].name, "foo.pos.linear");
    }
}
