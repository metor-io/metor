//! The per-system run record, `<instance>.system_status`.
//!
//! The host authors this frame, not the system: the coordinator issues every
//! cyclic step, so it is the authority on how many steps a system has taken,
//! how long the last one took, and what state the slot is in. One record per
//! slot per cycle, from one publishing path, for every cyclic kind: an
//! in-process runner, a pack entry, a dylib, a process worker, a wasm guest.
//! A system reports nothing here; its own channel is the log
//! ([`crate::log`]).
//!
//! The host appends the port to each system's descriptor at registration
//! ([`host_status_port`]) rather than the system declaring it, so a guest's
//! positional ring arrays never include it and the ring's single writer is
//! the host's.
//!
//! A free-running `AsyncSystem` is the one exception: the host never steps
//! it, so it publishes its own record through a [`StatusPort`] on its
//! context, at whatever cadence its loop has.

use metor_proto::types::Timestamp;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::Frame;
use crate::binder::RingSource;
use crate::descriptor::{PortConn, PortDesc};
use crate::port::Output;
use crate::slot::SlotState;

/// One system's run record at the end of a cycle.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "system_status")]
pub struct SystemStatus {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    /// Steps the host has issued to this slot, including no-op steps on a
    /// stopped or empty slot (`state` tells them apart).
    pub cycles: u64,
    /// How long the last step took, in microseconds. For a process system
    /// this is the host's doorbell-to-ack wait, scheduling included.
    pub last_execute_us: u64,
    /// The slot's run state, as [`SlotState::code`].
    pub state: u8,
    pub _pad: [u8; 7],
}

/// The descriptor entry the host appends to every system it registers.
pub fn host_status_port() -> PortDesc {
    PortDesc::of::<SystemStatus>().with_conn(PortConn::Host)
}

/// Publish one record. The single path every author of this frame uses.
pub fn publish_status(
    out: &mut Output<SystemStatus>,
    now: Timestamp,
    cycles: u64,
    last_execute_us: u64,
    state: u8,
) {
    out.publish(&SystemStatus {
        timestamp: now,
        cycles,
        last_execute_us,
        state,
        _pad: [0; 7],
    });
}

/// An async system's handle on its own status record.
pub struct StatusPort {
    out: Output<SystemStatus>,
    cycles: u64,
}

impl StatusPort {
    pub fn new(out: Output<SystemStatus>) -> Self {
        Self { out, cycles: 0 }
    }

    /// Bind over the next output ring the source offers.
    pub fn bind<S: RingSource>(src: &mut S) -> Self {
        Self::new(Output::bind(src))
    }

    /// Close one iteration of the loop: bumps the count and publishes a
    /// record stamped with the ambient cycle clock.
    pub fn tick(&mut self, last_execute_us: u64) {
        self.cycles += 1;
        publish_status(
            &mut self.out,
            crate::clock::now_or_wall(),
            self.cycles,
            last_execute_us,
            SlotState::Running.code(),
        );
    }
}
