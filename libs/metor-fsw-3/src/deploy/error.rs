//! Errors from resolving a deployment.

use crate::coordinator::{BuildError, Location};

/// An error resolving a deployment. Each variant says which reference
/// failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DeployError {
    #[error("{location}: {source}")]
    Located {
        location: Location,
        source: Box<DeployError>,
    },

    #[error("`{from}` names target `{target}`, which the deployment has no member of")]
    NoSuchTarget { from: String, target: String },

    #[error("`{from}` names link `{target}.{link}`, which is not a system there")]
    NoSuchLink {
        from: String,
        target: String,
        link: String,
    },

    #[error("`{from}` names link `{target}.{link}`, which does not listen on a fixed port")]
    NotAListener {
        from: String,
        target: String,
        link: String,
    },

    #[error("`{from}` names port `{port}` of `{target}.{link}`, which has no such port")]
    NoSuchPort {
        from: String,
        target: String,
        link: String,
        port: String,
    },

    #[error("`{first}` and `{second}` both bind {host}:{port}")]
    PortTaken {
        host: String,
        port: u16,
        first: String,
        second: String,
    },

    #[error("resolving definitions cycles through {path}")]
    Cycle { path: String },

    #[error("target `{target}`: {source}")]
    Build { target: String, source: BuildError },

    #[error("`{system}`'s output `{port}` names a peer's port that is not resolved")]
    Unresolved { system: String, port: String },

    #[error("resource `{resource}` dials a peer, but no system holds it to resolve it for")]
    UnresolvedDial { resource: String },

    #[error("`{from}` dials {addr}, which resolves to no address: {message}")]
    Address {
        from: String,
        addr: String,
        message: String,
    },

    #[error("`{system}`'s params: {message}")]
    Params { system: String, message: String },

    #[error("slot `{slot}`: {message}")]
    Slot { slot: String, message: String },

    #[error("slot `{slot}`: `{candidate}` has different ports than `{first}`")]
    SlotContract {
        slot: String,
        first: String,
        candidate: String,
    },

    #[error("slot `{slot}`: library name `{name}` is longer than {cap} bytes")]
    ArtifactName {
        slot: String,
        name: String,
        cap: usize,
    },
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
