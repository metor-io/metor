//! The [`Step`] trait the coordinator uses to run each system, the reasons a
//! step can end, and [`catch_step`], which stops a panicking step from taking
//! down the coordinator.

use std::borrow::Cow;
use std::panic::{AssertUnwindSafe, catch_unwind};

use metor_component::{FrameBytes, FrameStr};
use metor_proto::types::Timestamp;

use super::params::ParamError;
use crate::system::System;

/// A system the coordinator can run.
///
/// The coordinator calls [`Step::execute`] once per cycle. The trait hides each
/// system's concrete type, so systems of any type can share one schedule.
pub trait Step {
    fn execute(&mut self, now: Timestamp);

    /// Called once after `execute` panics, with the panic message.
    fn fault(&mut self, _now: Timestamp, _message: &str) {}

    /// Returns why the step ended, or `None` while it is still running.
    ///
    /// The coordinator never executes a step that has ended.
    fn ended(&self) -> Option<&EndReason> {
        None
    }

    /// Replaces the system this step runs with the system called `name`.
    fn load(&mut self, _name: &str) -> Result<(), LoadError> {
        Err(LoadError::NotLoadable)
    }

    /// Returns the system this step has loaded, if any.
    fn occupant(&self) -> Option<&Occupant> {
        None
    }
}

/// Identifies the system loaded into a step.
///
/// A channel reports it in its status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Occupant {
    /// The system's index in the list of names the step can load.
    pub index: u32,
    /// The file name of the library the system came from.
    pub artifact: FrameStr<64>,
    /// The blake3 hash of that library.
    pub hash: FrameBytes<32>,
}

/// How a step ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EndReason {
    Completed,
    Aborted,
    Failed(Cow<'static, str>),
    Panicked,
    /// The step is a slot with nothing loaded.
    Unloaded,
}

/// The reason a step refused a load request.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LoadError {
    #[error("nothing can be loaded here")]
    NotLoadable,
    #[error("`{0}` is not in the allow list")]
    Unknown(String),
    #[error("`{0}` no longer matches the slot's ports")]
    Mismatch(String),
    #[error("`{0}` is not the library build stored")]
    Hash(String),
    #[error("opening the pack: {0}")]
    Open(String),
    #[error("creating the system: {0}")]
    Create(ParamError),
}

/// The outcome of one [`catch_step`] call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stepped {
    Ran,
    /// The step has ended, either before this call or during it.
    Ended,
    /// The step panicked and was dropped, during this call or an earlier one.
    Unwound,
}

pub(crate) struct Runner<S: System> {
    pub system: S,
    pub state: S::State,
    pub inputs: S::Inputs,
    pub outputs: S::Outputs,
}

impl<S: System> Step for Runner<S> {
    fn execute(&mut self, now: Timestamp) {
        self.system
            .execute(now, &mut self.state, &mut self.inputs, &mut self.outputs);
    }

    fn fault(&mut self, now: Timestamp, message: &str) {
        self.system.fault(now, &mut self.outputs, message);
    }
}

/// Executes the step in `slot` unless it has ended.
///
/// If `execute` panics, the step's `fault` hook runs and the step is dropped,
/// leaving `slot` empty.
pub(crate) fn catch_step(slot: &mut Option<Box<dyn Step>>, now: Timestamp) -> Stepped {
    let Some(step) = slot.as_mut() else {
        return Stepped::Unwound;
    };
    // PANIC Safety: a panicking step is faulted once and removed before returning.
    let result = catch_unwind(AssertUnwindSafe(|| {
        if step.ended().is_none() {
            step.execute(now);
        }
        step.ended().is_some()
    }));
    match result {
        Ok(false) => Stepped::Ran,
        Ok(true) => Stepped::Ended,
        Err(payload) => {
            crate::panic::catch(|| step.fault(now, crate::panic::message(&*payload)));
            crate::panic::discard(payload);
            crate::panic::catch(|| drop(slot.take()));
            Stepped::Unwound
        }
    }
}
