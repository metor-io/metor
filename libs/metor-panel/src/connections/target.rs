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
use std::sync::atomic::{AtomicU64, Ordering};

use gpui::SharedString;
use metor_db::DB;
use metor_db::remote::{Hydrator, MirrorEvent, RemoteDb};
use stellarator::struc_con::ThreadBuilder;
use stellarator::util::CancelToken;

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

/// One row in the picker: identity, display fields, and how to connect.
#[derive(Clone)]
pub struct ConnectionTarget {
    pub id: TargetId,
    pub name: SharedString,
    /// Subtitle column: address, serial number, discovery origin.
    pub detail: SharedString,
    pub backend: Arc<dyn ConnectionBackend>,
}

impl ConnectionTarget {
    /// Built-in kind: mirror a remote metor-db over TCP. Live telemetry
    /// streams into the shared DB and history hydrates on demand; the mirror
    /// reconnects on its own until disconnected.
    pub fn tcp(name: impl Into<SharedString>, addr: SocketAddr) -> Self {
        Self {
            id: TargetId(SharedString::from(format!("tcp:{addr}"))),
            name: name.into(),
            detail: SharedString::from(addr.to_string()),
            backend: Arc::new(TcpBackend { addr }),
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
        }
    }
}

struct TcpBackend {
    addr: SocketAddr,
}

impl ConnectionBackend for TcpBackend {
    fn connect(&self, ctx: ConnectContext) -> Connected {
        let status = ctx.status.clone();
        // The supervisor emits Connecting before every attempt; once a
        // handshake has succeeded, later attempts read as reconnects.
        let connected_once = std::sync::atomic::AtomicBool::new(false);
        let remote = RemoteDb::new(self.addr).on_event(move |event| {
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
        });
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
        }
    }
}
