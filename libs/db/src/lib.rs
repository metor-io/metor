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
use msg_log_2::MsgLog;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use smallvec::SmallVec;
use std::{
    borrow::Cow,
    collections::HashMap,
    ffi::OsStr,
    net::{SocketAddr, ToSocketAddrs},
    path::{Path, PathBuf},
    sync::{
        Arc, RwLock,
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
use zerocopy::{FromBytes, IntoBytes};

pub use error::Error;

use crate::disruptor::Disruptor;

pub mod append_log;
mod arc_ring;
mod arrow;
pub mod disruptor;
mod error;
pub mod lod;
//mod msg_log;
pub mod manifest;
pub mod msg_log_2;
pub mod remote;
pub mod seal;
pub mod store;
pub mod tiering;
//pub(crate) mod time_series;
pub mod time_series_2;
pub use msg_log_2 as msg_log;
pub use time_series_2 as time_series;

mod vtable_stream;

pub struct DB {
    pub vtable_gen: AtomicCell<u64>,
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

    msg_logs: HashMap<PacketId, MsgLog>,

    vtable_registry: registry::HashMapRegistry,
    streams: HashMap<StreamId, Arc<FixedRateStreamState>>,

    /// Raw bytes of the most recently ingested table for each dynamic vtable
    /// (keyed by vtable / [`PacketId`]). The dynamic stream forwards exactly
    /// these bytes each tick, so the producer's latest table is authoritative:
    /// a key absent from the newest table simply drops out of the next streamed
    /// packet (authoritative-latest liveness, vtable-dynamic.md §8.6).
    latest_dynamic_tables: HashMap<PacketId, Vec<u8>>,

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
        f(&mut state)
    }

    fn db_config(&self) -> DbConfig {
        self.with_state(|db| db.db_config.clone())
    }

    pub fn save_db_state(&self) -> Result<(), Error> {
        let db_state = self.db_config();
        db_state.write(self.path.join("db_state"))
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
                component.time_series.set_max_node_age(lod::lod_node_max_age());
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
            recording_cell: PlayingCell::new(db_state.recording),
            default_stream_time_step: AtomicU64::new(
                db_state.default_stream_time_step.as_nanos() as u64
            ),
            last_updated: AtomicCell::new(Timestamp(last_updated)),
            earliest_timestamp: AtomicCell::new(earliest_timestamp),
        })
    }

    pub fn insert_vtable(&self, vtable: VTableMsg) -> Result<(), Error> {
        info!(id = ?vtable.id, "inserting vtable");
        self.with_state_mut(|state| {
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
            state.vtable_registry.map.insert(vtable.id, vtable.vtable);
            Ok::<_, Error>(())
        })?;
        Ok(())
    }

    /// Sinks a table `buf` described by the registered vtable `id`, creating any
    /// concrete component lazily on first sample.
    ///
    /// Unlike the old `Table::sink` path (which drove `Decomponentize` under a
    /// read lock and dropped `frame`/`element`), this drives
    /// [`VTable::for_each_field`] directly under [`DB::with_state_mut`]: the
    /// vtable is cloned out of the registry to free the `state` borrow, then each
    /// realized field get-or-creates its component (idempotent schema check) and
    /// pushes the sample. A newly created component (or first sample into an empty
    /// series) bumps `vtable_gen` so subscribers re-derive membership. For dynamic
    /// vtables the raw table bytes are cached as the authoritative latest table the
    /// dynamic stream forwards (see [`State::latest_dynamic_tables`]).
    pub fn ingest_table(&self, id: PacketId, buf: &[u8]) -> Result<(), Error> {
        let received = Timestamp::now();
        let mut max_ts = Timestamp(i64::MIN);
        self.with_state_mut(|state| {
            // Clone the vtable out so the closure can hold `&mut state`.
            let vtable = state
                .vtable_registry
                .get(&id)
                .ok_or(Error::Impeller(metor_proto::error::Error::VTableNotFound))?
                .clone();
            let dynamic = vtable.is_dynamic();
            let mut new_series = false;
            let mut db_err: Option<Error> = None;
            let res = vtable.for_each_field(Some(buf), &mut |rf| {
                let mut sink = || -> Result<(), Error> {
                    let view = rf.view.expect("table provided to for_each_field");
                    let ts = rf.timestamp.unwrap_or(received);
                    if !state.components.contains_key(&rf.component_id) {
                        // Static fields must be pre-registered (via `insert_vtable`);
                        // only dynamic element-members are created lazily on first
                        // sample. This keeps static ingest behaviorally identical.
                        if !rf.dynamic {
                            return Err(Error::ComponentNotFound(rf.component_id));
                        }
                        // Name the keyed member by its dotted path before
                        // `insert_component` falls back to the numeric id, so the
                        // panel shows `alarm_defs.limits.<key>`. A member that
                        // reappears already has real metadata, so leave it be.
                        if !state.component_metadata.contains_key(&rf.component_id)
                            && let Some(name) = dynamic_member_name(&rf)
                        {
                            state.set_component_metadata(
                                ComponentMetadata {
                                    component_id: rf.component_id,
                                    name,
                                    metadata: Default::default(),
                                },
                                &self.path,
                            )?;
                        }
                        let schema = ComponentSchema::new(rf.ty, rf.shape);
                        state.insert_component(rf.component_id, schema, &self.path)?;
                        new_series = true;
                    }
                    let component = state
                        .components
                        .get(&rf.component_id)
                        .expect("component just inserted");
                    if component.time_series.is_empty() {
                        new_series = true;
                    }
                    component.push_buf(ts, view.as_bytes())?;
                    if ts > max_ts {
                        max_ts = ts;
                    }
                    Ok(())
                };
                match sink() {
                    Ok(()) => Ok(()),
                    Err(err) => {
                        db_err = Some(err);
                        // Sentinel to stop iteration; the real error is surfaced below.
                        Err(metor_proto::error::Error::InvalidOp)
                    }
                }
            });
            if let Some(err) = db_err {
                return Err(err);
            }
            res?;
            if dynamic {
                state.latest_dynamic_tables.insert(id, buf.to_vec());
            }
            if new_series {
                self.vtable_gen.fetch_add(1, atomic::Ordering::SeqCst);
            }
            Ok::<_, Error>(())
        })?;
        if max_ts > Timestamp(i64::MIN) {
            self.last_updated.update_max(max_ts);
        }
        Ok(())
    }

    /// Builds one streamed `Table` packet for a dynamic vtable subscription from
    /// the cached latest table bytes (see [`State::latest_dynamic_tables`]),
    /// re-headed with the subscriber's stream `id`. Returns `None` when no table
    /// has been ingested for `id` yet.
    fn dynamic_stream_packet(&self, id: PacketId) -> Option<LenPacket> {
        let cached = self.with_state(|s| s.latest_dynamic_tables.get(&id).cloned())?;
        let mut pkt = LenPacket::table(id, cached.len());
        pkt.extend_from_slice(&cached);
        Some(pkt)
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

    pub fn get_component(&self, component_id: ComponentId) -> Option<&Component> {
        self.components.get(&component_id)
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

    /// The components live streams may carry — everything not hidden.
    fn visible_components(&self) -> HashMap<ComponentId, Component> {
        self.components
            .iter()
            .filter(|(id, _)| !self.is_component_hidden(id))
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

#[derive(Clone)]
pub struct Component {
    pub component_id: ComponentId,
    pub time_series: TimeSeries,
    pub wal: Disruptor,
    pub schema: ComponentSchema,
    pub last_timestamp: Arc<AtomicCell<Timestamp>>,
}

impl Component {
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
            wal: Disruptor::new(schema.size() * 1024),
            component_id,
            time_series,
            schema,
            last_timestamp: Arc::new(AtomicCell::new(Timestamp(i64::MIN))),
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
            wal: Disruptor::new(schema.size() * 256),
            component_id,
            time_series,
            schema,
            last_timestamp: Arc::new(AtomicCell::new(last_timestamp)),
        };
        stellarator::spawn(this.persist(engine_owned));
        stellarator::spawn(this.time_series.clone().lifecycle());
        Ok(this)
    }

    pub fn persist(&self, engine_owned: bool) -> impl Future<Output = ()> + 'static {
        let mut reader = self.wal.reader();
        let time_series = self.time_series.clone();
        let msg_size = self.schema.size() + size_of::<Timestamp>();
        async move {
            // The writer claim is lazy — taken on the first live sample, not
            // at spawn — because a component that only ever receives pushed
            // sealed nodes has no live head, and `install_node` keys its
            // newer-than-head guard on the writer's existence.
            //
            // `engine_owned` components are different: the LoD engine owns
            // their writer and emits buckets directly, so we decide here, at
            // construction, never to claim it. That removes the race where
            // this lazy claim and `setup_levels` fought over the writer —
            // whichever lost degraded silently. Any WAL traffic for such a
            // component is bogus; warn-drop it.
            let mut writer = None;
            let mut warned_engine_owned = false;
            loop {
                let buf = reader.next().await;
                if engine_owned {
                    if !warned_engine_owned {
                        warn!("wal traffic for engine-owned component; dropping samples");
                        warned_engine_owned = true;
                    }
                    continue;
                }
                if writer.is_none() {
                    writer = time_series.writer();
                    if writer.is_none() {
                        warn!("component writer owned elsewhere; dropping wal samples");
                        continue;
                    }
                }
                let writer = writer.as_mut().expect("checked above");
                let mut buf = &buf[..];
                'parse: while let Some(msg) = buf.get(..msg_size) {
                    let Some(timestamp) = msg.get(..size_of::<Timestamp>()) else {
                        break 'parse;
                    };
                    let timestamp = Timestamp(i64::from_le_bytes(
                        timestamp.try_into().expect("wrong size"),
                    ));
                    let Some(data) = msg.get(size_of::<Timestamp>()..) else {
                        break 'parse;
                    };
                    if let Err(err) = writer.push_buf(timestamp, data) {
                        tracing::error!(?err, "failed to persist wal message");
                    }
                    buf = &buf[msg_size..];
                }
            }
        }
    }

    fn as_vtable_op(&self) -> Arc<OpBuilder> {
        schema(
            self.schema.prim_type,
            &self.schema.shape(),
            component(self.component_id),
        )
    }

    pub fn push_buf(&self, timestamp: Timestamp, value_buf: &[u8]) -> Result<(), Error> {
        if timestamp < self.last_timestamp.latest() {
            return Err(Error::TimeTravel);
        }
        let Ok(mut grant) = self.wal.try_grant(value_buf.len() + size_of::<Timestamp>()) else {
            let reader_count = self.wal.reader_count();
            warn!(?timestamp, ?reader_count, "skipped buf due to overflow");
            // TODO(sphw): we should probably wait here, log, or even error out
            // not sure what is best
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
                let len = range_len.min(limit.unwrap_or(usize::MAX));
                // we preallocate the space we need, because we are iterating over the range's chunks backwards
                // So we want to starting writing from the end of the buffers
                let timestamp_buf_len = len * size_of::<Timestamp>();
                let data_buf_len = len * size;
                pkt.extend_from_slice((len as u64).as_bytes());
                pkt.inner.extend(std::iter::repeat_n(
                    0u8,
                    len * size_of::<Timestamp>() + len * size,
                ));
                let pkt_len: u32 = (size_of::<u64>()
                    + timestamp_buf_len
                    + data_buf_len
                    + size_of::<PacketHeader>()) as u32;
                pkt.inner[0..size_of::<u32>()].copy_from_slice(&pkt_len.to_le_bytes());
                let pkt_mut = pkt.as_mut_packet();
                let pkt_mut_body = &mut pkt_mut.body[size_of::<u64>()..];
                let (timestamps_dest, mut data_dest) = pkt_mut_body.split_at_mut(timestamp_buf_len);
                let mut timestamps_dest = <[Timestamp]>::mut_from_bytes(timestamps_dest)
                    .expect("Failed to create mutable slice for timestamps");

                let mut to_skip = range_len - len;
                for slice in range.as_iter() {
                    let timestamps = slice.timestamps();
                    let data = slice.data();
                    let timestamps_len = timestamps.len().saturating_sub(to_skip);
                    let timestamps = &timestamps[..timestamps_len];
                    let data = &data[..timestamps_len * size];
                    to_skip = to_skip.saturating_sub(timestamps_len);
                    if timestamps.is_empty() {
                        continue;
                    }
                    let start = timestamps_dest.len() - timestamps_len;
                    let end = timestamps_dest.len();
                    timestamps_dest[start..end].copy_from_slice(timestamps);
                    timestamps_dest = &mut timestamps_dest[..start];
                    data_dest[start * size..end * size].copy_from_slice(data);
                    data_dest = &mut data_dest[..start * size];
                }

                Ok(())
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
            tx.send_msg(&DbInfoResp {
                protocol_version: NODE_PROTOCOL_VERSION,
                features: 0,
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
                let staging = store::NodeStaging::create(
                    &db.path.join(component_id.to_string()),
                    &seal,
                )?;
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
    let vtable_id: PacketId = fastrand::u16(..).to_le_bytes();
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
    let mut components = db.with_state(|state| state.visible_components());
    loop {
        if !state.wait_for_playing().await {
            return Ok(());
        }
        let start = Instant::now();
        let current_timestamp = state.current_timestamp();
        let vtable_gen = db.vtable_gen.latest();
        if vtable_gen != current_vtable_gen {
            components = db.with_state(|state| state.visible_components());
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
    use metor_proto::vtable::builder::{
        list, map, path_component, raw_field, raw_table, schema, timestamp, vtable,
    };
    use metor_proto::vtable::builder::FieldBuilder;
    use metor_proto::types::PACKET_HEADER_LEN;
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
            raw_field(8, 8, schema(PrimType::F64, &[], path_component("cpu_usage"))),
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
            assert!(s.get_component(ComponentId::new("processes.cpu_usage")).is_some());
            assert!(s.is_component_hidden(&ComponentId::new("processes.pid")));
            assert!(s.is_component_hidden(&ComponentId::new("processes.cpu_usage")));
            assert!(s.get_component(ComponentId::new("processes.0.pid")).is_none());
        });

        // 2. ingest a 2-element table -> concrete components, cache, gen bump.
        let gen_before = db.vtable_gen.latest();
        db.ingest_table(id, &list_table(&[1001, 1002], &[0.5, 0.25]))
            .unwrap();
        assert!(db.vtable_gen.latest() > gen_before, "gen must bump on new series");
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
            assert!(s.latest_dynamic_tables.contains_key(&id));
        });

        // values landed in the concrete time series
        stellarator::sleep(std::time::Duration::from_millis(100)).await;
        let pid0 = db
            .with_state(|s| s.get_component(ComponentId::new("processes.0.pid")).cloned())
            .unwrap();
        let latest = pid0.time_series.latest().expect("pid0 sample persisted");
        assert_eq!(u64::from_le_bytes(latest.data().try_into().unwrap()), 1001);

        // 3. streamed packet round-trips to the same concrete ids/values.
        let sink = round_trip(db.dynamic_stream_packet(id).unwrap(), id, &v);
        assert_eq!(sink.values[&ComponentId::new("processes.0.pid")], 1001.0);
        assert_eq!(sink.values[&ComponentId::new("processes.0.cpu_usage")], 0.5);
        assert_eq!(sink.values[&ComponentId::new("processes.1.pid")], 1002.0);
        assert_eq!(sink.values[&ComponentId::new("processes.1.cpu_usage")], 0.25);

        // 4. a NEW element appears -> gen bumps, next packet includes it.
        let gen_before = db.vtable_gen.latest();
        db.ingest_table(id, &list_table(&[1001, 1002, 1003], &[0.5, 0.25, 0.75]))
            .unwrap();
        assert!(db.vtable_gen.latest() > gen_before, "gen must bump for the new element");
        let sink = round_trip(db.dynamic_stream_packet(id).unwrap(), id, &v);
        assert_eq!(sink.values[&ComponentId::new("processes.2.pid")], 1003.0);
        assert_eq!(sink.values[&ComponentId::new("processes.2.cpu_usage")], 0.75);

        // 5. LIVENESS: fewer elements -> dropped element absent next packet.
        db.ingest_table(id, &list_table(&[1001], &[0.5])).unwrap();
        let sink = round_trip(db.dynamic_stream_packet(id).unwrap(), id, &v);
        assert_eq!(sink.values[&ComponentId::new("processes.0.pid")], 1001.0);
        assert!(
            !sink.values.contains_key(&ComponentId::new("processes.1.pid")),
            "vanished element must drop out (authoritative-latest)"
        );
        assert!(!sink.values.contains_key(&ComponentId::new("processes.2.pid")));
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
            assert!(s.get_component(ComponentId::new("processes.htop.pid")).is_some());
            assert!(s.get_component(ComponentId::new("processes.init.cpu_usage")).is_some());
            assert!(s.latest_dynamic_tables.contains_key(&id));
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
        assert_eq!(sink.values[&ComponentId::new("processes.htop.cpu_usage")], 0.5);
        assert_eq!(sink.values[&ComponentId::new("processes.init.pid")], 1002.0);
        assert_eq!(sink.values[&ComponentId::new("processes.init.cpu_usage")], 0.25);
    }

    /// A scalar-valued map (a `FrameMap<u64, N>` member) has an empty
    /// member leaf, so the keyed member is named `<container>.<key>` directly.
    #[stellarator::test]
    async fn scalar_map_member_named_by_key() {
        let dir = tempfile::tempdir().unwrap();
        let db = DB::create(dir.path().to_path_buf()).unwrap();
        let id: PacketId = [11, 0];
        let members = vec![raw_field(0, 8, schema(PrimType::U64, &[], path_component("")))];
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
