use std::{collections::HashMap, marker::PhantomData};

use impeller2::{
    com_de::Decomponentize,
    component::PrimTypeElem,
    types::{ComponentId, ComponentView, PrimType},
};
use nox::{ArrayBuf, Dim, Tensor};
use zerocopy::{FromBytes, Immutable, IntoByteSlice, IntoBytes, KnownLayout};

use crate::AsVTable;

pub struct VTableSink<'a, T> {
    index: &'a VTableSinkIndex<T>,
    table: &'a mut T,
}

impl<'a, T: FromBytes + IntoBytes> VTableSink<'a, T> {
    pub fn apply_update(
        &mut self,
        update: &impeller2_wkt::UpdateComponent,
    ) -> Result<(), VTableSinkError> {
        self.apply_value(update.id, update.value.as_view(), None)
    }
}

impl<'a, T: AsVTable> VTableSink<'a, T> {
    pub fn with_index(index: &'a VTableSinkIndex<T>, table: &'a mut T) -> Self {
        Self { index, table }
    }
}

pub struct VTableSinkIndex<T> {
    fields: HashMap<ComponentId, Field>,
    _phantom_data: PhantomData<T>,
}

impl<T: AsVTable> VTableSinkIndex<T> {
    pub fn new() -> Self {
        let vtable = T::as_vtable();
        let fields = vtable
            .realize_fields(None)
            .flat_map(|res| {
                let field = res.ok()?;
                Some((
                    field.component_id,
                    Field {
                        shape: field.shape.to_vec(),
                        ty: field.ty,
                        offset: field.offset,
                    },
                ))
            })
            .collect();
        dbg!(&fields);
        Self {
            fields,
            _phantom_data: PhantomData,
        }
    }
}

#[derive(Debug)]
struct Field {
    shape: Vec<usize>,
    ty: PrimType,
    offset: usize,
}

impl<T: FromBytes + IntoBytes> Decomponentize for VTableSink<'_, T> {
    type Error = VTableSinkError;

    fn apply_value(
        &mut self,
        component_id: impeller2::types::ComponentId,
        value: impeller2::types::ComponentView<'_>,
        _timestamp: Option<impeller2::types::Timestamp>,
    ) -> Result<(), Self::Error> {
        let field = self
            .index
            .fields
            .get(&component_id)
            .ok_or(VTableSinkError::ComponentNotFound)?;
        if field.shape != value.shape() || field.ty != value.prim_type() {
            println!(
                "field.shape {:?}, value.shape {:?}",
                field.shape,
                value.shape()
            );
            return Err(VTableSinkError::IncompatibleShape);
        }
        let table = self.table.as_mut_bytes();
        let buf = value.as_bytes();
        let table_field = table
            .get_mut(field.offset..field.offset + buf.len())
            .ok_or(VTableSinkError::BufferOverflow)?;
        table_field.copy_from_slice(buf);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VTableSinkError {
    ComponentNotFound,
    IncompatibleShape,
    BufferOverflow,
}

pub trait ComponentViewExt {
    fn from_tensor<
        T: nox::Field + PrimTypeElem + Immutable + KnownLayout + Sized + IntoBytes,
        D: Dim,
    >(
        tensor: &Tensor<T, D>,
    ) -> ComponentView<'_>;
}

impl ComponentViewExt for ComponentView<'_> {
    fn from_tensor<
        T: nox::Field + PrimTypeElem + Immutable + KnownLayout + Sized + IntoBytes,
        D: Dim,
    >(
        tensor: &Tensor<T, D>,
    ) -> ComponentView<'_> {
        match T::PRIM_TYPE {
            PrimType::U8 => ComponentView::try_from_bytes_shape(
                tensor.inner().buf.as_buf().as_bytes(),
                D::shape_slice(&tensor.inner().buf),
                T::PRIM_TYPE,
            )
            .unwrap(),
            PrimType::U16 => ComponentView::try_from_bytes_shape(
                tensor.inner().buf.as_buf().as_bytes(),
                D::shape_slice(&tensor.inner().buf),
                T::PRIM_TYPE,
            )
            .unwrap(),
            PrimType::U32 => ComponentView::try_from_bytes_shape(
                tensor.inner().buf.as_buf().as_bytes(),
                D::shape_slice(&tensor.inner().buf),
                T::PRIM_TYPE,
            )
            .unwrap(),
            PrimType::U64 => ComponentView::try_from_bytes_shape(
                tensor.inner().buf.as_buf().as_bytes(),
                D::shape_slice(&tensor.inner().buf),
                T::PRIM_TYPE,
            )
            .unwrap(),
            PrimType::I8 => ComponentView::try_from_bytes_shape(
                tensor.inner().buf.as_buf().as_bytes(),
                D::shape_slice(&tensor.inner().buf),
                T::PRIM_TYPE,
            )
            .unwrap(),
            PrimType::I16 => ComponentView::try_from_bytes_shape(
                tensor.inner().buf.as_buf().as_bytes(),
                D::shape_slice(&tensor.inner().buf),
                T::PRIM_TYPE,
            )
            .unwrap(),
            PrimType::I32 => ComponentView::try_from_bytes_shape(
                tensor.inner().buf.as_buf().as_bytes(),
                D::shape_slice(&tensor.inner().buf),
                T::PRIM_TYPE,
            )
            .unwrap(),
            PrimType::I64 => ComponentView::try_from_bytes_shape(
                tensor.inner().buf.as_buf().as_bytes(),
                D::shape_slice(&tensor.inner().buf),
                T::PRIM_TYPE,
            )
            .unwrap(),
            PrimType::Bool => ComponentView::try_from_bytes_shape(
                tensor.inner().buf.as_buf().as_bytes(),
                D::shape_slice(&tensor.inner().buf),
                T::PRIM_TYPE,
            )
            .unwrap(),
            PrimType::F32 => ComponentView::try_from_bytes_shape(
                tensor.inner().buf.as_buf().as_bytes(),
                D::shape_slice(&tensor.inner().buf),
                T::PRIM_TYPE,
            )
            .unwrap(),
            PrimType::F64 => ComponentView::try_from_bytes_shape(
                tensor.inner().buf.as_buf().as_bytes(),
                D::shape_slice(&tensor.inner().buf),
                T::PRIM_TYPE,
            )
            .unwrap(),
        }
    }
}

#[cfg(test)]
mod tests {
    use nox::{Scalar, Vector, tensor};

    use super::*;

    #[test]
    fn test_sink() {
        #[derive(AsVTable, IntoBytes, FromBytes)]
        #[repr(C)]
        struct Foo {
            pub a: u64,
            pub b: Vector<f64, 4>,
            pub c: u64,
        }

        let index: VTableSinkIndex<Foo> = VTableSinkIndex::new();
        let mut foo = Foo {
            a: 0,
            b: tensor![0.0; 4],
            c: 0,
        };

        let mut sink = VTableSink::with_index(&index, &mut foo);
        let update: Scalar<u64> = 10.into();
        sink.apply_value(
            ComponentId::new("a"),
            ComponentView::from_tensor(&update),
            None,
        )
        .unwrap();
        let update: Vector<f64, 4> = tensor![1.0, 2.0, 3.0, 4.0];
        sink.apply_value(
            ComponentId::new("b"),
            ComponentView::from_tensor(&update),
            None,
        )
        .unwrap();
        assert_eq!(foo.a, 10);
        assert_eq!(foo.b, tensor![1.0, 2.0, 3.0, 4.0]);
    }
}
