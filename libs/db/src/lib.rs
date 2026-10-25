use datafusion::common::HashSet;
use futures_lite::StreamExt;
use metor_proto::registry::VTableRegistry;
use metor_proto::types::{PacketHeader, PacketTy};
use metor_proto::vtable::builder::{
    OpBuilder, component, raw_field, raw_table, schema, timestamp, vtable,
};
use metor_proto::vtable::{ElementKey, RealizedField, builder};
use metor_proto::{
    registry,
    schema::Schema,
    types::{
        ComponentId, ComponentView, IntoLenPacket, LenPacket, Msg, OwnedPacket as Packet, PacketId,
        PrimType, RequestId, Timestamp,
    },
    vtable::VTable,
};
use metor_proto_stellar::{PacketSink, PacketStream};
use metor_proto_wkt::*;
use msg_log::MsgLog;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use smallvec::SmallVec;
use std::{
    borrow::Cow,
    collections::HashMap,
    ffi::OsStr,
    net::{SocketAddr, ToSocketAddrs},
    path::{Path, PathBuf},
    sync::{
        Arc, PoisonError, RwLock,
        atomic::{self, AtomicBool, AtomicI64, AtomicU64},
    },
    time::{Duration, Instant},
};
use stellarator::{
    buf::Slice,
    io::{AsyncRead, AsyncWrite, OwnedReader, OwnedWriter, SplitExt},
    net::{TcpListener, TcpStream, UdpSocket},
    rent,
    struc_con::Joinable,
    sync::{Mutex, WaitQueue},
    util::AtomicCell,
};
use time_series::TimeSeries;
use tracing::{debug, info, trace, warn};
use vtable_stream::handle_vtable_stream;
use zerocopy::IntoBytes;

pub use error::Error;

use crate::disruptor::Disruptor;

pub mod append_log;
mod arc_ring;
mod arrow;
pub mod disruptor;
mod error;
pub mod lod;
pub mod manifest;
pub mod msg_log;
pub mod remote;
pub mod seal;
pub mod store;
pub mod tiering;
pub mod time_series;

mod vtable_stream;

pub mod snapshot;

pub struct DB {
    pub vtable_gen: AtomicCell<u64>,
    /// Invalidates cached display metadata independently of live sample arrival.
    pub metadata_gen: AtomicCell<u64>,
    state: RwLock<State>,
    pub recording_cell: PlayingCell,

    // metadata
    pub path: PathBuf,
    pub default_stream_time_step: AtomicU64,
    pub last_updated: AtomicCell<Timestamp>,
    /// Earliest timestamp known to this instance. Moves backwards when
    /// older history is hydrated from a store.
    pub earliest_timestamp: AtomicCell<Timestamp>,
}

#[derive(Default)]
pub struct State {
    components: HashMap<ComponentId, Component>,
    component_metadata: HashMap<ComponentId, ComponentMetadata>,
    metadata_generation: u64,

    msg_logs: HashMap<PacketId, MsgLog>,

    /// Msg ids this db serves as commands: a client forwards them up, and
    /// they never ride a [`SyncMsgs`] stream back down.
    command_ids: Vec<PacketId>,
    /// This db's telemetry namespace, when it serves one target's prefix.
    namespace: Option<String>,

    vtable_registry: registry::HashMapRegistry,
    streams: HashMap<StreamId, Arc<FixedRateStreamState>>,

    /// Raw bytes of the most recently ingested table for each dynamic vtable,
    /// created by [`DB::insert_vtable`] and empty until the first table. The
    /// dynamic stream forwards exactly these bytes each tick, so a key absent
    /// from the newest table drops out of the next streamed packet
    /// (vtable-dynamic.md §8.6).
    latest_dynamic_tables: HashMap<PacketId, std::sync::Mutex<Vec<u8>>>,

    udp_vtable_streams: HashSet<(SocketAddr, [u8; 2])>,

    pub db_config: DbConfig,
}

impl DB {
    pub fn create(path: PathBuf) -> Result<Self, Error> {
        Self::with_time_step(path, Duration::from_secs_f64(1.0 / 120.0))
    }

    pub fn with_time_step(path: PathBuf, time_step: Duration) -> Result<Self, Error> {
        info!(?path, "created db");

        std::fs::create_dir_all(&path)?;
        let default_stream_time_step = AtomicU64::new(time_step.as_nanos() as u64);
        let state = State {
            db_config: DbConfig {
                default_stream_time_step: time_step,
                ..Default::default()
            },
            ..Default::default()
        };
        let db = DB {
            state: RwLock::new(state),
            recording_cell: PlayingCell::new(true),
            path,
            vtable_gen: AtomicCell::new(0),
            metadata_gen: AtomicCell::new(0),
            default_stream_time_step,
            last_updated: AtomicCell::new(Timestamp(i64::MIN)),
            earliest_timestamp: AtomicCell::new(Timestamp::now()),
        };
        db.save_db_state()?;
        Ok(db)
    }

    pub fn with_state<O, F: FnOnce(&State) -> O>(&self, f: F) -> O {
        let state = self.state.read().unwrap();
        f(&state)
    }

    pub fn with_state_mut<O, F: FnOnce(&mut State) -> O>(&self, f: F) -> O {
        let mut state = self.state.write().unwrap();
        let generation = state.metadata_generation;
        let result = f(&mut state);
        if generation != state.metadata_generation {
            self.metadata_gen.store(state.metadata_generation);
        }
        result
    }

    fn db_config(&self) -> DbConfig {
        self.with_state(|db| db.db_config.clone())
    }

    pub fn save_db_state(&self) -> Result<(), Error> {
        let db_state = self.db_config();
        db_state.write(self.path.join("db_state"))
    }

    /// Drain accepted WAL data and flush mapped storage after producers stop.
    /// Persistence readers belong to the DB, so executor teardown cannot discard
    /// the final queued samples or messages. Callers must first join all writers.
    pub fn flush(&self) -> Result<(), Error> {
        self.with_state(|state| {
            for component in state.components.values() {
                component.flush_pending()?;
                for node in component.time_series.list.iter() {
                    node.index.flush()?;
                    node.data.flush()?;
                }
            }
            for log in state.msg_logs.values() {
                log.flush()?;
            }
            Ok::<_, Error>(())
        })?;
        self.save_db_state()
    }

    pub fn open(path: PathBuf) -> Result<Self, Error> {
        let mut component_metadata = HashMap::new();
        let mut components = HashMap::new();
        let mut msg_logs = HashMap::new();
        let mut last_updated = i64::MIN;
        let mut start_timestamp = i64::MAX;

        info!("Opening database: {}", path.display());
        for elem in std::fs::read_dir(&path)? {
            let Ok(elem) = elem else { continue };
            let path = elem.path();
            if !path.is_dir() || path.file_name() == Some(OsStr::new("msgs")) {
                trace!("Skipping non-component directory: {}", path.display());
                continue;
            }

            let component_id = ComponentId(
                path.file_name()
                    .and_then(|p| p.to_str())
                    .and_then(|p| p.parse().ok())
                    .ok_or(Error::InvalidComponentId)?,
            );

            let schema = ComponentSchema::read(path.join("schema"))?;
            let metadata = ComponentMetadata::read(path.join("metadata"))?;
            trace!("Read component metadata for {}", metadata.name);

            trace!("Opening component file {}", path.display());

            // A LoD component's writer belongs to the engine; open it in
            // drain mode so its `persist` never claims the writer that
            // `setup_levels` re-takes after restart.
            let engine_owned = lod::is_lod_name(&metadata.name);
            let component = Component::open(&path, component_id, schema.clone(), engine_owned)?;
            if engine_owned {
                component
                    .time_series
                    .set_max_node_age(lod::lod_node_max_age());
            }
            component_metadata.insert(component_id, metadata);
            if let Some(latest) = component.time_series.latest() {
                let timestamp = latest.timestamp();
                last_updated = timestamp.0.max(last_updated);
            };
            if let Some(first_timestamp) = component.time_series.start_timestamp() {
                start_timestamp = start_timestamp.min(first_timestamp.0);
            };
            components.insert(component_id, component);
        }
        if let Ok(msgs_dir) = std::fs::read_dir(path.join("msgs")) {
            for elem in msgs_dir {
                let Ok(elem) = elem else { continue };

                let path = elem.path();
                let msg_id: u16 = path
                    .file_name()
                    .and_then(|p| p.to_str())
                    .and_then(|p| p.parse().ok())
                    .ok_or(Error::InvalidMsgId)?;
                let msg_log = MsgLog::open(path)?;
                if let Some(first_timestamp) = msg_log.first_timestamp() {
                    start_timestamp = start_timestamp.min(first_timestamp.0);
                }

                if let Some(msg_ref) = msg_log.latest() {
                    last_updated = msg_ref.timestamp().0.max(last_updated);
                };
                msg_logs.insert(msg_id.to_le_bytes(), msg_log);
            }
        }

        info!(db.path = ?path, "opened db");
        let db_state = DbConfig::read(path.join("db_state"))?;
        let state = State {
            db_config: db_state.clone(),
            components,
            component_metadata,
            msg_logs,
            ..Default::default()
        };
        let earliest_timestamp = if start_timestamp == i64::MAX {
            Timestamp::now()
        } else {
            Timestamp(start_timestamp)
        };
        Ok(DB {
            state: RwLock::new(state),
            path,
            vtable_gen: AtomicCell::new(0),
            metadata_gen: AtomicCell::new(0),
            recording_cell: PlayingCell::new(db_state.recording),
            default_stream_time_step: AtomicU64::new(
                db_state.default_stream_time_step.as_nanos() as u64
            ),
            last_updated: AtomicCell::new(Timestamp(last_updated)),
            earliest_timestamp: AtomicCell::new(earliest_timestamp),
        })
    }

    /// Register an announced vtable under its id. A re-announce of the same
    /// vtable — a reconnect replay — is a no-op; a *different* vtable under
    /// a registered id is refused, so one link's records can never be
    /// decoded through another's schema.
    pub fn insert_vtable(&self, vtable: VTableMsg) -> Result<(), Error> {
        self.with_state_mut(|state| {
            if let Some(registered) = state.vtable_registry.map.get(&vtable.id) {
                return if postcard::to_allocvec(registered)?
                    == postcard::to_allocvec(&vtable.vtable)?
                {
                    debug!(id = ?vtable.id, "vtable already registered");
                    Ok(())
                } else {
                    Err(Error::VTableConflict(vtable.id))
                };
            }
            info!(id = ?vtable.id, "inserting vtable");
            // For static fields this registers the concrete component. For dynamic
            // frames (`List`/`Map`), `realize_fields(None)` yields each member *template*
            // (e.g. `processes.pid`) so its ty/shape is registered up front; the concrete
            // keyed/indexed components (`processes.0.pid`) are created lazily on first
            // sample by [`DB::ingest_table`].
            for res in vtable.vtable.realize_fields(None) {
                let RealizedField {
                    component_id,
                    shape,
                    ty,
                    dynamic,
                    ..
                } = res?;
                let component_schema = ComponentSchema::new(ty, shape);
                state.insert_component(component_id, component_schema, &self.path)?;
                if dynamic {
                    // A member template never receives samples (the concrete keyed
                    // components do), so hide it from listings / live streams.
                    let metadata = ComponentMetadata {
                        component_id,
                        name: component_id.to_string(),
                        metadata: Default::default(),
                    }
                    .with_hidden();
                    state.set_component_metadata(metadata, &self.path)?;
                }
                self.vtable_gen.fetch_add(1, atomic::Ordering::SeqCst);
            }
            if vtable.vtable.is_dynamic() {
                state
                    .latest_dynamic_tables
                    .insert(vtable.id, Default::default());
            }
            state.vtable_registry.map.insert(vtable.id, vtable.vtable);
            Ok::<_, Error>(())
        })?;
        Ok(())
    }

    /// Sinks a table `buf` described by the registered vtable `id`, stamping a
    /// field whose vtable names no timestamp with its arrival.
    ///
    /// A static table's components exist from [`insert_vtable`](Self::insert_vtable),
    /// so it sinks under the read lock without copying its vtable. A dynamic
    /// table sinks under the read lock while all its members exist, realizes
    /// new members under the write lock, and caches its bytes as the latest
    /// table the dynamic stream forwards (see [`State::latest_dynamic_tables`]).
    /// A new component, or a first sample into an empty series, bumps
    /// `vtable_gen` so subscribers re-derive membership.
    pub fn ingest_table(&self, id: PacketId, buf: &[u8]) -> Result<(), Error> {
        let received = Timestamp::now();
        let pushed = match self.ingest_static(id, buf, received)? {
            StaticIngest::Pushed(pushed) => pushed,
            StaticIngest::Dynamic => self.ingest_dynamic(id, buf, received)?,
        };
        if pushed.new_series {
            self.vtable_gen.fetch_add(1, atomic::Ordering::SeqCst);
        }
        if let Some(max_ts) = pushed.max_ts {
            self.last_updated.update_max(max_ts);
        }
        Ok(())
    }

    /// Sinks a static table under the read lock, or reports that the table
    /// is dynamic.
    fn ingest_static(
        &self,
        id: PacketId,
        buf: &[u8],
        received: Timestamp,
    ) -> Result<StaticIngest, Error> {
        self.with_state(|state| {
            let vtable = state
                .vtable_registry
                .get(&id)
                .ok_or(Error::Impeller(metor_proto::error::Error::VTableNotFound))?;
            if vtable.is_dynamic() {
                return Ok(StaticIngest::Dynamic);
            }
            let mut pushed = Pushed::default();
            vtable.for_each_field::<Error>(Some(buf), &mut |rf| {
                let component = state
                    .get_component(rf.component_id)
                    .ok_or(Error::ComponentNotFound(rf.component_id))?;
                pushed.merge(push_field(component, &rf, received)?);
                Ok(())
            })?;
            Ok(StaticIngest::Pushed(pushed))
        })
    }

    /// Sinks a dynamic table, taking the write lock only when a member is new.
    fn ingest_dynamic(
        &self,
        id: PacketId,
        buf: &[u8],
        received: Timestamp,
    ) -> Result<Pushed, Error> {
        let pushed = match self.with_state(|state| state.push_known_fields(id, buf, received))? {
            Some(pushed) => pushed,
            None => {
                self.with_state_mut(|state| state.realize_and_push(id, buf, received, &self.path))?
            }
        };
        self.with_state(|state| state.cache_latest_table(id, buf));
        Ok(pushed)
    }

    /// Builds one streamed `Table` packet for a dynamic vtable subscription from
    /// the cached latest table bytes (see [`State::latest_dynamic_tables`]),
    /// re-headed with the subscriber's stream `id`. Returns `None` when no table
    /// has been ingested for `id` yet.
    fn dynamic_stream_packet(&self, id: PacketId) -> Option<LenPacket> {
        self.with_state(|s| {
            let cached = s.latest_dynamic_tables.get(&id)?;
            let cached = cached.lock().unwrap_or_else(PoisonError::into_inner);
            if cached.is_empty() {
                return None;
            }
            let mut pkt = LenPacket::table(id, cached.len());
            pkt.extend_from_slice(&cached);
            Some(pkt)
        })
    }

    /// Set what this db advertises in its [`DbInfoResp`]: the namespace its
    /// components carry and the command set clients forward up.
    pub fn set_identity(&self, namespace: Option<String>, command_ids: Vec<PacketId>) {
        self.with_state_mut(|s| {
            s.namespace = namespace;
            s.command_ids = command_ids;
        });
    }

    /// Union more ids into the advertised command set.
    pub fn add_command_ids(&self, ids: &[PacketId]) {
        self.with_state_mut(|s| {
            for id in ids {
                if !s.command_ids.contains(id) {
                    s.command_ids.push(*id);
                }
            }
        });
    }

    pub fn push_msg(&self, timestamp: Timestamp, id: PacketId, msg: &[u8]) -> Result<(), Error> {
        let exists = self.with_state(|s| {
            if let Some(msg_log) = s.msg_logs.get(&id) {
                msg_log.push(timestamp, msg)?;
                Ok::<_, Error>(true)
            } else {
                Ok(false)
            }
        })?;
        if !exists {
            self.with_state_mut(move |s| {
                let msg_log = s.get_or_insert_msg_log(id, &self.path)?;
                msg_log.push(timestamp, msg)?;
                Ok::<_, Error>(())
            })?;
        }
        self.last_updated.update_max(timestamp);
        Ok(())
    }

    pub fn get_or_insert_fixed_rate_state(
        &self,
        stream_id: StreamId,
        behavior: FixedRateBehavior,
    ) -> Arc<FixedRateStreamState> {
        let mut state = self.state.write().expect("poisoned lock");
        state
            .streams
            .entry(stream_id)
            .or_insert_with(|| {
                Arc::new(FixedRateStreamState::new(
                    stream_id,
                    Duration::from_nanos(behavior.timestep),
                    match behavior.initial_timestamp {
                        InitialTimestamp::Earliest => self.earliest_timestamp.latest(),
                        InitialTimestamp::Latest => self.last_updated.latest(),
                        InitialTimestamp::Manual(timestamp) => timestamp,
                    },
                    behavior.frequency,
                ))
            })
            .clone()
    }
}

/// The dotted display name of a lazily-realized dynamic map/list member,
/// joining the container path, the element key, and the member leaf
/// (`alarm_defs.limits.gyro`). `None` for a static field or a
/// member template, which carry no runtime key.
fn dynamic_member_name(rf: &RealizedField) -> Option<String> {
    let container = rf.container?;
    let mut name = String::from(container);
    match rf.element? {
        ElementKey::Key(key) => {
            name.push('.');
            name.push_str(key);
        }
        ElementKey::Index(idx) => {
            use core::fmt::Write as _;
            let _ = write!(name, ".{idx}");
        }
    }
    if let Some(member) = rf.member.filter(|m| !m.is_empty()) {
        name.push('.');
        name.push_str(member);
    }
    Some(name)
}

impl State {
    pub fn insert_component(
        &mut self,
        component_id: ComponentId,
        schema: ComponentSchema,
        db_path: &Path,
    ) -> Result<(), Error> {
        self.insert_component_inner(component_id, schema, db_path, false)
    }

    /// Like [`Self::insert_component`], but for the LoD engine: the created
    /// component's writer belongs to the engine, so its `persist` runs in
    /// drain mode rather than racing to claim the writer.
    pub fn insert_engine_component(
        &mut self,
        component_id: ComponentId,
        schema: ComponentSchema,
        db_path: &Path,
    ) -> Result<(), Error> {
        self.insert_component_inner(component_id, schema, db_path, true)
    }

    fn insert_component_inner(
        &mut self,
        component_id: ComponentId,
        schema: ComponentSchema,
        db_path: &Path,
        engine_owned: bool,
    ) -> Result<(), Error> {
        if let Some(existing_component) = self.components.get(&component_id) {
            if existing_component.schema != schema {
                warn!( ?existing_component.schema, new_component.schema = ?schema,
                       ?existing_component.component_id,
                      "schema mismatch");
                return Err(Error::SchemaMismatch);
            }
            return Ok(());
        }
        info!(component.id = ?component_id.0, "inserting");
        let component_metadata = ComponentMetadata {
            component_id,
            name: component_id.to_string(),
            metadata: Default::default(),
        };
        let component = Component::create(db_path, component_id, schema, engine_owned)?;
        if !self.component_metadata.contains_key(&component_id) {
            self.set_component_metadata(component_metadata, db_path)?;
        }
        self.components.insert(component_id, component);
        Ok(())
    }

    pub fn get_component_metadata(&self, component_id: ComponentId) -> Option<&ComponentMetadata> {
        self.component_metadata.get(&component_id)
    }

    /// The component a dynamic table's field lands in, created and named on
    /// its first sample. A static field's component must already exist.
    fn realize_component(
        &mut self,
        rf: &RealizedField,
        db_path: &Path,
    ) -> Result<&Component, Error> {
        if !self.components.contains_key(&rf.component_id) {
            if !rf.dynamic {
                return Err(Error::ComponentNotFound(rf.component_id));
            }
            // Name the keyed member by its dotted path before
            // `insert_component` falls back to the numeric id, so the panel
            // shows `alarm_defs.limits.<key>`. A member that reappears already
            // has real metadata, so leave it be.
            if !self.component_metadata.contains_key(&rf.component_id)
                && let Some(name) = dynamic_member_name(rf)
            {
                self.set_component_metadata(
                    ComponentMetadata {
                        component_id: rf.component_id,
                        name,
                        metadata: Default::default(),
                    },
                    db_path,
                )?;
            }
            self.insert_component(
                rf.component_id,
                ComponentSchema::new(rf.ty, rf.shape),
                db_path,
            )?;
        }
        self.get_component(rf.component_id)
            .ok_or(Error::ComponentNotFound(rf.component_id))
    }

    pub fn get_component(&self, component_id: ComponentId) -> Option<&Component> {
        self.components.get(&component_id)
    }

    fn dynamic_vtable(&self, id: PacketId) -> Result<&VTable, Error> {
        self.vtable_registry
            .get(&id)
            .ok_or(Error::Impeller(metor_proto::error::Error::VTableNotFound))
    }

    /// Pushes a dynamic table whose members all exist. Returns `None`, having
    /// pushed nothing, when a member is missing.
    fn push_known_fields(
        &self,
        id: PacketId,
        buf: &[u8],
        received: Timestamp,
    ) -> Result<Option<Pushed>, Error> {
        let vtable = self.dynamic_vtable(id)?;
        let known = vtable.for_each_field::<Error>(Some(buf), &mut |rf| match self
            .components
            .contains_key(&rf.component_id)
        {
            true => Ok(()),
            false => Err(Error::ComponentNotFound(rf.component_id)),
        });
        match known {
            Err(Error::ComponentNotFound(_)) => return Ok(None),
            known => known?,
        }
        let mut pushed = Pushed::default();
        vtable.for_each_field::<Error>(Some(buf), &mut |rf| {
            let component = self
                .get_component(rf.component_id)
                .ok_or(Error::ComponentNotFound(rf.component_id))?;
            pushed.merge(push_field(component, &rf, received)?);
            Ok(())
        })?;
        Ok(Some(pushed))
    }

    /// Pushes a dynamic table, realizing each member on its first sample.
    fn realize_and_push(
        &mut self,
        id: PacketId,
        buf: &[u8],
        received: Timestamp,
        db_path: &Path,
    ) -> Result<Pushed, Error> {
        // Cloned out so realizing a member can take `&mut self`.
        let vtable = self.dynamic_vtable(id)?.clone();
        let mut pushed = Pushed::default();
        vtable.for_each_field::<Error>(Some(buf), &mut |rf| {
            let component = self.realize_component(&rf, db_path)?;
            pushed.merge(push_field(component, &rf, received)?);
            Ok(())
        })?;
        Ok(pushed)
    }

    /// Concurrent ingests of one table may cache out of push order; each
    /// table has one producer.
    fn cache_latest_table(&self, id: PacketId, buf: &[u8]) {
        if let Some(cached) = self.latest_dynamic_tables.get(&id) {
            let mut cached = cached.lock().unwrap_or_else(PoisonError::into_inner);
            cached.clear();
            cached.extend_from_slice(buf);
        }
    }

    pub fn component_metadata_iter(
        &self,
    ) -> impl Iterator<Item = (&ComponentId, &ComponentMetadata)> {
        self.component_metadata.iter()
    }

    /// Hidden components are DB-internal (derived LoD series): queryable
    /// by id, but excluded from live streams and UI listings.
    pub fn is_component_hidden(&self, component_id: &ComponentId) -> bool {
        self.component_metadata
            .get(component_id)
            .is_some_and(|m| m.is_hidden())
    }

    /// The components a fixed-rate stream packs: the visible ones, less
    /// samples past [`FIXED_RATE_MAX_SAMPLE`], such as images, which stay
    /// reachable through `RealTime` and `GetTimeSeries`.
    fn fixed_rate_components(&self) -> HashMap<ComponentId, Component> {
        self.components
            .iter()
            .filter(|(id, c)| {
                !self.is_component_hidden(id) && c.schema.size() <= FIXED_RATE_MAX_SAMPLE
            })
            .map(|(id, c)| (*id, c.clone()))
            .collect()
    }

    pub fn set_component_metadata(
        &mut self,
        metadata: ComponentMetadata,
        db_path: &Path,
    ) -> Result<(), Error> {
        let component_metadata_path = db_path.join(metadata.component_id.to_string());
        std::fs::create_dir_all(&component_metadata_path)?;
        let component_metadata_path = component_metadata_path.join("metadata");
        info!(component.name= ?metadata.name, component.id = ?metadata.component_id.0, "setting component metadata");
        metadata.write(component_metadata_path)?;
        self.component_metadata
            .insert(metadata.component_id, metadata);
        self.metadata_generation = self.metadata_generation.wrapping_add(1);
        Ok(())
    }

    /// Every recorded message log's id with its announced metadata, for
    /// pickers that enumerate the channels present in the db.
    pub fn msg_log_iter(&self) -> impl Iterator<Item = (PacketId, Option<&MsgMetadata>)> {
        self.msg_logs.iter().map(|(id, log)| (*id, log.metadata()))
    }

    pub fn get_or_insert_msg_log(
        &mut self,
        id: PacketId,
        db_path: &Path,
    ) -> Result<&mut MsgLog, Error> {
        Ok(match self.msg_logs.entry(id) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let msg_log = MsgLog::create(
                    db_path
                        .join("msgs")
                        .join(u16::from_le_bytes(id).to_string()),
                )?;
                // A new log is a metadata change: it is how a `SyncMsgs`
                // sweep learns there is another log to tail.
                self.metadata_generation = self.metadata_generation.wrapping_add(1);
                entry.insert(msg_log)
            }
        })
    }

    pub fn set_msg_metadata(
        &mut self,
        id: PacketId,
        metadata: MsgMetadata,
        db_path: &Path,
    ) -> Result<(), Error> {
        let msg_log = self.get_or_insert_msg_log(id, db_path)?;
        msg_log.set_metadata(metadata)?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ComponentSchema {
    pub prim_type: PrimType,
    pub dim: SmallVec<[usize; 4]>,
    size: usize,
}

impl Serialize for ComponentSchema {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let schema = self.to_schema();
        schema.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ComponentSchema {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let schema = Schema::<Vec<u64>>::deserialize(deserializer)?;
        Ok(schema.into())
    }
}

impl ComponentSchema {
    pub fn new(prim_type: PrimType, shape: impl Into<SmallVec<[usize; 4]>>) -> Self {
        let dim = shape.into();
        let size = dim.iter().product::<usize>() * prim_type.size();
        ComponentSchema {
            prim_type,
            dim,
            size,
        }
    }

    pub fn size(&self) -> usize {
        self.size
    }

    pub fn read(path: impl AsRef<Path>) -> Result<Self, Error> {
        let data = std::fs::read(path)?;
        Ok(postcard::from_bytes(&data)?)
    }

    fn write(&self, path: impl AsRef<Path>) -> Result<(), Error> {
        let data = postcard::to_allocvec(&self)?;
        std::fs::write(path, data)?;
        Ok(())
    }

    pub fn to_schema(&self) -> Schema<Vec<u64>> {
        Schema::new(self.prim_type, self.shape()).expect("failed to create shape")
    }

    pub fn shape(&self) -> SmallVec<[u64; 4]> {
        self.dim.iter().map(|&x| x as u64).collect()
    }

    pub fn parse_value<'a>(&'a self, buf: &'a [u8]) -> Result<(usize, ComponentView<'a>), Error> {
        let size = self.size();
        let buf = buf
            .get(..size)
            .ok_or(Error::Impeller(metor_proto::error::Error::BufferOverflow))?;
        let dim = &self.dim;
        let view = match self.prim_type {
            PrimType::U8 => ComponentView::U8(
                nox::ArrayView::from_bytes_shape_unchecked(buf, dim)
                    .ok_or(Error::Impeller(metor_proto::error::Error::BufferOverflow))?,
            ),
            PrimType::U16 => ComponentView::U16(
                nox::ArrayView::from_bytes_shape_unchecked(buf, dim)
                    .ok_or(Error::Impeller(metor_proto::error::Error::BufferOverflow))?,
            ),
            PrimType::U32 => ComponentView::U32(
                nox::ArrayView::from_bytes_shape_unchecked(buf, dim)
                    .ok_or(Error::Impeller(metor_proto::error::Error::BufferOverflow))?,
            ),
            PrimType::U64 => ComponentView::U64(
                nox::ArrayView::from_bytes_shape_unchecked(buf, dim)
                    .ok_or(Error::Impeller(metor_proto::error::Error::BufferOverflow))?,
            ),
            PrimType::I8 => ComponentView::I8(
                nox::ArrayView::from_bytes_shape_unchecked(buf, dim)
                    .ok_or(Error::Impeller(metor_proto::error::Error::BufferOverflow))?,
            ),
            PrimType::I16 => ComponentView::I16(
                nox::ArrayView::from_bytes_shape_unchecked(buf, dim)
                    .ok_or(Error::Impeller(metor_proto::error::Error::BufferOverflow))?,
            ),
            PrimType::I32 => ComponentView::I32(
                nox::ArrayView::from_bytes_shape_unchecked(buf, dim)
                    .ok_or(Error::Impeller(metor_proto::error::Error::BufferOverflow))?,
            ),
            PrimType::I64 => ComponentView::I64(
                nox::ArrayView::from_bytes_shape_unchecked(buf, dim)
                    .ok_or(Error::Impeller(metor_proto::error::Error::BufferOverflow))?,
            ),
            PrimType::Bool => ComponentView::Bool(
                nox::ArrayView::from_bytes_shape_unchecked(buf, dim)
                    .ok_or(Error::Impeller(metor_proto::error::Error::BufferOverflow))?,
            ),
            PrimType::F32 => ComponentView::F32(
                nox::ArrayView::from_bytes_shape_unchecked(buf, dim)
                    .ok_or(Error::Impeller(metor_proto::error::Error::BufferOverflow))?,
            ),
            PrimType::F64 => ComponentView::F64(
                nox::ArrayView::from_bytes_shape_unchecked(buf, dim)
                    .ok_or(Error::Impeller(metor_proto::error::Error::BufferOverflow))?,
            ),
        };
        Ok((size, view))
    }
}

impl From<Schema<Vec<u64>>> for ComponentSchema {
    fn from(value: Schema<Vec<u64>>) -> Self {
        ComponentSchema::new(value.prim_type(), value.shape())
    }
}

impl From<ComponentSchema> for Schema<Vec<u64>> {
    fn from(value: ComponentSchema) -> Self {
        value.to_schema()
    }
}

pub trait MetadataExt: Sized + Serialize + DeserializeOwned {
    fn read(path: impl AsRef<Path>) -> Result<Self, Error> {
        let data = std::fs::read(path)?;
        Ok(postcard::from_bytes(&data)?)
    }
    fn write(&self, path: impl AsRef<Path>) -> Result<(), Error> {
        let path = path.as_ref();
        let data = postcard::to_allocvec(&self)?;
        std::fs::write(path, data)?;
        Ok(())
    }
}

impl MetadataExt for EntityMetadata {}
impl MetadataExt for ComponentMetadata {}
impl MetadataExt for MsgMetadata {}

/// Dev knob: `METOR_MAX_NODE_AGE_SECS` rolls (and so seals) every
/// component's head after that much data time, instead of waiting for the
/// 32 MB node to fill. Lets manual verification see sealed spans — and
/// the LoD buckets derived from them — in seconds.
pub(crate) fn max_node_age_override() -> Option<Duration> {
    static OVERRIDE: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    OVERRIDE
        .get_or_init(|| {
            std::env::var("METOR_MAX_NODE_AGE_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
        })
        .map(Duration::from_secs)
}

/// What sinking a table under the read lock did.
enum StaticIngest {
    /// The table is static, and these samples were pushed.
    Pushed(Pushed),
    /// The table is dynamic, so it sinks through the dynamic path.
    Dynamic,
}

/// The newest timestamp pushed, and whether a push began an empty series.
#[derive(Default)]
struct Pushed {
    max_ts: Option<Timestamp>,
    new_series: bool,
}

impl Pushed {
    fn merge(&mut self, other: Pushed) {
        self.max_ts = self.max_ts.max(other.max_ts);
        self.new_series |= other.new_series;
    }
}

/// Pushes one realized field into `component`, stamped `received` when its
/// vtable names no timestamp.
fn push_field(
    component: &Component,
    rf: &RealizedField,
    received: Timestamp,
) -> Result<Pushed, Error> {
    // PANIC Safety: the callers walk a table, so every field realizes a view.
    let view = rf.view.as_ref().expect("table provided to for_each_field");
    let ts = rf.timestamp.unwrap_or(received);
    let new_series = component.time_series.is_empty();
    component.push_buf(ts, view.as_bytes())?;
    Ok(Pushed {
        max_ts: Some(ts),
        new_series,
    })
}

/// The largest sample a fixed-rate stream carries.
const FIXED_RATE_MAX_SAMPLE: usize = 32 * 1024;

/// Upper bound on a component's WAL, unless `WAL_MIN_ENTRIES` needs more.
const WAL_BUDGET: usize = 64 * 1024 * 1024;
const WAL_MIN_ENTRIES: usize = 4;
const WAL_CREATE_ENTRIES: usize = 1024;
const WAL_OPEN_ENTRIES: usize = 256;

/// WAL bytes for up to `entries` timestamped samples of `size` bytes,
/// trimmed to `WAL_BUDGET`. A multiple of the entry, so no grant wraps.
fn wal_capacity(size: usize, entries: usize) -> usize {
    let entry = size + size_of::<Timestamp>();
    entry * (WAL_BUDGET / entry).max(WAL_MIN_ENTRIES).min(entries)
}

#[derive(Clone)]
pub struct Component {
    pub component_id: ComponentId,
    pub time_series: TimeSeries,
    pub wal: Disruptor,
    pub schema: ComponentSchema,
    pub last_timestamp: Arc<AtomicCell<Timestamp>>,
    persistence: Arc<std::sync::Mutex<Option<ComponentPersistence>>>,
    /// Samples dropped because the WAL was full, to rate-limit the warning.
    wal_dropped: Arc<AtomicU64>,
}

struct ComponentPersistence {
    reader: disruptor::Reader,
    writer: Option<time_series::TimerSeriesWriter>,
    engine_owned: bool,
}

impl Component {
    /// Wrap existing storage for read-only consumers without starting an executor.
    /// Use `create` or `open` when the component needs live WAL persistence.
    pub fn from_time_series(
        component_id: ComponentId,
        schema: ComponentSchema,
        time_series: TimeSeries,
    ) -> Self {
        let last_timestamp = time_series
            .latest()
            .map(|t| t.timestamp())
            .unwrap_or(Timestamp(i64::MIN));
        Self {
            component_id,
            time_series,
            wal: Disruptor::new(wal_capacity(schema.size(), WAL_CREATE_ENTRIES)),
            schema,
            last_timestamp: Arc::new(AtomicCell::new(last_timestamp)),
            persistence: Default::default(),
            wal_dropped: Default::default(),
        }
    }

    /// `engine_owned` marks a component whose writer belongs to the LoD
    /// engine (it emits buckets directly); its `persist` runs in drain mode
    /// so it never races [`lod`]'s `setup_levels` for the writer claim.
    pub fn create(
        db_path: &Path,
        component_id: ComponentId,
        schema: ComponentSchema,
        engine_owned: bool,
    ) -> Result<Self, Error> {
        let component_path = db_path.join(component_id.to_string());
        std::fs::create_dir_all(&component_path)?;
        let component_schema_path = component_path.join("schema");
        if !component_schema_path.exists() {
            schema.write(component_schema_path)?;
        }
        let time_series = TimeSeries::create(component_path.clone())?;
        if let Some(age) = max_node_age_override() {
            time_series.set_max_node_age(age);
        }
        let this = Component {
            wal: Disruptor::new(wal_capacity(schema.size(), WAL_CREATE_ENTRIES)),
            component_id,
            time_series,
            schema,
            last_timestamp: Arc::new(AtomicCell::new(Timestamp(i64::MIN))),
            persistence: Default::default(),
            wal_dropped: Default::default(),
        };
        stellarator::spawn(this.clone().persist(engine_owned));
        stellarator::spawn(this.time_series.clone().lifecycle());
        Ok(this)
    }

    pub fn open(
        path: impl AsRef<Path>,
        component_id: ComponentId,
        schema: ComponentSchema,
        engine_owned: bool,
    ) -> Result<Self, Error> {
        let time_series = TimeSeries::open(path)?;
        if let Some(age) = max_node_age_override() {
            time_series.set_max_node_age(age);
        }

        let last_timestamp = time_series
            .latest()
            .map(|t| t.timestamp())
            .unwrap_or(Timestamp(i64::MIN));
        let this = Component {
            wal: Disruptor::new(wal_capacity(schema.size(), WAL_OPEN_ENTRIES)),
            component_id,
            time_series,
            schema,
            last_timestamp: Arc::new(AtomicCell::new(last_timestamp)),
            persistence: Default::default(),
            wal_dropped: Default::default(),
        };
        stellarator::spawn(this.persist(engine_owned));
        stellarator::spawn(this.time_series.clone().lifecycle());
        Ok(this)
    }

    pub fn persist(&self, engine_owned: bool) -> impl Future<Output = ()> + 'static {
        self.persistence
            .lock()
            .unwrap()
            .get_or_insert_with(|| ComponentPersistence {
                reader: self.wal.reader(),
                writer: None,
                engine_owned,
            });
        // This reader only wakes the task. The durable reader and writer stay
        // with the component when its executor is stopped, for DB::flush.
        let mut notify = self.wal.reader();
        let component = self.clone();
        async move {
            loop {
                if let Err(err) = component.flush_pending() {
                    tracing::error!(?err, "failed to persist wal samples");
                }
                drop(notify.next().await);
            }
        }
    }

    fn flush_pending(&self) -> Result<(), Error> {
        let mut persistence = self.persistence.lock().unwrap();
        let Some(ComponentPersistence {
            reader,
            writer,
            engine_owned,
        }) = persistence.as_mut()
        else {
            return Ok(());
        };
        let msg_size = self.schema.size() + size_of::<Timestamp>();
        while let Some(buf) = reader.try_next() {
            // LoD writes directly to its time series; its WAL is drain-only.
            if *engine_owned {
                warn!("wal traffic for engine-owned component; dropping samples");
                continue;
            }
            if writer.is_none() {
                *writer = self.time_series.writer();
            }
            let writer = writer.as_mut().ok_or_else(|| {
                Error::Io(std::io::Error::other("component writer owned elsewhere"))
            })?;
            for msg in buf.chunks_exact(msg_size) {
                let timestamp = Timestamp(i64::from_le_bytes(
                    msg[..size_of::<Timestamp>()].try_into().unwrap(),
                ));
                writer.push_buf(timestamp, &msg[size_of::<Timestamp>()..])?;
            }
        }
        Ok(())
    }

    fn as_vtable_op(&self) -> Arc<OpBuilder> {
        schema(
            self.schema.prim_type,
            &self.schema.shape(),
            component(self.component_id),
        )
    }

    /// Logs the 1st, 2nd, 4th, 8th, ... dropped sample.
    fn warn_wal_drop(&self, timestamp: Timestamp) {
        let dropped = self.wal_dropped.fetch_add(1, atomic::Ordering::Relaxed) + 1;
        if dropped.is_power_of_two() {
            let reader_count = self.wal.reader_count();
            warn!(component.id = ?self.component_id.0, ?timestamp, ?reader_count, dropped, "wal full; dropped sample");
        }
    }

    pub fn push_buf(&self, timestamp: Timestamp, value_buf: &[u8]) -> Result<(), Error> {
        if timestamp < self.last_timestamp.latest() {
            return Err(Error::TimeTravel);
        }
        let Ok(mut grant) = self.wal.try_grant(value_buf.len() + size_of::<Timestamp>()) else {
            self.warn_wal_drop(timestamp);
            return Ok(());
        };
        grant[..size_of::<Timestamp>()].copy_from_slice(timestamp.as_bytes());
        grant[size_of::<Timestamp>()..].copy_from_slice(value_buf);
        drop(grant);
        self.last_timestamp.update_max(timestamp);
        Ok(())
    }
}

pub struct Server {
    pub listener: TcpListener,
    pub db: Arc<DB>,
}

impl Server {
    pub fn new(path: impl AsRef<Path>, addr: SocketAddr) -> Result<Server, Error> {
        info!(?addr, "listening");
        let listener = TcpListener::bind(addr)?;
        Server::from_listener(listener, path)
    }

    pub fn from_listener(listener: TcpListener, path: impl AsRef<Path>) -> Result<Server, Error> {
        let path = path.as_ref().to_path_buf();
        let db = if path.exists() {
            DB::open(path)?
        } else {
            DB::create(path)?
        };
        let db = Arc::new(db);
        Ok(Server { listener, db })
    }

    pub async fn run(self) -> Result<(), Error> {
        let Self { listener, db } = self;
        let addr = listener.local_addr()?;
        let udp_db = db.clone();
        stellarator::struc_con::stellar(move || Self::handle_udp(addr, udp_db));
        loop {
            let stream = listener.accept().await?;
            let peer_addr = stream.peer_addr()?;
            trace!(?peer_addr, "accepted connection");
            let conn_db = db.clone();
            stellarator::struc_con::stellar(move || handle_conn(stream, conn_db));
        }
    }

    pub async fn handle_udp(addr: SocketAddr, db: Arc<DB>) -> Result<(), Error> {
        let socket = UdpSocket::bind(addr)?;
        let (rx, tx) = socket.split();
        let rx = PacketStream::new(rx);
        let tx = Arc::new(Mutex::new(PacketSink::new(tx)));
        handle_conn_inner(tx, rx, db).await?;
        Ok(())
    }
}

pub async fn serve_tmp_db(addr: SocketAddr) -> Result<(), Error> {
    let path = std::env::temp_dir().join(format!("metor_db_{}", fastrand::u64(..)));
    let server = Server::new(path, addr)?;
    server.run().await
}

pub async fn handle_conn(stream: TcpStream, db: Arc<DB>) {
    let (rx, tx) = stream.split();
    let rx = PacketStream::new(rx);
    let tx = Arc::new(Mutex::new(PacketSink::new(tx)));
    match handle_conn_inner(tx, rx, db).await {
        Ok(_) => {}
        Err(err) if err.is_stream_closed() => {}
        Err(err) => {
            warn!(?err, "error handling stream")
        }
    }
}

/// Per-connection protocol state. Node uploads span several packets
/// (offer, chunks, done), so the in-flight staging lives here rather than
/// in the DB.
#[derive(Default)]
struct ConnState {
    incoming_node: Option<IncomingNode>,
}

struct IncomingNode {
    component_id: ComponentId,
    seal: SealRecord,
    staging: store::NodeStaging,
}

async fn handle_conn_inner<A: AsyncRead + AsyncWrite + 'static>(
    mut tx: Arc<Mutex<PacketSink<OwnedWriter<A>>>>,
    mut rx: PacketStream<OwnedReader<A>>,
    db: Arc<DB>,
) -> Result<(), Error> {
    let mut buf = vec![0u8; 1024 * 1024 * 1024];
    let mut resp_pkt = LenPacket::new(PacketTy::Msg, [0, 0], 1024 * 1024 * 1024);
    let mut conn = ConnState::default();
    loop {
        let pkt = rx.next(buf).await?;
        let req_id = pkt.req_id();
        let mut pkt_tx = PacketTx {
            req_id,
            tx,
            pkt: Some(resp_pkt),
        };
        let result = handle_packet(&pkt, &db, &mut pkt_tx, &mut conn).await;
        buf = pkt.into_buf().into_inner();
        match result {
            Ok(_) => {}
            Err(err) if err.is_stream_closed() => {}
            Err(err) => {
                warn!(?err, "error handling packet");
                if let Err(err) = pkt_tx
                    .send_msg(&ErrorResponse {
                        description: err.to_string(),
                    })
                    .await
                {
                    warn!(?err, "error sending err resp");
                }
            }
        }
        resp_pkt = pkt_tx.pkt.expect("len pkt taken and not given back");
        tx = pkt_tx.tx;
    }
}

pub struct PacketTx<A: AsyncWrite + 'static> {
    pub(crate) req_id: RequestId,
    pub(crate) tx: Arc<Mutex<PacketSink<OwnedWriter<A>>>>,
    pub(crate) pkt: Option<LenPacket>,
}

impl<A: AsyncWrite + 'static> Clone for PacketTx<A> {
    fn clone(&self) -> Self {
        Self {
            req_id: self.req_id,
            tx: self.tx.clone(),
            pkt: self.pkt.clone(),
        }
    }
}

impl<A: AsyncWrite + 'static> PacketTx<A> {
    pub async fn send_msg<M: Msg>(&mut self, msg: &M) -> Result<(), Error> {
        let req_id = self.req_id;
        self.send_with_builder(|pkt| {
            let header = PacketHeader {
                packet_ty: metor_proto::types::PacketTy::Msg,
                id: M::ID,
                req_id,
            };
            pkt.as_mut_packet().header = header;
            pkt.clear();
            postcard::serialize_with_flavor(&msg, pkt).map_err(Error::from)
        })
        .await
    }

    pub async fn send_with_builder(
        &mut self,
        builder: impl FnOnce(&mut LenPacket) -> Result<(), Error> + '_,
    ) -> Result<(), Error> {
        let mut pkt = self.pkt.take().expect("missing len pkt");
        pkt.clear();
        if let Err(err) = builder(&mut pkt) {
            self.pkt = Some(pkt);
            return Err(err);
        }
        pkt.as_mut_packet().header.req_id = self.req_id;
        let tx = self.tx.lock().await;
        let res = rent!(tx.send(pkt).await, pkt);
        self.pkt = Some(pkt);
        res.map_err(Error::from)
    }

    pub async fn send_time_series(
        &mut self,
        id: PacketId,
        timestamps: &[Timestamp],
        data: &[u8],
    ) -> Result<(), Error> {
        let req_id = self.req_id;
        self.send_with_builder(|pkt| {
            let header = PacketHeader {
                packet_ty: metor_proto::types::PacketTy::TimeSeries,
                id,
                req_id,
            };
            pkt.as_mut_packet().header = header;
            pkt.clear();
            pkt.extend_from_slice(&(timestamps.len() as u64).to_le_bytes());
            pkt.extend_from_slice(timestamps.as_bytes());
            pkt.extend_from_slice(data);
            Ok(())
        })
        .await
    }
}

/// Upper bound on a `GetTimeSeries` response, header included.
const SERIES_BUDGET: usize = 256 * 1024 * 1024;
/// A time series packet's bytes before its timestamps: header and count.
const SERIES_HEAD: usize = size_of::<PacketHeader>() + size_of::<u64>();

/// Samples a `GetTimeSeries` response carries: the oldest of `range_len`,
/// at most `limit`, and no more than fit `SERIES_BUDGET`.
fn series_len(range_len: usize, limit: usize, size: usize) -> Result<usize, Error> {
    let entry = size.saturating_add(size_of::<Timestamp>());
    match (SERIES_BUDGET - SERIES_HEAD) / entry {
        0 => Err(Error::ComponentTooLarge(size)),
        fit => Ok(range_len.min(limit).min(fit)),
    }
}

/// Writes the oldest `len` samples of `range` into `pkt`, after skipping
/// its newest `skip`. Slices arrive newest first, so each fills the
/// destination from the back.
fn write_series(
    pkt: &mut LenPacket,
    range: &time_series::TimeSeriesSlice,
    mut skip: usize,
    len: usize,
    size: usize,
) -> Result<(), Error> {
    let stamps_len = len * size_of::<Timestamp>();
    let body_len = stamps_len + len * size;
    let pkt_len =
        u32::try_from(SERIES_HEAD + body_len).map_err(|_| Error::ComponentTooLarge(size))?;
    pkt.extend_from_slice((len as u64).as_bytes());
    pkt.inner.resize(pkt.inner.len() + body_len, 0);
    pkt.inner[..size_of::<u32>()].copy_from_slice(&pkt_len.to_le_bytes());
    let body = &mut pkt.as_mut_packet().body[size_of::<u64>()..];
    let (stamps, data) = body.split_at_mut(stamps_len);
    let mut end = len;
    for slice in range.as_iter() {
        if end == 0 {
            break;
        }
        let all = slice.timestamps();
        let skipped = skip.min(all.len());
        skip -= skipped;
        let kept = (all.len() - skipped).min(end);
        let from = all.len() - skipped - kept;
        let start = end - kept;
        let src = slice
            .data()
            .get(from * size..(from + kept) * size)
            .ok_or(Error::SeriesTruncated)?;
        stamps[start * size_of::<Timestamp>()..end * size_of::<Timestamp>()]
            .copy_from_slice(all[from..from + kept].as_bytes());
        data[start * size..end * size].copy_from_slice(src);
        end = start;
    }
    match end {
        0 => Ok(()),
        _ => Err(Error::SeriesTruncated),
    }
}

/// Moves a page's end of `len` (of `range_len`, oldest first) so that no
/// timestamp is split across pages, since the next page starts after the
/// last timestamp. The page shrinks back to the last timestamp change, or,
/// if it is one tie, grows to hold the whole tie: up to `cap` samples, past
/// any limit. `None` if that tie exceeds `cap`.
fn whole_ties(
    range: &time_series::TimeSeriesSlice,
    range_len: usize,
    len: usize,
    cap: usize,
) -> Option<usize> {
    let skip = range_len - len;
    if skip == 0 || len == 0 {
        return Some(len);
    }
    // Newest first, `skip` excluded samples come before the `len` kept.
    // Slices wholly excluded are passed by length; `newer` carries the
    // newest-passed slice's first stamp and how many equal it, so a tie
    // spanning slices is counted whole.
    let mut position = 0;
    let mut newer: Option<(Timestamp, usize)> = None;
    let mut edge = None;
    let (mut excluded_tie, mut kept_tie) = (0, 0);
    for slice in range.as_iter() {
        let stamps = slice.timestamps();
        let n = stamps.len();
        let newer_position = position;
        position += n;
        match edge {
            None if position < skip => {
                let Some(&first) = stamps.first() else {
                    continue;
                };
                let lead = stamps.partition_point(|t| *t == first);
                newer = match newer {
                    Some((stamp, run)) if lead == n && stamp == first => Some((first, n + run)),
                    _ => Some((first, lead)),
                };
            }
            None => {
                // The excluded edge, `skip - 1` positions from the newest.
                let at = n - 1 - (skip - 1 - newer_position);
                let stamp = stamps[at];
                excluded_tie = stamps[at..].partition_point(|t| *t == stamp);
                if let Some((first, run)) = newer
                    && at + excluded_tie == n
                    && first == stamp
                {
                    excluded_tie += run;
                }
                let older = stamps[..at].partition_point(|t| *t < stamp);
                kept_tie = at - older;
                edge = Some(stamp);
                if older > 0 {
                    break;
                }
            }
            Some(stamp) => {
                let older = stamps.partition_point(|t| *t < stamp);
                kept_tie += n - older;
                if older > 0 {
                    break;
                }
            }
        }
    }
    let kept_tie = kept_tie.min(len);
    match kept_tie < len {
        true => Some(len - kept_tie),
        false => Some(len + excluded_tie).filter(|whole| *whole <= cap),
    }
}

async fn handle_packet<A: AsyncWrite + 'static>(
    pkt: &Packet<Slice<Vec<u8>>>,
    db: &Arc<DB>,
    tx: &mut PacketTx<A>,
    conn: &mut ConnState,
) -> Result<(), Error> {
    trace!(?pkt, "handling pkt");
    match &pkt {
        Packet::Msg(m) if m.id == VTableMsg::ID => {
            let vtable = m.parse::<VTableMsg>()?;
            db.insert_vtable(vtable)?;
        }
        Packet::Msg(m) if m.id == UdpUnicast::ID => {
            let udp_broadcast = m.parse::<UdpUnicast>()?;
            let db = db.clone();
            let addr = udp_broadcast
                .addr
                .to_socket_addrs()?
                .next()
                .ok_or_else(|| {
                    Error::Io(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "missing socket ip",
                    ))
                })?;
            handle_unicast_stream(addr, udp_broadcast.stream, db);
        }
        Packet::Msg(m) if m.id == Stream::ID => {
            let stream = m.parse::<Stream>()?;
            let tx = tx.tx.clone();
            let db = db.clone();
            handle_stream(tx, stream, db, m.req_id);
        }
        Packet::Msg(m) if m.id == SetStreamState::ID => {
            let set_stream_state = m.parse::<SetStreamState>()?;
            let stream_id = set_stream_state.id;
            db.with_state(|s| {
                let Some(state) = s.streams.get(&stream_id) else {
                    return Err(Error::StreamNotFound(stream_id));
                };
                debug!(msg = ?set_stream_state, "set_stream_state received");
                if let Some(playing) = set_stream_state.playing {
                    state.set_playing(playing);
                }
                if let Some(timestamp) = set_stream_state.timestamp {
                    state.set_timestamp(timestamp);
                }
                if let Some(frequency) = set_stream_state.frequency {
                    state.set_frequency(frequency);
                }
                Ok(())
            })?;
        }
        Packet::Msg(m) if m.id == GetSchema::ID => {
            let get_schema = m.parse::<GetSchema>()?;
            let schema = db.with_state(|state| {
                state
                    .components
                    .iter()
                    .filter(|(component_id, _)| *component_id == &get_schema.component_id)
                    .map(|(_, component)| component.schema.to_schema())
                    .next()
                    .ok_or(Error::ComponentNotFound(get_schema.component_id))
            })?;
            tx.send_msg(&SchemaMsg(schema)).await?;
        }
        Packet::Msg(m) if m.id == GetTimeSeries::ID => {
            let get_time_series = m.parse::<GetTimeSeries>()?;
            let GetTimeSeries {
                component_id,
                range,
                limit,
                id,
            } = get_time_series;
            let component = db.with_state(|state| {
                let Some(component) = state.components.get(&component_id) else {
                    return Err(Error::ComponentNotFound(component_id));
                };
                Ok(component.clone())
            })?;

            let req_id = tx.req_id;
            tx.send_with_builder(move |pkt| {
                let header = PacketHeader {
                    packet_ty: metor_proto::types::PacketTy::TimeSeries,
                    id,
                    req_id,
                };
                pkt.as_mut_packet().header = header;
                pkt.clear();
                let size = component.schema.size();
                let range = component
                    .time_series
                    .get_range(range)
                    .ok_or(Error::TimeRangeOutOfBounds)?;
                let range_len = range.len();
                let len = series_len(range_len, limit.unwrap_or(usize::MAX), size)?;
                let len = whole_ties(
                    &range,
                    range_len,
                    len,
                    series_len(usize::MAX, usize::MAX, size)?,
                )
                .ok_or(Error::ComponentTooLarge(size))?;
                write_series(pkt, &range, range_len - len, len, size)
            })
            .await?;
        }
        Packet::Msg(m) if m.id == SetComponentMetadata::ID => {
            let SetComponentMetadata(metadata) = m.parse::<SetComponentMetadata>()?;
            db.with_state_mut(|state| state.set_component_metadata(metadata, &db.path))?;
        }
        Packet::Msg(m) if m.id == GetComponentMetadata::ID => {
            let GetComponentMetadata { component_id } = m.parse::<GetComponentMetadata>()?;

            tx.send_with_builder(|pkt| {
                let header = PacketHeader {
                    packet_ty: metor_proto::types::PacketTy::Msg,
                    id: ComponentMetadata::ID,
                    req_id: m.req_id,
                };
                pkt.as_mut_packet().header = header;
                pkt.clear();
                db.with_state(|state| {
                    let Some(metadata) = state.component_metadata.get(&component_id) else {
                        return Err(Error::ComponentNotFound(component_id));
                    };
                    postcard::serialize_with_flavor(&metadata, pkt).map_err(Error::from)
                })
            })
            .await?;
        }

        Packet::Msg(m) if m.id == DumpMetadata::ID => {
            let msg = db.with_state(|state| {
                let component_metadata = state.component_metadata.values().cloned().collect();

                let msg_metadata = state
                    .msg_logs
                    .values()
                    .flat_map(|m| m.metadata())
                    .cloned()
                    .collect();
                DumpMetadataResp {
                    component_metadata,
                    msg_metadata,
                    db_config: state.db_config.clone(),
                }
            });
            tx.send_msg(&msg).await?;
        }
        Packet::Msg(m) if m.id == DumpSchema::ID => {
            let msg = db.with_state(|state| {
                let schemas = state
                    .components
                    .values()
                    .map(|c| (c.component_id, c.schema.to_schema()))
                    .collect();
                DumpSchemaResp { schemas }
            });

            tx.send_msg(&msg).await?;
        }

        Packet::Msg(m) if m.id == SubscribeLastUpdated::ID => {
            let mut tx = tx.clone();
            let db = db.clone();
            stellarator::spawn(async move {
                loop {
                    let last_updated = db.last_updated.latest();
                    {
                        match tx.send_msg(&LastUpdated(last_updated)).await {
                            Err(err) if err.is_stream_closed() => {}
                            Err(err) => {
                                warn!(?err, "failed to send packet");
                                return;
                            }
                            _ => (),
                        }
                    }
                    db.last_updated.wait_for(|time| time > last_updated).await;
                }
            });
        }
        Packet::Msg(m) if m.id == SetDbConfig::ID => {
            let SetDbConfig {
                recording,
                metadata,
            } = m.parse::<SetDbConfig>()?;
            if let Some(recording) = recording {
                db.with_state_mut(|s| {
                    s.db_config.recording = recording;
                });
                db.recording_cell.set_playing(recording);
            }
            db.with_state_mut(|s| {
                s.db_config.metadata.extend(metadata);
            });
            db.save_db_state()?;
            tx.send_msg(&db.db_config()).await?;
        }
        Packet::Msg(m) if m.id == GetEarliestTimestamp::ID => {
            tx.send_msg(&EarliestTimestamp(db.earliest_timestamp.latest()))
                .await?;
        }
        Packet::Msg(m) if m.id == GetDbSettings::ID => {
            let settings = db.db_config();
            tx.send_msg(&settings).await?;
        }
        Packet::Table(table) => {
            trace!(table.len = table.buf.len(), "sinking table");
            db.ingest_table(table.id, stellarator::buf::deref(&table.buf))?;
        }

        Packet::Msg(m) if m.id == GetDbSettings::ID => {
            let settings = db.db_config();
            tx.send_msg(&settings).await?;
        }
        Packet::Msg(m) if m.id == SQLQuery::ID => {
            let SQLQuery(query) = m.parse::<SQLQuery>()?;
            let db = db.clone();
            let (tokio_tx, rx) = thingbuf::mpsc::channel::<Vec<u8>>(4);
            let res = stellarator::struc_con::tokio(move |_| async move {
                let mut ctx = db.as_session_context()?;
                db.insert_views(&mut ctx).await?;
                let df = ctx.sql(&query).await?;
                let mut stream = df.execute_stream().await?;

                while let Some(batch) = stream.next().await {
                    let batch = batch?;
                    let mut buf = vec![];
                    let mut writer =
                        ::arrow::ipc::writer::StreamWriter::try_new(&mut buf, batch.schema_ref())?;
                    writer.write(&batch)?;
                    writer.finish()?;
                    let _ = tokio_tx.send(buf).await;
                }
                Ok::<_, Error>(())
            })
            .join();
            while let Some(batch) = rx.recv().await {
                tx.send_msg(&ArrowIPC {
                    batch: Some(Cow::Owned(batch)),
                })
                .await?;
            }
            res.await??;
            tx.send_msg(&ArrowIPC { batch: None }).await?;
        }
        Packet::Msg(m) if m.id == SetMsgMetadata::ID => {
            let SetMsgMetadata { id, metadata } = m.parse::<SetMsgMetadata>()?;
            db.with_state_mut(|s| s.set_msg_metadata(id, metadata, &db.path))?;
        }
        Packet::Msg(m) if m.id == MsgStream::ID => {
            let MsgStream { msg_id } = m.parse::<MsgStream>()?;
            let msg_log =
                db.with_state_mut(|s| s.get_or_insert_msg_log(msg_id, &db.path).cloned())?;
            let req_id = m.req_id;
            stellarator::spawn(handle_msg_stream(msg_id, req_id, msg_log, tx.tx.clone()));
        }
        Packet::Msg(m) if m.id == SyncMsgs::ID => {
            stellarator::spawn(handle_msg_sync(tx.tx.clone(), db.clone(), m.req_id));
        }
        Packet::Msg(m) if m.id == FixedRateMsgStream::ID => {
            let FixedRateMsgStream { msg_id, fixed_rate } = m.parse::<FixedRateMsgStream>()?;
            let msg_log =
                db.with_state_mut(|s| s.get_or_insert_msg_log(msg_id, &db.path).cloned())?;
            let stream_state =
                db.get_or_insert_fixed_rate_state(fixed_rate.stream_id, fixed_rate.behavior);
            stellarator::spawn(handle_fixed_rate_msg_stream(
                msg_id,
                m.req_id,
                msg_log,
                tx.tx.clone(),
                stream_state,
            ));
        }
        Packet::Msg(m) if m.id == GetMsgMetadata::ID => {
            let GetMsgMetadata { msg_id } = m.parse::<GetMsgMetadata>()?;
            let Some(metadata) =
                db.with_state(|s| s.msg_logs.get(&msg_id).and_then(|m| m.metadata().cloned()))
            else {
                return Err(Error::MsgNotFound(msg_id));
            };
            tx.send_msg(&metadata).await?;
        }
        Packet::Msg(m) if m.id == GetMsgs::ID => {
            let GetMsgs {
                msg_id,
                range,
                limit,
            } = m.parse::<GetMsgs>()?;
            let msg_log = db.with_state_mut(|s| {
                s.msg_logs
                    .get(&msg_id)
                    .ok_or(Error::MsgNotFound(msg_id))
                    .cloned()
            })?;
            let slice = msg_log.get_range(range);
            let data = if let Some(slice) = slice {
                // Node order is newest-first; reverse so the batch reads
                // chronologically and `limit` keeps the oldest-first prefix.
                let node_slices: Vec<_> = slice.as_iter().collect();
                let mut collected = Vec::new();
                'outer: for node_slice in node_slices.iter().rev() {
                    for (timestamp, msg) in node_slice.msgs() {
                        if let Some(limit) = limit
                            && collected.len() >= limit
                        {
                            break 'outer;
                        }
                        collected.push((timestamp, msg.to_vec()));
                    }
                }
                collected
            } else {
                Vec::new()
            };
            tx.send_msg(&MsgBatch { data }).await?;
        }
        Packet::Msg(m) if m.id == SaveArchive::ID => {
            let SaveArchive { path, format } = m.parse()?;
            db.save_archive(&path, format)?;
            tx.send_msg(&ArchiveSaved { path }).await?;
        }
        Packet::Msg(m) if m.id == VTableStream::ID => {
            let VTableStream { id } = m.parse::<VTableStream>()?;
            let vtable = db
                .with_state(|state| state.vtable_registry.get(&id).cloned())
                .ok_or(Error::InvalidMsgId)?;
            stellarator::spawn(handle_vtable_stream(
                id,
                vtable,
                db.clone(),
                tx.tx.clone(),
                m.req_id,
            ));
        }
        Packet::Msg(m) if m.id == UdpVTableStream::ID => {
            let UdpVTableStream { id, addr } = m.parse::<UdpVTableStream>()?;
            let addr = addr.to_socket_addrs()?.next().ok_or_else(|| {
                Error::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "missing socket ip",
                ))
            })?;
            db.with_state_mut(|state| {
                // ensure idempotency
                if state.udp_vtable_streams.contains(&(addr, id)) {
                    return Ok(());
                }
                let vtable = state
                    .vtable_registry
                    .get(&id)
                    .ok_or(Error::InvalidMsgId)?
                    .clone();
                let db = db.clone();
                let req_id = m.req_id;
                stellarator::struc_con::stellar(move || async move {
                    info!("VTable streaming to udp://{}", addr);
                    let mut socket = UdpSocket::ephemeral()?;
                    socket.connect(addr);
                    let (_, tx) = socket.split();
                    let tx = Arc::new(Mutex::new(PacketSink::new(tx)));
                    handle_vtable_stream(id, vtable, db, tx, req_id).await
                });
                state.udp_vtable_streams.insert((addr, id));
                Ok::<_, Error>(())
            })?;
        }
        Packet::Msg(m) if m.id == GetDbInfo::ID => {
            let (command_ids, namespace) =
                db.with_state(|s| (s.command_ids.clone(), s.namespace.clone()));
            tx.send_msg(&DbInfoResp {
                protocol_version: NODE_PROTOCOL_VERSION,
                features: FEATURE_MSG_SYNC,
                command_ids,
                namespace,
            })
            .await?;
        }
        Packet::Msg(m) if m.id == GetNodeManifest::ID => {
            let GetNodeManifest { component_id } = m.parse::<GetNodeManifest>()?;
            // Unknown components list empty rather than erroring: to a
            // syncing peer "never heard of it" and "no sealed nodes yet"
            // call for the same response.
            let component = db.with_state(|state| state.components.get(&component_id).cloned());
            let nodes: Vec<SealRecord> = component
                .map(|component| {
                    component
                        .time_series
                        .manifest()
                        .spans
                        .iter()
                        .filter(|s| s.state == manifest::SpanState::Resident)
                        .map(|s| s.seal)
                        .collect()
                })
                .unwrap_or_default();
            tx.send_msg(&NodeManifestResp {
                component_id,
                nodes,
            })
            .await?;
        }
        Packet::Msg(m) if m.id == FetchNode::ID => {
            let FetchNode {
                component_id,
                start_ts,
                chunk_bytes,
            } = m.parse::<FetchNode>()?;
            let component = db.with_state(|state| {
                state
                    .components
                    .get(&component_id)
                    .cloned()
                    .ok_or(Error::ComponentNotFound(component_id))
            })?;
            let manifest = component.time_series.manifest();
            let seal = manifest
                .span(start_ts)
                .filter(|s| s.state == manifest::SpanState::Resident)
                .map(|s| s.seal)
                .ok_or(Error::TimeRangeOutOfBounds)?;
            let node = component
                .time_series
                .list
                .iter()
                .find(|n| n.timestamps().first() == Some(&start_ts))
                .ok_or(Error::TimeRangeOutOfBounds)?;
            let sealed = store::SealedNode::from_node(&node, seal)
                .ok_or(Error::Store(store::StoreError::ChecksumMismatch))?;
            let chunk_bytes = (chunk_bytes as usize).clamp(4 * 1024, 1024 * 1024);
            for chunk in sealed.chunks(chunk_bytes) {
                tx.send_msg(&NodeChunk {
                    component_id,
                    start_ts,
                    file: chunk.file,
                    offset: chunk.offset,
                    payload: chunk.bytes.to_vec(),
                })
                .await?;
            }
            tx.send_msg(&FetchNodeDone { seal }).await?;
        }
        Packet::Msg(m) if m.id == OfferNode::ID => {
            let OfferNode {
                component_id,
                component_name,
                schema,
                seal,
            } = m.parse::<OfferNode>()?;
            let schema = ComponentSchema::from(schema);
            if schema.size() as u64 != seal.element_size {
                return Err(Error::SchemaMismatch);
            }
            db.with_state_mut(|state| {
                let is_new = state.get_component(component_id).is_none();
                state.insert_component(component_id, schema, &db.path)?;
                // A freshly auto-created component otherwise keeps the
                // numeric-id placeholder `insert_component` defaults to;
                // adopt the human name the peer already put on the wire.
                if is_new
                    && !component_name.is_empty()
                    && let Some(meta) = state.get_component_metadata(component_id).cloned()
                {
                    state.set_component_metadata(
                        ComponentMetadata {
                            name: component_name,
                            ..meta
                        },
                        &db.path,
                    )?;
                }
                Ok::<(), Error>(())
            })?;
            let component = db.with_state(|state| {
                state
                    .components
                    .get(&component_id)
                    .cloned()
                    .ok_or(Error::ComponentNotFound(component_id))
            })?;
            let already_have = component
                .time_series
                .manifest()
                .span(seal.start_ts)
                .is_some_and(|s| {
                    s.state == manifest::SpanState::Resident && s.seal.checksum == seal.checksum
                });
            if already_have {
                conn.incoming_node = None;
                tx.send_msg(&OfferNodeResp {
                    accept: false,
                    already_have: true,
                })
                .await?;
            } else {
                let staging =
                    store::NodeStaging::create(&db.path.join(component_id.to_string()), &seal)?;
                conn.incoming_node = Some(IncomingNode {
                    component_id,
                    seal,
                    staging,
                });
                tx.send_msg(&OfferNodeResp {
                    accept: true,
                    already_have: false,
                })
                .await?;
            }
        }
        Packet::Msg(m) if m.id == NodeChunk::ID => {
            let chunk = m.parse::<NodeChunk>()?;
            let incoming = conn
                .incoming_node
                .as_ref()
                .filter(|i| {
                    i.component_id == chunk.component_id && i.seal.start_ts == chunk.start_ts
                })
                .ok_or(Error::BadMessage)?;
            incoming.staging.append(&store::SealedChunk {
                file: chunk.file,
                offset: chunk.offset,
                bytes: &chunk.payload,
            })?;
        }
        Packet::Msg(m) if m.id == PushNodeDone::ID => {
            let done = m.parse::<PushNodeDone>()?;
            let incoming = conn
                .incoming_node
                .take()
                .filter(|i| {
                    i.component_id == done.component_id
                        && i.seal.start_ts == done.start_ts
                        && i.seal.checksum == done.checksum
                })
                .ok_or(Error::BadMessage)?;
            let component = db.with_state(|state| {
                state
                    .components
                    .get(&incoming.component_id)
                    .cloned()
                    .ok_or(Error::ComponentNotFound(incoming.component_id))
            })?;
            let installed = component.time_series.install_node(
                incoming.staging,
                &incoming.seal,
                manifest::SpanSource::LocalIngest,
            );
            if let Err(err) = &installed {
                warn!(?err, component_id = ?incoming.component_id, "failed to install offered node");
            } else {
                db.earliest_timestamp.update_min(incoming.seal.start_ts);
                db.last_updated.update_max(incoming.seal.end_ts);
            }
            tx.send_msg(&NodeAck {
                component_id: done.component_id,
                start_ts: done.start_ts,
                checksum: done.checksum,
                durable: installed.is_ok(),
            })
            .await?;
        }
        // Node-transfer protocol messages never carry telemetry: one that
        // no arm above matched is a stray reply, not a message to record.
        // Every other unmatched message (alarms, sequences, …) MUST fall
        // through — recording it in the msg log is the pub/sub mechanism
        // its views read from.
        Packet::Msg(m) if NODE_PROTOCOL_MESSAGES.contains(&m.id) => {
            warn!(id = ?m.id, "ignoring stray node-protocol message");
        }
        Packet::Msg(m) => {
            let timestamp = m.timestamp.unwrap_or(Timestamp::now());
            db.push_msg(timestamp, m.id, &m.buf)?
        }
        _ => {}
    }
    Ok(())
}

pub async fn handle_msg_stream<A: AsyncWrite>(
    msg_id: PacketId,
    req_id: RequestId,
    msg_log: MsgLog,
    tx: Arc<Mutex<PacketSink<A>>>,
) -> Result<(), Error> {
    let mut pkt = LenPacket::msg(msg_id, 64).with_request_id(req_id);
    loop {
        msg_log.wait().await;
        let Some(msg_ref) = msg_log.latest() else {
            continue;
        };
        let tx = tx.lock().await;
        pkt.clear();
        let Some(msg) = msg_ref.data() else {
            continue;
        };
        pkt.extend_from_slice(msg);
        rent!(tx.send(pkt).await, pkt)?;
    }
}

/// Stream every message log outside the command set over one connection:
/// each log's metadata, its latest record, then its live records. New logs
/// join as `metadata_gen` moves.
async fn handle_msg_sync<A: AsyncWrite + 'static>(
    sink: Arc<Mutex<PacketSink<A>>>,
    db: Arc<DB>,
    req_id: RequestId,
) -> Result<(), Error> {
    let mut visited: HashSet<PacketId> = HashSet::new();
    loop {
        let fresh: Vec<(PacketId, MsgLog)> = db.with_state(|s| {
            s.msg_logs
                .iter()
                .filter(|(id, _)| !visited.contains(*id) && !s.command_ids.contains(id))
                .map(|(id, log)| (*id, log.clone()))
                .collect()
        });
        for (id, log) in fresh {
            visited.insert(id);
            stellarator::spawn(sync_msg_log(sink.clone(), id, log, req_id));
        }
        db.metadata_gen.wait().await;
    }
}

/// One log's tail: the WAL reader is taken before `latest` is read, so no
/// record falls between the two; the reader's own copy of the seeded latest
/// is dropped once.
async fn sync_msg_log<A: AsyncWrite>(
    sink: Arc<Mutex<PacketSink<A>>>,
    id: PacketId,
    log: MsgLog,
    req_id: RequestId,
) -> Result<(), Error> {
    use crate::msg_log::read_msg;

    async fn send<A: AsyncWrite>(
        sink: &Mutex<PacketSink<A>>,
        pkt: LenPacket,
    ) -> Result<bool, Error> {
        let tx = sink.lock().await;
        let (sent, _) = tx.send(pkt).await;
        match sent {
            Ok(_) => Ok(true),
            Err(err) => {
                let err = Error::from(err);
                if err.is_stream_closed() {
                    Ok(false)
                } else {
                    Err(err)
                }
            }
        }
    }

    if let Some(metadata) = log.metadata().cloned() {
        let pkt = (&SetMsgMetadata { id, metadata }).with_request_id(req_id);
        if !send(&sink, pkt).await? {
            return Ok(());
        }
    }

    let mut reader = log.wal_reader();
    // `latest` reads the persisted head, which trails the WAL by one
    // persist pass; flushing behind the reader makes the seed complete,
    // and a record committed between the two arrives twice — the seam the
    // skip below closes.
    log.flush_pending()?;
    let mut seed: Option<(Timestamp, Vec<u8>)> = None;
    if let Some(latest) = log.latest()
        && let Some(data) = latest.data()
    {
        let ts = latest.timestamp();
        seed = Some((ts, data.to_vec()));
        let mut pkt = LenPacket::msg_with_timestamp(id, ts, data.len());
        pkt.extend_from_slice(data);
        if !send(&sink, pkt.with_request_id(req_id)).await? {
            return Ok(());
        }
    }

    loop {
        while let Some(grant) = reader.try_next() {
            let mut buf: &[u8] = &grant;
            while let Some((rest, ts, msg)) = read_msg(buf) {
                buf = rest;
                if let Some((seed_ts, seed_msg)) = seed.take()
                    && seed_ts == ts
                    && seed_msg == msg
                {
                    continue;
                }
                let mut pkt = LenPacket::msg_with_timestamp(id, ts, msg.len());
                pkt.extend_from_slice(msg);
                if !send(&sink, pkt.with_request_id(req_id)).await? {
                    return Ok(());
                }
            }
        }
        log.wait().await;
    }
}

pub async fn handle_fixed_rate_msg_stream<A: AsyncWrite>(
    msg_id: PacketId,
    req_id: RequestId,
    msg_log: MsgLog,
    tx: Arc<Mutex<PacketSink<A>>>,
    stream_state: Arc<FixedRateStreamState>,
) -> Result<(), Error> {
    let mut pkt = LenPacket::msg_with_timestamp(msg_id, Timestamp(0), 64).with_request_id(req_id);
    let mut last_sent_timestamp: Option<Timestamp> = None;
    loop {
        if !stream_state.wait_for_playing().await {
            return Ok(());
        }
        let start = Instant::now();
        let current_timestamp = stream_state.current_timestamp();
        let Some(msg_ref) = msg_log.get_nearest(current_timestamp) else {
            continue;
        };
        let Some(msg) = msg_ref.data() else {
            continue;
        };
        let msg_timestamp = msg_ref.timestamp();

        if Some(msg_timestamp) == last_sent_timestamp {
            stream_state
                .wait_for_tick(start.elapsed(), current_timestamp)
                .await;
            continue;
        }

        {
            let tx = tx.lock().await;
            pkt.clear();
            pkt.extend_from_slice(msg_timestamp.as_bytes());
            pkt.extend_from_slice(msg);
            rent!(tx.send(pkt).await, pkt)?;
        }
        last_sent_timestamp = Some(msg_timestamp);

        stream_state
            .wait_for_tick(start.elapsed(), current_timestamp)
            .await;
    }
}

pub struct FixedRateStreamState {
    stream_id: StreamId,
    time_step: AtomicU64,
    frequency: AtomicU64,
    is_scrubbed: AtomicBool,
    current_tick: AtomicI64,
    playing_cell: PlayingCell,
}

impl FixedRateStreamState {
    fn new(
        stream_id: StreamId,
        time_step: Duration,
        current_tick: Timestamp,
        frequency: u64,
    ) -> FixedRateStreamState {
        FixedRateStreamState {
            stream_id,
            time_step: AtomicU64::new(time_step.as_nanos() as u64),
            is_scrubbed: AtomicBool::new(false),
            current_tick: AtomicI64::new(current_tick.0),
            playing_cell: PlayingCell::new(true),
            frequency: AtomicU64::new(frequency),
        }
    }

    fn is_scrubbed(&self) -> bool {
        self.is_scrubbed.swap(false, atomic::Ordering::Relaxed)
    }

    fn set_timestamp(&self, timestamp: Timestamp) {
        self.is_scrubbed.store(true, atomic::Ordering::SeqCst);
        self.current_tick
            .store(timestamp.0, atomic::Ordering::SeqCst);
        self.playing_cell.wait_cell.wake_all();
    }

    fn set_frequency(&self, frequency: u64) {
        self.frequency.store(frequency, atomic::Ordering::SeqCst);
    }

    fn time_step(&self) -> Duration {
        Duration::from_nanos(self.time_step.load(atomic::Ordering::Relaxed))
    }

    fn sleep_time(&self) -> Duration {
        Duration::from_micros(1_000_000 / self.frequency.load(atomic::Ordering::Relaxed))
    }

    fn current_timestamp(&self) -> Timestamp {
        Timestamp(self.current_tick.load(atomic::Ordering::Relaxed))
    }

    fn try_increment_tick(&self, last_timestamp: Timestamp) {
        let time_step = self.time_step();
        let _ = self.current_tick.compare_exchange(
            last_timestamp.0,
            (last_timestamp + time_step).0,
            atomic::Ordering::Acquire,
            atomic::Ordering::Relaxed,
        );
    }

    fn is_playing(&self) -> bool {
        self.playing_cell.is_playing()
    }

    fn set_playing(&self, playing: bool) {
        self.playing_cell.set_playing(playing)
    }

    async fn wait_for_playing(&self) -> bool {
        self.playing_cell
            .wait_cell
            .wait_for(|| self.is_playing() || self.is_scrubbed())
            .await
            .is_ok()
    }

    async fn wait_for_tick(&self, elapsed: Duration, last_timestamp: Timestamp) {
        let sleep_time = self.sleep_time().saturating_sub(elapsed);
        futures_lite::future::race(
            async {
                self.playing_cell.wait_for_change().await;
            },
            stellarator::sleep(sleep_time),
        )
        .await;
        self.try_increment_tick(last_timestamp);
    }
}

fn handle_unicast_stream(addr: SocketAddr, stream: Stream, db: Arc<DB>) {
    stellarator::struc_con::stellar(move || async move {
        info!("UDP unicasting to {}", addr);
        let mut socket = UdpSocket::ephemeral()?;
        socket.connect(addr);
        let (_, tx) = socket.split();
        let tx = Arc::new(Mutex::new(PacketSink::new(tx)));
        handle_stream(tx.clone(), stream.clone(), db.clone(), 0)
            .await
            .unwrap();
        Ok::<_, Error>(())
    });
}

fn handle_stream<A: AsyncWrite + 'static>(
    tx: Arc<Mutex<PacketSink<A>>>,
    stream: Stream,
    db: Arc<DB>,
    req_id: RequestId,
) -> stellarator::JoinHandle<()> {
    match stream.behavior {
        StreamBehavior::RealTime => stellarator::spawn(async move {
            match handle_real_time_stream(tx, req_id, db).await {
                Ok(_) => {}
                Err(err) if err.is_stream_closed() => {}
                Err(err) => {
                    warn!(?err, "error streaming data");
                }
            }
        }),
        StreamBehavior::FixedRate(fixed_rate) => {
            let state = Arc::new(FixedRateStreamState::new(
                stream.id,
                Duration::from_nanos(fixed_rate.timestep),
                match fixed_rate.initial_timestamp {
                    InitialTimestamp::Earliest => db.earliest_timestamp.latest(),
                    InitialTimestamp::Latest => db.last_updated.latest(),
                    InitialTimestamp::Manual(timestamp) => timestamp,
                },
                fixed_rate.frequency,
            ));
            debug!(stream.id = ?stream.id, "inserting stream");
            db.with_state_mut(|s| s.streams.insert(stream.id, state.clone()));
            stellarator::spawn(async move {
                match handle_fixed_stream(tx, req_id, state, db).await {
                    Ok(_) => {}
                    Err(err) if err.is_stream_closed() => {}
                    Err(err) => {
                        warn!(?err, "error streaming data");
                    }
                }
            })
        }
    }
}

async fn handle_real_time_stream<A: AsyncWrite + 'static>(
    sink: Arc<Mutex<PacketSink<A>>>,
    req_id: RequestId,
    db: Arc<DB>,
) -> Result<(), Error> {
    let mut visited_ids = HashSet::new();
    loop {
        db.with_state(|state| {
            DBVisitor.visit(&state.components, |component| {
                if visited_ids.contains(&component.component_id)
                    || state.is_component_hidden(&component.component_id)
                {
                    return Ok(());
                }
                visited_ids.insert(component.component_id);
                let sink = sink.clone();
                let component = component.clone();
                stellarator::spawn(handle_real_time_component(sink, component, req_id));
                Ok(())
            })
        })?;
        db.vtable_gen.wait().await;
    }
}

async fn handle_real_time_component<A: AsyncWrite>(
    stream: Arc<Mutex<PacketSink<A>>>,
    component: Component,
    req_id: RequestId,
) -> Result<(), Error> {
    let timestamp_loc = raw_table(0, size_of::<Timestamp>() as u32);
    let prim_type = component.schema.prim_type;
    let vtable = vtable([raw_field(
        (prim_type.padding(8) + size_of::<Timestamp>()) as u32,
        component.schema.size() as u32,
        timestamp(timestamp_loc, component.as_vtable_op()),
    )]);
    let waiter = component.time_series.waiter();
    let vtable_id = metor_proto::types::table_id(&vtable);
    {
        let stream = stream.lock().await;
        stream
            .send(
                VTableMsg {
                    id: vtable_id,
                    vtable,
                }
                .with_request_id(req_id),
            )
            .await
            .0?;
    }

    let mut table = LenPacket::table(vtable_id, 2048 - 16);
    loop {
        let _ = waiter.wait().await;
        let Some(latest) = component.time_series.latest() else {
            continue;
        };
        table.push_aligned(latest.timestamp());
        table.pad_for_type(prim_type);
        table.extend_from_slice(latest.data());
        {
            let stream = stream.lock().await;
            if let Err(err) = rent!(stream.send(table.with_request_id(req_id)).await, table) {
                debug!(%err, "error sending table");
                if Error::from(err).is_stream_closed() {
                    return Ok(());
                }
            }
        }
        table.clear();
    }
}

async fn handle_fixed_stream<A: AsyncWrite>(
    stream: Arc<Mutex<PacketSink<A>>>,
    req_id: RequestId,
    state: Arc<FixedRateStreamState>,
    db: Arc<DB>,
) -> Result<(), Error> {
    let mut current_vtable_gen = db.vtable_gen.latest();
    let mut current_field_count: Option<usize> = None;
    let id: PacketId = state.stream_id.to_le_bytes()[..2].try_into().unwrap();
    let mut table = LenPacket::table(id, 2048 - 16);
    let mut components = db.with_state(|state| state.fixed_rate_components());
    loop {
        if !state.wait_for_playing().await {
            return Ok(());
        }
        let start = Instant::now();
        let current_timestamp = state.current_timestamp();
        let vtable_gen = db.vtable_gen.latest();
        if vtable_gen != current_vtable_gen {
            components = db.with_state(|state| state.fixed_rate_components());
            let stream = stream.lock().await;
            let vtable = DBVisitor.vtable(&components)?;
            let msg = VTableMsg { id, vtable };
            stream.send(msg.with_request_id(req_id)).await.0?;
            current_vtable_gen = vtable_gen;
        }
        table.clear();
        match DBVisitor.populate_table(&components, &mut table, current_timestamp) {
            Ok(fields) => {
                if current_field_count
                    .map(|current| current != fields)
                    .unwrap_or(true)
                {
                    let stream = stream.lock().await;
                    let vtable = DBVisitor.vtable(&components)?;
                    let msg = VTableMsg { id, vtable };
                    stream.send(msg.with_request_id(req_id)).await.0?;
                }
                current_field_count = Some(fields);
            }
            Err(err) => warn!(?err, "failed to populate table"),
        }
        {
            let stream = stream.lock().await;
            stream
                .send(
                    StreamTimestamp {
                        timestamp: current_timestamp,
                        stream_id: state.stream_id,
                    }
                    .with_request_id(req_id),
                )
                .await
                .0?;
            rent!(stream.send(table.with_request_id(req_id)).await, table)?;
        }
        state
            .wait_for_tick(start.elapsed(), current_timestamp)
            .await;
    }
}

struct DBVisitor;

impl DBVisitor {
    fn vtable(&self, components: &HashMap<ComponentId, Component>) -> Result<VTable, Error> {
        let mut fields = vec![];
        let mut offset = 0;
        self.visit(components, |entity| {
            if !entity.time_series.is_empty() {
                offset += PrimType::U64.padding(offset);
                let op = timestamp(builder::raw_table(offset as u32, 8), entity.as_vtable_op());
                offset += size_of::<Timestamp>();
                offset += entity.schema.prim_type.padding(offset);
                let len = entity.schema.size();
                let size = len as u32;
                fields.push(raw_field(offset as u32, size, op));
                offset += len;
            }
            Ok(())
        })?;
        Ok(vtable(fields))
    }

    fn populate_table(
        &self,
        components: &HashMap<ComponentId, Component>,
        table: &mut LenPacket,
        timestamp: Timestamp,
    ) -> Result<usize, Error> {
        let mut fields = 0;
        self.visit(components, |entity| {
            let Some(start_timestamp) = entity.time_series.start_timestamp() else {
                return Ok(());
            };
            let tick = start_timestamp.max(timestamp);
            let Some(nearest) = entity.time_series.binary_search_nearest(tick, true) else {
                return Ok(());
            };
            table.push_aligned(nearest.timestamp());
            table.pad_for_type(entity.schema.prim_type);
            table.extend_from_slice(nearest.data());
            fields += 1;
            Ok(())
        })?;
        Ok(fields)
    }

    fn visit(
        &self,
        components: &HashMap<ComponentId, Component>,
        mut f: impl for<'a> FnMut(&'a Component) -> Result<(), Error>,
    ) -> Result<(), Error> {
        components
            .iter()
            .try_for_each(|(_, component)| f(component))
    }
}

pub struct PlayingCell {
    is_playing: AtomicBool,
    wait_cell: WaitQueue,
}

impl PlayingCell {
    fn new(is_playing: bool) -> Self {
        Self {
            is_playing: AtomicBool::new(is_playing),
            wait_cell: WaitQueue::new(),
        }
    }

    pub fn set_playing(&self, playing: bool) {
        self.is_playing.store(playing, atomic::Ordering::SeqCst);
        self.wait_cell.wake_all();
    }

    fn is_playing(&self) -> bool {
        self.is_playing.load(atomic::Ordering::Relaxed)
    }

    pub async fn wait(&self) -> bool {
        self.wait_cell.wait_for(|| self.is_playing()).await.is_ok()
    }

    async fn wait_for_change(&self) {
        let _ = self.wait_cell.wait().await;
    }
}

impl MetadataExt for DbConfig {}

pub trait AtomicTimestampExt {
    fn update_max(&self, val: Timestamp);
    fn update_min(&self, val: Timestamp);
}

impl AtomicTimestampExt for AtomicCell<Timestamp> {
    fn update_max(&self, val: Timestamp) {
        self.value.fetch_max(val.0, atomic::Ordering::AcqRel);
        self.wait_queue.wake_all();
    }

    fn update_min(&self, val: Timestamp) {
        self.value.fetch_min(val.0, atomic::Ordering::AcqRel);
        self.wait_queue.wake_all();
    }
}

#[cfg(test)]
mod dynamic_ingest_tests {
    use super::*;
    use core::convert::Infallible;
    use metor_proto::com_de::Decomponentize;
    use metor_proto::types::PACKET_HEADER_LEN;
    use metor_proto::vtable::builder::FieldBuilder;
    use metor_proto::vtable::builder::{
        list, map, path_component, raw_field, raw_table, schema, timestamp, vtable,
    };
    use std::collections::HashMap;

    /// Records every scalar component it sees as an `f64` (mirrors
    /// `metor-proto`'s `test_dynamic_list` sink).
    #[derive(Default)]
    struct DynSink {
        values: HashMap<ComponentId, f64>,
    }

    impl Decomponentize for DynSink {
        type Error = Infallible;
        fn apply_value(
            &mut self,
            component_id: ComponentId,
            value: ComponentView<'_>,
            _timestamp: Option<Timestamp>,
        ) -> Result<(), Infallible> {
            self.values.insert(component_id, value.to_f64());
            Ok(())
        }
    }

    fn put_u32(buf: &mut Vec<u8>, v: u32) {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    fn put_u64(buf: &mut Vec<u8>, v: u64) {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    fn put_f64(buf: &mut Vec<u8>, v: f64) {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    fn put_i64(buf: &mut Vec<u8>, v: i64) {
        buf.extend_from_slice(&v.to_le_bytes());
    }

    fn process_members() -> Vec<FieldBuilder> {
        vec![
            raw_field(0, 8, schema(PrimType::U64, &[], path_component("pid"))),
            raw_field(
                8,
                8,
                schema(PrimType::F64, &[], path_component("cpu_usage")),
            ),
        ]
    }

    /// Builds a `processes` list table with `pids[i]` / `cpus[i]` elements.
    fn list_table(pids: &[u64], cpus: &[f64]) -> Vec<u8> {
        assert_eq!(pids.len(), cpus.len());
        let mut t = Vec::new();
        put_i64(&mut t, 1000); // timestamp @0
        put_u32(&mut t, 16); // slot.trailer_off @8
        put_u32(&mut t, (pids.len() * 16) as u32); // slot.byte_len @12
        for (pid, cpu) in pids.iter().zip(cpus) {
            put_u64(&mut t, *pid);
            put_f64(&mut t, *cpu);
        }
        t
    }

    /// Parses a `dynamic_stream_packet` `LenPacket` and applies it through the
    /// subscriber `vtable` to a fresh `DynSink`, returning the realized values.
    fn round_trip(pkt: LenPacket, id: PacketId, vtable: &VTable) -> DynSink {
        // Parse with offset so the table body keeps the same 8-byte alignment it
        // has on the wire (the 4-byte length prefix stays in the allocation, like
        // `LengthDelReader` leaves it), avoiding an `Alignment` error in `apply`.
        let owned = Packet::parse_with_offset(pkt.inner, PACKET_HEADER_LEN).unwrap();
        let Packet::Table(table) = owned else {
            panic!("expected a table packet");
        };
        assert_eq!(table.id, id);
        let mut sink = DynSink::default();
        vtable
            .apply(stellarator::buf::deref(&table.buf), &mut sink)
            .unwrap()
            .unwrap();
        sink
    }

    #[stellarator::test]
    async fn dynamic_list_ingest_and_stream() {
        let dir = tempfile::tempdir().unwrap();
        let db = DB::create(dir.path().to_path_buf()).unwrap();
        let id: PacketId = [7, 0];
        let v = vtable([raw_field(
            8,
            8,
            timestamp(raw_table(0, 8), list("processes", process_members(), 16)),
        )]);

        // 1. insert_vtable registers the member templates (hidden), no concrete yet.
        db.insert_vtable(VTableMsg {
            vtable: v.clone(),
            id,
        })
        .unwrap();
        db.with_state(|s| {
            assert!(s.get_component(ComponentId::new("processes.pid")).is_some());
            assert!(
                s.get_component(ComponentId::new("processes.cpu_usage"))
                    .is_some()
            );
            assert!(s.is_component_hidden(&ComponentId::new("processes.pid")));
            assert!(s.is_component_hidden(&ComponentId::new("processes.cpu_usage")));
            assert!(
                s.get_component(ComponentId::new("processes.0.pid"))
                    .is_none()
            );
        });

        // 2. ingest a 2-element table -> concrete components, cache, gen bump.
        let gen_before = db.vtable_gen.latest();
        db.ingest_table(id, &list_table(&[1001, 1002], &[0.5, 0.25]))
            .unwrap();
        assert!(
            db.vtable_gen.latest() > gen_before,
            "gen must bump on new series"
        );
        db.with_state(|s| {
            for id in [
                "processes.0.pid",
                "processes.0.cpu_usage",
                "processes.1.pid",
                "processes.1.cpu_usage",
            ] {
                assert!(
                    s.get_component(ComponentId::new(id)).is_some(),
                    "missing concrete component {id}"
                );
                assert!(!s.is_component_hidden(&ComponentId::new(id)));
            }
            assert!(!s.latest_dynamic_tables[&id].lock().unwrap().is_empty());
        });

        // values landed in the concrete time series
        stellarator::sleep(std::time::Duration::from_millis(100)).await;
        let pid0 = db
            .with_state(|s| {
                s.get_component(ComponentId::new("processes.0.pid"))
                    .cloned()
            })
            .unwrap();
        let latest = pid0.time_series.latest().expect("pid0 sample persisted");
        assert_eq!(u64::from_le_bytes(latest.data().try_into().unwrap()), 1001);

        // 3. streamed packet round-trips to the same concrete ids/values.
        let sink = round_trip(db.dynamic_stream_packet(id).unwrap(), id, &v);
        assert_eq!(sink.values[&ComponentId::new("processes.0.pid")], 1001.0);
        assert_eq!(sink.values[&ComponentId::new("processes.0.cpu_usage")], 0.5);
        assert_eq!(sink.values[&ComponentId::new("processes.1.pid")], 1002.0);
        assert_eq!(
            sink.values[&ComponentId::new("processes.1.cpu_usage")],
            0.25
        );

        // 4. a NEW element appears -> gen bumps, next packet includes it.
        let gen_before = db.vtable_gen.latest();
        db.ingest_table(id, &list_table(&[1001, 1002, 1003], &[0.5, 0.25, 0.75]))
            .unwrap();
        assert!(
            db.vtable_gen.latest() > gen_before,
            "gen must bump for the new element"
        );
        let sink = round_trip(db.dynamic_stream_packet(id).unwrap(), id, &v);
        assert_eq!(sink.values[&ComponentId::new("processes.2.pid")], 1003.0);
        assert_eq!(
            sink.values[&ComponentId::new("processes.2.cpu_usage")],
            0.75
        );

        // 5. LIVENESS: fewer elements -> dropped element absent next packet.
        db.ingest_table(id, &list_table(&[1001], &[0.5])).unwrap();
        let sink = round_trip(db.dynamic_stream_packet(id).unwrap(), id, &v);
        assert_eq!(sink.values[&ComponentId::new("processes.0.pid")], 1001.0);
        assert!(
            !sink
                .values
                .contains_key(&ComponentId::new("processes.1.pid")),
            "vanished element must drop out (authoritative-latest)"
        );
        assert!(
            !sink
                .values
                .contains_key(&ComponentId::new("processes.2.pid"))
        );
    }

    #[stellarator::test]
    async fn dynamic_map_ingest_and_stream() {
        let dir = tempfile::tempdir().unwrap();
        let db = DB::create(dir.path().to_path_buf()).unwrap();
        let id: PacketId = [9, 0];
        let v = vtable([raw_field(
            8,
            8,
            timestamp(raw_table(0, 8), map("processes", process_members(), 24, 8)),
        )]);
        db.insert_vtable(VTableMsg {
            vtable: v.clone(),
            id,
        })
        .unwrap();

        // hand-built map table with two keyed entries (mirrors test_dynamic_map).
        let mut t = Vec::new();
        put_i64(&mut t, 1000); // timestamp @0
        put_u32(&mut t, 16); // slot.trailer_off @8
        put_u32(&mut t, 48); // slot.byte_len @12
        put_u32(&mut t, 64); // entry[0].key_off -> "htop"
        put_u32(&mut t, 4); // entry[0].key_len
        put_u64(&mut t, 1001); // entry[0].value.pid
        put_f64(&mut t, 0.5); // entry[0].value.cpu
        put_u32(&mut t, 68); // entry[1].key_off -> "init"
        put_u32(&mut t, 4); // entry[1].key_len
        put_u64(&mut t, 1002); // entry[1].value.pid
        put_f64(&mut t, 0.25); // entry[1].value.cpu
        t.extend_from_slice(b"htop"); // @64
        t.extend_from_slice(b"init"); // @68

        db.ingest_table(id, &t).unwrap();
        db.with_state(|s| {
            assert!(
                s.get_component(ComponentId::new("processes.htop.pid"))
                    .is_some()
            );
            assert!(
                s.get_component(ComponentId::new("processes.init.cpu_usage"))
                    .is_some()
            );
            assert!(!s.latest_dynamic_tables[&id].lock().unwrap().is_empty());
            // The keyed member carries its dotted name, not the numeric id, and
            // is visible (only the template stays hidden).
            let meta = s
                .get_component_metadata(ComponentId::new("processes.htop.pid"))
                .expect("keyed member metadata");
            assert_eq!(meta.name, "processes.htop.pid");
            assert!(!meta.is_hidden());
        });

        let sink = round_trip(db.dynamic_stream_packet(id).unwrap(), id, &v);
        assert_eq!(sink.values[&ComponentId::new("processes.htop.pid")], 1001.0);
        assert_eq!(
            sink.values[&ComponentId::new("processes.htop.cpu_usage")],
            0.5
        );
        assert_eq!(sink.values[&ComponentId::new("processes.init.pid")], 1002.0);
        assert_eq!(
            sink.values[&ComponentId::new("processes.init.cpu_usage")],
            0.25
        );
    }

    /// Builds a `processes` map table of `(key, pid, cpu)` entries.
    fn map_table(ts: i64, entries: &[(&str, u64, f64)]) -> Vec<u8> {
        let mut t = Vec::new();
        put_i64(&mut t, ts);
        put_u32(&mut t, 16);
        put_u32(&mut t, (entries.len() * 24) as u32);
        let mut key_off = 16 + entries.len() * 24;
        for (key, pid, cpu) in entries {
            put_u32(&mut t, key_off as u32);
            put_u32(&mut t, key.len() as u32);
            put_u64(&mut t, *pid);
            put_f64(&mut t, *cpu);
            key_off += key.len();
        }
        for (key, ..) in entries {
            t.extend_from_slice(key.as_bytes());
        }
        t
    }

    fn map_db(dir: &Path) -> (DB, PacketId, VTable) {
        let db = DB::create(dir.to_path_buf()).unwrap();
        let id: PacketId = [9, 1];
        let v = vtable([raw_field(
            8,
            8,
            timestamp(raw_table(0, 8), map("processes", process_members(), 24, 8)),
        )]);
        db.insert_vtable(VTableMsg {
            vtable: v.clone(),
            id,
        })
        .unwrap();
        (db, id, v)
    }

    fn sample_times(db: &DB, name: &str) -> Vec<i64> {
        let component = db
            .with_state(|s| s.get_component(ComponentId::new(name)).cloned())
            .unwrap();
        let Some(slice) = component
            .time_series
            .get_range(Timestamp(i64::MIN)..Timestamp(i64::MAX))
        else {
            return Vec::new();
        };
        slice
            .as_iter()
            .flat_map(|node| node.timestamps().iter().map(|t| t.0).collect::<Vec<_>>())
            .collect()
    }

    fn cached_table(db: &DB, id: PacketId) -> Vec<u8> {
        db.with_state(|s| s.latest_dynamic_tables[&id].lock().unwrap().clone())
    }

    #[stellarator::test]
    async fn dynamic_known_keys_push_under_read_lock() {
        let dir = tempfile::tempdir().unwrap();
        let (db, id, _) = map_db(dir.path());
        assert!(db.dynamic_stream_packet(id).is_none());

        db.ingest_table(id, &map_table(1000, &[("htop", 1, 0.5), ("init", 2, 0.25)]))
            .unwrap();
        let t2 = map_table(2000, &[("htop", 3, 0.5), ("init", 4, 0.25)]);
        let pushed = db
            .with_state(|s| s.push_known_fields(id, &t2, Timestamp(0)))
            .unwrap();
        assert!(pushed.is_some());
        let t3 = map_table(3000, &[("htop", 5, 0.5), ("init", 6, 0.25)]);
        db.ingest_table(id, &t3).unwrap();

        stellarator::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(sample_times(&db, "processes.htop.pid"), [1000, 2000, 3000]);
        assert_eq!(
            sample_times(&db, "processes.init.cpu_usage"),
            [1000, 2000, 3000]
        );
        assert_eq!(cached_table(&db, id), t3);
    }

    #[stellarator::test]
    async fn dynamic_new_key_realized_without_double_push() {
        let dir = tempfile::tempdir().unwrap();
        let (db, id, v) = map_db(dir.path());
        db.ingest_table(id, &map_table(1000, &[("htop", 1, 0.5)]))
            .unwrap();

        let t2 = map_table(2000, &[("htop", 3, 0.5), ("init", 4, 0.25)]);
        let pushed = db
            .with_state(|s| s.push_known_fields(id, &t2, Timestamp(0)))
            .unwrap();
        assert!(pushed.is_none());
        let gen_before = db.vtable_gen.latest();
        db.ingest_table(id, &t2).unwrap();
        assert!(db.vtable_gen.latest() > gen_before);

        stellarator::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(sample_times(&db, "processes.htop.pid"), [1000, 2000]);
        assert_eq!(sample_times(&db, "processes.init.pid"), [2000]);
        let sink = round_trip(db.dynamic_stream_packet(id).unwrap(), id, &v);
        assert_eq!(sink.values[&ComponentId::new("processes.init.pid")], 4.0);
    }

    #[stellarator::test]
    async fn dynamic_key_disappears() {
        let dir = tempfile::tempdir().unwrap();
        let (db, id, v) = map_db(dir.path());
        db.ingest_table(id, &map_table(1000, &[("htop", 1, 0.5), ("init", 2, 0.25)]))
            .unwrap();
        let t2 = map_table(2000, &[("init", 4, 0.25)]);
        db.ingest_table(id, &t2).unwrap();

        stellarator::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(sample_times(&db, "processes.htop.pid"), [1000]);
        assert_eq!(sample_times(&db, "processes.init.pid"), [1000, 2000]);
        assert_eq!(cached_table(&db, id), t2);
        let sink = round_trip(db.dynamic_stream_packet(id).unwrap(), id, &v);
        assert!(
            !sink
                .values
                .contains_key(&ComponentId::new("processes.htop.pid"))
        );
        assert_eq!(sink.values[&ComponentId::new("processes.init.pid")], 4.0);
    }

    /// A scalar-valued map (a `FrameMap<u64, N>` member) has an empty
    /// member leaf, so the keyed member is named `<container>.<key>` directly.
    #[stellarator::test]
    async fn scalar_map_member_named_by_key() {
        let dir = tempfile::tempdir().unwrap();
        let db = DB::create(dir.path().to_path_buf()).unwrap();
        let id: PacketId = [11, 0];
        let members = vec![raw_field(
            0,
            8,
            schema(PrimType::U64, &[], path_component("")),
        )];
        let v = vtable([raw_field(
            8,
            8,
            timestamp(raw_table(0, 8), map("counts", members, 16, 8)),
        )]);
        db.insert_vtable(VTableMsg {
            vtable: v.clone(),
            id,
        })
        .unwrap();

        let mut t = Vec::new();
        put_i64(&mut t, 1000); // timestamp @0
        put_u32(&mut t, 16); // slot.trailer_off @8
        put_u32(&mut t, 32); // slot.byte_len @12
        put_u32(&mut t, 48); // entry[0].key_off -> "drop"
        put_u32(&mut t, 4); // entry[0].key_len
        put_u64(&mut t, 7); // entry[0].value
        put_u32(&mut t, 52); // entry[1].key_off -> "over"
        put_u32(&mut t, 4); // entry[1].key_len
        put_u64(&mut t, 9); // entry[1].value
        t.extend_from_slice(b"drop"); // @48
        t.extend_from_slice(b"over"); // @52

        db.ingest_table(id, &t).unwrap();
        db.with_state(|s| {
            let meta = s
                .get_component_metadata(ComponentId::new("counts.drop"))
                .expect("keyed member metadata");
            assert_eq!(meta.name, "counts.drop");
            assert!(!meta.is_hidden());
            // Not the numeric-id fallback.
            assert_ne!(meta.name, ComponentId::new("counts.drop").to_string());
            assert_eq!(
                s.get_component_metadata(ComponentId::new("counts.over"))
                    .unwrap()
                    .name,
                "counts.over"
            );
        });
    }
}

#[cfg(test)]
mod msg_sync_tests {
    use super::*;
    use metor_proto_stellar::Client;
    use std::sync::Mutex as StdMutex;

    use postcard_schema::Schema as _;

    #[derive(postcard_schema::Schema, serde::Serialize, serde::Deserialize)]
    struct Payload {
        i: u32,
    }

    const CMD: PacketId = [0x51, 0];
    const LOG_A: PacketId = [0x33, 7];
    const LOG_B: PacketId = [0x34, 7];

    type Seen = Arc<StdMutex<Vec<(PacketId, Option<Timestamp>, Vec<u8>)>>>;

    fn serve(db: Arc<DB>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();
        let server = Server { listener, db };
        stellarator::struc_con::stellar(move || server.run());
        addr
    }

    /// Send `SyncMsgs` and record every packet the server pushes back.
    async fn sync(addr: SocketAddr) -> Seen {
        let mut client = Client::connect(addr).await.unwrap();
        client.send((&SyncMsgs).with_request_id(2)).await.0.unwrap();
        let seen: Seen = Arc::new(StdMutex::new(Vec::new()));
        let sink = seen.clone();
        let Client { mut rx, .. } = client;
        stellarator::spawn(async move {
            let mut buf = vec![0u8; 4096];
            loop {
                let Ok(pkt) = rx.next_grow(buf).await else {
                    return;
                };
                if let Packet::Msg(m) = &pkt {
                    sink.lock()
                        .unwrap()
                        .push((m.id, m.timestamp, m.buf[..].to_vec()));
                }
                buf = pkt.into_buf().into_inner();
            }
        });
        seen
    }

    async fn wait_for(pred: impl Fn() -> bool, what: &str) {
        for _ in 0..200 {
            if pred() {
                return;
            }
            stellarator::sleep(Duration::from_millis(25)).await;
        }
        panic!("never saw {what}");
    }

    fn payloads(seen: &Seen, id: PacketId) -> Vec<Vec<u8>> {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|(pkt_id, _, _)| *pkt_id == id)
            .map(|(_, _, buf)| buf.clone())
            .collect()
    }

    #[stellarator::test]
    async fn db_info_advertises_commands_and_features() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(DB::create(dir.path().join("db")).unwrap());
        db.set_identity(Some("sat".into()), vec![CMD]);
        let addr = serve(db);
        let mut client = Client::connect(addr).await.unwrap();
        let info: DbInfoResp = client.request(&GetDbInfo).await.unwrap();
        assert_eq!(info.protocol_version, NODE_PROTOCOL_VERSION);
        assert_ne!(info.features & FEATURE_MSG_SYNC, 0);
        assert_eq!(info.command_ids, vec![CMD]);
        assert_eq!(info.namespace.as_deref(), Some("sat"));
    }

    /// The seeded latest arrives with its source timestamp, live records
    /// follow, a log in the command set never streams, and a log created
    /// after the request joins.
    #[stellarator::test]
    async fn sync_msgs_streams_latest_then_live_and_skips_commands() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(DB::create(dir.path().join("db")).unwrap());
        db.set_identity(None, vec![CMD]);
        db.with_state_mut(|s| {
            s.set_msg_metadata(
                LOG_A,
                MsgMetadata {
                    name: "log_a".into(),
                    schema: Payload::SCHEMA.into(),
                    metadata: Default::default(),
                },
                &db.path,
            )
        })
        .unwrap();
        db.push_msg(Timestamp(10), LOG_A, b"a0").unwrap();
        db.push_msg(Timestamp(11), LOG_A, b"a1").unwrap();
        db.push_msg(Timestamp(12), CMD, b"c0").unwrap();

        let addr = serve(db.clone());
        let seen = sync(addr).await;

        let probe = seen.clone();
        wait_for(
            move || !payloads(&probe, LOG_A).is_empty(),
            "the seeded latest",
        )
        .await;
        assert_eq!(payloads(&seen, LOG_A), vec![b"a1".to_vec()]);
        assert_eq!(
            seen.lock()
                .unwrap()
                .iter()
                .find(|(id, _, _)| *id == LOG_A)
                .and_then(|(_, ts, _)| *ts),
            Some(Timestamp(11)),
            "the source timestamp survives",
        );
        assert!(
            seen.lock()
                .unwrap()
                .iter()
                .any(|(id, _, _)| *id == SetMsgMetadata::ID),
            "the log's metadata precedes its records",
        );

        // Live records follow, and a log created after the request joins.
        db.push_msg(Timestamp(13), LOG_A, b"a2").unwrap();
        db.push_msg(Timestamp(14), LOG_B, b"b0").unwrap();
        let probe = seen.clone();
        wait_for(
            move || payloads(&probe, LOG_A).len() == 2 && !payloads(&probe, LOG_B).is_empty(),
            "the live record and the new log",
        )
        .await;
        assert_eq!(payloads(&seen, LOG_A), vec![b"a1".to_vec(), b"a2".to_vec()]);
        assert_eq!(payloads(&seen, LOG_B), vec![b"b0".to_vec()]);

        // The command log never streams, before or after.
        db.push_msg(Timestamp(15), CMD, b"c1").unwrap();
        stellarator::sleep(Duration::from_millis(200)).await;
        assert!(payloads(&seen, CMD).is_empty());
    }

    /// The WAL reader is taken before `latest`, so the seeded record can
    /// arrive twice; it is dropped once.
    #[stellarator::test]
    async fn sync_msgs_does_not_duplicate_the_seed() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(DB::create(dir.path().join("db")).unwrap());
        db.push_msg(Timestamp(1), LOG_A, b"first").unwrap();
        let addr = serve(db.clone());
        let seen = sync(addr).await;
        let probe = seen.clone();
        wait_for(move || !payloads(&probe, LOG_A).is_empty(), "the seed").await;
        db.push_msg(Timestamp(2), LOG_A, b"second").unwrap();
        let probe = seen.clone();
        wait_for(
            move || payloads(&probe, LOG_A).len() == 2,
            "the live record",
        )
        .await;
        stellarator::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            payloads(&seen, LOG_A),
            vec![b"first".to_vec(), b"second".to_vec()]
        );
    }
}

#[cfg(test)]
mod vtable_id_tests {
    use super::*;
    use metor_proto::vtable::builder::{component, raw_field, schema, vtable};
    use metor_proto_stellar::Client;

    const ID: PacketId = [7, 3];

    fn announce(name: &str) -> VTable {
        vtable([raw_field(
            0,
            8,
            schema(PrimType::F64, &[1], component(name)),
        )])
    }

    /// A reconnect replay re-announces the same bytes: no components are
    /// re-inserted and nothing invalidates.
    #[stellarator::test]
    async fn insert_vtable_is_idempotent_on_equal_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let db = DB::create(dir.path().to_path_buf()).unwrap();
        let msg = VTableMsg {
            id: ID,
            vtable: announce("one"),
        };
        db.insert_vtable(msg.clone()).unwrap();
        let generation = db.vtable_gen.latest();
        db.insert_vtable(msg).unwrap();
        assert_eq!(db.vtable_gen.latest(), generation);
        assert!(
            db.with_state(|s| s.get_component(ComponentId::new("one")).is_some())
                && db.with_state(|s| s.components.len() == 1)
        );
    }

    /// Two producers announcing different schemas under one id: the second
    /// is refused and the first stays registered.
    #[stellarator::test]
    async fn insert_vtable_refuses_a_different_vtable_under_one_id() {
        let dir = tempfile::tempdir().unwrap();
        let db = DB::create(dir.path().to_path_buf()).unwrap();
        db.insert_vtable(VTableMsg {
            id: ID,
            vtable: announce("first"),
        })
        .unwrap();
        let err = db
            .insert_vtable(VTableMsg {
                id: ID,
                vtable: announce("second"),
            })
            .expect_err("a different vtable under a registered id is refused");
        assert!(matches!(err, Error::VTableConflict(ID)), "{err:?}");
        db.with_state(|s| {
            assert!(s.get_component(ComponentId::new("first")).is_some());
            assert!(s.get_component(ComponentId::new("second")).is_none());
        });
    }

    /// The real-time stream ids its per-component vtable by content, so two
    /// connections to one db agree.
    #[stellarator::test]
    async fn real_time_stream_ids_are_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(DB::create(dir.path().to_path_buf()).unwrap());
        db.insert_vtable(VTableMsg {
            id: ID,
            vtable: announce("live"),
        })
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();
        let server = Server { listener, db };
        stellarator::struc_con::stellar(move || server.run());

        let mut ids = Vec::new();
        for _ in 0..2 {
            let mut client = Client::connect(addr).await.unwrap();
            client
                .send(
                    (&Stream {
                        behavior: StreamBehavior::RealTime,
                        id: 1,
                    })
                        .with_request_id(1),
                )
                .await
                .0
                .unwrap();
            let mut buf = vec![0u8; 4096];
            loop {
                let pkt = client.rx.next_grow(buf).await.unwrap();
                if let Packet::Msg(m) = &pkt
                    && m.id == VTableMsg::ID
                {
                    ids.push(m.parse::<VTableMsg>().unwrap().id);
                    break;
                }
                buf = pkt.into_buf().into_inner();
            }
        }
        assert_eq!(ids[0], ids[1]);
    }
}

#[cfg(test)]
mod static_ingest_tests {
    use super::*;
    use metor_proto::vtable::builder::{
        component, raw_field, raw_table, schema, timestamp, vtable,
    };

    const ID: PacketId = [9, 1];

    /// Two `f64` fields sharing the table's leading timestamp.
    fn two_fields() -> VTable {
        vtable([
            raw_field(
                8,
                8,
                timestamp(raw_table(0, 8), schema(PrimType::F64, &[1], component("a"))),
            ),
            raw_field(
                16,
                8,
                timestamp(raw_table(0, 8), schema(PrimType::F64, &[1], component("b"))),
            ),
        ])
    }

    fn sample(timestamp: i64, a: f64, b: f64) -> Vec<u8> {
        let mut buf = timestamp.to_le_bytes().to_vec();
        buf.extend_from_slice(&a.to_le_bytes());
        buf.extend_from_slice(&b.to_le_bytes());
        buf
    }

    fn store() -> (tempfile::TempDir, DB) {
        let dir = tempfile::tempdir().unwrap();
        let db = DB::create(dir.path().to_path_buf()).unwrap();
        db.insert_vtable(VTableMsg {
            id: ID,
            vtable: two_fields(),
        })
        .unwrap();
        (dir, db)
    }

    fn latest(db: &DB, name: &str) -> Timestamp {
        db.with_state(|state| {
            state
                .get_component(ComponentId::new(name))
                .unwrap()
                .last_timestamp
                .latest()
        })
    }

    #[stellarator::test]
    async fn static_table_lands_in_its_components() {
        let (_dir, db) = store();
        db.ingest_table(ID, &sample(5, 1.5, 2.5)).unwrap();
        db.ingest_table(ID, &sample(7, 1.5, 2.5)).unwrap();
        for name in ["a", "b"] {
            assert_eq!(latest(&db, name), Timestamp(7));
        }
        assert_eq!(db.last_updated.latest(), Timestamp(7));
    }

    /// A series' first sample changes membership.
    #[stellarator::test]
    async fn first_sample_bumps_vtable_gen() {
        let (_dir, db) = store();
        let before = db.vtable_gen.latest();
        db.ingest_table(ID, &sample(1, 0.0, 0.0)).unwrap();
        assert!(db.vtable_gen.latest() > before);
    }

    #[stellarator::test]
    async fn unknown_table_is_refused() {
        let (_dir, db) = store();
        let err = db.ingest_table([0, 0], &sample(1, 0.0, 0.0)).unwrap_err();
        assert!(
            matches!(
                err,
                Error::Impeller(metor_proto::error::Error::VTableNotFound)
            ),
            "{err:?}"
        );
    }

    /// A table too short for its vtable surfaces the walk's error.
    #[stellarator::test]
    async fn short_table_is_refused() {
        let (_dir, db) = store();
        assert!(db.ingest_table(ID, &5i64.to_le_bytes()).is_err());
    }
}

#[cfg(test)]
mod wal_tests {
    use super::*;
    use std::time::Duration;

    const IMAGE_SIZE: usize = 2 * 1024 * 1024;

    fn image_schema() -> ComponentSchema {
        ComponentSchema::new(PrimType::U16, &[1024usize, 1024][..])
    }

    fn unpersisted(dir: &Path, schema: ComponentSchema) -> Component {
        let id = ComponentId::new("wal");
        let time_series = TimeSeries::create(dir.join(id.to_string())).unwrap();
        Component::from_time_series(id, schema, time_series)
    }

    fn entries(buf: &[u8], size: usize) -> impl Iterator<Item = (i64, &[u8])> {
        assert_eq!(buf.len() % (size + 8), 0);
        buf.chunks_exact(size + 8).map(|entry| {
            (
                i64::from_le_bytes(entry[..8].try_into().unwrap()),
                &entry[8..],
            )
        })
    }

    /// Timestamps of every whole entry the reader can see.
    fn drain(reader: &mut disruptor::Reader, size: usize) -> Vec<i64> {
        let mut timestamps = Vec::new();
        while let Some(buf) = reader.try_next() {
            timestamps.extend(entries(&buf, size).map(|(ts, _)| ts));
        }
        timestamps
    }

    async fn wait_persisted(component: &Component, ts: i64) {
        for _ in 0..10_000 {
            if component.time_series.latest().map(|l| l.timestamp()) == Some(Timestamp(ts)) {
                return;
            }
            stellarator::sleep(Duration::from_millis(1)).await;
        }
        panic!("sample {ts} never persisted");
    }

    #[test]
    fn capacity_holds_whole_entries_within_budget() {
        assert_eq!(wal_capacity(8, WAL_CREATE_ENTRIES), 16 * 1024);
        let image = wal_capacity(IMAGE_SIZE, WAL_CREATE_ENTRIES);
        assert_eq!(image, 31 * (IMAGE_SIZE + 8));
        assert!(image <= WAL_BUDGET);
        let size = 40 * 1024 * 1024;
        assert_eq!(wal_capacity(size, WAL_CREATE_ENTRIES), 4 * (size + 8));
    }

    #[test]
    fn stalled_wal_drops_large_samples_without_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let component = unpersisted(dir.path(), image_schema());
        assert_eq!(component.wal.capacity(), 31 * (IMAGE_SIZE + 8));
        let mut reader = component.wal.reader();
        let mut buf = vec![0u8; IMAGE_SIZE];
        for ts in 0..40 {
            buf.fill(ts as u8);
            component.push_buf(Timestamp(ts), &buf).unwrap();
        }
        assert_eq!(drain(&mut reader, IMAGE_SIZE), (0..31).collect::<Vec<_>>());
        assert_eq!(component.wal_dropped.load(atomic::Ordering::Relaxed), 9);

        // Space freed by the reader takes new samples at the next offset.
        for ts in 40..50 {
            buf.fill(ts as u8);
            component.push_buf(Timestamp(ts), &buf).unwrap();
        }
        let grant = reader.try_next().unwrap();
        let read: Vec<_> = entries(&grant, IMAGE_SIZE).collect();
        assert_eq!(read.len(), 10);
        for ((ts, data), expected) in read.into_iter().zip(40i64..) {
            assert_eq!(ts, expected);
            assert!(data.iter().all(|b| *b == ts as u8));
        }
    }

    #[stellarator::test]
    async fn large_component_persists_every_sample() {
        let dir = tempfile::tempdir().unwrap();
        let id = ComponentId::new("image");
        let component = Component::create(dir.path(), id, image_schema(), false).unwrap();
        assert!(component.wal.capacity() <= WAL_BUDGET);
        let mut buf = vec![0u8; IMAGE_SIZE];
        for ts in 0..100 {
            buf.fill(ts as u8);
            component.push_buf(Timestamp(ts), &buf).unwrap();
            wait_persisted(&component, ts).await;
        }
        for ts in 0..100 {
            let sample = component.time_series.get(Timestamp(ts)).unwrap();
            assert!(sample.data().iter().all(|b| *b == ts as u8), "ts {ts}");
        }
        assert_eq!(component.time_series.iter_node_slices().count(), 2);
        assert_eq!(component.wal_dropped.load(atomic::Ordering::Relaxed), 0);
    }
}

#[cfg(test)]
mod series_tests {
    use super::*;
    use metor_proto_stellar::Client;
    use std::time::Duration;

    const ID: PacketId = [9, 2];
    const IMAGE_SIZE: usize = 2 * 1024 * 1024;
    /// Image samples per page: what fits in `SERIES_BUDGET`.
    const PAGE: usize = 127;

    #[test]
    fn series_len_clamps_to_the_budget() {
        assert_eq!(series_len(300, usize::MAX, IMAGE_SIZE).unwrap(), PAGE);
        assert_eq!(series_len(300, 10, IMAGE_SIZE).unwrap(), 10);
        assert_eq!(series_len(1_000_000, usize::MAX, 8).unwrap(), 1_000_000);
        let largest = SERIES_BUDGET - SERIES_HEAD - size_of::<Timestamp>();
        assert_eq!(series_len(5, usize::MAX, largest).unwrap(), 1);
        assert!(matches!(
            series_len(1, usize::MAX, largest + 1),
            Err(Error::ComponentTooLarge(s)) if s == largest + 1
        ));
    }

    /// A table of an `f64` at offset 8 and a 1024² `U16` image at 16.
    fn image_vtable() -> VTable {
        vtable([
            raw_field(
                8,
                8,
                timestamp(raw_table(0, 8), schema(PrimType::F64, &[1], component("f"))),
            ),
            raw_field(
                16,
                IMAGE_SIZE as u32,
                timestamp(
                    raw_table(0, 8),
                    schema(PrimType::U16, &[1024, 1024], component("img")),
                ),
            ),
        ])
    }

    /// Pixel `i` of sample `ts` is `ts + i`.
    fn fill(table: &mut [u8], ts: i64) {
        table[..8].copy_from_slice(&ts.to_le_bytes());
        table[8..16].copy_from_slice(&(ts as f64).to_le_bytes());
        let start = ts as u16 as usize;
        table[16..].copy_from_slice(ramp()[start..start + IMAGE_SIZE / 2].as_bytes());
    }

    /// `ramp()[j] == j as u16`, long enough for any start below 2^16.
    fn ramp() -> &'static [u16] {
        static RAMP: std::sync::OnceLock<Vec<u16>> = std::sync::OnceLock::new();
        RAMP.get_or_init(|| (0..IMAGE_SIZE / 2 + (1 << 16)).map(|j| j as u16).collect())
    }

    fn image_db() -> (tempfile::TempDir, Arc<DB>) {
        let dir = tempfile::tempdir().unwrap();
        let db = DB::create(dir.path().join("db")).unwrap();
        db.insert_vtable(VTableMsg {
            id: ID,
            vtable: image_vtable(),
        })
        .unwrap();
        db.with_state_mut(|s| {
            let meta = ComponentMetadata::from("img").with_image(ImageFormat::Mono16, 12);
            s.set_component_metadata(meta, &db.path)
        })
        .unwrap();
        (dir, Arc::new(db))
    }

    fn latest(db: &DB, name: &str) -> Option<Timestamp> {
        db.with_state(|s| {
            let component = s.get_component(ComponentId::new(name))?;
            Some(component.time_series.latest()?.timestamp())
        })
    }

    async fn wait_persisted(db: &DB, ts: i64) {
        for _ in 0..10_000 {
            if latest(db, "img") == Some(Timestamp(ts)) && latest(db, "f") == Some(Timestamp(ts)) {
                return;
            }
            stellarator::sleep(Duration::from_millis(1)).await;
        }
        panic!("sample {ts} never persisted");
    }

    /// Ingests samples `1..=count`, keeping the WAL from filling.
    async fn ingest(db: &DB, count: i64) {
        let mut table = vec![0u8; 16 + IMAGE_SIZE];
        for ts in 1..=count {
            fill(&mut table, ts);
            db.ingest_table(ID, &table).unwrap();
            if ts % 16 == 0 || ts == count {
                wait_persisted(db, ts).await;
            }
        }
    }

    fn serve(db: Arc<DB>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();
        let server = Server { listener, db };
        stellarator::struc_con::stellar(move || server.run());
        addr
    }

    fn query(start: i64, limit: Option<usize>) -> GetTimeSeries {
        GetTimeSeries {
            id: ID,
            range: Timestamp(start)..Timestamp(i64::MAX),
            component_id: ComponentId::new("img"),
            limit,
        }
    }

    /// Requests from `start`, checks every sample, and returns its timestamps.
    async fn page(client: &mut Client, start: i64, limit: Option<usize>) -> Vec<i64> {
        let series = client.request(&query(start, limit)).await.unwrap();
        let stamps: Vec<i64> = series.timestamps().unwrap().iter().map(|t| t.0).collect();
        let data = series.data().unwrap();
        assert_eq!(data.len(), stamps.len() * IMAGE_SIZE);
        let mut expected = vec![0u8; 16 + IMAGE_SIZE];
        for (ts, sample) in stamps.iter().zip(data.as_chunks::<IMAGE_SIZE>().0) {
            fill(&mut expected, *ts);
            assert!(sample[..] == expected[16..], "sample {ts} differs");
        }
        stamps
    }

    #[stellarator::test]
    async fn get_time_series_pages_large_samples_oldest_first() {
        let (_dir, db) = image_db();
        ingest(&db, 300).await;
        let mut client = Client::connect(serve(db)).await.unwrap();

        let mut seen = Vec::new();
        let mut sizes = Vec::new();
        let mut start = 0;
        loop {
            let stamps = page(&mut client, start, None).await;
            let Some(last) = stamps.last() else {
                break;
            };
            start = last + 1;
            sizes.push(stamps.len());
            seen.extend(stamps);
        }
        assert_eq!(sizes, [PAGE, PAGE, 300 - 2 * PAGE]);
        assert_eq!(seen, (1..=300).collect::<Vec<_>>());
    }

    /// Ingests one sample per timestamp in `stamps`, ties included.
    async fn ingest_stamps(db: &DB, stamps: &[i64]) {
        let mut table = vec![0u8; 16 + IMAGE_SIZE];
        for (n, ts) in stamps.iter().enumerate() {
            fill(&mut table, *ts);
            table[8..16].copy_from_slice(&(n as f64).to_le_bytes());
            db.ingest_table(ID, &table).unwrap();
            if n % 16 == 15 {
                wait_persisted(db, *ts).await;
            }
        }
        if let Some(last) = stamps.last() {
            wait_persisted(db, *last).await;
        }
    }

    /// The `f` samples `(timestamp, value)` a request from `start` returns.
    async fn page_f(client: &mut Client, start: i64, limit: Option<usize>) -> Vec<(i64, f64)> {
        let series = client
            .request(&GetTimeSeries {
                component_id: ComponentId::new("f"),
                ..query(start, limit)
            })
            .await
            .unwrap();
        let stamps = series.timestamps().unwrap().iter().map(|t| t.0);
        let values = series.data().unwrap().as_chunks::<8>().0.iter();
        stamps.zip(values.map(|v| f64::from_le_bytes(*v))).collect()
    }

    /// Pages from 0 by `limit`, each page after the last one's timestamp.
    async fn page_all_f(client: &mut Client, limit: usize) -> Vec<Vec<(i64, f64)>> {
        let (mut pages, mut start) = (Vec::new(), 0);
        loop {
            let page = page_f(client, start, Some(limit)).await;
            let Some((last, _)) = page.last() else {
                return pages;
            };
            start = last + 1;
            pages.push(page);
        }
    }

    #[stellarator::test]
    async fn paging_never_splits_equal_timestamps() {
        let (_dir, db) = image_db();
        ingest_stamps(&db, &[1, 2, 3, 3, 3, 4, 5]).await;
        let mut client = Client::connect(serve(db)).await.unwrap();
        let stamps = |pages: &[Vec<(i64, f64)>]| -> Vec<Vec<i64>> {
            pages
                .iter()
                .map(|p| p.iter().map(|(t, _)| *t).collect())
                .collect()
        };
        let pages = page_all_f(&mut client, 3).await;
        assert_eq!(stamps(&pages), [vec![1, 2], vec![3, 3, 3], vec![4, 5]]);
        let values: Vec<f64> = pages.iter().flatten().map(|(_, v)| *v).collect();
        assert_eq!(values, [0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        // A limit inside a tie still returns the whole tie.
        let pages = page_all_f(&mut client, 1).await;
        assert_eq!(
            stamps(&pages),
            [vec![1], vec![2], vec![3, 3, 3], vec![4], vec![5]]
        );
    }

    /// Image samples roll a node every 64, so a tie at 63 spans two.
    #[stellarator::test]
    async fn paging_keeps_a_tie_across_nodes() {
        let (_dir, db) = image_db();
        let stamps: Vec<i64> = (0..64).chain([63, 63, 64]).collect();
        ingest_stamps(&db, &stamps).await;
        let mut client = Client::connect(serve(db)).await.unwrap();
        let first = page(&mut client, 0, Some(64)).await;
        assert_eq!(first, (0..63).collect::<Vec<_>>());
        assert_eq!(page(&mut client, 63, Some(1)).await, [63, 63, 63]);
        assert_eq!(page(&mut client, 63, Some(3)).await, [63, 63, 63]);
        assert_eq!(page(&mut client, 63, None).await, [63, 63, 63, 64]);
    }

    #[stellarator::test]
    async fn a_tie_past_the_cap_is_refused() {
        let (_dir, db) = image_db();
        ingest_stamps(&db, &[1, 1, 1, 2]).await;
        let range = db.with_state(|s| {
            let f = s.get_component(ComponentId::new("f")).unwrap();
            f.time_series
                .get_range(Timestamp(0)..Timestamp(i64::MAX))
                .unwrap()
        });
        assert_eq!(whole_ties(&range, 4, 1, 3), Some(3));
        assert_eq!(whole_ties(&range, 4, 1, 2), None);
        assert_eq!(whole_ties(&range, 4, 4, 2), Some(4));
        assert_eq!(whole_ties(&range, 4, 0, 2), Some(0));
    }

    #[stellarator::test]
    async fn get_time_series_limits_and_single_samples() {
        let (_dir, db) = image_db();
        ingest(&db, 3).await;
        let mut client = Client::connect(serve(db)).await.unwrap();
        assert!(page(&mut client, 0, Some(0)).await.is_empty());
        assert_eq!(page(&mut client, 2, Some(1)).await, [2]);
        assert_eq!(page(&mut client, 3, None).await, [3]);
        assert!(page(&mut client, 4, None).await.is_empty());
    }

    /// The first table packet of a stream on `client`, after its vtable.
    async fn first_tables(client: &mut Client, count: usize) -> Vec<usize> {
        let mut lens = Vec::new();
        let mut buf = vec![0u8; 1024];
        while lens.len() < count {
            let pkt = client.rx.next_grow(buf).await.unwrap();
            if let Packet::Table(table) = &pkt {
                lens.push(table.buf.len());
            }
            buf = pkt.into_buf().into_inner();
        }
        lens
    }

    fn stream(behavior: StreamBehavior) -> Stream {
        Stream { behavior, id: 7 }
    }

    /// Ingests one more sample every 20 ms while `until` is pending, so
    /// real-time streams have something new to send.
    async fn feeding<T>(db: &DB, from: i64, until: impl Future<Output = T>) -> T {
        let feed = async {
            let mut table = vec![0u8; 16 + IMAGE_SIZE];
            let mut ts = from;
            loop {
                fill(&mut table, ts);
                db.ingest_table(ID, &table).unwrap();
                stellarator::sleep(Duration::from_millis(20)).await;
                ts += 1;
            }
        };
        futures_lite::future::or(until, feed).await
    }

    #[stellarator::test]
    async fn fixed_rate_leaves_images_out_and_real_time_keeps_them() {
        let (_dir, db) = image_db();
        ingest(&db, 2).await;
        let addr = serve(db.clone());

        let mut client = Client::connect(addr).await.unwrap();
        let fixed = stream(StreamBehavior::FixedRate(FixedRateBehavior {
            initial_timestamp: InitialTimestamp::Earliest,
            timestep: 1,
            frequency: 100,
        }));
        client.send((&fixed).with_request_id(1)).await.0.unwrap();
        assert!(
            first_tables(&mut client, 3)
                .await
                .iter()
                .all(|len| *len < 64)
        );

        let mut client = Client::connect(addr).await.unwrap();
        let live = stream(StreamBehavior::RealTime);
        client.send((&live).with_request_id(1)).await.0.unwrap();
        let mut lens = feeding(&db, 3, first_tables(&mut client, 6)).await;
        lens.sort();
        lens.dedup();
        assert_eq!(lens, [16, 8 + IMAGE_SIZE]);
    }
}
