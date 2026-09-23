//! `Db`: the embedded database, recording every record its config wires into
//! it and serving the store to whoever connects.

use core::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use futures_lite::future;
use metor_db::{DB, Server};
use metor_fsw_3_ring::Notifier;
use metor_proto::types::{PacketId, PacketTy, Timestamp, table_id};
use metor_proto_wkt::VTableMsg;
use schemars::JsonSchema;
use serde::Deserialize;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::coordinator::ParamError;
use crate::log::Log;
use crate::port::{DynInputs, Output};
use crate::record::RecordSchema;
use crate::{Frame, Stop, system};

use super::transport::{Endpoint, Transport};
use super::wire;

/// What a `Db` is configured with.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DbParams {
    /// Where the store is served; a db listens, so `connect` is refused.
    pub transport: Transport,
    /// The store's directory; unset, a fresh one under the temp dir.
    #[serde(default)]
    pub path: Option<PathBuf>,
    /// The target's telemetry namespace, which the store identifies as.
    #[serde(default)]
    pub namespace: Option<String>,
    /// This system's name.
    #[serde(default)]
    pub link: String,
    /// The messages subscribers take through this db, which `metor build` fills.
    #[serde(default)]
    pub command_ids: Vec<PacketId>,
}

/// What a `Db` reports about what it ingested, each cycle its counters change.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug, PartialEq)]
#[frame(name = "db_status")]
#[repr(C)]
pub struct DbStatus {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    pub records: u64,
    pub refused: u64,
}

impl DbStatus {
    fn changed(&self, other: &Self) -> bool {
        (self.records, self.refused) != (other.records, other.refused)
    }
}

/// Records every record its config wires into it into an embedded metor-db.
pub struct Db {
    endpoint: Option<Endpoint>,
    db: Arc<DB>,
    namespace: Option<String>,
    command_ids: Vec<PacketId>,
}

impl Db {
    /// Binds the listener and opens or creates the store, so a taken address
    /// or an unwritable path fails the build.
    pub fn new(params: DbParams) -> Result<Self, ParamError> {
        let endpoint = params.transport.bind()?;
        super::record_bound(&params.link, &endpoint);
        if endpoint.listener().is_none() {
            return Err(ParamError::Decode(
                "a db listens; it takes no `connect`".into(),
            ));
        }
        let path = params.path.unwrap_or_else(|| {
            std::env::temp_dir().join(format!("metor-db-{}-{}", params.link, std::process::id()))
        });
        let store = if path.exists() {
            DB::open(path.clone())
        } else {
            DB::create(path.clone())
        };
        let db = store.map_err(|error| {
            ParamError::Decode(format!("opening the db at `{}`: {error}", path.display()))
        })?;
        Ok(Self {
            endpoint: Some(endpoint),
            db: Arc::new(db),
            namespace: params.namespace,
            command_ids: params.command_ids,
        })
    }
}

#[system]
impl Db {
    /// Announces every port into the store, serves it, and ingests what arrives.
    async fn run(
        &mut self,
        inputs: &mut DynInputs<Notifier>,
        db_status: &mut Output<DbStatus>,
        log: &mut Log,
        stop: Stop,
    ) {
        // PANIC Safety: `run` is called once, and the endpoint is the bind's.
        let endpoint = self.endpoint.take().expect("one run per instance");
        let db = self.db.clone();
        let ids = announce(&db, inputs, log);
        db.set_identity(self.namespace.clone(), self.command_ids.clone());
        let addr = endpoint.local_addr();
        if let Some(addr) = addr {
            log.info(format!("serving {} on {addr}", db.path.display()));
        }
        let _udp = addr.map(|addr| stellarator::spawn(udp(addr, db.clone())).drop_guard());
        let _serve = stellarator::spawn(serve(endpoint, db.clone(), stop.clone())).drop_guard();
        metor_db::lod::spawn(db.clone());

        let mut status = DbStatus {
            timestamp: Timestamp(0),
            records: 0,
            refused: 0,
        };
        let mut reported = status;
        loop {
            let ready = async {
                inputs.any_ready().await;
                true
            };
            let stopped = async {
                stop.wait().await;
                false
            };
            if !future::or(stopped, ready).await {
                return;
            }
            ingest(&db, inputs, &ids, &mut status);
            status.timestamp = Timestamp::now();
            if status.changed(&reported) {
                let _ = db_status.write(&status);
                reported = status;
            }
        }
    }
}

/// One connection served from the store: the task, cancelled with its slot,
/// and the flag it frees the slot with.
struct Conn {
    open: Rc<Cell<bool>>,
    _task: stellarator::JoinHandleDropGuard<()>,
}

/// Accepts peers into a fixed set of slots, one connection each; a peer past
/// the last free slot is closed at once. Every connection ends with this
/// future.
async fn serve(endpoint: Endpoint, db: Arc<DB>, stop: Stop) {
    let Some(listener) = endpoint.listener() else {
        return;
    };
    let mut slots: Vec<Option<Conn>> = (0..endpoint.slots()).map(|_| None).collect();
    while !stop.is_set() {
        let Some(stream) = super::transport::accept(listener, &stop).await else {
            continue;
        };
        let Some(slot) = slots.iter_mut().find(|slot| !occupied(slot)) else {
            continue;
        };
        let open = Rc::new(Cell::new(true));
        let (conn_db, conn_open) = (db.clone(), open.clone());
        let task = stellarator::spawn(async move {
            metor_db::handle_conn(stream, conn_db).await;
            conn_open.set(false);
        });
        *slot = Some(Conn {
            open,
            _task: task.drop_guard(),
        });
    }
}

/// Whether a slot holds a connection that is still up.
fn occupied(slot: &Option<Conn>) -> bool {
    slot.as_ref().is_some_and(|conn| conn.open.get())
}

/// Serves the store's datagram protocol on the listener's address.
async fn udp(addr: std::net::SocketAddr, db: Arc<DB>) {
    if let Err(error) = Server::handle_udp(addr, db).await {
        tracing::warn!(%error, "db udp ended");
    }
}

/// Registers each port's vtable and metadata, or message metadata, with the
/// store, and returns the id each port's records are ingested under. A frame
/// whose table id the store already holds for another vtable gets none.
fn announce(
    db: &Arc<DB>,
    inputs: &mut DynInputs<Notifier>,
    log: &mut Log,
) -> Vec<Option<PacketId>> {
    let mut ids = Vec::new();
    for (def, _) in inputs.iter_mut() {
        let id = match &def.schema {
            RecordSchema::Frame { vtable, metadata } => {
                let id = table_id(vtable);
                let table = VTableMsg {
                    id,
                    vtable: vtable.clone(),
                };
                if let Err(error) = db.insert_vtable(table) {
                    log.fault(
                        "table_id_collision",
                        format!(
                            "port `{}` announces a table the store refuses: {error}",
                            def.name
                        ),
                    );
                    ids.push(None);
                    continue;
                }
                for component in metadata {
                    let result = db.with_state_mut(|state| {
                        state.set_component_metadata(component.clone(), &db.path)
                    });
                    if let Err(error) = result {
                        log.fault(
                            "component_metadata",
                            format!("port `{}`: {error}", def.name),
                        );
                    }
                }
                id
            }
            RecordSchema::Msg { id, name, codec } => {
                let metadata = wire::msg_metadata(name, codec);
                let result =
                    db.with_state_mut(|state| state.set_msg_metadata(*id, metadata, &db.path));
                if let Err(error) = result {
                    log.fault("msg_metadata", format!("port `{}`: {error}", def.name));
                }
                *id
            }
        };
        ids.push(Some(id));
    }
    ids
}

/// Writes every waiting record into the store, counting what it refused. A
/// port with no id drains, and its records count as refused.
fn ingest(
    db: &DB,
    inputs: &mut DynInputs<Notifier>,
    ids: &[Option<PacketId>],
    status: &mut DbStatus,
) {
    for ((def, input), id) in inputs.iter_mut().zip(ids) {
        let Some(id) = id else {
            status.refused += input.drain().count() as u64;
            continue;
        };
        let ty = def.schema.packet_ty();
        for record in input.drain() {
            let Ok(bytes) = record else { continue };
            let result = match ty {
                PacketTy::Table => db.ingest_table(*id, bytes),
                _ => db.push_msg(Timestamp::now(), *id, bytes),
            };
            match result {
                Ok(()) => status.records += 1,
                Err(_) => status.refused += 1,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use metor_fsw_3_ring::{Config, RingBuffer};
    use metor_proto::types::PacketId;

    use stellarator::net::TcpStream;

    use crate::DefCx;
    use crate::async_system::stop_pair;
    use crate::port::{Input, ring_capacity};
    use crate::record::Record;
    use crate::system::{InputBinding, SystemInputs};
    use crate::tests::utils::{Fixed, Imu};

    use super::*;

    fn ring<R: Record>() -> RingBuffer {
        RingBuffer::create_in_memory(Config {
            capacity: ring_capacity(R::MAX_LEN, 8).expect("valid capacity"),
            max_readers: 2,
        })
    }

    /// An `Imu` frame port of instance `plant` on target `a`, and a `Fixed`
    /// message port.
    fn ports(rings: &[RingBuffer; 2]) -> DynInputs<Notifier> {
        let wake = Notifier::default();
        let plant = DefCx {
            id: "plant",
            namespace: Some("a"),
            ..DefCx::empty()
        };
        let defs = [
            Input::<Imu>::def(&plant, "imu"),
            Input::<Fixed>::def(&DefCx::empty(), "cmds.fixed"),
        ];
        DynInputs::bind(
            defs.into_iter()
                .zip(rings)
                .map(|(def, ring)| InputBinding {
                    def,
                    views: vec![ring.view(wake.clone()).expect("a free slot")],
                })
                .collect(),
        )
    }

    fn produce(rings: &[RingBuffer; 2], n: i64) {
        rings[0]
            .writer(Notifier::default())
            .expect("a free writer")
            .try_write(Imu::new(n, n as f64).as_bytes())
            .expect("ring has room");
        let mut buf = [0u8; Fixed::MAX_LEN];
        let fixed = Fixed { a: 1, b: n as f64 };
        let bytes = fixed.encode(&mut buf).expect("encodes");
        rings[1]
            .writer(Notifier::default())
            .expect("a free writer")
            .try_write(bytes)
            .expect("ring has room");
    }

    fn store() -> (tempfile::TempDir, Arc<DB>) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let db = DB::create(dir.path().join("db")).expect("creates");
        (dir, Arc::new(db))
    }

    fn status() -> DbStatus {
        DbStatus {
            timestamp: Timestamp(0),
            records: 0,
            refused: 0,
        }
    }

    #[test]
    fn test_ingest_frames_and_messages() {
        let (_dir, db) = store();
        let rings = [ring::<Imu>(), ring::<Fixed>()];
        let mut inputs = ports(&rings);
        let ids = announce(&db, &mut inputs, &mut Log);
        let RecordSchema::Frame { metadata, .. } = &Input::<Imu>::def(
            &DefCx {
                id: "plant",
                namespace: Some("a"),
                ..DefCx::empty()
            },
            "imu",
        )
        .schema
        else {
            panic!("a frame")
        };
        assert_eq!(
            ids,
            vec![
                Some(table_id_of(&mut inputs)),
                Some(Fixed::schema().packet_id())
            ]
        );
        assert_eq!(metadata[0].name, "a.plant.imu.sample");

        for n in 1..=3 {
            produce(&rings, n);
        }
        let mut status = status();
        ingest(&db, &mut inputs, &ids, &mut status);
        assert_eq!((status.records, status.refused), (6, 0));

        // Pushes land in each store's write-ahead log; what is synchronous is
        // the component's newest stamp and the announced message metadata.
        let component = metadata[0].component_id;
        let newest = db.with_state(|state| {
            state
                .get_component(component)
                .map(|component| component.last_timestamp.latest())
        });
        assert_eq!(newest, Some(Timestamp(3)));
        let logged = db.with_state(|state| {
            state
                .msg_log_iter()
                .find(|(id, _)| *id == Fixed::schema().packet_id())
                .and_then(|(_, metadata)| metadata.map(|m| m.name.clone()))
        });
        assert_eq!(logged.as_deref(), Some("Fixed"));
        let name = db.with_state(|state| {
            state
                .get_component_metadata(component)
                .map(|metadata| metadata.name.clone())
        });
        assert_eq!(name.as_deref(), Some("a.plant.imu.sample"));
    }

    /// The table id a frame port announces under.
    fn table_id_of(inputs: &mut DynInputs<Notifier>) -> PacketId {
        let (def, _) = inputs.iter_mut().next().expect("a frame port");
        def.schema.packet_id()
    }

    #[test]
    fn test_colliding_table_is_refused() {
        let (_dir, db) = store();
        let rings = [ring::<Imu>(), ring::<Fixed>()];
        let mut inputs = ports(&rings);
        let id = table_id_of(&mut inputs);
        let RecordSchema::Frame { vtable, .. } = Input::<Imu>::def(&DefCx::empty(), "other").schema
        else {
            panic!("a frame")
        };
        db.insert_vtable(VTableMsg { id, vtable })
            .expect("the first vtable under an id is taken");
        let ids = announce(&db, &mut inputs, &mut Log);
        assert_eq!(
            ids[0], None,
            "a different vtable under a held id is refused"
        );
        assert!(ids[1].is_some());
        produce(&rings, 1);
        let mut status = status();
        ingest(&db, &mut inputs, &ids, &mut status);
        assert_eq!((status.records, status.refused), (1, 1));
    }

    /// A listening endpoint on a free port, with the address it bound.
    fn endpoint(slots: usize) -> (Endpoint, std::net::SocketAddr) {
        let endpoint = Transport::Listen {
            addr: "127.0.0.1:0".into(),
            max_connections: slots,
        }
        .bind()
        .expect("a free port");
        let addr = endpoint.local_addr().expect("a listener binds");
        (endpoint, addr)
    }

    /// The peer's next byte, or the error its socket closed with.
    async fn read_byte(stream: &TcpStream) -> Result<usize, stellarator::Error> {
        stream.read(vec![0u8; 1]).await.0
    }

    #[stellarator::test]
    async fn test_one_slot_refuses_a_second_client() {
        let (_dir, db) = store();
        let (endpoint, addr) = endpoint(1);
        let (_handle, stop) = stop_pair();
        let served = stellarator::spawn(serve(endpoint, db, stop)).drop_guard();

        let first = TcpStream::connect(addr).await.expect("the db accepts");
        let second = TcpStream::connect(addr)
            .await
            .expect("the listener accepts");
        assert_eq!(
            read_byte(&second).await.ok(),
            Some(0),
            "a peer past the slots is closed at once"
        );

        drop(served);
        assert_eq!(
            read_byte(&first).await.ok(),
            Some(0),
            "dropping the coordinator's guard closes what it served"
        );
    }

    #[test]
    fn test_reject_connect_transport() {
        let params = DbParams {
            transport: Transport::Connect {
                addr: "127.0.0.1:1".into(),
            },
            path: None,
            namespace: None,
            link: "db".into(),
            command_ids: Vec::new(),
        };
        let Err(ParamError::Decode(message)) = Db::new(params) else {
            panic!("a db does not dial")
        };
        assert!(message.contains("listens"), "{message}");
    }

    #[test]
    fn test_open_store_at_path() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let params = DbParams {
            transport: Transport::Listen {
                addr: "127.0.0.1:0".into(),
                max_connections: 1,
            },
            path: Some(dir.path().join("store")),
            namespace: Some("gw".into()),
            link: "db".into(),
            command_ids: vec![[1, 2]],
        };
        let db = Db::new(params).expect("a free port and a writable dir");
        assert!(dir.path().join("store").is_dir());
        assert_eq!(db.command_ids, vec![[1, 2]]);
    }
}
