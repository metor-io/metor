use metor_proto::types::{ComponentId, PacketId};
use thiserror::Error;

use super::params::ParamError;
use crate::def::DefError;

/// Errors registering system types or building a coordinator.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum BuildError {
    #[error("wall clock rate must be finite and at least 0.001 Hz")]
    InvalidClockRate,

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

    #[error("dynamic input `{system}.{port}` has more than one producer")]
    DynamicFanIn { system: String, port: String },

    #[error("output `{system}.{port}` names record `{record}`, which no registered port declares")]
    UnknownRecord {
        system: String,
        port: String,
        record: String,
    },

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

    #[error("type `{ty}` declares an output named `status`, which the coordinator reserves")]
    ReservedPort { ty: String },

    #[error("cyclic system `{id}` cannot be placed on thread `{thread}`")]
    ThreadOnCyclic { id: String, thread: String },

    #[error("thread `{thread}` failed to start")]
    ThreadStart { thread: String },

    #[error("definition for `{id}`: {source}")]
    Def {
        id: String,
        #[source]
        source: DefError,
    },

    #[error("params for `{id}`: {source}")]
    Params {
        id: String,
        #[source]
        source: ParamError,
    },
}
