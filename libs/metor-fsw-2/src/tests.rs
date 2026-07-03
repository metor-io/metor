//! Frame acceptance tests (frames.md "Tests").

use core::mem::offset_of;
use std::collections::HashMap;

use metor_fsw::{AsVTable, Componentize};
use metor_proto::types::{ComponentId, ComponentView, Timestamp};
use zerocopy::{Immutable, IntoBytes, KnownLayout};

use crate::{Frame, FrameList, FrameMap, FrameWriter, KeyError};

// ---------------------------------------------------------------------------
// Shared recording sink
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RecSink {
    values: HashMap<ComponentId, f64>,
    timestamps: HashMap<ComponentId, Option<Timestamp>>,
}

impl metor_fsw::Decomponentize for RecSink {
    type Error = core::convert::Infallible;
    fn apply_value(
        &mut self,
        component_id: ComponentId,
        value: ComponentView<'_>,
        timestamp: Option<Timestamp>,
    ) -> Result<(), Self::Error> {
        self.values.insert(component_id, value.to_f64());
        self.timestamps.insert(component_id, timestamp);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 1. IMU frame: frame tag + shared timestamp + timestamp suppression
// ---------------------------------------------------------------------------

#[derive(Frame, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
#[metor_fsw(name = "imu")]
struct Imu {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    omega: f64,
    accel: f64,
}

#[test]
fn imu_frame_tag_and_timestamp_suppression() {
    let imu = Imu {
        timestamp: Timestamp(4242),
        omega: 1.0,
        accel: 2.0,
    };
    let vtable = Imu::as_vtable();
    // `frame` is carried on `RealizedField`, not handed to `apply_value`, so realize
    // explicitly to inspect it.
    let fields: Vec<_> = vtable
        .realize_fields(Some(imu.as_bytes()))
        .map(|f| f.unwrap())
        .collect();
    let by_id: HashMap<ComponentId, _> = fields.iter().map(|f| (f.component_id, f)).collect();

    for member in ["imu.omega", "imu.accel"] {
        let f = by_id
            .get(&ComponentId::new(member))
            .unwrap_or_else(|| panic!("{member} missing"));
        assert_eq!(
            f.frame,
            Some(ComponentId::new("imu")),
            "{member} should carry the frame id"
        );
        assert_eq!(
            f.timestamp,
            Some(Timestamp(4242)),
            "{member} should carry the shared timestamp"
        );
    }
    // The timestamp field is the source only — never a standalone component.
    assert!(
        !by_id.contains_key(&ComponentId::new("imu.timestamp")),
        "imu.timestamp must be suppressed"
    );
    assert_eq!(Imu::FRAME_ID, ComponentId::new("imu"));
    assert_eq!(imu.timestamp(), Timestamp(4242));
}

// ---------------------------------------------------------------------------
// 2. Static-frame Componentize / Decomponentize round-trip
// ---------------------------------------------------------------------------

#[derive(Frame, IntoBytes, Immutable, KnownLayout, Default, Debug, PartialEq)]
#[repr(C)]
#[metor_fsw(name = "imu")]
struct ImuRt {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    gyro_x: f64,
    gyro_y: f64,
    count: u64,
}

#[test]
fn static_frame_componentize_round_trip() {
    let src = ImuRt {
        timestamp: Timestamp(7),
        gyro_x: 1.5,
        gyro_y: -2.5,
        count: 99,
    };
    let mut dst = ImuRt::default();
    src.sink_columns(&mut dst);
    // The timestamp is suppressed (source only), so it does not round-trip; every
    // other component does.
    assert_eq!(dst.gyro_x, src.gyro_x);
    assert_eq!(dst.gyro_y, src.gyro_y);
    assert_eq!(dst.count, src.count);
    assert_eq!(dst.timestamp, Timestamp(0));
}

// ---------------------------------------------------------------------------
// 2b. Array (`[T; N]`) frame field: one multi-element component, round-tripped
// through `apply` into a recording sink.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct VecSink {
    values: HashMap<ComponentId, Vec<f64>>,
}

impl metor_fsw::Decomponentize for VecSink {
    type Error = core::convert::Infallible;
    fn apply_value(
        &mut self,
        component_id: ComponentId,
        value: ComponentView<'_>,
        _timestamp: Option<Timestamp>,
    ) -> Result<(), Self::Error> {
        self.values
            .insert(component_id, value.iter().map(|e| e.as_f64()).collect());
        Ok(())
    }
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, Default, Debug, PartialEq)]
#[repr(C)]
#[metor_fsw(name = "arr")]
struct ArrFrame {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    v: [f64; 3],
    // `u64` (not `u32`) keeps the `#[repr(C)]` frame padding-free, a prerequisite
    // for `IntoBytes`.
    n: u64,
}

#[test]
fn array_field_frame_round_trip() {
    let frame = ArrFrame {
        timestamp: Timestamp(77),
        v: [1.5, -2.5, 3.25],
        n: 42,
    };

    // (a) Componentize → Decomponentize in-process round-trip.
    let mut dst = ArrFrame::default();
    frame.sink_columns(&mut dst);
    assert_eq!(dst.v, frame.v);
    assert_eq!(dst.n, frame.n);

    // (b) bytes → vtable `apply` into a recording sink. NOTE: the blanket
    // `AsVTable for [T; N]` (metor-fsw/src/vtable.rs) expands an array into N indexed
    // scalar components, so the vtable path yields `arr.v.0/1/2` rather than the
    // single shape-[3] `arr.v` that `as_component_view` (part a) produces. This
    // representation split is inherent framework behavior.
    let mut sink = VecSink::default();
    ArrFrame::as_vtable()
        .apply(frame.as_bytes(), &mut sink)
        .unwrap()
        .unwrap();
    assert_eq!(sink.values[&ComponentId::new("arr.v.0")], vec![1.5]);
    assert_eq!(sink.values[&ComponentId::new("arr.v.1")], vec![-2.5]);
    assert_eq!(sink.values[&ComponentId::new("arr.v.2")], vec![3.25]);
    assert_eq!(sink.values[&ComponentId::new("arr.n")], vec![42.0]);
}

// ---------------------------------------------------------------------------
// 3. FrameList / FrameMap: build → serialize → apply
// ---------------------------------------------------------------------------

#[derive(AsVTable, IntoBytes, Immutable, KnownLayout, Clone, Copy)]
#[repr(C)]
struct Process {
    pid: u64,
    cpu_usage: f64,
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
struct SysList {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    processes: FrameList<Process, 8>,
}

#[test]
fn frame_list_build_and_apply() {
    let frame = SysList {
        timestamp: Timestamp(1000),
        processes: FrameList::EMPTY,
    };
    let mut w = FrameWriter::new(&frame);
    w.list(offset_of!(SysList, processes), |l| {
        l.push(Process {
            pid: 1001,
            cpu_usage: 0.5,
        });
        l.push(Process {
            pid: 1002,
            cpu_usage: 0.25,
        });
    });

    let mut sink = RecSink::default();
    SysList::as_vtable()
        .apply(w.table(), &mut sink)
        .unwrap()
        .unwrap();

    assert_eq!(sink.values[&ComponentId::new("processes.0.pid")], 1001.0);
    assert_eq!(sink.values[&ComponentId::new("processes.0.cpu_usage")], 0.5);
    assert_eq!(sink.values[&ComponentId::new("processes.1.pid")], 1002.0);
    assert_eq!(sink.values[&ComponentId::new("processes.1.cpu_usage")], 0.25);
    // Elements inherit the shared frame timestamp.
    assert_eq!(
        sink.timestamps[&ComponentId::new("processes.0.pid")],
        Some(Timestamp(1000))
    );
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
struct SysMap {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    processes: FrameMap<Process, 8>,
}

#[test]
fn frame_map_build_and_apply() {
    let frame = SysMap {
        timestamp: Timestamp(1000),
        processes: FrameMap::EMPTY,
    };
    let mut w = FrameWriter::new(&frame);
    w.map(offset_of!(SysMap, processes), |m| {
        m.insert(
            "htop",
            Process {
                pid: 1001,
                cpu_usage: 0.5,
            },
        );
        m.insert(
            "init",
            Process {
                pid: 1002,
                cpu_usage: 0.25,
            },
        );
    })
    .unwrap();

    let mut sink = RecSink::default();
    SysMap::as_vtable()
        .apply(w.table(), &mut sink)
        .unwrap()
        .unwrap();

    assert_eq!(sink.values[&ComponentId::new("processes.htop.pid")], 1001.0);
    assert_eq!(
        sink.values[&ComponentId::new("processes.htop.cpu_usage")],
        0.5
    );
    assert_eq!(sink.values[&ComponentId::new("processes.init.pid")], 1002.0);
    assert_eq!(
        sink.timestamps[&ComponentId::new("processes.htop.pid")],
        Some(Timestamp(1000))
    );
}

#[test]
fn frame_map_rejects_dot_key_at_write_time() {
    let frame = SysMap {
        timestamp: Timestamp(0),
        processes: FrameMap::EMPTY,
    };
    let mut w = FrameWriter::new(&frame);
    let res = w.map(offset_of!(SysMap, processes), |m| {
        m.insert(
            "a.b",
            Process {
                pid: 1,
                cpu_usage: 0.0,
            },
        );
    });
    assert_eq!(res, Err(KeyError::DotInKey));
}

// ---------------------------------------------------------------------------
// 4. Nested dynamics: processes.htop.threads.0.state (the §4 prefix rule)
// ---------------------------------------------------------------------------

#[derive(AsVTable, IntoBytes, Immutable, KnownLayout, Clone, Copy)]
#[repr(C)]
struct Thread {
    state: u8,
}

#[derive(AsVTable, IntoBytes, Immutable, KnownLayout, Clone, Copy)]
#[repr(C)]
struct Host {
    threads: FrameList<Thread, 4>,
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
struct SysNested {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    processes: FrameMap<Host, 4>,
}

fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}
fn put_i64(buf: &mut Vec<u8>, v: i64) {
    buf.extend_from_slice(&v.to_le_bytes());
}

#[test]
fn nested_dynamic_prefix_rule() {
    // The derived vtable is the contract under test (frames.md §4): the outer map is
    // reached statically → full prefix `processes`; the inner `threads` list is a
    // member template → own name only. The trailer bytes mirror the
    // `test_dynamic_nested` layout.
    let vtable = SysNested::as_vtable();

    let mut t = Vec::new();
    put_i64(&mut t, 1000); // timestamp @0
    put_u32(&mut t, 16); // slot_outer.trailer_off @8
    put_u32(&mut t, 16); // slot_outer.byte_len @12 (one 16-byte entry)
    // entry[0] @16
    put_u32(&mut t, 40); // key_off -> "htop"
    put_u32(&mut t, 4); // key_len
    // Host value @24 (entry 16 + value_offset 8): inner list slot
    put_u32(&mut t, 44); // slot_inner.trailer_off
    put_u32(&mut t, 1); // slot_inner.byte_len (one 1-byte thread)
    while t.len() < 40 {
        t.push(0);
    }
    t.extend_from_slice(b"htop"); // @40
    t.push(2); // thread[0].state @44
    assert_eq!(t.len(), 45);

    let mut sink = RecSink::default();
    vtable.apply(&t, &mut sink).unwrap().unwrap();

    assert_eq!(
        sink.values[&ComponentId::new("processes.htop.threads.0.state")],
        2.0
    );
    assert_eq!(
        sink.timestamps[&ComponentId::new("processes.htop.threads.0.state")],
        Some(Timestamp(1000))
    );
}

// ---------------------------------------------------------------------------
// 5. MAX_SIZE formula (frames.md §3.4)
// ---------------------------------------------------------------------------

#[derive(Frame, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
struct SysBoth {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    procs: FrameList<Process, 8>,
    hosts: FrameMap<Process, 4, 16>,
}

#[test]
fn max_size_formula() {
    use crate::dynamic::{map_stride, map_value_offset};
    let _ = map_value_offset::<Process>();

    let list_budget = metor_fsw_ring::round_up8(8 * core::mem::size_of::<Process>());
    let map_budget =
        metor_fsw_ring::round_up8(4 * map_stride::<Process>() as usize + 4 * 16);
    let expected = core::mem::size_of::<SysBoth>() + list_budget + map_budget + 8;

    assert_eq!(<SysBoth as Componentize>::MAX_SIZE, expected);
    // Concretely: 24 fixed + 128 list + 160 map + 8 pad = 320.
    assert_eq!(<SysBoth as Componentize>::MAX_SIZE, 320);
}
