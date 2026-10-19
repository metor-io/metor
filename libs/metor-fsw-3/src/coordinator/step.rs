//! The [`Step`] trait the coordinator uses to run each system, the reasons a
//! step can end, and [`StepState`], which stops a panicking step from taking
//! down the coordinator.

use std::borrow::Cow;
use std::panic::{AssertUnwindSafe, catch_unwind};

use metor_component::{FrameBytes, FrameStr};
use metor_proto::types::Timestamp;

use super::params::MakeError;
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
    Create(MakeError),
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

/// A step, or the gap a panicking step leaves.
pub(crate) enum StepState {
    Live(Box<dyn Step>),
    Panicked,
}

impl StepState {
    /// Returns why the step ended, or `None` while it runs.
    pub(crate) fn end_reason(&self) -> Option<&EndReason> {
        match self {
            Self::Live(step) => step.ended(),
            Self::Panicked => Some(&EndReason::Panicked),
        }
    }

    /// Executes the step unless it has ended.
    ///
    /// If `execute` panics, the step's `fault` hook runs, the step is dropped,
    /// and this returns true.
    pub(crate) fn run(&mut self, now: Timestamp) -> bool {
        let Self::Live(step) = self else {
            return false;
        };
        if !catch_execute(step.as_mut(), now) {
            return false;
        }
        let step = core::mem::replace(self, Self::Panicked);
        crate::panic::catch(|| drop(step));
        true
    }
}

/// Executes `step` unless it has ended.
///
/// If `execute` panics, the step's `fault` hook runs and this returns true.
/// The caller then drops the step.
pub(crate) fn catch_execute(step: &mut dyn Step, now: Timestamp) -> bool {
    if step.ended().is_some() {
        return false;
    }
    // PANIC Safety: the panic is caught, and the caller drops the step.
    let Err(payload) = catch_unwind(AssertUnwindSafe(|| step.execute(now))) else {
        return false;
    };
    let message = crate::panic::into_message(payload);
    crate::panic::catch(|| step.fault(now, &message));
    true
}
