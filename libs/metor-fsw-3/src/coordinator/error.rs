use metor_proto::types::ComponentId;
use thiserror::Error;

/// Errors associated with [`CoordinatorConfig::build`].
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum BuildError {
    #[error("wall clock rate must be finite and at least 0.001 Hz")]
    InvalidClockRate,

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
        max_size: usize,
    },

    #[error("type `{ty}` declares an output named `status`, which the coordinator reserves")]
    ReservedPort { ty: String },
}
