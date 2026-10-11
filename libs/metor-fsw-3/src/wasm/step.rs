//! One system instance in a guest of its own, stepped through copies of its
//! rings.

use std::borrow::Cow;

use metor_fsw_3_ring::NoWake;
use metor_proto::types::Timestamp;
use metor_proto_wkt::LogLevel;

use crate::coordinator::{EndReason, Step};
use crate::mirror::{Corrupt, Mirrors};
use crate::pack::Status;

use super::guest::{Guest, Trap};

/// A `WasmStep` is one instance living in its own guest.
///
/// Each cycle it copies the real input rings into the guest's, calls
/// `metor_fsw_execute`, and copies the guest's output rings into the real
/// ones, whatever the call returned.
pub struct WasmStep {
    /// Declared before `guest`: dropping its handles writes into guest memory.
    mirrors: Mirrors<NoWake>,
    guest: Guest,
    instance: u32,
    fuel: u64,
    end: Option<EndReason>,
}

impl WasmStep {
    pub(super) fn new(mirrors: Mirrors<NoWake>, guest: Guest, instance: u32, fuel: u64) -> Self {
        Self {
            mirrors,
            guest,
            instance,
            fuel,
            end: None,
        }
    }
}

impl Step for WasmStep {
    fn execute(&mut self, now: Timestamp) {
        if self.end.is_some() {
            return;
        }
        let fed = self.mirrors.drain_inputs();
        let result = self.guest.execute(self.instance, now.0, self.fuel);
        let taken = self.mirrors.drain_outputs();
        if let Some(end) = end_of(fed.and(taken), result) {
            if let Some((kind, message)) = end.line {
                self.mirrors.write_line(now, LogLevel::Error, kind, message);
            }
            self.end = Some(end.reason);
            self.mirrors.release_inputs();
        }
        self.mirrors.report_drops(now);
    }

    /// Nothing: the host's line already reports how a guest ended.
    fn fault(&mut self, _now: Timestamp, _message: &str) {}

    fn ended(&self) -> Option<&EndReason> {
        self.end.as_ref()
    }
}

/// How a step ended, and the host's line about it when the guest wrote none.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct End {
    pub(super) reason: EndReason,
    /// The line's `kind` and message.
    pub(super) line: Option<(&'static str, &'static str)>,
}

impl End {
    fn host(reason: EndReason, kind: &'static str, message: &'static str) -> Self {
        Self {
            reason,
            line: Some((kind, message)),
        }
    }
}

/// The end one cycle's copies and call add up to, `None` while it runs.
pub(super) fn end_of(copied: Result<(), Corrupt>, result: Result<u32, Trap>) -> Option<End> {
    if copied.is_err() {
        let reason = EndReason::Failed(Cow::Borrowed("corrupt ring"));
        return Some(End::host(reason, "corrupt", "a guest ring is corrupt"));
    }
    match result {
        Ok(word) => match Status::try_from_raw(word) {
            Some(status) => status.end().map(|reason| End { reason, line: None }),
            None => Some(End::host(
                EndReason::Panicked,
                "bad_status",
                "the guest returned no known status",
            )),
        },
        Err(Trap::OutOfFuel) => Some(End::host(
            EndReason::Failed(Cow::Borrowed("out of fuel")),
            "fuel",
            "the guest ran out of fuel",
        )),
        Err(trap) => Some(End::host(EndReason::Panicked, "trap", trap.as_str())),
    }
}
