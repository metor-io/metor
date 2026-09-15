//! Everything [`Coordinator::build`](super::Coordinator::build) rejects.

use metor_proto::types::ComponentId;
use thiserror::Error;

/// A config the coordinator refused to build. Every variant names the system
/// and port that produced it.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum BuildError {
    #[error("system id `{id}` is used twice")]
    DuplicateId { id: String },

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
    FrameMismatch {
        id: String,
        port: String,
        from: String,
        expected: ComponentId,
        found: ComponentId,
    },

    #[error("output `{system}.{port}` needs a ring larger than this host can address")]
    RingTooLarge {
        system: String,
        port: String,
        max_size: usize,
    },
}
