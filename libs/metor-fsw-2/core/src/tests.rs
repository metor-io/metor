//! Frame acceptance tests: derive output, dynamic members, and size accounting.

use core::mem::offset_of;
use std::collections::HashMap;

use metor_component::{AsVTable, Componentize, Metadatatize};
use metor_fsw_ring::{Config, NoWake, RingBuffer};
use metor_proto::types::{ComponentId, ComponentView, Timestamp};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::{
    DynamicWriteError, Frame, FrameList, FrameMap, FrameWriteError, FrameWriter, KeyError, Output,
};

// --- Shared recording sink ---

#[derive(Default)]
struct RecSink {
    values: HashMap<ComponentId, f64>,
    timestamps: HashMap<ComponentId, Option<Timestamp>>,
}

impl metor_component::Decomponentize for RecSink {
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

fn components<F: Frame>(table: &[u8]) -> RecSink {
    let mut sink = RecSink::default();
    F::as_vtable().apply(table, &mut sink).unwrap().unwrap();
    sink
}

// --- Frame tag, shared timestamp, and timestamp suppression ---

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
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
    // The frame tag rides on `RealizedField` and is never handed to `apply_value`,
    // so realize the fields explicitly to see it.
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
    // The timestamp field only sources the shared timestamp; it must not appear
    // as a component of its own.
    assert!(
        !by_id.contains_key(&ComponentId::new("imu.timestamp")),
        "imu.timestamp must be suppressed"
    );
    assert_eq!(Imu::FRAME_ID, ComponentId::new("imu"));
    assert_eq!(imu.timestamp(), Timestamp(4242));
}

// --- Static-frame Componentize / Decomponentize round-trip ---

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default, Debug, PartialEq)]
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
    // Every component round-trips except the suppressed timestamp field.
    assert_eq!(dst.gyro_x, src.gyro_x);
    assert_eq!(dst.gyro_y, src.gyro_y);
    assert_eq!(dst.count, src.count);
    assert_eq!(dst.timestamp, Timestamp(0));
}

// --- Array (`[T; N]`) frame field ---

#[derive(Default)]
struct VecSink {
    values: HashMap<ComponentId, Vec<f64>>,
}

impl metor_component::Decomponentize for VecSink {
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

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default, Debug, PartialEq)]
#[repr(C)]
#[metor_fsw(name = "arr")]
struct ArrFrame {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    v: [f64; 3],
    // `u64` rather than `u32` so the `#[repr(C)]` layout has no padding, which
    // `IntoBytes` requires.
    n: u64,
}

#[test]
fn array_field_frame_round_trip() {
    let frame = ArrFrame {
        timestamp: Timestamp(77),
        v: [1.5, -2.5, 3.25],
        n: 42,
    };

    // (a) In-process Componentize into Decomponentize round-trip.
    let mut dst = ArrFrame::default();
    frame.sink_columns(&mut dst);
    assert_eq!(dst.v, frame.v);
    assert_eq!(dst.n, frame.n);

    // (b) Bytes through the vtable into a recording sink. The two paths represent
    // arrays differently on purpose: `as_component_view` (part a) emits `arr.v` as
    // a single shape-[3] component, while the blanket `AsVTable` impl for `[T; N]`
    // expands the array into N indexed scalars, so the vtable path yields
    // `arr.v.0/1/2`.
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

// --- Skipped fields: `_`-prefixed padding and explicit overrides ---

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "padf")]
struct PadFrame {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    flag: u8,
    // Zerocopy `IntoBytes` forbids implicit padding, so the layout hole is a
    // named field. It must never surface as telemetry.
    _pad: [u8; 7],
    value: f64,
}

#[test]
fn underscore_pad_field_skipped() {
    let frame = PadFrame {
        timestamp: Timestamp(1),
        flag: 3,
        _pad: [0; 7],
        value: 2.5,
    };
    let sink = components::<PadFrame>(frame.as_bytes());
    assert!(sink.values.contains_key(&ComponentId::new("padf.flag")));
    assert!(sink.values.contains_key(&ComponentId::new("padf.value")));
    // The `[u8; 7]` blanket impl would expand `_pad` into seven indexed leaves.
    for i in 0..7 {
        assert!(
            !sink
                .values
                .contains_key(&ComponentId::new(&format!("padf._pad.{i}"))),
            "padf._pad.{i} must not be telemetered"
        );
    }
    let names: Vec<String> = PadFrame::metadata("").map(|m| m.name).collect();
    assert!(names.iter().all(|n| !n.contains("_pad")), "{names:?}");
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "ovr")]
struct SkipOverrideFrame {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    // A `_`-field opted back in, and a normal field force-hidden.
    #[metor_fsw(skip = false)]
    _shown: f64,
    #[metor_fsw(skip)]
    hidden: f64,
    plain: f64,
}

#[test]
fn skip_attribute_overrides_default() {
    let frame = SkipOverrideFrame {
        timestamp: Timestamp(1),
        _shown: 1.0,
        hidden: 2.0,
        plain: 3.0,
    };
    let sink = components::<SkipOverrideFrame>(frame.as_bytes());
    assert!(sink.values.contains_key(&ComponentId::new("ovr._shown")));
    assert!(sink.values.contains_key(&ComponentId::new("ovr.plain")));
    assert!(!sink.values.contains_key(&ComponentId::new("ovr.hidden")));
}

// A nested struct reaches telemetry through the standalone derives (the v1
// macro crate), not the `Frame` derive; padding must skip on that path too.
#[derive(
    AsVTable,
    Metadatatize,
    Componentize,
    metor_component::Decomponentize,
    IntoBytes,
    Immutable,
    KnownLayout,
    FromBytes,
    Default,
    Clone,
    Copy,
)]
#[repr(C)]
struct PadWheel {
    speed: f64,
    arm: u8,
    _pad: [u8; 7],
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "wheelf")]
struct PadWheelFrame {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    #[metor_fsw(nest)]
    wheels: [PadWheel; 2],
}

#[test]
fn nested_struct_pad_field_skipped() {
    let frame = PadWheelFrame {
        timestamp: Timestamp(1),
        wheels: [PadWheel {
            speed: 2.0,
            arm: 1,
            _pad: [0; 7],
        }; 2],
    };
    let sink = components::<PadWheelFrame>(frame.as_bytes());
    assert!(
        sink.values
            .contains_key(&ComponentId::new("wheelf.wheels.0.speed"))
    );
    assert!(
        sink.values
            .contains_key(&ComponentId::new("wheelf.wheels.1.arm"))
    );
    for w in 0..2 {
        for i in 0..7 {
            let id = format!("wheelf.wheels.{w}._pad.{i}");
            assert!(
                !sink.values.contains_key(&ComponentId::new(&id)),
                "{id} must not be telemetered"
            );
        }
    }
    let names: Vec<String> = PadWheelFrame::metadata("").map(|m| m.name).collect();
    assert!(names.iter().all(|n| !n.contains("_pad")), "{names:?}");
}

// --- FrameList / FrameMap: build, serialize, apply ---

#[derive(AsVTable, IntoBytes, Immutable, KnownLayout, Clone, Copy)]
#[repr(C)]
struct Process {
    pid: u64,
    cpu_usage: f64,
}

fn process(pid: u64, cpu_usage: f64) -> Process {
    Process { pid, cpu_usage }
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
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
    w.list(&frame.processes, offset_of!(SysList, processes), |l| {
        l.push(process(1001, 0.5));
        l.push(process(1002, 0.25));
    })
    .unwrap();

    let sink = components::<SysList>(w.table());

    assert_eq!(sink.values[&ComponentId::new("processes.0.pid")], 1001.0);
    assert_eq!(sink.values[&ComponentId::new("processes.0.cpu_usage")], 0.5);
    assert_eq!(sink.values[&ComponentId::new("processes.1.pid")], 1002.0);
    assert_eq!(
        sink.values[&ComponentId::new("processes.1.cpu_usage")],
        0.25
    );
    // Elements inherit the shared frame timestamp.
    assert_eq!(
        sink.timestamps[&ComponentId::new("processes.0.pid")],
        Some(Timestamp(1000))
    );
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
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
    w.map(&frame.processes, offset_of!(SysMap, processes), |m| {
        m.insert("htop", process(1001, 0.5));
        m.insert("init", process(1002, 0.25));
    })
    .unwrap();

    let sink = components::<SysMap>(w.table());

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
    let res = w.map(&frame.processes, offset_of!(SysMap, processes), |m| {
        m.insert("a.b", process(1, 0.0));
    });
    assert_eq!(res, Err(DynamicWriteError::Key(KeyError::DotInKey)));
}

#[test]
fn dynamic_writers_enforce_declared_bounds_and_roll_back() {
    let list = SysList {
        timestamp: Timestamp(0),
        processes: FrameList::EMPTY,
    };
    let mut writer = FrameWriter::new(&list);
    let fixed_len = writer.table().len();
    let err = writer
        .list(&list.processes, offset_of!(SysList, processes), |items| {
            for pid in 0..=8 {
                items.push(process(pid, 0.0));
            }
        })
        .unwrap_err();
    assert_eq!(err, DynamicWriteError::ListFull { max: 8 });
    assert_eq!(writer.table().len(), fixed_len);

    let map = SysMap {
        timestamp: Timestamp(0),
        processes: FrameMap::EMPTY,
    };
    let mut writer = FrameWriter::new(&map);
    let err = writer
        .map(&map.processes, offset_of!(SysMap, processes), |entries| {
            entries.insert(&"x".repeat(33), process(1, 0.0));
        })
        .unwrap_err();
    assert_eq!(err, DynamicWriteError::KeyTooLong { len: 33, max: 32 });

    let mut writer = FrameWriter::new(&map);
    let err = writer
        .map(&map.processes, offset_of!(SysMap, processes), |entries| {
            for pid in 0..=8 {
                entries.insert(&format!("p{pid}"), process(pid, 0.0));
            }
        })
        .unwrap_err();
    assert_eq!(err, DynamicWriteError::MapFull { max: 8 });
}

#[test]
fn output_rejects_dynamic_overflow_before_ring_write() {
    let ring = RingBuffer::create_in_memory(Config {
        capacity: crate::buffer_capacity::<SysList>(2),
        max_readers: 1,
    });
    let mut output = Output::<SysList>::new(ring.writer(NoWake).unwrap());
    let frame = SysList {
        timestamp: Timestamp(0),
        processes: FrameList::EMPTY,
    };
    let err = output
        .write_with(&frame, |writer| {
            let _ = writer.list(&frame.processes, offset_of!(SysList, processes), |items| {
                for pid in 0..=8 {
                    items.push(process(pid, 0.0));
                }
            });
        })
        .unwrap_err();
    assert_eq!(
        err,
        FrameWriteError::Dynamic(DynamicWriteError::ListFull { max: 8 })
    );
}

/// Locks the exact trailer layout of a map member: the fixed region, an
/// 8-aligned fixed-stride entry array (header, padding to the value's
/// alignment, value bytes), then the key pool the rebased offsets point into.
#[test]
fn frame_map_exact_byte_layout() {
    use crate::dynamic::{map_stride, map_value_offset};

    let frame = SysMap {
        timestamp: Timestamp(1000),
        processes: FrameMap::EMPTY,
    };
    let mut w = FrameWriter::new(&frame);
    w.map(&frame.processes, offset_of!(SysMap, processes), |m| {
        m.insert("htop", process(1001, 0.5));
        m.insert("init", process(1002, 0.25));
    })
    .unwrap();

    // Process { pid: u64, cpu_usage: f64 }: the value starts right after the
    // 8-byte header and the 24-byte entry needs no tail padding.
    assert_eq!(map_value_offset::<Process>(), 8);
    assert_eq!(map_stride::<Process>(), 24);

    let fixed = core::mem::size_of::<SysMap>(); // 16, already 8-aligned
    let mut want = Vec::new();
    want.extend_from_slice(&1000i64.to_le_bytes()); // timestamp
    want.extend_from_slice(&(fixed as u32).to_le_bytes()); // slot.trailer_off
    want.extend_from_slice(&48u32.to_le_bytes()); // slot.byte_len = 2 * stride
    // entry[0]: key_off -> pool start (fixed + 48), key_len 4, then the value.
    want.extend_from_slice(&64u32.to_le_bytes());
    want.extend_from_slice(&4u32.to_le_bytes());
    want.extend_from_slice(&1001u64.to_le_bytes());
    want.extend_from_slice(&0.5f64.to_le_bytes());
    // entry[1]: key_off past "htop".
    want.extend_from_slice(&68u32.to_le_bytes());
    want.extend_from_slice(&4u32.to_le_bytes());
    want.extend_from_slice(&1002u64.to_le_bytes());
    want.extend_from_slice(&0.25f64.to_le_bytes());
    want.extend_from_slice(b"htopinit");

    assert_eq!(w.table(), &want[..]);
}

/// A rejected key rolls the whole map member back: earlier members survive,
/// the failed member's slot stays zeroed, and the same writer can keep
/// appending members at correct offsets afterwards.
#[test]
fn frame_map_key_error_rolls_the_member_back() {
    let frame = SysBoth {
        timestamp: Timestamp(7),
        procs: FrameList::EMPTY,
        hosts: FrameMap::EMPTY,
    };
    let mut w = FrameWriter::new(&frame);
    w.list(&frame.procs, offset_of!(SysBoth, procs), |l| {
        l.push(process(1, 1.0));
    })
    .unwrap();
    let len_after_list = w.table().len();

    let res = w.map(&frame.hosts, offset_of!(SysBoth, hosts), |m| {
        m.insert("ok", process(2, 2.0));
        m.insert("bad.key", process(3, 3.0));
    });
    assert_eq!(res, Err(DynamicWriteError::Key(KeyError::DotInKey)));
    assert_eq!(
        w.table().len(),
        len_after_list,
        "the failed member's bytes are rolled back"
    );

    // A retry with valid keys lands at the same trailer position.
    w.map(&frame.hosts, offset_of!(SysBoth, hosts), |m| {
        m.insert("ok", process(2, 2.0));
    })
    .unwrap();

    let sink = components::<SysBoth>(w.table());
    assert_eq!(sink.values[&ComponentId::new("procs.0.pid")], 1.0);
    assert_eq!(sink.values[&ComponentId::new("hosts.ok.pid")], 2.0);
    assert_eq!(
        sink.values.get(&ComponentId::new("hosts.bad.key.pid")),
        None
    );
}

/// Recycling the backing through `finish`/`from_scratch` produces tables
/// byte-identical to a fresh writer's, including after a larger frame grew
/// the buffers.
#[test]
fn frame_scratch_reuse_is_byte_identical() {
    let frame = SysMap {
        timestamp: Timestamp(1000),
        processes: FrameMap::EMPTY,
    };
    let build = |w: &mut FrameWriter<SysMap>| {
        w.map(&frame.processes, offset_of!(SysMap, processes), |m| {
            m.insert("htop", process(1001, 0.5));
        })
        .unwrap();
    };

    let mut fresh = FrameWriter::new(&frame);
    build(&mut fresh);
    let want = fresh.table().to_vec();

    // Grow the recycled backing with a bigger table first, then rebuild the
    // small one from the same scratch.
    let mut big = FrameWriter::from_scratch(fresh.finish(), &frame);
    big.map(&frame.processes, offset_of!(SysMap, processes), |m| {
        for i in 0..8 {
            m.insert(&format!("proc_{i}"), process(i, 0.0));
        }
    })
    .unwrap();

    let mut reused = FrameWriter::from_scratch(big.finish(), &frame);
    build(&mut reused);
    assert_eq!(reused.table(), &want[..]);
}

// --- Nested dynamics: processes.htop.threads.0.state ---

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

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
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
    // Naming contract for the derived vtable. The outer map is reached statically,
    // so its ops carry the full dotted prefix `processes`; the inner `threads` list
    // lives in a member template and is named by its own field alone. The names
    // only join up when the trailer is walked. The trailer below is one map entry
    // keyed "htop" whose Host value holds a one-element thread list.
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

// --- MAX_SIZE formula ---

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
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
    let map_budget = metor_fsw_ring::round_up8(4 * map_stride::<Process>() as usize + 4 * 16);
    let expected = core::mem::size_of::<SysBoth>() + list_budget + map_budget + 8;

    assert_eq!(<SysBoth as Componentize>::MAX_SIZE, expected);
    // Concretely: 24 fixed + 128 list + 160 map + 8 pad = 320.
    assert_eq!(<SysBoth as Componentize>::MAX_SIZE, 320);
}
