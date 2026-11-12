//! Errors from resolving a deployment.

use crate::coordinator::{BuildError, Location};

/// An error resolving a deployment. Each variant says which reference
/// failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum DeployError {
    #[error("{location}: {source}")]
    #[diagnostic(forward(source))]
    Located {
        location: Location,
        source: Box<DeployError>,
    },

    #[error("`{from}` names target `{target}`, which the deployment has no member of")]
    #[diagnostic(code(M0210))]
    NoSuchTarget { from: String, target: String },

    #[error("`{from}` names link `{target}.{link}`, which is not a system there")]
    #[diagnostic(code(M0211))]
    NoSuchLink {
        from: String,
        target: String,
        link: String,
    },

    #[error("`{from}` names link `{target}.{link}`, which does not listen")]
    #[diagnostic(code(M0212), help("give the link a `Listen` transport"))]
    NotAListener {
        from: String,
        target: String,
        link: String,
    },

    #[error("`{from}` names port `{port}` of `{target}.{link}`, which has no such port")]
    #[diagnostic(code(M0213))]
    NoSuchPort {
        from: String,
        target: String,
        link: String,
        port: String,
    },

    #[error("`{first}` and `{second}` both bind {host}:{port}")]
    #[diagnostic(code(M0214), help("give each listener its own port"))]
    PortTaken {
        host: String,
        port: u16,
        first: String,
        second: String,
    },

    #[error("resolving definitions cycles through {path}")]
    #[diagnostic(code(M0215))]
    Cycle { path: String },

    #[error("target `{target}`: {source}")]
    #[diagnostic(forward(source))]
    Build { target: String, source: BuildError },

    #[error("`{system}`'s output `{port}` names a peer's port that is not resolved")]
    #[diagnostic(code(M0216))]
    Unresolved { system: String, port: String },

    #[error("resource `{resource}` dials a peer, but no system holds it to resolve it for")]
    #[diagnostic(code(M0217))]
    UnresolvedDial { resource: String },

    #[error("slot `{slot}`: {message}")]
    #[diagnostic(code(M0218))]
    Slot { slot: String, message: String },

    #[error("slot `{slot}`: `{candidate}` has different ports than `{first}`")]
    #[diagnostic(code(M0219))]
    SlotContract {
        slot: String,
        first: String,
        candidate: String,
    },

    #[error("slot `{slot}`: library name `{name}` is longer than {cap} bytes")]
    #[diagnostic(code(M0220))]
    ArtifactName {
        slot: String,
        name: String,
        cap: usize,
    },

    #[error("no port is left on {host} for a listener on port 0")]
    #[diagnostic(
        code(M0221),
        help("give some listeners on that host a fixed port, or a lower METOR_FIRST_PORT")
    )]
    NoFreePort { host: String },
}

impl DeployError {
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
