use metor_proto::types::PacketId;
use metor_proto::vtable::Op;
use metor_proto::vtable::builder::vtable;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::record::RecordSchema;
use crate::tests::utils::*;
use crate::{AsVTable, Fields, Frame, Metadatatize, Record, Timestamp};

#[derive(Fields, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
struct Pos {
    x: f64,
    y: f64,
}

#[derive(Fields, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
struct Group {
    #[fsw(flatten)]
    pos: Pos,
    #[fsw(rename = "speed")]
    v: f64,
    #[fsw(component_id = "accel")]
    a: f64,
    #[fsw(skip)]
    hidden: f64,
    _pad: f64,
    #[fsw(skip = false)]
    _kept: f64,
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[fsw(name = "grouped")]
#[repr(C)]
struct Grouped {
    #[fsw(timestamp)]
    timestamp: Timestamp,
    g: Group,
    gs: [Pos; 2],
    #[fsw(flatten)]
    top: Pos,
}

/// A group of components with its own metadata entry.
#[derive(Fields, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[fsw(group)]
#[repr(C)]
struct Labeled {
    x: f64,
}

/// A group frame that flattens a group into its own path.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[fsw(name = "grouped_twice", group)]
#[repr(C)]
struct GroupedTwice {
    #[fsw(timestamp)]
    timestamp: Timestamp,
    #[fsw(flatten)]
    labeled: Labeled,
    y: f64,
}

/// A group frame whose own field and flattened group share a leaf.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[fsw(name = "grouped_collide", group)]
#[repr(C)]
struct GroupedCollide {
    #[fsw(timestamp)]
    timestamp: Timestamp,
    x: f64,
    #[fsw(flatten)]
    labeled: Labeled,
}

/// A frame holding another frame.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[fsw(name = "outer")]
#[repr(C)]
struct Outer {
    #[fsw(timestamp)]
    timestamp: Timestamp,
    z: f64,
    imu: Imu,
}

/// `Imu` spelled with `#[fsw(..)]`.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[fsw(name = "imu")]
#[repr(C)]
struct FswImu {
    #[fsw(timestamp)]
    timestamp: Timestamp,
    sample: f64,
}

#[derive(
    metor_component::AsVTable, metor_component::Metadatatize, IntoBytes, Immutable, KnownLayout,
)]
#[repr(C)]
struct StandalonePos {
    x: f64,
    y: f64,
}

fn names<T: Record>() -> Vec<String> {
    match T::schema() {
        RecordSchema::Frame { metadata, .. } => metadata.into_iter().map(|m| m.name).collect(),
        RecordSchema::Msg { .. } => Vec::new(),
    }
}

fn frame_ops<T: Record>() -> Vec<Op> {
    match T::schema() {
        RecordSchema::Frame { vtable, .. } => vtable.ops,
        RecordSchema::Msg { .. } => Vec::new(),
    }
}

#[test]
fn test_default_frame_name() {
    assert_eq!(BareName::NAME, "bare_name");
}

#[test]
fn test_packet_ids_are_pinned() {
    let ids: [(PacketId, PacketId); 10] = [
        (
            Imu::schema().packet_id(),
            [203, 135, 227, 47, 139, 56, 204, 138],
        ),
        (
            Image::schema().packet_id(),
            [66, 148, 119, 0, 2, 51, 244, 78],
        ),
        (
            ImuPair::schema().packet_id(),
            [138, 64, 20, 229, 10, 5, 217, 24],
        ),
        (
            BareName::schema().packet_id(),
            [20, 60, 135, 139, 100, 141, 45, 25],
        ),
        (
            Nav::schema().packet_id(),
            [151, 2, 69, 202, 56, 147, 36, 102],
        ),
        (
            Control::schema().packet_id(),
            [252, 133, 251, 180, 162, 125, 37, 165],
        ),
        (
            Pair::schema().packet_id(),
            [96, 82, 13, 129, 195, 152, 190, 246],
        ),
        (
            PairA::schema().packet_id(),
            [81, 210, 20, 74, 9, 49, 127, 169],
        ),
        (
            PairB::schema().packet_id(),
            [94, 102, 3, 169, 187, 233, 36, 72],
        ),
        (
            Nested::schema().packet_id(),
            [218, 118, 245, 68, 174, 171, 203, 150],
        ),
    ];
    for (id, pinned) in ids {
        assert_eq!(id, pinned);
    }
}

#[test]
fn test_fsw_and_frame_attributes_agree() {
    assert_eq!(FswImu::schema().packet_id(), Imu::schema().packet_id());
    assert_eq!(names::<FswImu>(), names::<Imu>());
}

#[test]
fn test_stamp_names_the_timestamp_field() {
    assert_eq!(Imu::STAMP, (0, 8));
    assert_eq!(Outer::STAMP, (0, 8));
}

#[test]
fn test_field_paths() {
    let expected = [
        "grouped.g.x",
        "grouped.g.y",
        "grouped.g.speed",
        "grouped.g.accel",
        "grouped.g._kept",
        "grouped.gs.0.x",
        "grouped.gs.0.y",
        "grouped.gs.1.x",
        "grouped.gs.1.y",
        "grouped.x",
        "grouped.y",
    ];
    assert_eq!(names::<Grouped>(), expected);
}

#[test]
fn test_frame_in_frame_takes_the_outer_stamp_and_id() {
    assert_eq!(names::<Outer>(), ["outer.z", "outer.imu.sample"]);
    let RecordSchema::Frame { vtable, .. } = Outer::schema() else {
        panic!("a frame schema");
    };
    let fields: Vec<_> = vtable.realize_fields(None).map(Result::unwrap).collect();
    assert_eq!(fields.len(), 2);
    for field in &fields {
        assert_eq!(field.frame, Some(Outer::ID));
        assert_eq!(field.timestamp_at, Some(0));
    }
    let ops = frame_ops::<Outer>();
    let frames = ops.iter().filter(|op| matches!(op, Op::Frame { .. }));
    let stamps = ops.iter().filter(|op| matches!(op, Op::Timestamp { .. }));
    assert_eq!(frames.count(), 2);
    assert_eq!(stamps.count(), 2);
}

#[test]
fn test_fields_match_the_standalone_derives() {
    let fields = vtable(Pos::vtable_fields("p".to_string()));
    let standalone = vtable(StandalonePos::vtable_fields("p".to_string()));
    assert_eq!(
        serde_json::to_value(&fields).unwrap(),
        serde_json::to_value(&standalone).unwrap()
    );
    let metadata: Vec<_> = Pos::metadata("p".to_string()).collect();
    let standalone: Vec<_> = StandalonePos::metadata("p".to_string()).collect();
    assert_eq!(metadata, standalone);
}

#[test]
fn test_fields_have_no_stamp_or_frame() {
    let fields = vtable(Group::vtable_fields("g".to_string()));
    let tagged = fields
        .ops
        .iter()
        .filter(|op| matches!(op, Op::Frame { .. } | Op::Timestamp { .. }));
    assert_eq!(tagged.count(), 0);
}

#[test]
fn test_duplicate_leaf() {
    assert_eq!(
        Collide::schema().duplicate_leaf().as_deref(),
        Some("collide.x")
    );
    assert_eq!(Grouped::schema().duplicate_leaf(), None);
    let groups = names::<GroupedTwice>();
    assert_eq!(groups.iter().filter(|n| *n == "grouped_twice").count(), 2);
    assert_eq!(GroupedTwice::schema().duplicate_leaf(), None);
    assert_eq!(
        GroupedCollide::schema().duplicate_leaf().as_deref(),
        Some("grouped_collide.x")
    );
    assert_eq!(Note::schema().duplicate_leaf(), None);
}
