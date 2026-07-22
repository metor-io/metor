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

pub mod discovery;
pub mod persist;
mod picker;
mod target;

pub use discovery::mdns_source;
pub use picker::ConnectionPicker;
pub use target::{
    AddressResolver, ConnectContext, Connected, ConnectionBackend, ConnectionStatus,
    ConnectionTarget, StatusHandle, TargetId, Resolved,
};

use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use gpui::{App, AppContext as _, Context, Entity, Global, SharedString, Task, WeakEntity};
use metor_db::DB;
use metor_db::remote::Hydrator;
use stellarator::util::CancelToken;

use crate::hydration::Hydrators;

/// How often the store drains registry ops and polls backend status.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// How often the layout autosave snapshot runs while connected.
const AUTOSAVE_INTERVAL: Duration = Duration::from_secs(2);

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
    /// A discovery backend's late-bound resources; drained by the poll task
    /// and cleared once applied.
    resolved: Option<std::sync::mpsc::Receiver<target::Resolved>>,
    /// Poll-task bookkeeping: last status generation folded into the UI.
    last_generation: u64,
}

impl ActiveConnection {
    pub fn status(&self) -> ConnectionStatus {
        self.status.get()
    }
}

/// One picker line: a target when it's live (registered or re-materialized
/// from a TCP recent), or display fields alone for a custom-backend recent
/// whose discoverer hasn't re-produced it yet.
pub struct PickerEntry {
    pub id: TargetId,
    pub name: SharedString,
    pub detail: SharedString,
    pub target: Option<ConnectionTarget>,
    pub favorite: bool,
}

/// The picker's section ordering: pinned targets, then recently connected
/// ones not already pinned, then the rest of the registry in discovery
/// order.
#[derive(Default)]
pub struct PickerSections {
    pub favorites: Vec<PickerEntry>,
    pub recents: Vec<PickerEntry>,
    pub discovered: Vec<PickerEntry>,
}

/// Pure fold of the target registry plus the persisted index; kept free of
/// gpui and IO so ordering and dedup rules are unit-testable.
#[derive(Default)]
pub struct ConnectionsState {
    /// Discovery order; upserts update display fields in place.
    targets: Vec<ConnectionTarget>,
    favorites: Vec<TargetId>,
    /// Most-recent first, capped at [`persist::RECENTS_CAP`].
    recents: Vec<persist::RecentEntry>,
    /// Re-materializes address-carrying recents; also drives the dialog's
    /// address input. Defaults to the built-in `host:port` TCP kind.
    resolver: AddressResolver,
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

    pub fn is_favorite(&self, id: &TargetId) -> bool {
        self.favorites.contains(id)
    }

    pub fn toggle_favorite(&mut self, id: &TargetId) {
        if let Some(ix) = self.favorites.iter().position(|f| f == id) {
            self.favorites.remove(ix);
        } else {
            self.favorites.push(id.clone());
        }
    }

    pub fn record_connected(&mut self, target: &ConnectionTarget, at_unix: i64) {
        self.recents.retain(|r| r.id != target.id.as_str());
        self.recents.insert(
            0,
            persist::RecentEntry {
                id: target.id.as_str().to_string(),
                name: target.name.to_string(),
                detail: target.detail.to_string(),
                address: target.address.as_ref().map(|a| a.to_string()),
                last_connected_unix: at_unix,
            },
        );
        self.recents.truncate(persist::RECENTS_CAP);
    }

    pub fn set_resolver(&mut self, resolver: AddressResolver) {
        self.resolver = resolver;
    }

    pub fn resolver(&self) -> &AddressResolver {
        &self.resolver
    }

    pub fn load_index(&mut self, index: persist::ConnectionsIndex) {
        self.favorites = index
            .favorites
            .into_iter()
            .map(|id| TargetId(id.into()))
            .collect();
        self.recents = index.recents;
    }

    pub fn index(&self) -> persist::ConnectionsIndex {
        persist::ConnectionsIndex {
            favorites: self.favorites.iter().map(|f| f.as_str().to_string()).collect(),
            recents: self.recents.clone(),
        }
    }

    /// Resolve `id` to something connectable: the live registry first, then
    /// an address-carrying recent re-materialized through the resolver.
    fn resolve(&self, id: &TargetId) -> Option<ConnectionTarget> {
        if let Some(target) = self.targets.iter().find(|t| &t.id == id) {
            return Some(target.clone());
        }
        let recent = self.recents.iter().find(|r| r.id == id.as_str())?;
        let address = recent.address.as_ref()?;
        let mut target = (self.resolver.resolve)(address).ok()?;
        // The resolver only saw the address; the recent remembers what the
        // user actually called this system.
        target.name = SharedString::from(recent.name.clone());
        if !recent.detail.is_empty() {
            target.detail = SharedString::from(recent.detail.clone());
        }
        Some(target)
    }

    fn entry(&self, id: &TargetId) -> Option<PickerEntry> {
        let target = self.resolve(id);
        let (name, detail) = if let Some(target) = &target {
            (target.name.clone(), target.detail.clone())
        } else {
            let recent = self.recents.iter().find(|r| r.id == id.as_str())?;
            (
                SharedString::from(recent.name.clone()),
                SharedString::from(recent.detail.clone()),
            )
        };
        Some(PickerEntry {
            id: id.clone(),
            name,
            detail,
            target,
            favorite: self.is_favorite(id),
        })
    }

    pub fn sections(&self) -> PickerSections {
        let mut sections = PickerSections::default();
        let mut seen: Vec<TargetId> = Vec::new();
        for id in &self.favorites {
            if let Some(entry) = self.entry(id) {
                seen.push(id.clone());
                sections.favorites.push(entry);
            }
        }
        for recent in &self.recents {
            let id = TargetId(SharedString::from(recent.id.clone()));
            if seen.contains(&id) {
                continue;
            }
            if let Some(entry) = self.entry(&id) {
                seen.push(id);
                sections.recents.push(entry);
            }
        }
        for target in &self.targets {
            if seen.contains(&target.id) {
                continue;
            }
            sections.discovered.push(PickerEntry {
                id: target.id.clone(),
                name: target.name.clone(),
                detail: target.detail.clone(),
                target: Some(target.clone()),
                favorite: false,
            });
        }
        sections
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
    /// The window's tile tree, handed over by the app root; the autosave
    /// tick snapshots it while anything is connected.
    tiles: Option<WeakEntity<crate::tiles::TileGroup>>,
    /// Last layout JSON written to disk; skips rewrites while idle. Pane and
    /// item mutations don't notify the tile group, so autosave poll-compares
    /// instead of observing.
    last_saved_layout: Option<String>,
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
        cx.set_global(GlobalConnections(entity.clone()));
        cx.on_app_quit(move |cx| {
            entity.update(cx, |store, cx| store.flush_layout(cx));
            async {}
        })
        .detach();
        RegistryHandle { tx }
    }

    fn new(db: Arc<DB>, registry_rx: mpsc::Receiver<RegistryOp>, cx: &mut Context<Self>) -> Self {
        let poll = cx.spawn(async move |this, cx| {
            let ticks_per_autosave = (AUTOSAVE_INTERVAL.as_millis() / POLL_INTERVAL.as_millis())
                .max(1) as u32;
            let mut tick = 0u32;
            loop {
                cx.background_executor().timer(POLL_INTERVAL).await;
                tick = tick.wrapping_add(1);
                let autosave = tick.is_multiple_of(ticks_per_autosave);
                let alive = this.update(cx, |store, cx| {
                    let mut changed = false;
                    while let Ok(op) = store.registry_rx.try_recv() {
                        store.state.apply(op);
                        changed = true;
                    }
                    // Apply late-bound discovery resolutions: the backend
                    // learned its mode after connect() returned, and the
                    // hydrator/authority routing happens here instead.
                    let mut resolutions = Vec::new();
                    for conn in &mut store.active {
                        if let Some(rx) = &conn.resolved
                            && let Ok(resolved) = rx.try_recv()
                        {
                            conn.resolved = None;
                            conn.hydrator = resolved.hydrator.clone();
                            resolutions.push((conn.target.id.clone(), resolved));
                            changed = true;
                        }
                    }
                    for (id, resolved) in resolutions {
                        if let Some(hydrator) = resolved.hydrator {
                            Hydrators::global(cx).insert(id, hydrator);
                        }
                        if resolved.local_authority {
                            store.spawn_lod_once();
                        }
                    }
                    for conn in &mut store.active {
                        let generation = conn.status.generation();
                        if generation != conn.last_generation {
                            conn.last_generation = generation;
                            changed = true;
                        }
                    }
                    if autosave {
                        store.flush_layout(cx);
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
        let mut state = ConnectionsState::default();
        state.load_index(persist::load_index());
        Self {
            state,
            active: Vec::new(),
            db,
            lod_spawned: false,
            tiles: None,
            last_saved_layout: None,
            registry_rx,
            _poll: poll,
        }
    }

    /// Hand over the window's tile tree for layout autosave and restore.
    pub fn set_tiles(&mut self, tiles: WeakEntity<crate::tiles::TileGroup>) {
        self.tiles = Some(tiles);
    }

    pub fn tiles(&self) -> Option<Entity<crate::tiles::TileGroup>> {
        self.tiles.as_ref().and_then(|w| w.upgrade())
    }

    /// Snapshot the layout to every active connection's file when it moved
    /// since the last write. The layout is "what the user ran while
    /// connected to X" — on reconnect to any X it is offered back.
    fn flush_layout(&mut self, cx: &mut Context<Self>) {
        if self.active.is_empty() {
            return;
        }
        let Some(tiles) = self.tiles() else {
            return;
        };
        let json = tiles.read(cx).to_json(cx);
        if self.last_saved_layout.as_deref() == Some(json.as_str()) {
            return;
        }
        for conn in &self.active {
            if let Err(err) = persist::save_layout(&conn.target.id, &json) {
                tracing::error!(%err, id = %conn.target.id, "layout autosave failed");
            }
        }
        self.last_saved_layout = Some(json);
    }

    /// Note a layout the picker just loaded, so the next autosave tick
    /// doesn't immediately rewrite every file with it.
    pub(crate) fn note_loaded_layout(&mut self, json: String) {
        self.last_saved_layout = Some(json);
    }

    pub fn set_resolver(&mut self, resolver: AddressResolver) {
        self.state.set_resolver(resolver);
    }

    pub fn toggle_favorite(&mut self, id: &TargetId, cx: &mut Context<Self>) {
        self.state.toggle_favorite(id);
        if let Err(err) = persist::save_index(&self.state.index()) {
            tracing::error!(%err, "save connections index failed");
        }
        cx.notify();
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
        if connected.local_authority {
            self.spawn_lod_once();
        }
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or_default();
        self.state.record_connected(&target, now_unix);
        if let Err(err) = persist::save_index(&self.state.index()) {
            tracing::error!(%err, "save connections index failed");
        }
        self.active.push(ActiveConnection {
            target,
            cancel,
            status,
            hydrator: connected.hydrator,
            resolved: connected.resolved,
            last_generation: 0,
        });
        cx.notify();
    }

    /// Spawn the DB-wide LoD companion for the first local-authority
    /// connection. Deliberately outlives the connection: the companion
    /// serves the whole DB, and its buckets stay valid after a disconnect.
    fn spawn_lod_once(&mut self) {
        if self.lod_spawned {
            return;
        }
        self.lod_spawned = true;
        let db = self.db.clone();
        drop(stellarator::struc_con::stellar(move || async move {
            metor_db::lod::spawn(db);
            std::future::pending::<()>().await
        }));
    }

    /// Cancel `id`'s backend. The stellar executor drops the backend's tasks
    /// and IO when the token fires; there is no per-backend teardown.
    pub fn disconnect(&mut self, id: &TargetId, cx: &mut Context<Self>) {
        let Some(index) = self.active.iter().position(|c| &c.target.id == id) else {
            return;
        };
        self.flush_layout(cx);
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
