use super::*;

/// A `DlSlot` over stub exports, no artifact involved: `lib` is a handle
/// to this very process (with no pack to close) and `state` a dangling
/// non-null the stubs never dereference.
#[cfg(all(any(target_os = "linux", target_os = "macos"), not(miri)))]
fn stub_slot(execute: ExecuteFn, destroy: DestroyFn) -> DlSlot {
    unsafe extern "C" fn bind(
        _: *mut c_void,
        _: *const FswRing,
        _: usize,
        _: *const FswRing,
        _: usize,
        _: *const u8,
        _: usize,
    ) {
    }
    unsafe extern "C" fn nop(_: *mut c_void) {}
    DlSlot {
        lib: Rc::new(PackLib {
            pack: PackGuard {
                ptr: core::ptr::null_mut(),
                close: None,
            },
            lib: libloading::os::unix::Library::this().into(),
        }),
        bind_init: bind,
        execute,
        shutdown: nop,
        destroy,
        state: core::ptr::NonNull::<c_void>::dangling().as_ptr(),
        inputs: Vec::new(),
        outputs: Vec::new(),
        name: Arc::from("stub"),
        instance: Arc::from("stub"),
        slot_state: SlotState::Running,
    }
}

/// The poll-once guard: `step_seq` latches a terminal `Done` and
/// re-serves it without ever reaching the execute export again.
#[cfg(all(any(target_os = "linux", target_os = "macos"), not(miri)))]
#[test]
fn step_seq_latches_done() {
    use core::sync::atomic::{AtomicU32, Ordering::Relaxed};
    static POLLS: AtomicU32 = AtomicU32::new(0);
    unsafe extern "C" fn exec(_: *mut c_void, _: u64) -> u32 {
        POLLS.fetch_add(1, Relaxed);
        FswStatus::Done as u32
    }
    unsafe extern "C" fn nop(_: *mut c_void) {}
    let mut slot = stub_slot(exec, nop);
    assert_eq!(slot.step_seq(Timestamp(1)), FswStatus::Done);
    assert_eq!(slot.step_seq(Timestamp(2)), FswStatus::Done);
    assert_eq!(POLLS.load(Relaxed), 1, "a Ready future is never re-polled");
    assert!(matches!(slot.slot_state, SlotState::Done { .. }));
}

/// `Panicked` destroys the foreign state exactly once (releasing its
/// ring roles, the same policy as `step`) and is re-served from the
/// latch; the nulled state keeps `Drop` a no-op.
#[cfg(all(any(target_os = "linux", target_os = "macos"), not(miri)))]
#[test]
fn step_seq_panicked_destroys_once() {
    use core::sync::atomic::{AtomicU32, Ordering::Relaxed};
    static POLLS: AtomicU32 = AtomicU32::new(0);
    static DESTROYS: AtomicU32 = AtomicU32::new(0);
    unsafe extern "C" fn exec(_: *mut c_void, _: u64) -> u32 {
        POLLS.fetch_add(1, Relaxed);
        FswStatus::Panicked as u32
    }
    unsafe extern "C" fn destroy(_: *mut c_void) {
        DESTROYS.fetch_add(1, Relaxed);
    }
    let mut slot = stub_slot(exec, destroy);
    assert_eq!(slot.step_seq(Timestamp(1)), FswStatus::Panicked);
    assert_eq!(slot.step_seq(Timestamp(2)), FswStatus::Panicked);
    assert_eq!(POLLS.load(Relaxed), 1);
    drop(slot);
    assert_eq!(
        DESTROYS.load(Relaxed),
        1,
        "destroyed at the panic, not at Drop"
    );
}

/// An entry declaring a host capability is a clean load rejection,
/// never a bind-time panic; an empty list (every real export) passes.
#[test]
fn load_rejects_declared_capabilities() {
    let mk = |capabilities| PackEntryDesc {
        descriptor: crate::SystemDescriptor {
            name: "rogue".to_string(),
            kind: crate::SystemKind::Cyclic,
            inputs: Vec::new(),
            outputs: Vec::new(),
            capabilities,
        },
        params_schema: OwnedNamedType::from(<() as postcard_schema::Schema>::SCHEMA),
        params_docs: Vec::new(),
        reloadable: true,
        params_default: None,
    };

    assert!(reject_capabilities(mk(Vec::new())).is_ok());
    let err = reject_capabilities(mk(vec![crate::Capability::ReceiveAll]))
        .map(|_| ())
        .expect_err("host-only capability rejected");
    assert!(
        matches!(&err, DlError::UnsupportedCapabilities(c) if c == &[crate::Capability::ReceiveAll]),
        "{err:?}"
    );
}
