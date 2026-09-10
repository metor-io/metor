//! The gateway member's embedded db.
//!
//! [`DbState`] is the pack-shared state behind the gateway's systems: an
//! ordinary `metor_db::Server` owned by a member's wiring. It binds its
//! listener and opens the store at construction, so a taken port or an
//! unwritable directory is a resolve-time error as for
//! [`LinkState`](crate::LinkState), and spawns the accept loop, the
//! level-of-detail pass, and tiering from
//! [`start`](crate::SharedLifecycle::start) on the coordinator's runtime.
//!
//! # Ownership discipline
//!
//! Attached cyclic systems receive the scoped `&mut DbState` grant; what
//! they take out of it is the `Arc<DB>`, which is `Send + Sync` and guards
//! every write itself. The db's per-connection threads therefore never
//! touch the coordinator's loop task: a ground query runs on the
//! connection's own thread while the cycle runs on ours.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use metor_db::store::LocalDirStore;
use metor_db::tiering::TieringConfig;
use metor_db::{DB, Server};
use metor_proto::types::PacketId;
use stellarator::JoinHandleDropGuard;
use stellarator::net::TcpListener;

/// Wiring params of the built-in embedded db (`state type="Db"`).
#[derive(serde::Serialize, serde::Deserialize, postcard_schema::Schema, Debug, Clone)]
pub struct DbParams {
    /// The address ground tools dial.
    pub addr: SocketAddr,
    /// The data directory. `None` is a fresh temp dir the OS reclaims.
    #[serde(default)]
    pub path: Option<PathBuf>,
    /// Human node name advertised over mDNS. `None` falls back to the
    /// namespace, then the OS hostname, at advertise time.
    #[serde(default)]
    pub name: Option<String>,
    /// Archive directory sealed spans are offloaded to before they are
    /// purged. Without one nothing is evicted.
    #[serde(default)]
    pub store: Option<PathBuf>,
    /// Purge offloaded spans once resident sealed bytes exceed this.
    #[serde(default)]
    pub max_bytes: Option<u64>,
    /// Purge offloaded spans whose newest sample is older than this.
    #[serde(default)]
    pub max_age_secs: Option<f64>,
}

/// The pack-shared embedded db: bound and opened at construction, served
/// from [`start`](crate::SharedLifecycle::start).
pub struct DbState {
    /// Holds the bound listener until `start` consumes it.
    server: Option<Server>,
    db: Arc<DB>,
    local_addr: SocketAddr,
    /// The configured node name; `None` resolves to the namespace, else the
    /// hostname, when the advertisement is made.
    name: Option<String>,
    /// This db's state declaration name, the `link` half of the identity it
    /// advertises.
    link: String,
    /// The target namespace, when the front-end set one.
    namespace: Option<String>,
    tiering: Option<(PathBuf, TieringConfig)>,
    /// The union of the attached ingests' forwarded command ids, gathered in
    /// their `configure` and advertised from `start`.
    commands: Vec<PacketId>,
    accept_guard: Option<JoinHandleDropGuard<()>>,
    /// The mDNS advertisement, live between `start` and `shutdown`. `None`
    /// for a loopback bind or a daemon that couldn't start.
    advertiser: Option<mdns_sd::ServiceDaemon>,
}

impl DbState {
    /// Bind the listener and open (or create) the store. Failure — the port
    /// is taken, the directory is unwritable — surfaces as the state
    /// declaration's construction error at resolve. `namespace` names the
    /// default data directory, so it arrives here rather than through
    /// [`with_identity`](Self::with_identity).
    pub fn open(params: DbParams, namespace: Option<&str>) -> Result<Self, metor_db::Error> {
        let listener = TcpListener::bind(params.addr)?;
        let local_addr = listener.local_addr()?;
        let path = params
            .path
            .unwrap_or_else(|| temp_path(namespace.unwrap_or("gw")));
        let server = Server::from_listener(listener, path)?;
        let db = server.db.clone();
        Ok(Self {
            server: Some(server),
            db,
            local_addr,
            name: None,
            link: String::new(),
            namespace: namespace.map(str::to_string),
            tiering: params.store.map(|store| {
                let config = TieringConfig {
                    max_db_bytes: params.max_bytes,
                    max_age: params.max_age_secs.map(Duration::from_secs_f64),
                    ..TieringConfig::default()
                };
                (store, config)
            }),
            commands: Vec::new(),
            accept_guard: None,
            advertiser: None,
        })
    }

    /// Set this db's identity: its state declaration name and the
    /// configured node name (from [`DbParams::name`]). A builder step off
    /// [`open`](Self::open) so the registry factory threads them in without
    /// changing `open`'s signature.
    pub fn with_identity(mut self, link: &str, name: Option<String>) -> Self {
        self.link = link.to_string();
        self.name = name;
        self
    }

    /// The handle attached systems write through. `DB` guards its own
    /// state, so a clone crosses to a spawned task.
    pub fn db(&self) -> &Arc<DB> {
        &self.db
    }

    /// The address the listener actually bound, port `0` resolved.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Union `ids` into what `GetDbInfo` advertises. Attached ingests call
    /// this from `configure`, which runs before `start` serves the first
    /// probe, so a ground client never sees a partial command set.
    pub(crate) fn add_commands(&mut self, ids: &[PacketId]) {
        for id in ids {
            if !self.commands.contains(id) {
                self.commands.push(*id);
            }
        }
        self.db.add_command_ids(ids);
    }
}

/// A fresh directory under the OS temp dir, the `serve_tmp_db` shape with
/// the member's namespace in the name so a live gateway's store is findable.
fn temp_path(namespace: &str) -> PathBuf {
    std::env::temp_dir().join(format!("metor-gw-{namespace}-{}", fastrand::u64(..)))
}

impl crate::SharedLifecycle for DbState {
    /// Spawn the server and its background passes; runs on the
    /// coordinator's loop task before the first attached system's init, so
    /// the identity a probe reads is set before anything can be accepted.
    fn start(&mut self) {
        self.db
            .set_identity(self.namespace.clone(), self.commands.clone());
        let name = self
            .name
            .clone()
            .or_else(|| self.namespace.clone())
            .unwrap_or_else(|| gethostname::gethostname().to_string_lossy().into_owned());
        self.advertiser = crate::telemetry::discovery::advertise(
            &name,
            self.local_addr,
            self.namespace.as_deref(),
            &self.link,
            Some("gateway"),
        );
        let server = self.server.take().expect("start runs once");
        self.accept_guard = Some(
            stellarator::spawn(async move {
                if let Err(err) = server.run().await {
                    tracing::warn!(%err, "db server stopped");
                }
            })
            .drop_guard(),
        );
        metor_db::lod::spawn(self.db.clone());
        if let Some((store, config)) = self.tiering.take() {
            let store = Arc::new(LocalDirStore::new(store));
            metor_db::tiering::spawn(self.db.clone(), Some(store), config);
        }
    }

    /// Dropping the guard cancels the accept loop, so no further connection
    /// is served; connections already handed to their own threads end with
    /// the process. Shutting the mDNS daemon down unregisters the
    /// advertisement with a goodbye.
    fn shutdown(&mut self) {
        if let Some(advertiser) = self.advertiser.take() {
            let _ = advertiser.shutdown();
        }
        self.accept_guard = None;
    }
}

mod ingest;
mod record;

pub use ingest::{IngestOut, IngestParams, IngestSystem, SourceStatus};
pub use record::{RecordPorts, RecordSystem};

#[cfg(test)]
mod tests;
