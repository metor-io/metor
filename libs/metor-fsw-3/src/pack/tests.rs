//! Tests that call the pack exports in process, using the shared test table.

use metor_fsw_3_ring::{NoWake, RingBuffer};
use metor_proto_wkt::{LogEvent, LogLevel};

use super::*;
use crate::DefCx;
use crate::coordinator::SystemKind;
use crate::record::Record;
use crate::tests::utils::{self, Imu, Nav, Recorder};

/// Builds the table these tests export. Each thread builds it once.
fn build() -> SystemTable {
    crate::tests::utils::table(&Recorder::default())
}

/// A ring plus the export that its [`RawRing`] borrows from.
struct TestRing {
    ring: RingBuffer,
    export: metor_fsw_3_ring::RingExport,
}

impl TestRing {
    fn raw(&self) -> RawRing {
        RawRing::of(&self.export)
    }
}

fn ring<T: Record>(readers: usize) -> TestRing {
    let ring = utils::ring::<T>(8, readers);
    let export = ring.export();
    TestRing { ring, export }
}

/// Returns the instance JSON the host would send for `ty`.
///
/// An unknown type gets an empty definition, since `create` fails before it
/// binds any ports.
fn instance_def(ty: &str) -> Vec<u8> {
    instance_named(ty, ty)
}

fn instance_named(ty: &str, id: &str) -> Vec<u8> {
    let def = build()
        .get(ty)
        .map(|entry| entry.def.clone())
        .unwrap_or_else(|| {
            crate::SystemDef::new::<(), (), crate::ring::NoWake>("unknown", &crate::DefCx::empty())
                .expect("a static definition")
        });
    let instance = crate::pack::def::Instance {
        id: id.to_string(),
        def,
        adapter: None,
    };
    serde_json::to_vec(&instance).expect("encodes")
}

/// Calls `create` the way the host does and returns the result.
fn create_raw(ty: &str, params: &str, inputs: &[RawSlice], outputs: &[RawRing]) -> Instance {
    let def = instance_def(ty);
    create_with(ty, &def, params, (inputs, outputs), RawRing::EMPTY)
}

/// Calls `create` with instance JSON `def` and IO region `table`.
fn create_with(
    ty: &str,
    def: &[u8],
    params: &str,
    (inputs, outputs): (&[RawSlice], &[RawRing]),
    mut table: RawRing,
) -> Instance {
    let mut error = RawSlice::EMPTY;
    // SAFETY: every array outlives the call, the rings outlive the instance,
    // and a given region's owner is live.
    let handle = unsafe {
        let args = CreateArgs {
            ty: RawSlice::of(ty.as_bytes()),
            params: RawSlice::of(params.as_bytes()),
            def: RawSlice::of(def),
            inputs: RawSlice::of(inputs),
            outputs: RawSlice::of(outputs),
            table: &raw mut table,
        };
        create(build, args, &raw mut error)
    };
    Instance {
        handle,
        error,
        table,
    }
}

/// One `create` result. It is destroyed when the test drops it.
struct Instance {
    handle: *mut c_void,
    error: RawSlice,
    /// The IO region `create` wrote back.
    table: RawRing,
}

impl Instance {
    fn step(&self, now: i64) -> Status {
        // SAFETY: the handle came from `create` on this thread.
        Status::from_raw(unsafe { execute(self.handle, now) })
    }

    /// Returns the error bytes `create` wrote, or an empty slice.
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

fn descriptor(build: fn() -> SystemTable, buffer: &mut [u8]) -> (u32, usize) {
    let mut written = usize::MAX;
    // SAFETY: the buffer and the length slot are writable, do not overlap, and
    // live for the whole call.
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
    // SAFETY: both arrays outlive the call, and the result slots are writable.
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
    let producer = crate::Output::<Imu>::def(&DefCx::empty(), "imu");
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
        outputs: vec![crate::OutputConfig::with_record("out", "gyro")],
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
            .register("imu", crate::tests::utils::imu_source(), move || {
                let _ = &capture;
                0
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
fn test_bad_params_refused() {
    let refused = |params: &str| {
        let (nav, log) = (ring::<Nav>(1), ring::<LogEvent>(1));
        let instance = create_raw("nav", params, &[RawSlice::EMPTY], &[nav.raw(), log.raw()]);
        assert!(instance.handle.is_null());
        serde_json::from_slice::<ParamError>(instance.error()).expect("decodes")
    };
    assert_eq!(
        refused(r#"{"gain":2.0}"#),
        ParamError::UnknownKey("gain".into())
    );
    assert!(matches!(refused("{"), ParamError::Decode(_)));
}

#[test]
fn test_pipeline_single_cycle() {
    let (imu, nav) = (ring::<Imu>(1), ring::<Nav>(1));
    let (imu_log, nav_log) = (ring::<LogEvent>(1), ring::<LogEvent>(1));
    let edges = [imu.raw()];
    let source = create_raw("imu", "", &[], &[imu.raw(), imu_log.raw()]);
    let filter = create_raw(
        "nav",
        "",
        &[RawSlice::of(&edges)],
        &[nav.raw(), nav_log.raw()],
    );
    assert!(!source.handle.is_null() && !filter.handle.is_null());
    let mut sink = utils::input::<Nav>(&nav.ring);

    assert_eq!(source.step(1), Status::Ok);
    assert_eq!(filter.step(1), Status::Ok);

    let estimate = sink.latest().expect("valid").expect("record").estimate;
    assert_eq!(estimate, 2.0);
}

#[test]
fn test_system_panic_status_and_log() {
    let (imu, log) = (ring::<Imu>(1), ring::<LogEvent>(1));
    let boom = create_raw("boom", "", &[], &[imu.raw(), log.raw()]);
    let mut lines = utils::input::<LogEvent>(&log.ring);
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
    let (imu, nav, log) = (ring::<Imu>(1), ring::<Nav>(1), ring::<LogEvent>(1));
    let edges = [imu.raw()];
    let filter = create_raw("nav", "", &[RawSlice::of(&edges)], &[nav.raw(), log.raw()]);
    assert!(imu.ring.view(NoWake).is_err());
    assert!(nav.ring.writer(NoWake).is_err());

    drop(filter);
    assert!(imu.ring.view(NoWake).is_ok());
    assert!(nav.ring.writer(NoWake).is_ok());
}

#[test]
fn test_unowned_ring_attach() {
    let (imu, nav, log) = (ring::<Imu>(1), ring::<Nav>(1), ring::<LogEvent>(1));
    let unowned = |ring: &TestRing| RawRing {
        owner: None,
        ..ring.raw()
    };
    let edges = [unowned(&imu)];
    let filter = create_raw(
        "nav",
        "",
        &[RawSlice::of(&edges)],
        &[unowned(&nav), unowned(&log)],
    );
    assert!(!filter.handle.is_null());
    let mut source = utils::output::<Imu>(&imu.ring);
    let mut sink = utils::input::<Nav>(&nav.ring);
    source.write(&Imu::new(1, 3.0)).expect("room");
    assert_eq!(filter.step(1), Status::Ok);
    assert_eq!(sink.latest().expect("valid").expect("record").estimate, 6.0);

    drop(filter);
    assert!(imu.ring.view(NoWake).is_ok());
    assert!(nav.ring.writer(NoWake).is_ok());
    assert_eq!(sink.latest().expect("valid").expect("record").estimate, 6.0);
}

#[test]
fn test_alloc_free() {
    let ptr = alloc(64, 16);
    assert!(!ptr.is_null() && (ptr as usize).is_multiple_of(16));
    // SAFETY: `alloc` returned 64 zeroed bytes.
    assert!(
        unsafe { core::slice::from_raw_parts(ptr, 64) }
            .iter()
            .all(|b| *b == 0)
    );
    // SAFETY: the same length and alignment `alloc` took, freed once.
    unsafe { free(ptr, 64, 16) };

    assert!(alloc(0, 8).is_null());
    assert!(alloc(8, 3).is_null());
    // SAFETY: a null pointer is ignored.
    unsafe { free(core::ptr::null_mut(), 8, 8) };
}

/// Waits one cycle, then completes.
#[crate::sequence]
async fn blink(seq: &crate::Seq) -> Result<(), crate::SeqError> {
    seq.next_cycle().await;
    Ok(())
}

#[test]
fn test_descriptor_kinds() {
    let mut table = build();
    table
        .register_sequence("blink", blink(), Default::default)
        .expect("valid records");
    let kinds = PackDef::from_table(&table);
    let kind = |ty: &str| {
        let system = kinds.systems.iter().find(|s| s.ty == ty).expect("exported");
        system.kind
    };
    assert_eq!(kind("imu"), SystemKind::Cyclic);
    assert_eq!(kind("trap"), SystemKind::Cyclic);
    assert_eq!(kind("relay"), SystemKind::Async);
    assert_eq!(kind("blink"), SystemKind::Sequence);

    let mut untagged = serde_json::to_value(&kinds.systems[0]).expect("encodes");
    untagged.as_object_mut().expect("an object").remove("kind");
    let decoded: def::PackSystemDef = serde_json::from_value(untagged).expect("decodes");
    assert_eq!(decoded.kind, SystemKind::Cyclic);
}

#[test]
fn test_unknown_status_is_panicked() {
    assert_eq!(Status::from_raw(0), Status::Ok);
    assert_eq!(Status::from_raw(1), Status::Panicked);
    assert_eq!(Status::from_raw(7), Status::Panicked);
    assert_eq!(Status::try_from(4), Ok(Status::Failed));
    assert_eq!(DefStatus::try_from(4), Ok(DefStatus::Refused));
    assert_eq!(Status::try_from(7), Err(7));
    assert_eq!(DefStatus::try_from(5), Err(5));
}

#[test]
fn test_end_status_words() {
    use crate::EndReason;
    for (end, word) in [
        (None, 0),
        (Some(EndReason::Panicked), 1),
        (Some(EndReason::Completed), 2),
        (Some(EndReason::Aborted), 3),
        (Some(EndReason::Failed("reason".into())), 4),
    ] {
        let status = Status::of_end(end.as_ref());
        assert_eq!(status as u32, word);
        assert_eq!(Status::from_raw(word), status);
        assert_eq!(
            status.end().map(|end| core::mem::discriminant(&end)),
            end.as_ref().map(core::mem::discriminant)
        );
    }
    // `Unloaded` has no status word of its own, so a pack reports it as failed.
    assert_eq!(Status::of_end(Some(&EndReason::Unloaded)), Status::Failed);
    assert_eq!(Status::from_raw(5).end(), Some(EndReason::Panicked));
}

/// The `metor-fsw-abi` Python package pins this version number.
///
/// A pack's editable wheel requires that exact version.
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

/// `metor_config` declares the built-in links' pack at this ABI version.
///
/// The built-in links ship with the host, so the versions must match.
#[test]
fn test_builtin_abi_version() {
    let source = include_str!("../../python/metor-config/metor_config/_config.py");
    assert!(
        source.contains(&format!("\nABI_VERSION = {ABI_VERSION}\n")),
        "metor_config declares another ABI"
    );
}

mod region {
    use super::*;
    use crate::io::{Cqe, Sqe};

    /// `uplink`'s rings: its CQ in, its SQ and log out.
    struct Rings {
        cq: TestRing,
        sq: TestRing,
        log: TestRing,
    }

    fn rings() -> Rings {
        Rings {
            cq: ring::<Cqe>(1),
            sq: ring::<Sqe>(1),
            log: ring::<LogEvent>(1),
        }
    }

    fn uplink(id: &str, rings: &Rings, table: RawRing) -> Instance {
        let def = instance_named("uplink", id);
        let edge = [rings.cq.raw()];
        let inputs = [RawSlice::of(&edge)];
        let outputs = [rings.sq.raw(), rings.log.raw()];
        create_with("uplink", &def, "", (&inputs, &outputs), table)
    }

    fn bytes(table: &RawRing) -> &[u8] {
        // SAFETY: the pack keeps the region alive while its member lives.
        unsafe { core::slice::from_raw_parts(table.base, table.len) }
    }

    #[test]
    fn first_create_returns_the_ctors_region() {
        let rings = rings();
        let made = uplink("first", &rings, RawRing::EMPTY);
        assert!(
            !made.handle.is_null(),
            "{:?}",
            String::from_utf8_lossy(made.error())
        );
        assert!(made.table.len > 0 && made.table.owner.is_some());
        let region = bytes(&made.table);
        assert!(region.windows(3).any(|w| w == b"cmd"), "the ctor's buffer");
    }

    #[test]
    fn later_create_allocates_into_the_given_region() {
        let rings = rings();
        let first = uplink("a", &rings, RawRing::EMPTY);
        let table = first.table;
        drop(first);
        let second = uplink("b", &rings, table);
        assert!(
            !second.handle.is_null(),
            "{:?}",
            String::from_utf8_lossy(second.error())
        );
        assert_eq!(
            (second.table.base, second.table.len),
            (table.base, table.len),
            "the same region, adopted"
        );
    }

    #[test]
    fn system_without_io_returns_no_region() {
        let made = create_raw(
            "imu",
            "",
            &[],
            &[ring::<Imu>(1).raw(), ring::<LogEvent>(1).raw()],
        );
        assert!(!made.handle.is_null());
        assert_eq!(made.table.len, 0);
    }
}
