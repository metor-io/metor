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

fn ring<T: Record>(readers: usize) -> RingBuffer {
    RingBuffer::create_in_memory(Config {
        capacity: ring_capacity(T::MAX_LEN, 8).expect("valid capacity"),
        max_readers: readers,
    })
}

fn raw(ring: &RingBuffer) -> RawRing {
    let (base, len) = ring.region();
    RawRing { base, len }
}

/// Calls `create` the way an export does, returning the instance or the error.
fn create_raw(ty: &str, params: &str, inputs: &[RawPort], outputs: &[RawRing]) -> Instance {
    let mut error = RawSlice::EMPTY;
    // SAFETY: every array outlives the call, and the rings outlive the instance.
    let handle = unsafe {
        create(
            build,
            RawSlice::of(ty.as_bytes()),
            RawSlice::of(params.as_bytes()),
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

#[test]
fn the_descriptor_decodes_as_the_tables_projection() {
    let bytes = def(build);
    // SAFETY: the descriptor lives for the life of the library.
    let decoded: PackDef<'static> =
        serde_json::from_slice(unsafe { bytes.as_bytes() }).expect("the descriptor decodes");
    let types: Vec<_> = decoded.systems.iter().map(|s| s.ty).collect();
    assert_eq!(types[0], "imu");
    assert!(types.contains(&"boom"));
    // The same bytes come back on a second call.
    assert_eq!(def(build).ptr, bytes.ptr);
}

#[test]
fn an_unknown_type_returns_null_with_no_error() {
    let instance = create_raw("gyro", "", &[], &[]);
    assert!(instance.handle.is_null());
    assert!(instance.error().is_empty());
}

#[test]
fn a_bad_params_key_returns_the_tables_own_error() {
    let nav = ring::<Nav>(1);
    let outputs = [raw(&nav)];
    let instance = create_raw("nav", r#"{"gain":2.0}"#, &[input(&[])], &outputs);
    assert!(instance.handle.is_null());
    let decoded: ParamError = serde_json::from_slice(instance.error()).expect("decodes");
    assert_eq!(decoded, ParamError::UnknownKey("gain".into()));
}

#[test]
fn malformed_params_are_a_decode_error() {
    let nav = ring::<Nav>(1);
    let outputs = [raw(&nav)];
    let instance = create_raw("nav", "{", &[input(&[])], &outputs);
    assert!(instance.handle.is_null());
    let decoded: ParamError = serde_json::from_slice(instance.error()).expect("decodes");
    assert!(matches!(decoded, ParamError::Decode(_)));
}

#[test]
fn a_two_system_pipeline_moves_a_record_within_one_cycle() {
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
fn a_panicking_system_returns_panicked_and_writes_one_fault_line() {
    let (imu, log) = (ring::<Imu>(1), ring::<LogEvent>(1));
    let outputs = [raw(&imu), raw(&log)];
    let boom = create_raw("boom", "", &[], &outputs);
    let mut lines = Input::<LogEvent>::try_new(vec![log.view(NoWake).expect("free slot")])
        .expect("supported alignment");
    assert_eq!(boom.step(1), Status::Ok);
    assert_eq!(boom.step(2), Status::Panicked);

    let seen: Vec<LogEvent> = lines.drain().map(|r| r.expect("decodes")).collect();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].level, LogLevel::Error);
    assert_eq!(seen[0].message, "boom on cycle 2");
    assert_eq!(seen[0].fields, vec![("kind".into(), "panic".into())]);
}

#[test]
fn destroy_frees_the_reader_slot_and_the_writer() {
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
fn an_unknown_status_word_is_a_panic() {
    assert_eq!(Status::from_raw(0), Status::Ok);
    assert_eq!(Status::from_raw(1), Status::Panicked);
    assert_eq!(Status::from_raw(7), Status::Panicked);
}

#[test]
fn the_abi_version_is_one() {
    assert_eq!(ABI_VERSION, 1);
}

/// The `metor-fsw-abi` distribution exists to pin this number; a pack's
/// editable wheel requires it exactly.
#[test]
fn the_abi_distributions_version_is_the_abi_version() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/python/metor-fsw-abi/pyproject.toml"
    );
    let manifest: toml::Value = std::fs::read_to_string(path)
        .expect("the distribution is in the tree")
        .parse()
        .expect("valid TOML");
    let version = manifest["project"]["version"]
        .as_str()
        .expect("a version string");
    assert_eq!(version, ABI_VERSION.to_string());
}
