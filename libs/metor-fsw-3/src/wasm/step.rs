//! Runs one system instance inside its own wasm guest.

use std::borrow::Cow;

use metor_fsw_3_ring::NoWake;
use metor_proto::types::Timestamp;
use metor_proto_wkt::LogLevel;

use crate::coordinator::{EndReason, Step, StepCx};
use crate::mirror::{Corrupt, Mirrors};
use crate::pack::Status;

use super::guest::{Guest, Trap};
use super::io::IoCopy;

/// A system instance running in its own wasm guest.
///
/// Each cycle, it copies new records from the real input rings into the
/// guest's copies, calls `metor_fsw_execute`, and then copies the guest's
/// output records into the real output rings. Outputs are copied even if the
/// call failed. A system with a `Cap` also has its IO buffer bytes copied
/// around the call.
pub struct WasmStep {
    /// Must be declared before `guest`, because dropping these handles writes
    /// into guest memory.
    mirrors: Mirrors<NoWake>,
    /// Declared before `guest`, since it reads guest memory.
    io: Option<IoCopy>,
    guest: Guest,
    instance: u32,
    fuel: u64,
    end: Option<EndReason>,
}

impl WasmStep {
    pub(super) fn new(
        mirrors: Mirrors<NoWake>,
        io: Option<IoCopy>,
        guest: Guest,
        instance: u32,
        fuel: u64,
    ) -> Self {
        Self {
            mirrors,
            io,
            guest,
            instance,
            fuel,
            end: None,
        }
    }
}

impl Step for WasmStep {
    fn execute(&mut self, now: Timestamp, _cx: &mut StepCx<'_>) {
        if self.end.is_some() {
            return;
        }
        let fed = self.mirrors.drain_inputs();
        let read = self.io.as_mut().map_or(Ok(()), IoCopy::completions);
        let result = self.guest.execute(self.instance, now.0, self.fuel);
        let written = self.io.as_mut().map_or(Ok(()), IoCopy::submissions);
        let taken = self.mirrors.drain_outputs();
        let end = match read.and(written) {
            Ok(()) => End::from_cycle(fed.and(taken), result),
            Err(_) => Some(End::corrupt_table()),
        };
        if let Some(end) = end {
            if let Some((kind, message)) = end.line {
                self.mirrors.write_line(now, LogLevel::Error, kind, message);
            }
            self.end = Some(end.reason);
            self.mirrors.release_inputs();
        }
        self.mirrors.report_drops(now);
    }

    /// Does nothing, because the host already logged how the guest ended.
    fn fault(&mut self, _now: Timestamp, _message: &str) {}

    fn ended(&self) -> Option<&EndReason> {
        self.end.as_ref()
    }
}

/// How a step ended, plus the log line the host writes about it.
///
/// `line` is `None` when the guest reported the end itself.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct End {
    pub(super) reason: EndReason,
    /// The log line's `kind` and message.
    pub(super) line: Option<(&'static str, &'static str)>,
}

impl End {
    /// An end the host logs a line about, with `kind` and `message`.
    fn logged(reason: EndReason, kind: &'static str, message: &'static str) -> Self {
        Self {
            reason,
            line: Some((kind, message)),
        }
    }

    /// The end of a step whose guest's buffer table did not match its mirror.
    fn corrupt_table() -> Self {
        let reason = EndReason::Failed(Cow::Borrowed("corrupt table"));
        Self::logged(reason, "corrupt", "a guest's buffer table is corrupt")
    }

    /// Decides whether one cycle ended the step.
    ///
    /// `copied` is the result of copying the rings, and `result` is the result
    /// of the `execute` call. Returns `None` if the step is still running.
    pub(super) fn from_cycle(
        copied: Result<(), Corrupt>,
        result: Result<u32, Trap>,
    ) -> Option<Self> {
        if copied.is_err() {
            let reason = EndReason::Failed(Cow::Borrowed("corrupt ring"));
            return Some(Self::logged(reason, "corrupt", "a guest ring is corrupt"));
        }
        match result {
            Ok(word) => match Status::try_from(word) {
                Ok(status) => status.end().map(|reason| End { reason, line: None }),
                Err(_) => Some(Self::logged(
                    EndReason::Panicked,
                    "bad_status",
                    "the guest returned no known status",
                )),
            },
            Err(Trap::OutOfFuel) => Some(Self::logged(
                EndReason::Failed(Cow::Borrowed("out of fuel")),
                "fuel",
                "the guest ran out of fuel",
            )),
            Err(trap) => Some(Self::logged(EndReason::Panicked, "trap", trap.as_str())),
        }
    }
}
