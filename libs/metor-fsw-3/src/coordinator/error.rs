use metor_proto::types::{ComponentId, PacketId};
use thiserror::Error;

use super::params::ParamError;
use crate::def::DefError;

/// Errors from registering system types or building a coordinator.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum BuildError {
    #[error("wall clock rate must be finite and at least 0.001 Hz")]
    InvalidClockRate,

    #[error("a deterministic build needs a simulated clock")]
    DeterministicWall,

    #[error("a deterministic build needs a simulated clock with a nonzero dt")]
    DeterministicZeroDt,

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

    #[error("channel `{name}` is longer than {cap} bytes")]
    ChannelName { name: String, cap: usize },

    #[error("channel `{name}` is named by two systems")]
    DuplicateChannel { name: String },

    #[error("port `{id}.{port}` is reserved for the channel adapter")]
    ReservedChannelPort { id: String, port: String },

    #[error("`{system}` has no grant for its `Cap` parameter `{param}`")]
    Ungranted { system: String, param: String },

    #[error("`{system}` grants its parameter `{param}` twice")]
    DuplicateGrant { system: String, param: String },

    #[error("`{system}` grants `{param}`, which is not a `Cap` parameter")]
    UnknownGrant { system: String, param: String },

    #[error("`{id}`: the io steps take no channel")]
    IoAdapter { id: String },

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

    #[error("sim resource `{resource}` has no model; grant it to a `DeviceSide`")]
    NoDeviceEnd { resource: String },

    #[error("sim resource `{resource}` has two models, `{first}` and `{second}`")]
    TwoDeviceEnds {
        resource: String,
        first: String,
        second: String,
    },

    #[error("`{system}` serves resource `{resource}`, which is not simulated")]
    DeviceEndOnReal { system: String, resource: String },

    #[error("drivers use real resource `{real}` and sim resource `{sim}`; a target uses one kind")]
    MixedBackings { real: String, sim: String },

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
