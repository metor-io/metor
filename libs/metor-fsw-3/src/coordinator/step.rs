//! The [`Step`] trait every bound system is stepped through, how one ends,
//! and [`catch_step`], which contains a step's panic.

use std::borrow::Cow;
use std::panic::{AssertUnwindSafe, catch_unwind};

use metor_component::{FrameBytes, FrameStr};
use metor_proto::types::Timestamp;

use super::params::ParamError;
use crate::system::System;

/// A bound system, stepped once per cycle with the system type erased.
pub trait Step {
    fn execute(&mut self, now: Timestamp);

    /// Called once after `execute` panicked, with the payload's message.
    fn fault(&mut self, _now: Timestamp, _message: &str) {}

    /// How the step ended, once it has. An ended step is not executed again.
    fn ended(&self) -> Option<&EndReason> {
        None
    }

    /// Replaces what this step runs with the system `name` names.
    fn load(&mut self, _name: &str) -> Result<(), LoadError> {
        Err(LoadError::NotLoadable)
    }

    /// What this step has loaded, if it loads anything.
    fn occupant(&self) -> Option<&Occupant> {
        None
    }
}

/// One loaded system, as a channel's status reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Occupant {
    /// Its index among the names the step can load.
    pub index: u32,
    /// Its library's file name.
    pub artifact: FrameStr<64>,
    /// The blake3 of its library.
    pub hash: FrameBytes<32>,
}

/// How a step ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EndReason {
    Completed,
    Aborted,
    Failed(Cow<'static, str>),
    Panicked,
    /// A slot with no occupant.
    Unloaded,
}

/// Why a step refused a `Load`.
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

/// What one [`catch_step`] call did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stepped {
    Ran,
    /// The step reports an end, before or after this call's execute.
    Ended,
    /// The step panicked and was dropped, now or on an earlier call.
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

/// Executes a step that has not ended, faulting and dropping it if it panics.
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
