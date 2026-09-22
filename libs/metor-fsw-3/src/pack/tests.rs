//! The exports, driven in process against the shared test table.

use metor_fsw_3_ring::{Config, NoWake, RingBuffer};
use metor_proto_wkt::{LogEvent, LogLevel};

use super::*;
use crate::port::{Input, ring_capacity};
use crate::record::Record;
use crate::tests::utils::{Imu, Nav, Recorder};

/// The one table these tests export; a thread builds it once.
fn build() -> SystemTable {
    crate::tests::utils::table(&Recorder::default())
}

struct TestRing {
    ring: RingBuffer,
    export: metor_fsw_3_ring::RingExport,
}

impl core::ops::Deref for TestRing {
    type Target = RingBuffer;
    fn deref(&self) -> &RingBuffer {
        &self.ring
    }
}

fn ring<T: Record>(readers: usize) -> TestRing {
    let ring = RingBuffer::create_in_memory(Config {
        capacity: ring_capacity(T::MAX_LEN, 8).expect("valid capacity"),
        max_readers: readers,
    });
    let export = ring.export();
    TestRing { ring, export }
}

fn raw(ring: &TestRing) -> RawRing {
    RawRing::of(&ring.export)
}

/// The def the host would resolve for `ty`, as JSON; an unknown type gets an
/// empty one, since `create` never reaches the bindings.
fn instance_def(ty: &str) -> Vec<u8> {
    let def = build()
        .get(ty)
        .map(|entry| entry.def.clone())
        .unwrap_or_else(|| {
            crate::SystemDef::new::<(), ()>("unknown", &crate::DefCx::empty())
                .expect("a static definition")
        });
    let instance = crate::pack::def::Instance {
        id: ty.to_string(),
        def,
        thread: crate::thread::DEFAULT_THREAD.to_string(),
    };
    serde_json::to_vec(&instance).expect("encodes")
}

/// Calls `create` the way an export does, returning the instance or the error.
fn create_raw(ty: &str, params: &str, inputs: &[RawPort], outputs: &[RawRing]) -> Instance {
    let def = instance_def(ty);
    let mut error = RawSlice::EMPTY;
    // SAFETY: every array outlives the call, and the rings outlive the instance.
    let handle = unsafe {
        create(
            build,
            RawSlice::of(ty.as_bytes()),
            RawSlice::of(params.as_bytes()),
            RawSlice::of(&def),
            RawSlice::of(inputs),
            RawSlice::of(outputs),
            &raw mut error,
        )
    };
    Instance { handle, error }
}

/// One `create` result, destroyed when the test drops it.
struct Instance {
    handle: *mut c_void,
    error: RawSlice,
}

impl Instance {
    fn step(&self, now: i64) -> Status {
        // SAFETY: the handle came from `create` on this thread.
        Status::from_raw(unsafe { execute(self.handle, now) })
    }

    /// The error bytes `create` wrote, empty when it wrote none.
    fn error(&self) -> &[u8] {
        // SAFETY: the buffer lives until the next `create` on this thread.
        unsafe { self.error.as_bytes() }
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            // SAFETY: the handle came from `create` and is destroyed once.
            unsafe { destroy(self.handle) };
        }
    }
}

fn input(rings: &[RawRing]) -> RawPort {
    RawPort {
        rings: rings.as_ptr(),
        len: rings.len(),
    }
}

fn descriptor(build: fn() -> SystemTable, buffer: &mut [u8]) -> (u32, usize) {
    let mut written = usize::MAX;
    // SAFETY: buffer and length slot are writable, separate, and live for the call.
    let status = unsafe { def(build, buffer.as_mut_ptr(), buffer.len(), &mut written) };
    (status, written)
}

#[test]
fn test_descriptor_export() {
    let mut bytes = vec![0u8; 64 * 1024];
    let (status, len) = descriptor(build, &mut bytes);
    assert_eq!(status, DefStatus::Ok as u32);
    let decoded: PackDef = serde_json::from_slice(&bytes[..len]).expect("decodes");
    let types: Vec<_> = decoded.systems.iter().map(|s| s.ty.as_str()).collect();
    assert_eq!(types[0], "imu");
    assert!(types.contains(&"boom"));
    let mut exact = vec![0; len];
    assert_eq!(descriptor(build, &mut exact), (status, len));
    assert_eq!(exact, bytes[..len]);

    let mut short = vec![0xa5; len + 1];
    assert_eq!(
        descriptor(build, &mut short[1..len]),
        (DefStatus::TooSmall as u32, 0)
    );
    assert_eq!((short[0], short[len]), (0xa5, 0xa5));
    assert_eq!(descriptor(build, &mut []), (DefStatus::TooSmall as u32, 0));
}

/// Calls the definition export the way the host does.
fn system_def_raw(ty: &str, cx: &DefCxOwned, buffer: &mut [u8]) -> (u32, usize) {
    let cx = serde_json::to_vec(cx).expect("encodes");
    let mut written = usize::MAX;
    // SAFETY: both arrays outlive the call; the result slots are writable.
    let status = unsafe {
        system_def(
            build,
            RawSlice::of(ty.as_bytes()),
            RawSlice::of(&cx),
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut written,
        )
    };
    (status, written)
}

#[test]
fn test_configured_port_export() {
    let producer = crate::Output::<Imu>::def("imu");
    let cx = DefCxOwned {
        inputs: vec![("plant.imu".to_string(), producer)],
        ..DefCxOwned::default()
    };
    let mut bytes = vec![0u8; 64 * 1024];
    let (status, len) = system_def_raw("tap", &cx, &mut bytes);
    assert_eq!(status, DefStatus::Ok as u32);
    let def: SystemDef = serde_json::from_slice(&bytes[..len]).expect("decodes");
    assert_eq!(def.inputs[0].name, "plant.imu");
    assert_eq!(
        system_def_raw("tap", &cx, &mut bytes[..len - 1]),
        (DefStatus::TooSmall as u32, 0)
    );
}

#[test]
fn test_definition_export_errors() {
    let mut bytes = vec![0u8; 4096];
    let (status, len) = system_def_raw("missing", &DefCxOwned::default(), &mut bytes);
    assert_eq!(status, DefStatus::Refused as u32);
    assert!(matches!(
        serde_json::from_slice(&bytes[..len]).expect("decodes"),
        DefError::Pack { message } if message.contains("missing")
    ));

    let cx = DefCxOwned {
        outputs: vec![crate::OutputConfig {
            port: "out".into(),
            record: "gyro".into(),
        }],
        ..DefCxOwned::default()
    };
    let (status, len) = system_def_raw("emit", &cx, &mut bytes);
    assert_eq!(status, DefStatus::Refused as u32);
    assert_eq!(
        serde_json::from_slice::<DefError>(&bytes[..len]).expect("decodes"),
        DefError::UnknownRecord {
            port: "out".into(),
            record: "gyro".into(),
        }
    );
}

#[test]
fn test_descriptor_panic_containment() {
    fn broken() -> SystemTable {
        panic!("table build");
    }
    // Each test thread has an uninitialized table.
    assert_eq!(
        descriptor(broken, &mut [0; 128]),
        (DefStatus::Panicked as u32, 0)
    );
    assert_eq!(
        descriptor(SystemTable::new, &mut [0; 128]),
        (DefStatus::Ok as u32, 14)
    );
}

#[test]
fn test_table_capture_lifetime() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static DROPS: AtomicUsize = AtomicUsize::new(0);
    struct Capture;
    impl Drop for Capture {
        fn drop(&mut self) {
            DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn captured() -> SystemTable {
        let capture = Capture;
        let mut table = SystemTable::new();
        table
            .register_system("imu", move |_| {
                let _ = &capture;
                Ok((crate::tests::utils::ImuSource, 0))
            })
            .expect("valid records");
        table
    }
    std::thread::spawn(|| {
        let mut buffer = [0; 4096];
        for _ in 0..3 {
            assert_eq!(descriptor(captured, &mut buffer).0, DefStatus::Ok as u32);
        }
        assert_eq!(DROPS.load(Ordering::Relaxed), 0);
    })
    .join()
    .expect("worker");
    assert_eq!(DROPS.load(Ordering::Relaxed), 1);
}

#[test]
fn test_unknown_system_type() {
    let instance = create_raw("gyro", "", &[], &[]);
    assert!(instance.handle.is_null());
    let error: ParamError = serde_json::from_slice(instance.error()).expect("error");
    assert!(matches!(error, ParamError::Decode(_)));
}

#[test]
fn test_unknown_param_key() {
    let nav = ring::<Nav>(1);
    let outputs = [raw(&nav)];
    let instance = create_raw("nav", r#"{"gain":2.0}"#, &[input(&[])], &outputs);
    assert!(instance.handle.is_null());
    let decoded: ParamError = serde_json::from_slice(instance.error()).expect("decodes");
    assert_eq!(decoded, ParamError::UnknownKey("gain".into()));
}

#[test]
fn test_malformed_params() {
    let nav = ring::<Nav>(1);
    let outputs = [raw(&nav)];
    let instance = create_raw("nav", "{", &[input(&[])], &outputs);
    assert!(instance.handle.is_null());
    let decoded: ParamError = serde_json::from_slice(instance.error()).expect("decodes");
    assert!(matches!(decoded, ParamError::Decode(_)));
}

#[test]
fn test_pipeline_single_cycle() {
    let (imu, nav) = (ring::<Imu>(1), ring::<Nav>(1));
    let imu_out = [raw(&imu)];
    let nav_out = [raw(&nav)];
    let edges = [raw(&imu)];
    let source = create_raw("imu", "", &[], &imu_out);
    let filter = create_raw("nav", "", &[input(&edges)], &nav_out);
    assert!(!source.handle.is_null() && !filter.handle.is_null());
    let mut sink = Input::<Nav>::try_new(vec![nav.view(NoWake).expect("free slot")])
        .expect("supported alignment");

    assert_eq!(source.step(1), Status::Ok);
    assert_eq!(filter.step(1), Status::Ok);

    let estimate = sink.latest().expect("valid").expect("record").estimate;
    assert_eq!(estimate, 2.0);
}

#[test]
fn test_system_panic_status_and_log() {
    let (imu, log) = (ring::<Imu>(1), ring::<LogEvent>(1));
    let outputs = [raw(&imu), raw(&log)];
    let boom = create_raw("boom", "", &[], &outputs);
    let mut lines = Input::<LogEvent>::try_new(vec![log.view(NoWake).expect("free slot")])
        .expect("supported alignment");
    assert_eq!(boom.step(1), Status::Ok);
    assert_eq!(boom.step(2), Status::Panicked);
    assert_eq!(boom.step(3), Status::Panicked);

    let seen: Vec<LogEvent> = lines.drain().map(|r| r.expect("decodes")).collect();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].level, LogLevel::Error);
    assert_eq!(seen[0].message, "boom on cycle 2");
    assert_eq!(seen[0].fields, vec![("kind".into(), "panic".into())]);
}

#[test]
fn test_destroy_releases_ring_handles() {
    let (imu, nav) = (ring::<Imu>(1), ring::<Nav>(1));
    let nav_out = [raw(&nav)];
    let edges = [raw(&imu)];
    let filter = create_raw("nav", "", &[input(&edges)], &nav_out);
    assert!(imu.view(NoWake).is_err());
    assert!(nav.writer(NoWake).is_err());

    drop(filter);
    assert!(imu.view(NoWake).is_ok());
    assert!(nav.writer(NoWake).is_ok());
}

#[test]
fn test_unknown_status_is_panicked() {
    assert_eq!(Status::from_raw(0), Status::Ok);
    assert_eq!(Status::from_raw(1), Status::Panicked);
    assert_eq!(Status::from_raw(7), Status::Panicked);
}

/// The `metor-fsw-abi` distribution exists to pin this number; a pack's
/// editable wheel requires it exactly.
#[test]
fn test_python_abi_version() {
    let manifest: toml::Value = include_str!("../../python/metor-fsw-abi/pyproject.toml")
        .parse()
        .expect("valid TOML");
    let version = manifest["project"]["version"]
        .as_str()
        .expect("a version string");
    assert_eq!(version, ABI_VERSION.to_string());
}

/// The built-in links ship with the host, so `metor_config` declares their
/// pack at this ABI.
#[test]
fn test_builtin_abi_version() {
    let source = include_str!("../../python/metor-config/metor_config/_config.py");
    assert!(
        source.contains(&format!("\nABI_VERSION = {ABI_VERSION}\n")),
        "metor_config declares another ABI"
    );
}
