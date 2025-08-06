use std::borrow::Cow;

use impeller2::vtable::{
    VTable,
    builder::{FieldBuilder, schema, vtable},
};

pub trait AsVTable {
    fn vtable_fields(prefix: Option<Cow<'_, str>>) -> impl Iterator<Item = FieldBuilder>;
    fn as_vtable() -> VTable {
        vtable(Self::vtable_fields(None))
    }
}

impl<const N: usize, T: AsVTable> AsVTable for [T; N] {
    fn vtable_fields(prefix: Option<Cow<'_, str>>) -> impl Iterator<Item = FieldBuilder> {
        (0..N)
            .flat_map(|i| {
                let prefix = if let Some(prefix) = &prefix {
                    format!("{}.{}", prefix, i)
                } else {
                    format!("{}", i)
                };
                T::vtable_fields(Some(Cow::Owned(prefix)))
                    .map(move |f| f.offset_by((i * size_of::<T>()) as u16))
            })
            .collect::<Vec<_>>()
            .into_iter()
    }
}
