use impeller2::{
    component::PrimTypeElem,
    vtable::builder::{self, FieldBuilder, raw_field, schema},
};
use impeller2_wkt::ComponentMetadata;
use nox::{Body, Field, SpatialForce, SpatialInertia, SpatialMotion, SpatialTransform};
use std::{borrow::Cow, mem::offset_of};

use crate::{AsVTable, Metadatatize};

impl AsVTable for Body {
    fn vtable_fields(prefix: Option<Cow<'_, str>>) -> impl Iterator<Item = FieldBuilder> {
        let component = |name: &str| {
            if let Some(prefix) = &prefix {
                Cow::Owned(format!("{}.{}", prefix, name))
            } else {
                Cow::Owned(name.to_string())
            }
        };
        let pos = component("pos");
        let vel = component("vel");
        let accel = component("accel");
        let inertia = component("inertia");
        let force = component("force");
        SpatialTransform::<f64>::vtable_fields(Some(pos))
            .chain(
                SpatialMotion::<f64>::vtable_fields(Some(vel))
                    .map(|field| field.offset_by(dbg!(offset_of!(Body, vel)) as u16)),
            )
            .chain(
                SpatialMotion::<f64>::vtable_fields(Some(accel))
                    .map(|field| field.offset_by(offset_of!(Body, accel) as u16)),
            )
            .chain(
                SpatialInertia::<f64>::vtable_fields(Some(inertia))
                    .map(|field| field.offset_by(offset_of!(Body, inertia) as u16)),
            )
            .chain(
                SpatialForce::<f64>::vtable_fields(Some(force))
                    .map(|field| field.offset_by(offset_of!(Body, force) as u16)),
            )
    }
}

impl Metadatatize for nox::Body {
    fn metadata() -> impl Iterator<Item = ComponentMetadata> {
        [
            "pos.angular",
            "pos.linear",
            "vel.angular",
            "vel.linear",
            "accel.angular",
            "accel.linear",
            "inertia.mass",
            "inertia.moment_of_inertia",
            "inertia.momentum",
            "force.angular",
            "force.linear",
        ]
        .map(ComponentMetadata::from)
        .into_iter()
    }
}

impl<T: PrimTypeElem + Field> AsVTable for SpatialTransform<T> {
    fn vtable_fields(prefix: Option<Cow<'_, str>>) -> impl Iterator<Item = FieldBuilder> {
        let component = |name| {
            if let Some(prefix) = &prefix {
                builder::component(format!("{}.{}", prefix, name).as_str())
            } else {
                builder::component(name)
            }
        };
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
    fn vtable_fields(prefix: Option<Cow<'_, str>>) -> impl Iterator<Item = FieldBuilder> {
        let component = |name| {
            if let Some(prefix) = &prefix {
                builder::component(format!("{}.{}", prefix, name).as_str())
            } else {
                builder::component(name)
            }
        };
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
    fn vtable_fields(prefix: Option<Cow<'_, str>>) -> impl Iterator<Item = FieldBuilder> {
        let component = |name| {
            if let Some(prefix) = &prefix {
                builder::component(format!("{}.{}", prefix, name).as_str())
            } else {
                builder::component(name)
            }
        };
        [
            raw_field(
                0 * size_of::<T>() as u16,
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
    fn vtable_fields(prefix: Option<Cow<'_, str>>) -> impl Iterator<Item = FieldBuilder> {
        let component = |name| {
            if let Some(prefix) = &prefix {
                builder::component(format!("{}.{}", prefix, name).as_str())
            } else {
                builder::component(name)
            }
        };
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
