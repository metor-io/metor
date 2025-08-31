use impeller2::vtable::{
    VTable,
    builder::{FieldBuilder, vtable},
};

use crate::path::ComponentPath;

pub trait AsVTable {
    fn vtable_fields(path: impl ComponentPath) -> impl Iterator<Item = FieldBuilder>;
    fn as_vtable() -> VTable {
        vtable(Self::vtable_fields(()))
    }
}

impl<const N: usize, T: AsVTable> AsVTable for [T; N] {
    fn vtable_fields(path: impl ComponentPath) -> impl Iterator<Item = FieldBuilder> {
        (0..N)
            .flat_map(|i| {
                let path = path.clone().chain(i.to_string());
                T::vtable_fields(path).map(move |f| f.offset_by((i * size_of::<T>()) as u16))
            })
            .collect::<Vec<_>>()
            .into_iter()
    }
}
