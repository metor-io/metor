use metor_proto::vtable::{
    VTable,
    builder::{FieldBuilder, vtable},
};

use crate::path::ComponentPath;

pub trait AsVTable {
    /// *Static* form: leaves are absolute `component(path.chain(name).id)` ops,
    /// used when the type is reached through compile-time struct nesting.
    fn vtable_fields(path: impl ComponentPath) -> impl Iterator<Item = FieldBuilder>;

    /// *Dynamic member-template* form (frames.md §4): leaves are
    /// `path_component(name)` ops with offsets relative to the element base and no
    /// absolute path baked in — the runtime path is composed by the enclosing
    /// `list`/`map` at realize time. Used when the type is the element of a
    /// `FrameList`/`FrameMap`.
    ///
    /// `prefix` is the dotted name of this value *relative to the element base*
    /// (empty for the element root). A scalar leaf emits `path_component(prefix)`;
    /// a derived struct recurses with `prefix.field`; a nested dynamic member emits
    /// `list`/`map` named by its relative `prefix.field`. Taken by value so the
    /// returned iterator need not borrow a caller temporary.
    ///
    /// Defaulted to empty so existing hand-written `AsVTable` impls (e.g. the nox
    /// spatial types) keep compiling — only types actually used as dynamic elements
    /// (scalars, frames, `FrameList`/`FrameMap`) override it.
    fn element_fields(_prefix: String) -> impl Iterator<Item = FieldBuilder> {
        core::iter::empty()
    }

    fn as_vtable() -> VTable {
        vtable(Self::vtable_fields(()))
    }
}

impl<const N: usize, T: AsVTable> AsVTable for [T; N] {
    fn vtable_fields(path: impl ComponentPath) -> impl Iterator<Item = FieldBuilder> {
        (0..N)
            .flat_map(|i| {
                let path = path.clone().chain(i.to_string());
                T::vtable_fields(path).map(move |f| f.offset_by((i * size_of::<T>()) as u32))
            })
            .collect::<Vec<_>>()
            .into_iter()
    }
}

#[cfg(test)]
mod tests {
    use core::convert::Infallible;
    use std::collections::HashMap;

    use metor_proto::com_de::Decomponentize;
    use metor_proto::types::{ComponentId, ComponentView, Timestamp};
    use zerocopy::{Immutable, IntoBytes};

    #[derive(Default)]
    struct TestSink {
        timestamps: HashMap<ComponentId, Option<Timestamp>>,
    }

    impl Decomponentize for TestSink {
        type Error = Infallible;
        fn apply_value(
            &mut self,
            component_id: ComponentId,
            _value: ComponentView<'_>,
            timestamp: Option<Timestamp>,
        ) -> Result<(), Self::Error> {
            self.timestamps.insert(component_id, timestamp);
            Ok(())
        }
    }

    #[derive(crate::AsVTable, IntoBytes, Immutable)]
    #[repr(C)]
    struct Telemetry {
        #[metor_fsw(timestamp)]
        time: Timestamp,
        nested: Nested,
        value: f64,
    }

    #[derive(crate::AsVTable, IntoBytes, Immutable)]
    #[repr(C)]
    struct Nested {
        inner: f64,
    }

    #[test]
    fn test_derive_timestamp_field() {
        use crate::AsVTable;
        let vtable = Telemetry::as_vtable();
        let telemetry = Telemetry {
            time: Timestamp(1234),
            nested: Nested { inner: 1.0 },
            value: 2.0,
        };
        let mut sink = TestSink::default();
        vtable
            .apply(telemetry.as_bytes(), &mut sink)
            .unwrap()
            .unwrap();
        for component in ["nested.inner", "value"] {
            assert_eq!(
                sink.timestamps.get(&ComponentId::new(component)),
                Some(&Some(Timestamp(1234))),
                "{component} should carry the marked timestamp"
            );
        }
        // The `#[metor_fsw(timestamp)]` field is the timestamp *source* only; it is
        // not emitted as a standalone `time` component (frames.md Q1 → suppress).
        assert_eq!(sink.timestamps.get(&ComponentId::new("time")), None);
    }

    #[derive(crate::AsVTable, IntoBytes, Immutable)]
    #[repr(C)]
    struct Outer {
        _pad: u64,
        telemetry: Telemetry,
    }

    #[test]
    fn test_derive_timestamp_field_nested() {
        use crate::AsVTable;
        let vtable = Outer::as_vtable();
        let outer = Outer {
            _pad: 0,
            telemetry: Telemetry {
                time: Timestamp(99),
                nested: Nested { inner: 1.0 },
                value: 2.0,
            },
        };
        let mut sink = TestSink::default();
        vtable.apply(outer.as_bytes(), &mut sink).unwrap().unwrap();
        assert_eq!(
            sink.timestamps
                .get(&ComponentId::new("telemetry.value")),
            Some(&Some(Timestamp(99))),
            "nested struct's timestamp source should shift with its fields"
        );
    }
}
