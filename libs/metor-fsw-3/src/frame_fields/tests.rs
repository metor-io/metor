use metor_component::FrameMap;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use super::*;
use crate::tests::utils::{Nested, Note, Pair};
use crate::{DefCx, Frame, Output, Record, Timestamp};

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[frame(name = "procs", max_len = 128)]
#[repr(C)]
struct Procs {
    #[frame(timestamp)]
    timestamp: Timestamp,
    procs: FrameMap<u32, u64>,
}

fn def<T: Record>(port: &'static str) -> PortDef {
    Output::<T>::def(&DefCx::empty(), port)
}

fn paths(table: &FieldTable) -> Vec<&str> {
    table.leaves.iter().map(|l| l.path.as_str()).collect()
}

#[test]
fn test_pair_leaves() {
    let table = FieldTable::of(&def::<Pair>("ns.mux.pair")).expect("a fixed frame");
    assert_eq!(paths(&table), ["a", "b"]);
    assert_eq!(table.leaves[0].offset, 8);
    assert_eq!(table.leaves[1].offset, 16);
    assert_eq!(table.leaves[1].len, 24);
    assert_eq!(table.stamp_at, Some(0));
    assert_eq!(table.len, size_of::<Pair>() as u32);
    assert_eq!(FieldTable::signature(&table.leaves[1]), "f64[3]");
}

#[test]
fn test_nested_paths() {
    let table = FieldTable::of(&def::<Nested>("nested")).expect("a fixed frame");
    assert_eq!(paths(&table), ["inner.x", "inner.y", "z"]);
    let inner: Vec<_> = table.under("inner").map(|l| l.offset).collect();
    assert_eq!(inner, [8, 16]);
}

/// Leaves with no stamp; `derive(Frame)` requires one, so the def is built by hand.
#[derive(metor_component::AsVTable, metor_component::Metadatatize)]
#[repr(C)]
struct Unstamped {
    a: f64,
    ab: f64,
}

fn unstamped() -> PortDef {
    use metor_component::{AsVTable, Metadatatize};
    PortDef {
        schema: RecordSchema::Frame {
            vtable: metor_proto::vtable::builder::vtable(Unstamped::vtable_fields("u".to_string())),
            metadata: Unstamped::metadata("u".to_string()).collect(),
        },
        max_len: 16,
        name: "u".into(),
        ..def::<Pair>("u")
    }
}

#[test]
fn test_unstamped_frame() {
    let table = FieldTable::of(&unstamped()).expect("a fixed frame");
    assert_eq!(table.stamp_at, None);
    assert_eq!(paths(&table), ["a", "ab"]);
    assert_eq!(table.under("a").count(), 1);
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[frame(name = "plain")]
#[repr(C)]
struct Plain {
    #[frame(timestamp)]
    timestamp: Timestamp,
    c: [f64; 2],
}

#[test]
fn test_plain_array_is_numbered_leaves() {
    let table = FieldTable::of(&def::<Plain>("plain")).expect("a fixed frame");
    assert_eq!(paths(&table), ["c.0", "c.1"]);
    assert_eq!(table.under("c").count(), 2);
    assert!(!is_under("ab", "a"));
}

/// `unstamped` with its two leaves stamped from `a_at` and `ab_at`.
fn stamped(a_at: Option<u32>, ab_at: Option<u32>) -> PortDef {
    use metor_component::{AsVTable, Metadatatize};
    use metor_proto::vtable::builder::{raw_table, vtable};
    let stamp = |field: metor_proto::vtable::builder::FieldBuilder, at: Option<u32>| match at {
        Some(at) => field.with_timestamp(raw_table(at, 8)),
        None => field,
    };
    let mut fields = Unstamped::vtable_fields("u".to_string());
    let (a, ab) = (fields.next().expect("a"), fields.next().expect("ab"));
    PortDef {
        schema: RecordSchema::Frame {
            vtable: vtable([stamp(a, a_at), stamp(ab, ab_at)]),
            metadata: Unstamped::metadata("u".to_string()).collect(),
        },
        ..unstamped()
    }
}

#[test]
fn test_leaves_that_disagree_on_the_stamp_are_refused() {
    let mixed = Err(FieldError::MixedStamps("pair".into()));
    assert_eq!(FieldTable::of(&stamped(Some(0), Some(8))), mixed);
    assert_eq!(FieldTable::of(&stamped(None, Some(0))), mixed);
}

#[test]
fn test_refusals() {
    let refused = |def: PortDef| FieldTable::of(&def).err();
    let message = Some(FieldError::Message("note".into()));
    assert_eq!(refused(def::<Note>("note")), message);
    let dynamic = Some(FieldError::Dynamic("procs".into()));
    assert_eq!(refused(def::<Procs>("procs")), dynamic);
    let mut bare = def::<Pair>("pair");
    if let RecordSchema::Frame { metadata, .. } = &mut bare.schema {
        metadata.clear();
    }
    assert!(matches!(refused(bare), Some(FieldError::Unnamed { .. })));
}
