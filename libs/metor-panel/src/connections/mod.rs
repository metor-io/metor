//! The connection system: a reactive registry of connectable targets and
//! the set of connections currently feeding the shared DB.
//!
//! All connections write into the one local [`DB`] — component namespaces
//! are assumed largely disjoint, so concurrent targets coexist the same way
//! concurrent producers always have. A target's [`ConnectionBackend`] spawns
//! its feed on stellar threads raced against a per-connection
//! [`CancelToken`]; disconnecting cancels the token and the executor tears
//! the backend down. Name collisions across targets are not defended
//! against — they behave like two producers fighting over one component.
//!
//! Discovery is push-based and thread-friendly: [`ConnectionsStore::init`]
//! returns a [`RegistryHandle`] that service-discovery or cloud-backend
//! threads upsert targets through; the store drains it on the gpui thread
//! and repaints observers.

mod target;

pub use target::{
    ConnectContext, Connected, ConnectionBackend, ConnectionStatus, ConnectionTarget,
    StatusHandle, TargetId,
};

use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use gpui::{App, AppContext as _, Context, Entity, Global, Task};
use metor_db::DB;
use metor_db::remote::Hydrator;
use stellarator::util::CancelToken;

use crate::hydration::Hydrators;

/// How often the store drains registry ops and polls backend status.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// A registry mutation from a discovery producer.
pub enum RegistryOp {
    Upsert(ConnectionTarget),
    Remove(TargetId),
}

/// Send + Clone handle for discovery threads (an mDNS scan, a cloud poll).
/// Ops are drained by the store's poll task on the gpui thread, so targets
/// appear in the picker without the producer touching any UI machinery.
#[derive(Clone)]
pub struct RegistryHandle {
    tx: mpsc::Sender<RegistryOp>,
}

impl RegistryHandle {
    pub fn upsert(&self, target: ConnectionTarget) {
        let _ = self.tx.send(RegistryOp::Upsert(target));
    }

    pub fn remove(&self, id: TargetId) {
        let _ = self.tx.send(RegistryOp::Remove(id));
    }
}

/// A target we are currently feeding the DB from.
pub struct ActiveConnection {
    pub target: ConnectionTarget,
    cancel: CancelToken,
    status: StatusHandle,
    hydrator: Option<Hydrator>,
    /// Poll-task bookkeeping: last status generation folded into the UI.
    last_generation: u64,
}

impl ActiveConnection {
    pub fn status(&self) -> ConnectionStatus {
        self.status.get()
    }
}

/// Pure fold of the target registry; kept free of gpui and IO so ordering
/// and dedup rules are unit-testable.
#[derive(Default)]
pub struct ConnectionsState {
    /// Discovery order; upserts update display fields in place.
    targets: Vec<ConnectionTarget>,
}

impl ConnectionsState {
    pub fn upsert_target(&mut self, target: ConnectionTarget) {
        match self.targets.iter_mut().find(|t| t.id == target.id) {
            Some(existing) => *existing = target,
            None => self.targets.push(target),
        }
    }

    pub fn remove_target(&mut self, id: &TargetId) {
        self.targets.retain(|t| &t.id != id);
    }

    pub fn targets(&self) -> &[ConnectionTarget] {
        &self.targets
    }

    fn apply(&mut self, op: RegistryOp) {
        match op {
            RegistryOp::Upsert(target) => self.upsert_target(target),
            RegistryOp::Remove(id) => self.remove_target(&id),
        }
    }
}

/// Owns the registry state and the active-connection set. Views observe the
/// entity ([`try_global`]) and re-read on notify; backends report in through
/// their [`StatusHandle`]s, folded by the poll task.
pub struct ConnectionsStore {
    state: ConnectionsState,
    active: Vec<ActiveConnection>,
    db: Arc<DB>,
    /// The LoD companion task is DB-wide and never stopped, so it spawns at
    /// most once — on the first local-authority connection.
    lod_spawned: bool,
    registry_rx: mpsc::Receiver<RegistryOp>,
    _poll: Task<()>,
}

pub struct GlobalConnections(pub Entity<ConnectionsStore>);

impl Global for GlobalConnections {}

pub fn try_global(cx: &App) -> Option<Entity<ConnectionsStore>> {
    cx.try_global::<GlobalConnections>().map(|g| g.0.clone())
}

impl ConnectionsStore {
    /// Install the store and hand back the producer side of the registry.
    pub fn init(db: Arc<DB>, cx: &mut App) -> RegistryHandle {
        let (tx, rx) = mpsc::channel();
        let entity = cx.new(|cx| ConnectionsStore::new(db, rx, cx));
        cx.set_global(GlobalConnections(entity));
        RegistryHandle { tx }
    }

    fn new(db: Arc<DB>, registry_rx: mpsc::Receiver<RegistryOp>, cx: &mut Context<Self>) -> Self {
        let poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL_INTERVAL).await;
                let alive = this.update(cx, |store, cx| {
                    let mut changed = false;
                    while let Ok(op) = store.registry_rx.try_recv() {
                        store.state.apply(op);
                        changed = true;
                    }
                    for conn in &mut store.active {
                        let generation = conn.status.generation();
                        if generation != conn.last_generation {
                            conn.last_generation = generation;
                            changed = true;
                        }
                    }
                    if changed {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        Self {
            state: ConnectionsState::default(),
            active: Vec::new(),
            db,
            lod_spawned: false,
            registry_rx,
            _poll: poll,
        }
    }

    pub fn state(&self) -> &ConnectionsState {
        &self.state
    }

    pub fn active(&self) -> &[ActiveConnection] {
        &self.active
    }

    pub fn is_connected(&self, id: &TargetId) -> bool {
        self.active.iter().any(|c| &c.target.id == id)
    }

    pub fn upsert_target(&mut self, target: ConnectionTarget, cx: &mut Context<Self>) {
        self.state.upsert_target(target);
        cx.notify();
    }

    /// Stand up `target`'s backend against the shared DB. A no-op when the
    /// target is already connected.
    pub fn connect(&mut self, target: ConnectionTarget, cx: &mut Context<Self>) {
        if self.is_connected(&target.id) {
            return;
        }
        let cancel = CancelToken::new();
        let status = StatusHandle::default();
        let connected = target.backend.connect(ConnectContext {
            db: self.db.clone(),
            cancel: cancel.clone(),
            status: status.clone(),
        });
        if let Some(hydrator) = &connected.hydrator {
            Hydrators::global(cx).insert(target.id.clone(), hydrator.clone());
        }
        if connected.local_authority && !self.lod_spawned {
            self.lod_spawned = true;
            let db = self.db.clone();
            // Deliberately outlives the connection: the LoD companion serves
            // the whole DB, and its buckets stay valid after a disconnect.
            drop(stellarator::struc_con::stellar(move || async move {
                metor_db::lod::spawn(db);
                std::future::pending::<()>().await
            }));
        }
        self.active.push(ActiveConnection {
            target,
            cancel,
            status,
            hydrator: connected.hydrator,
            last_generation: 0,
        });
        cx.notify();
    }

    /// Cancel `id`'s backend. The stellar executor drops the backend's tasks
    /// and IO when the token fires; there is no per-backend teardown.
    pub fn disconnect(&mut self, id: &TargetId, cx: &mut Context<Self>) {
        let Some(index) = self.active.iter().position(|c| &c.target.id == id) else {
            return;
        };
        let conn = self.active.remove(index);
        conn.cancel.cancel();
        conn.status.set(ConnectionStatus::Disconnected);
        if conn.hydrator.is_some() {
            Hydrators::global(cx).remove(id);
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests;
