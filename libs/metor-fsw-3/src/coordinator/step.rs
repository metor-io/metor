//! The [`Step`] trait every bound system is stepped through, and how one ends.

use std::borrow::Cow;

use metor_proto::types::Timestamp;

use super::params::ParamError;

/// A bound system, stepped once per cycle with the system type erased.
pub trait Step {
    fn execute(&mut self, now: Timestamp);

    /// Called once after `execute` panicked, with the payload's message.
    fn fault(&mut self, _now: Timestamp, _message: &str) {}

    /// How the step ended, once it has. An ended step is not executed again.
    fn ended(&self) -> Option<&End> {
        None
    }

    /// Replaces what this step runs with the system `name` names.
    fn load(&mut self, _name: &str) -> Result<(), LoadError> {
        Err(LoadError::NotLoadable)
    }
}

/// How a step ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum End {
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
