use metor_proto::types::{ComponentId, PacketId};
use thiserror::Error;

use super::config::Location;
use super::params::MakeError;
use crate::def::DefError;

/// Errors from registering system types or building a coordinator.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[cfg_attr(feature = "host", derive(miette::Diagnostic))]
pub enum BuildError {
    #[error("{location}: {source}")]
    #[cfg_attr(feature = "host", diagnostic(forward(source)))]
    Located {
        location: Location,
        source: Box<BuildError>,
    },

    #[error("a deterministic build needs a simulated clock")]
    #[cfg_attr(
        feature = "host",
        diagnostic(code(M0301), help("pass --sim-dt and --epoch with --deterministic"))
    )]
    DeterministicWall,

    #[error("a deterministic build needs a dt of at least 1 µs")]
    #[cfg_attr(
        feature = "host",
        diagnostic(code(M0302), help("pass --sim-dt and --epoch with --deterministic"))
    )]
    DeterministicZeroDt,

    #[error("a deterministic build needs a simulated clock with an epoch of 0 or later")]
    #[cfg_attr(
        feature = "host",
        diagnostic(code(M0303), help("pass --sim-dt and --epoch with --deterministic"))
    )]
    DeterministicEpoch,

    #[error("system id `{id}` is used twice")]
    #[cfg_attr(
        feature = "host",
        diagnostic(code(M0304), help("give each system its own id"))
    )]
    DuplicateId { id: String },

    #[error("system `{system}` declares input port `{port}` more than once")]
    #[cfg_attr(feature = "host", diagnostic(code(M0305)))]
    DuplicateInput { system: String, port: String },

    #[error("system `{system}` declares output port `{port}` more than once")]
    #[cfg_attr(feature = "host", diagnostic(code(M0306)))]
    DuplicateOutput { system: String, port: String },

    #[error("port `{port}` of system `{system}` names leaf `{leaf}` more than once")]
    #[cfg_attr(
        feature = "host",
        diagnostic(
            code(M0338),
            help("rename or stop flattening one of the fields that share the name")
        )
    )]
    DuplicateLeaf {
        system: String,
        port: String,
        leaf: String,
    },

    #[error("system `{id}` has unregistered type `{ty}`")]
    #[cfg_attr(
        feature = "host",
        diagnostic(
            code(M0307),
            help("check that the target lists the pack this type comes from")
        )
    )]
    UnknownType { id: String, ty: String },

    #[error("input `{id}.{port}` reads from unknown system `{from}`")]
    #[cfg_attr(feature = "host", diagnostic(code(M0308)))]
    UnknownSystem {
        id: String,
        port: String,
        from: String,
    },

    #[error("system `{system}` has no input port `{port}`")]
    #[cfg_attr(feature = "host", diagnostic(code(M0309)))]
    UnknownInput { system: String, port: String },

    #[error("system `{system}` has no output port `{port}`")]
    #[cfg_attr(feature = "host", diagnostic(code(M0310)))]
    UnknownOutput { system: String, port: String },

    #[error("input `{id}.{port}` carries {expected:?} but `{from}` produces {found:?}")]
    #[cfg_attr(feature = "host", diagnostic(code(M0311)))]
    IdMismatch {
        id: String,
        port: String,
        from: String,
        expected: ComponentId,
        found: ComponentId,
    },

    #[error("port `{system}.{port}` requires unsupported frame alignment {alignment}")]
    #[cfg_attr(feature = "host", diagnostic(code(M0312)))]
    UnsupportedFrameAlignment {
        system: String,
        port: String,
        alignment: usize,
    },

    #[error("output `{system}.{port}` needs a ring larger than this host can address")]
    #[cfg_attr(feature = "host", diagnostic(code(M0313)))]
    RingTooLarge {
        system: String,
        port: String,
        max_len: usize,
    },

    #[error("undeclared input `{system}.{port}` reads a system configured after it")]
    #[cfg_attr(
        feature = "host",
        diagnostic(code(M0314), help("add the producer before the system that reads it"))
    )]
    DynamicFromLater { system: String, port: String },

    #[error("record `{record}` is declared differently by two registered ports")]
    #[cfg_attr(feature = "host", diagnostic(code(M0315)))]
    RecordConflict { record: String },

    #[error("records `{first}` and `{second}` share record id {id:?}")]
    #[cfg_attr(feature = "host", diagnostic(code(M0316)))]
    RecordIdConflict {
        id: ComponentId,
        first: String,
        second: String,
    },

    #[error("records `{first}` and `{second}` define different schemas for message id {id:?}")]
    #[cfg_attr(feature = "host", diagnostic(code(M0317)))]
    MessageIdConflict {
        id: PacketId,
        first: String,
        second: String,
    },

    #[error("system `{id}` of type `{ty}` cannot run {adapter}")]
    #[cfg_attr(feature = "host", diagnostic(code(M0318)))]
    UnsupportedAdapter {
        id: String,
        ty: String,
        adapter: String,
    },

    #[error("type `{ty}` declares an output named `status`, which the coordinator reserves")]
    #[cfg_attr(feature = "host", diagnostic(code(M0319)))]
    ReservedPort { ty: String },

    #[error("definition of type `{ty}`: {source}")]
    #[cfg_attr(feature = "host", diagnostic(code(M0320)))]
    TypeDef {
        ty: String,
        #[source]
        source: DefError,
    },

    #[error("channel `{name}` is named by two systems")]
    #[cfg_attr(feature = "host", diagnostic(code(M0321)))]
    DuplicateChannel { name: String },

    #[error("port `{id}.{port}` is reserved for the channel adapter")]
    #[cfg_attr(feature = "host", diagnostic(code(M0322)))]
    ReservedChannelPort { id: String, port: String },

    #[error("`{system}` has no grant for its `Cap` parameter `{param}`")]
    #[cfg_attr(feature = "host", diagnostic(code(M0323)))]
    Ungranted { system: String, param: String },

    #[error("`{system}`'s parameter `{param}` is bound to two resources")]
    #[cfg_attr(feature = "host", diagnostic(code(M0324)))]
    DuplicateGrant { system: String, param: String },

    #[error("`{system}` grants `{param}`, which is not a `Cap` parameter")]
    #[cfg_attr(feature = "host", diagnostic(code(M0325)))]
    UnknownGrant { system: String, param: String },

    #[error("`{system}` holds a `Cap`, so the config needs an fsw.io")]
    #[cfg_attr(
        feature = "host",
        diagnostic(code(M0326), help("add an fsw.io system to the target"))
    )]
    NoIoStep { system: String },

    #[error("`{system}` holds a `Cap`, so an fsw.io must follow it")]
    #[cfg_attr(
        feature = "host",
        diagnostic(code(M0327), help("add fsw.io after `{system}`"))
    )]
    NoIoStepAfter { system: String },

    #[error("the systems with a `Cap` ask for {ops} ops in flight, past the limit")]
    #[cfg_attr(feature = "host", diagnostic(code(M0328)))]
    TooManyIoOps { ops: usize },

    #[error("{count} resources, past the limit")]
    #[cfg_attr(feature = "host", diagnostic(code(M0329)))]
    TooManyResources { count: usize },

    #[error("the IO backend did not start: {message}")]
    #[cfg_attr(feature = "host", diagnostic(code(M0330)))]
    IoBackend { message: String },

    #[error("`{system}` grants `{param}` resource `{resource}`, which the target does not create")]
    #[cfg_attr(feature = "host", diagnostic(code(M0331)))]
    UnknownResource {
        system: String,
        param: String,
        resource: String,
    },

    /// `cap` is `{system}.{param}`.
    #[error("`{cap}` takes a `{expected}`, but resource `{resource}` provides `{found}`")]
    #[cfg_attr(feature = "host", diagnostic(code(M0332)))]
    Interface {
        cap: String,
        resource: String,
        expected: String,
        found: String,
    },

    #[error(
        "`{system}` grants `{param}` sim resource `{resource}`, which names its holders itself"
    )]
    #[cfg_attr(feature = "host", diagnostic(code(M0333)))]
    SimGrant {
        system: String,
        param: String,
        resource: String,
    },

    #[error("sim resource `{resource}` names holder `{system}`, which is not in the config")]
    #[cfg_attr(feature = "host", diagnostic(code(M0334)))]
    UnknownHolder { resource: String, system: String },

    #[error("resource `{resource}` did not open: {message}")]
    #[cfg_attr(feature = "host", diagnostic(code(M0335)))]
    Open { resource: String, message: String },

    #[error("definition for `{id}`: {source}")]
    #[cfg_attr(feature = "host", diagnostic(code(M0336)))]
    Def {
        id: String,
        #[source]
        source: DefError,
    },

    #[error("making `{id}`: {source}")]
    #[cfg_attr(feature = "host", diagnostic(code(M0337)))]
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

    /// Port `port` of system `id` takes a name the channel adapter reserves.
    #[cfg(feature = "host")]
    pub(crate) fn reserved_channel_port(id: &str, port: &str) -> Self {
        Self::ReservedChannelPort {
            id: id.to_string(),
            port: port.to_string(),
        }
    }
}
