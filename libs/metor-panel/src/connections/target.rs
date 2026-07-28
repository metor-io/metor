//! What a connectable thing *is*: identity, display fields, and the
//! backend that stands it up against the shared DB.
//!
//! A backend is deliberately open-ended — the built-in [`ConnectionTarget::tcp`]
//! kind mirrors a remote metor-db, while [`ConnectionTarget::custom`] accepts
//! any closure that feeds the DB (an in-process sim, a cloud tunnel, a serial
//! bridge). Disconnect is uniform across all of them: every thread a backend
//! spawns through [`ConnectContext::spawn`] runs on a stellar runtime raced
//! against the connection's [`CancelToken`], so cancelling the token tears the
//! whole backend down without any per-backend teardown code.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use gpui::SharedString;
use metor_db::DB;
use metor_db::remote::{Hydrator, MirrorEvent, Peer, RemoteDb, fsw_stream, identify};
use stellarator::struc_con::ThreadBuilder;
use stellarator::util::CancelToken;

use super::options::{ConnectionOption, ConnectionOptions, OptionSpec};

/// Stable identity for a connectable target; keys the per-target layout
/// cache and the favorites/recents index, so it must survive restarts and
/// address changes. Wrapper authors supply it; the built-in TCP kind derives
/// `tcp:<addr>` for ad-hoc addresses.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct TargetId(pub SharedString);

impl TargetId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TargetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a connection stands, as reported by its backend. `Failed` still
/// retries when the backend reconnects on its own (the TCP mirror does).
#[derive(Clone, Debug, PartialEq, Default)]
pub enum ConnectionStatus {
    #[default]
    Connecting,
    Connected,
    Reconnecting,
    Failed(SharedString),
    /// Set by the panel after cancelling the backend, never by the backend.
    Disconnected,
}

#[derive(Default)]
struct StatusInner {
    status: std::sync::Mutex<ConnectionStatus>,
    generation: AtomicU64,
}

/// Send handle a backend writes status through from its own threads. The
/// store polls [`generation`](Self::generation) and repaints observers only
/// when it moves, so backends can set status as often as they like.
#[derive(Clone, Default)]
pub struct StatusHandle(Arc<StatusInner>);

impl StatusHandle {
    pub fn set(&self, status: ConnectionStatus) {
        let mut current = self.0.status.lock().unwrap();
        if *current != status {
            *current = status;
            self.0.generation.fetch_add(1, Ordering::Release);
        }
    }

    pub fn get(&self) -> ConnectionStatus {
        self.0.status.lock().unwrap().clone()
    }

    pub fn generation(&self) -> u64 {
        self.0.generation.load(Ordering::Acquire)
    }
}

/// Everything a backend needs to stand itself up against the shared DB.
pub struct ConnectContext {
    pub db: Arc<DB>,
    /// Cancelling this is the whole disconnect protocol: threads spawned via
    /// [`spawn`](Self::spawn) race against it, and the stellar executor drops
    /// their tasks and closes their IO when it fires.
    pub cancel: CancelToken,
    pub status: StatusHandle,
    /// The operator's answers to the knobs this target declared, complete
    /// against its spec. A snapshot: changing one restarts the backend
    /// through here rather than mutating it in flight.
    pub options: ConnectionOptions,
}

impl ConnectContext {
    /// Spawn a stellar runtime thread whose lifetime is tied to this
    /// connection's cancel token. The common backend body: spawn tasks, then
    /// park on `pending()`. The thread handle is detached — dropping a
    /// `Thread` doesn't cancel it, and the token in the store outlives it.
    pub fn spawn<F, Fut>(&self, f: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        drop(
            ThreadBuilder::default()
                .cancel_token(self.cancel.clone())
                .stellar(f),
        );
    }
}

/// Live resources a backend hands back for the panel to route.
#[derive(Default)]
pub struct Connected {
    /// Demand-driven fetcher for remote-only history, when the backend has one.
    pub hydrator: Option<Hydrator>,
    /// Whether this panel is the system of record for the data this backend
    /// feeds; drives the LoD-companion spawn policy. The TCP mirror reports
    /// false — it receives LoD series from the origin via manifest seeding.
    pub local_authority: bool,
    /// A discovery backend learns its shape only once the peer identifies;
    /// it sends the real resources here (one-shot) and the store applies
    /// them on its poll. Backends that know their shape synchronously leave
    /// this `None`.
    pub resolved: Option<std::sync::mpsc::Receiver<Resolved>>,
}

/// The late-bound half of [`Connected`], for identity-discovery backends.
pub struct Resolved {
    pub hydrator: Option<Hydrator>,
    pub local_authority: bool,
}

/// Spawns whatever feeds the shared DB for one target. Called on the gpui
/// thread; must return promptly, handing ongoing work to background threads
/// that observe `ctx.cancel` and report through `ctx.status`.
pub trait ConnectionBackend: Send + Sync + 'static {
    fn connect(&self, ctx: ConnectContext) -> Connected;
}

impl<F> ConnectionBackend for F
where
    F: Fn(ConnectContext) -> Connected + Send + Sync + 'static,
{
    fn connect(&self, ctx: ConnectContext) -> Connected {
        self(ctx)
    }
}

/// How the dialog's "Connect to address…" input turns a typed string into
/// a target. The built-in resolver parses `host:port` into the TCP mirror
/// kind; a wrapper author installs their own to speak another protocol
/// (`serial:/dev/ttyUSB0`, a vehicle URL, …). The same resolver
/// re-materializes address-carrying recents across restarts, so custom
/// protocols get history for free.
#[derive(Clone)]
pub struct AddressResolver {
    /// Placeholder shown in the address input, e.g. `host:port`.
    pub placeholder: SharedString,
    /// Parse user input into a target, or explain why not — the error
    /// string renders inline under the input.
    pub resolve: Arc<dyn Fn(&str) -> Result<ConnectionTarget, SharedString> + Send + Sync>,
}

impl Default for AddressResolver {
    /// The built-in grammar: a bare `host:port` connects with identity
    /// discovery (metor-db or fsw link, whichever answers), while a
    /// `db://` or `fsw://` prefix pins the protocol.
    fn default() -> Self {
        Self {
            placeholder: SharedString::new_static("host:port"),
            resolve: Arc::new(|input| {
                let input = input.trim();
                let (scheme, rest) = match input.split_once("://") {
                    Some((scheme, rest)) => (Some(scheme), rest),
                    None => (None, input),
                };
                let addr: SocketAddr = rest.parse().map_err(|_| {
                    SharedString::new_static(
                        "expected host:port, db://host:port, or fsw://host:port",
                    )
                })?;
                match scheme {
                    None => Ok(ConnectionTarget::tcp(rest.to_string(), addr)),
                    Some("db") => Ok(ConnectionTarget::db(rest.to_string(), addr)),
                    Some("fsw") => Ok(ConnectionTarget::fsw(rest.to_string(), addr)),
                    Some(other) => Err(SharedString::from(format!(
                        "unknown scheme `{other}://` — expected host:port, db://host:port, \
                         or fsw://host:port"
                    ))),
                }
            }),
        }
    }
}

/// One row in the picker: identity, display fields, and how to connect.
#[derive(Clone)]
pub struct ConnectionTarget {
    pub id: TargetId,
    pub name: SharedString,
    /// Subtitle column: address, serial number, discovery origin.
    pub detail: SharedString,
    pub backend: Arc<dyn ConnectionBackend>,
    /// The string the installed [`AddressResolver`] can rebuild this target
    /// from; lets the recents index re-materialize it across restarts
    /// without its discoverer. Set by [`tcp`](Self::tcp) and by
    /// [`with_address`](Self::with_address) for resolver-produced kinds;
    /// discovery-only targets leave it unset and don't persist.
    pub(crate) address: Option<SharedString>,
    /// Knobs this target's backend reads at connect time. Empty for the
    /// built-in kinds and for anything a discoverer produces — those fall
    /// back to the panel-wide set installed on the store.
    pub options: OptionSpec,
}

impl ConnectionTarget {
    /// Built-in kind: connect with identity discovery. Whatever answers at
    /// `addr` — a metor-db server (mirrored, with on-demand history
    /// hydration) or an fsw link server (streamed in, with panel commands
    /// forwarded up) — the backend commits to that mode and reconnects on
    /// its own until disconnected. All three built-in kinds share the
    /// `tcp:<addr>` id: a layout and a recents row belong to the system at
    /// an address, not to the transport mode.
    pub fn tcp(name: impl Into<SharedString>, addr: SocketAddr) -> Self {
        Self {
            id: TargetId(SharedString::from(format!("tcp:{addr}"))),
            name: name.into(),
            detail: SharedString::from(addr.to_string()),
            backend: Arc::new(DiscoverBackend { addr }),
            address: Some(SharedString::from(addr.to_string())),
            options: OptionSpec::default(),
        }
    }

    /// [`tcp`](Self::tcp) pinned to the metor-db mirror mode (`db://` in
    /// the address input): no identity probe, fails against an fsw link.
    pub fn db(name: impl Into<SharedString>, addr: SocketAddr) -> Self {
        Self {
            id: TargetId(SharedString::from(format!("tcp:{addr}"))),
            name: name.into(),
            detail: SharedString::from(format!("{addr} · metor-db")),
            backend: Arc::new(TcpBackend { addr }),
            address: Some(SharedString::from(format!("db://{addr}"))),
            options: OptionSpec::default(),
        }
    }

    /// [`tcp`](Self::tcp) pinned to the fsw link mode (`fsw://` in the
    /// address input): a metor-db answering here is a clean failure.
    pub fn fsw(name: impl Into<SharedString>, addr: SocketAddr) -> Self {
        Self {
            id: TargetId(SharedString::from(format!("tcp:{addr}"))),
            name: name.into(),
            detail: SharedString::from(format!("{addr} · fsw")),
            backend: Arc::new(FswBackend { addr }),
            address: Some(SharedString::from(format!("fsw://{addr}"))),
            options: OptionSpec::default(),
        }
    }

    pub fn custom(
        id: impl Into<SharedString>,
        name: impl Into<SharedString>,
        detail: impl Into<SharedString>,
        backend: impl ConnectionBackend,
    ) -> Self {
        Self {
            id: TargetId(id.into()),
            name: name.into(),
            detail: detail.into(),
            backend: Arc::new(backend),
            address: None,
            options: OptionSpec::default(),
        }
    }

    /// Mark this target as rebuildable from `address` by the installed
    /// [`AddressResolver`], so recents survive restarts. Resolvers should
    /// set this on every target they produce.
    pub fn with_address(mut self, address: impl Into<SharedString>) -> Self {
        self.address = Some(address.into());
        self
    }

    /// Declare the knobs this target's backend reads out of
    /// [`ConnectContext::options`]. Replaces the panel-wide default set for
    /// this target rather than adding to it — a backend that knows its own
    /// configuration surface knows all of it.
    pub fn with_options(mut self, options: impl IntoIterator<Item = ConnectionOption>) -> Self {
        self.options = options.into_iter().collect();
        self
    }
}

/// The `MirrorEvent → ConnectionStatus` mapping every db-mirror path
/// shares: the supervisor emits Connecting before every attempt; once a
/// handshake has succeeded, later attempts read as reconnects.
fn mirror_status(status: StatusHandle) -> impl Fn(MirrorEvent) + Send + Sync + 'static {
    let connected_once = AtomicBool::new(false);
    move |event| {
        let status_for = |event| match event {
            MirrorEvent::Connecting => {
                if connected_once.load(Ordering::Relaxed) {
                    ConnectionStatus::Reconnecting
                } else {
                    ConnectionStatus::Connecting
                }
            }
            MirrorEvent::Connected => {
                connected_once.store(true, Ordering::Relaxed);
                ConnectionStatus::Connected
            }
            MirrorEvent::Disconnected => ConnectionStatus::Reconnecting,
            MirrorEvent::Failed => {
                ConnectionStatus::Failed(SharedString::new_static("connection failed"))
            }
        };
        status.set(status_for(event));
    }
}

struct TcpBackend {
    addr: SocketAddr,
}

impl ConnectionBackend for TcpBackend {
    fn connect(&self, ctx: ConnectContext) -> Connected {
        let remote = RemoteDb::new(self.addr).on_event(mirror_status(ctx.status.clone()));
        let hydrator = remote.hydrator();
        let db = ctx.db.clone();
        ctx.spawn(move || async move {
            remote.spawn(db);
            // The mirror and hydrator tasks own this thread's runtime; park
            // so it never winds down until the cancel token fires.
            std::future::pending::<()>().await
        });
        Connected {
            hydrator: Some(hydrator),
            local_authority: false,
            resolved: None,
        }
    }
}

const LINK_BACKOFF_INITIAL: Duration = Duration::from_millis(500);
const LINK_BACKOFF_MAX: Duration = Duration::from_secs(10);

/// The fsw mode's whole lifecycle: identify (or take the discovery
/// connection), stream until the link drops, back off, repeat. A metor-db
/// answering is a clean failure each round — the mode is pinned.
async fn fsw_loop(addr: SocketAddr, db: Arc<DB>, status: StatusHandle, mut next: Option<Peer>) {
    let mut backoff = LINK_BACKOFF_INITIAL;
    let mut connected_once = false;
    loop {
        let peer = match next.take() {
            Some(peer) => Ok(peer),
            None => {
                status.set(if connected_once {
                    ConnectionStatus::Reconnecting
                } else {
                    ConnectionStatus::Connecting
                });
                identify(addr).await
            }
        };
        match peer {
            Ok(Peer::Fsw { info, rx, tx, buf }) => {
                status.set(ConnectionStatus::Connected);
                connected_once = true;
                backoff = LINK_BACKOFF_INITIAL;
                let err = fsw_stream(info, rx, tx, buf, &db).await;
                tracing::info!(%err, %addr, "fsw link dropped; reconnecting");
                status.set(ConnectionStatus::Reconnecting);
            }
            Ok(Peer::Db(_)) => status.set(ConnectionStatus::Failed(SharedString::new_static(
                "a metor-db answered at this address; connect it with db:// (or the plain address)",
            ))),
            Err(err) => status.set(if connected_once {
                ConnectionStatus::Reconnecting
            } else {
                ConnectionStatus::Failed(SharedString::from(format!("{addr}: {err}")))
            }),
        }
        stellarator::sleep(backoff).await;
        backoff = (backoff * 2).min(LINK_BACKOFF_MAX);
    }
}

/// Pinned fsw link mode (`fsw://`).
struct FswBackend {
    addr: SocketAddr,
}

impl ConnectionBackend for FswBackend {
    fn connect(&self, ctx: ConnectContext) -> Connected {
        let (db, status, addr) = (ctx.db.clone(), ctx.status.clone(), self.addr);
        ctx.spawn(move || async move { fsw_loop(addr, db, status, None).await });
        Connected {
            hydrator: None,
            // The panel's DB is the system of record for an fsw stream, the
            // same authority the old dial-in flow had.
            local_authority: true,
            resolved: None,
        }
    }
}

/// The bare-address kind: identify once, commit to what answered for the
/// connection's lifetime, and send the mode's real resources through the
/// [`Resolved`] channel.
struct DiscoverBackend {
    addr: SocketAddr,
}

impl ConnectionBackend for DiscoverBackend {
    fn connect(&self, ctx: ConnectContext) -> Connected {
        let (tx, rx) = std::sync::mpsc::channel();
        let (db, status, addr) = (ctx.db.clone(), ctx.status.clone(), self.addr);
        ctx.spawn(move || async move {
            let mut backoff = LINK_BACKOFF_INITIAL;
            loop {
                status.set(ConnectionStatus::Connecting);
                // The terminal arms never return, but the compiler sees a
                // loop; clones keep the borrows straight.
                match identify(addr).await {
                    Ok(Peer::Db(_)) => {
                        let remote = RemoteDb::new(addr).on_event(mirror_status(status.clone()));
                        let _ = tx.send(Resolved {
                            hydrator: Some(remote.hydrator()),
                            local_authority: false,
                        });
                        // The probe connection is gone; the mirror dials and
                        // supervises itself from here.
                        remote.spawn(db.clone());
                        std::future::pending::<()>().await
                    }
                    Ok(peer @ Peer::Fsw { .. }) => {
                        let _ = tx.send(Resolved {
                            hydrator: None,
                            local_authority: true,
                        });
                        fsw_loop(addr, db.clone(), status.clone(), Some(peer)).await
                    }
                    Err(err) => {
                        status.set(ConnectionStatus::Failed(SharedString::from(format!(
                            "{addr}: {err}"
                        ))));
                        stellarator::sleep(backoff).await;
                        backoff = (backoff * 2).min(LINK_BACKOFF_MAX);
                    }
                }
            }
        });
        Connected {
            hydrator: None,
            local_authority: false,
            resolved: Some(rx),
        }
    }
}
