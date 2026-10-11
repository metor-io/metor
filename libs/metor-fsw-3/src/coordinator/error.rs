use crate::io::End;
use metor_proto::types::{ComponentId, PacketId};
use thiserror::Error;

use super::params::ParamError;
use crate::def::DefError;

/// Errors from registering system types or building a coordinator.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum BuildError {
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

    #[error("type `{ty}` declares an output named `status`, which the coordinator reserves")]
    ReservedPort { ty: String },

    #[error("`{system}` has no grant for its `Cap` parameter `{param}`")]
    Ungranted { system: String, param: String },

    #[error("`{system}` grants its parameter `{param}` twice")]
    DuplicateGrant { system: String, param: String },

    #[error("`{system}` grants `{param}`, which is not a `Cap` parameter")]
    UnknownGrant { system: String, param: String },

    #[error("io steps: {message}")]
    IoSystems { message: String },

    #[error("resource `{id}` is created twice")]
    DuplicateResource { id: String },

    #[error("`{system}` grants `{param}` resource `{resource}`, which the target does not create")]
    UnknownResource {
        system: String,
        param: String,
        resource: String,
    },

    /// `cap` is `{system}.{param}`.
    #[error("`{cap}` takes a `{expected}`, but resource `{resource}` provides `{found}`")]
    Interface {
        cap: String,
        resource: String,
        expected: String,
        found: String,
    },

    #[error("sim resource `{resource}` has no holder on its {missing:?} end")]
    OneEnd { resource: String, missing: End },

    #[error("`{system}` holds the device end of `{resource}`, which is not simulated")]
    RealDeviceEnd { system: String, resource: String },

    #[error("resource `{resource}` did not open: {message}")]
    Open { resource: String, message: String },

    #[error("resource `{resource}` has a `{backing}` backing, which this build does not run")]
    Backing { resource: String, backing: String },

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
