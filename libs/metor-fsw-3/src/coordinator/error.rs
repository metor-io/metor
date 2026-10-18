use metor_proto::types::{ComponentId, PacketId};
use thiserror::Error;

use super::config::Location;
use super::params::MakeError;
use crate::def::DefError;

/// Errors from registering system types or building a coordinator.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum BuildError {
    #[error("{location}: {source}")]
    Located {
        location: Location,
        source: Box<BuildError>,
    },

    #[error("a deterministic build needs a simulated clock")]
    DeterministicWall,

    #[error("a deterministic build needs a dt of at least 1 µs")]
    DeterministicZeroDt,

    #[error("a deterministic build needs a simulated clock with an epoch of 0 or later")]
    DeterministicEpoch,

    #[error("system id `{id}` is used twice")]
    DuplicateId { id: String },

    #[error("system `{system}` declares input port `{port}` more than once")]
    DuplicateInput { system: String, port: String },

    #[error("system `{system}` declares output port `{port}` more than once")]
    DuplicateOutput { system: String, port: String },

    #[error("system `{id}` has unregistered type `{ty}`")]
    UnknownType { id: String, ty: String },

    #[error("input `{id}.{port}` reads from unknown system `{from}`")]
    UnknownSystem {
        id: String,
        port: String,
        from: String,
    },

    #[error("system `{system}` has no input port `{port}`")]
    UnknownInput { system: String, port: String },

    #[error("system `{system}` has no output port `{port}`")]
    UnknownOutput { system: String, port: String },

    #[error("input `{id}.{port}` carries {expected:?} but `{from}` produces {found:?}")]
    IdMismatch {
        id: String,
        port: String,
        from: String,
        expected: ComponentId,
        found: ComponentId,
    },

    #[error("port `{system}.{port}` requires unsupported frame alignment {alignment}")]
    UnsupportedFrameAlignment {
        system: String,
        port: String,
        alignment: usize,
    },

    #[error("output `{system}.{port}` needs a ring larger than this host can address")]
    RingTooLarge {
        system: String,
        port: String,
        max_len: usize,
    },

    #[error("undeclared input `{system}.{port}` reads a system configured after it")]
    DynamicFromLater { system: String, port: String },

    #[error("record `{record}` is declared differently by two registered ports")]
    RecordConflict { record: String },

    #[error("records `{first}` and `{second}` share record id {id:?}")]
    RecordIdConflict {
        id: ComponentId,
        first: String,
        second: String,
    },

    #[error("records `{first}` and `{second}` define different schemas for message id {id:?}")]
    MessageIdConflict {
        id: PacketId,
        first: String,
        second: String,
    },

    #[error("system `{id}` of type `{ty}` cannot run {adapter}")]
    UnsupportedAdapter {
        id: String,
        ty: String,
        adapter: String,
    },

    #[error("type `{ty}` declares an output named `status`, which the coordinator reserves")]
    ReservedPort { ty: String },

    #[error("definition of type `{ty}`: {source}")]
    TypeDef {
        ty: String,
        #[source]
        source: DefError,
    },

    #[error("definition for `{id}`: {source}")]
    Def {
        id: String,
        #[source]
        source: DefError,
    },

    #[error("making `{id}`: {source}")]
    Make {
        id: String,
        #[source]
        source: MakeError,
    },
}

impl BuildError {
    /// Cites `location`, when the config gives one, as where the error is.
    pub fn at(self, location: Option<&Location>) -> Self {
        match location {
            Some(location) if !matches!(self, Self::Located { .. }) => Self::Located {
                location: location.clone(),
                source: Box::new(self),
            },
            _ => self,
        }
    }
}
