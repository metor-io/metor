//! `Db`, a link that embeds a metor-db database.
//!
//! It stores every record wired into its inputs, and serves the database over
//! TCP and UDP to any client that connects, such as the panel.

use core::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use futures_lite::future;
use metor_db::{DB, Server};
use metor_fsw_3_ring::Notifier;
use metor_proto::types::{PacketId, Timestamp, table_id};
use metor_proto_wkt::VTableMsg;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::coordinator::ParamError;
use crate::log::Log;
use crate::port::{DynInputs, Output};
use crate::record::RecordSchema;
use crate::{Frame, Stop, system};

use super::DbParams;
use super::transport::Endpoint;

/// Ingest counters for a `Db`. It writes one on every cycle where a counter
/// changed.
#[derive(
    Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug, Default, PartialEq,
)]
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
        Self {
            timestamp: other.timestamp,
            ..*self
        } != *other
    }
}

/// A link that stores every record wired into it in an embedded metor-db.
pub struct Db {
    endpoint: Option<Endpoint>,
    db: Arc<DB>,
    params: DbParams,
}

impl Db {
    /// Binds the listener and opens the database, creating it if needed.
    ///
    /// Both happen here so that a taken address or an unwritable path fails
    /// the build instead of the run.
    pub fn new(params: DbParams) -> Result<Self, ParamError> {
        let endpoint = params.link.transport.bind()?;
        if endpoint.listener().is_none() {
            return Err(ParamError::Decode(
                "a db listens; it takes no `connect`".into(),
            ));
        }
        let path = params.path.clone().unwrap_or_else(|| {
            std::env::temp_dir().join(format!(
                "metor-db-{}-{}",
                params.link.link,
                std::process::id()
            ))
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
            params,
        })
    }
}

/// Runs the `Db` link.
///
/// Registers every input port's schema with the database, starts serving it,
/// and then writes each record that arrives on an input into it.
#[system]
pub async fn db(
    #[state] s: &mut Db,
    inputs: &mut DynInputs<Notifier>,
    db_status: &mut Output<DbStatus>,
    log: &mut Log,
    stop: Stop,
) {
    // PANIC Safety: `new` always sets the endpoint, and a system runs once.
    let endpoint = s.endpoint.take().expect("one run per instance");
    let db = s.db.clone();
    let sinks = register(&db, inputs, log);
    db.set_identity(
        s.params.link.namespace.clone(),
        s.params.command_ids.clone(),
    );
    let addr = endpoint.local_addr();
    if let Some(addr) = addr {
        log.info(format!("serving {} on {addr}", db.path.display()));
    }
    let _udp = addr.map(|addr| stellarator::spawn(udp(addr, db.clone())).drop_guard());
    let _serve = stellarator::spawn(serve(endpoint, db.clone(), stop.clone())).drop_guard();
    metor_db::lod::spawn(db.clone());

    let mut status = DbStatus::default();
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
        ingest(&db, inputs, &sinks, &mut status);
        status.timestamp = Timestamp::now();
        if status.changed(&reported) {
            let _ = db_status.write(&status);
            reported = status;
        }
    }
}

/// One client connection to the database.
///
/// Dropping a `Conn` cancels its task. The task clears `open` when the client
/// disconnects, which marks the slot free.
struct Conn {
    open: Rc<Cell<bool>>,
    _task: stellarator::JoinHandleDropGuard<()>,
}

/// Accepts clients into a fixed number of slots, one connection per slot.
///
/// A client that arrives when every slot is taken is closed right away. The
/// slots hold each connection's task, so dropping this future closes every
/// connection.
async fn serve(endpoint: Endpoint, db: Arc<DB>, stop: Stop) {
    let Some(listener) = endpoint.listener() else {
        return;
    };
    let mut slots: Vec<Option<Conn>> = (0..endpoint.slots()).map(|_| None).collect();
    let accept = async {
        loop {
            let Some(stream) = super::transport::accept(listener).await else {
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
    };
    futures_lite::future::or(accept, stop.wait()).await;
}

/// Returns true if `slot` holds a connection that is still open.
fn occupied(slot: &Option<Conn>) -> bool {
    slot.as_ref().is_some_and(|conn| conn.open.get())
}

/// Serves the database's UDP protocol on the same address as the listener.
async fn udp(addr: std::net::SocketAddr, db: Arc<DB>) {
    if let Err(error) = Server::handle_udp(addr, db).await {
        tracing::warn!(%error, "db udp ended");
    }
}

/// Where the records from one input port are stored.
enum Sink {
    /// Frames, stored under this table id.
    Table(PacketId),
    /// Messages with this id.
    Msg(PacketId),
    /// The database rejected this port's schema, so its records are dropped.
    Refused,
}

/// Registers each input port with the database.
///
/// A frame port registers its vtable and component metadata. A message port
/// registers its message metadata. If the database already holds a different
/// vtable under the frame's table id, the port is refused and a fault is
/// logged. Returns one [`Sink`] per port.
fn register(db: &DB, inputs: &mut DynInputs<Notifier>, log: &mut Log) -> Vec<Sink> {
    let mut sinks = Vec::new();
    for (def, _) in inputs.iter_mut() {
        sinks.push(match &def.schema {
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
                            "port `{}` registers a table the store refuses: {error}",
                            def.name
                        ),
                    );
                    sinks.push(Sink::Refused);
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
                Sink::Table(id)
            }
            RecordSchema::Msg { id, name, codec } => {
                let metadata = codec.msg_metadata(name);
                let result =
                    db.with_state_mut(|state| state.set_msg_metadata(*id, metadata, &db.path));
                if let Err(error) = result {
                    log.fault("msg_metadata", format!("port `{}`: {error}", def.name));
                }
                Sink::Msg(*id)
            }
        });
    }
    sinks
}

/// Writes every waiting record into the database and updates the counters.
///
/// Records on a refused port are still drained, and each one counts as
/// refused.
fn ingest(db: &DB, inputs: &mut DynInputs<Notifier>, sinks: &[Sink], status: &mut DbStatus) {
    for ((_, input), sink) in inputs.iter_mut().zip(sinks) {
        for record in input.drain() {
            let stored = match (sink, record) {
                (Sink::Refused, _) => false,
                (_, Err(_)) => continue,
                (Sink::Table(id), Ok(bytes)) => db.ingest_table(*id, bytes).is_ok(),
                (Sink::Msg(id), Ok(bytes)) => db.push_msg(Timestamp::now(), *id, bytes).is_ok(),
            };
            match stored {
                true => status.records += 1,
                false => status.refused += 1,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use metor_fsw_3_ring::RingBuffer;
    use stellarator::net::TcpStream;

    use crate::DefCx;
    use crate::async_system::stop_pair;
    use crate::link::{LinkParams, Transport};
    use crate::port::Input;
    use crate::record::Record;
    use crate::system::{InputBinding, Ports};
    use crate::tests::utils::{Fixed, Imu, ring};

    use super::*;

    /// Builds two input ports: an `Imu` frame port for instance `plant` on
    /// target `a`, and a `Fixed` message port.
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

    #[test]
    fn test_ingest_frames_and_messages() {
        let (_dir, db) = store();
        let rings = [ring::<Imu>(8, 2), ring::<Fixed>(8, 2)];
        let mut inputs = ports(&rings);
        let (imu, _) = inputs.iter_mut().next().expect("a frame port");
        let RecordSchema::Frame { metadata, .. } = imu.schema.clone() else {
            panic!("a frame")
        };
        let sinks = register(&db, &mut inputs, &mut Log);
        assert!(matches!(sinks[0], Sink::Table(_)), "a table sink");
        assert!(matches!(sinks[1], Sink::Msg(id) if id == Fixed::schema().packet_id()));
        assert_eq!(metadata[0].name, "a.plant.imu.sample");

        for n in 1..=3 {
            produce(&rings, n);
        }
        let mut status = DbStatus::default();
        ingest(&db, &mut inputs, &sinks, &mut status);
        assert_eq!((status.records, status.refused), (6, 0));

        // Pushed records land in the store's write-ahead log, which this test
        // does not read. The component's latest timestamp and the message
        // metadata update synchronously, so the test checks those.
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

    #[test]
    fn test_colliding_table_is_refused() {
        let (_dir, db) = store();
        let rings = [ring::<Imu>(8, 2), ring::<Fixed>(8, 2)];
        let mut inputs = ports(&rings);
        let id = inputs
            .iter_mut()
            .next()
            .expect("a frame port")
            .0
            .schema
            .packet_id();
        let RecordSchema::Frame { vtable, .. } = Input::<Imu>::def(&DefCx::empty(), "other").schema
        else {
            panic!("a frame")
        };
        db.insert_vtable(VTableMsg { id, vtable })
            .expect("the first vtable under an id is taken");
        let sinks = register(&db, &mut inputs, &mut Log);
        assert!(
            matches!(sinks[0], Sink::Refused),
            "a different vtable under a held id is refused"
        );
        assert!(matches!(sinks[1], Sink::Msg(_)));
        produce(&rings, 1);
        let mut status = DbStatus::default();
        ingest(&db, &mut inputs, &sinks, &mut status);
        assert_eq!((status.records, status.refused), (1, 1));
    }

    /// Binds a listening endpoint on a free port and returns it with its
    /// address.
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

    /// Reads one byte from the peer, or returns the error the socket closed
    /// with.
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
            link: LinkParams {
                transport: Transport::Connect {
                    addr: "127.0.0.1:1".into(),
                },
                namespace: None,
                link: "db".into(),
            },
            path: None,
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
            link: LinkParams {
                transport: Transport::Listen {
                    addr: "127.0.0.1:0".into(),
                    max_connections: 1,
                },
                namespace: Some("gw".into()),
                link: "db".into(),
            },
            path: Some(dir.path().join("store")),
            command_ids: vec![[1, 2]],
        };
        let db = Db::new(params).expect("a free port and a writable dir");
        assert!(dir.path().join("store").is_dir());
        assert_eq!(db.params.command_ids, vec![[1, 2]]);
    }
}
