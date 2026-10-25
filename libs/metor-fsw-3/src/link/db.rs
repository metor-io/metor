//! `Db`, a link that embeds a metor-db database.
//!
//! It stores every record wired into its inputs, and serves the database over
//! TCP and UDP to any client that connects, such as the panel.

use std::path::PathBuf;
use std::sync::Arc;

use futures_lite::future;
use metor_db::{DB, Server};
use metor_fsw_3_ring::Notifier;
use metor_proto::types::{PacketId, Timestamp, table_id};
use metor_proto_wkt::VTableMsg;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use stellarator::net::TcpListener;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::coordinator::MakeError;
use crate::def::RecordError;
use crate::log::Log;
use crate::port::{DynInputs, Output};
use crate::record::RecordSchema;
use crate::system::PortDef;
use crate::{Frame, Records, Stop, system};

/// The configuration for a `Db`.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct DbParams {
    /// The address the database serves clients on.
    pub listen: String,
    /// The directory the database is stored in. When unset, the database
    /// uses a fresh directory under the system temp dir.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// The target's telemetry namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// The names of the message records that subscribers receive through
    /// this database.
    #[serde(default)]
    pub commands: Vec<String>,
}

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
    listener: Option<TcpListener>,
    db: Arc<DB>,
    params: DbParams,
}

impl Db {
    /// Binds the listener and opens the database, creating it if needed.
    ///
    /// Both happen here so that a taken address or an unwritable path fails
    /// the build instead of the run.
    pub fn new(params: DbParams) -> Result<Self, MakeError> {
        let listener = TcpListener::bind(params.listen.as_str()).map_err(|e| MakeError::Bind {
            addr: params.listen.clone(),
            message: e.to_string(),
        })?;
        let port = listener.local_addr().map_or(0, |addr| addr.port());
        let path = params.path.clone().unwrap_or_else(|| {
            std::env::temp_dir().join(format!("metor-db-{}-{port}", std::process::id()))
        });
        let store = if path.exists() {
            DB::open(path.clone())
        } else {
            DB::create(path.clone())
        };
        let db = store.map_err(|error| MakeError::OpenDb {
            path: path.display().to_string(),
            message: error.to_string(),
        })?;
        Ok(Self {
            listener: Some(listener),
            db: Arc::new(db),
            params,
        })
    }
}

/// Runs the `Db` link.
///
/// Resolves its command names against the host's records, registers every
/// input port's schema with the database, starts serving it, and then writes
/// each record that arrives on an input into it. An unknown command name is
/// a fault, and the link stops.
#[system]
pub async fn db(
    #[state] s: &mut Db,
    inputs: &mut DynInputs<Notifier>,
    db_status: &mut Output<DbStatus>,
    records: &Records,
    log: &mut Log,
    stop: Stop,
) {
    let commands: Result<Vec<PacketId>, RecordError> = s
        .params
        .commands
        .iter()
        .map(|name| records.msg_id(name))
        .collect();
    let commands = match commands {
        Ok(ids) => ids,
        Err(error) => return log.fault("unknown_command", error.to_string()),
    };
    // PANIC Safety: `new` always sets the listener, and a system runs once.
    let listener = s.listener.take().expect("one run per instance");
    let db = s.db.clone();
    let sinks: Vec<Sink> = inputs
        .iter_mut()
        .map(|(def, _)| Sink::register(&db, def, log))
        .collect();
    db.set_identity(s.params.namespace.clone(), commands);
    let addr = listener.local_addr().ok();
    if let Some(addr) = addr {
        log.info(format!("serving {} on {addr}", db.path.display()));
    }
    let _udp = addr.map(|addr| {
        let db = db.clone();
        let udp = async move {
            if let Err(error) = Server::handle_udp(addr, db).await {
                tracing::warn!(%error, "db udp ended");
            }
        };
        stellarator::spawn(udp).drop_guard()
    });
    let _serve = stellarator::spawn(serve(listener, db.clone(), stop.clone())).drop_guard();
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

/// Accepts every client and serves each on its own task.
///
/// Closed connections are pruned on each accept, and dropping this future
/// closes every connection.
async fn serve(listener: TcpListener, db: Arc<DB>, stop: Stop) {
    // Dropping a guard cancels its client's task.
    let mut conns: Vec<stellarator::JoinHandleDropGuard<()>> = Vec::new();
    let accept = async {
        loop {
            let Some(stream) = accept(&listener).await else {
                continue;
            };
            conns.retain(|conn| !conn.is_finished());
            let task = stellarator::spawn(metor_db::handle_conn(stream, db.clone()));
            conns.push(task.drop_guard());
        }
    };
    futures_lite::future::or(accept, stop.wait()).await;
}

/// Accepts the next client, or returns `None` after an accept fails.
///
/// An accept failure comes from the host (for example, running out of file
/// descriptors), not from the client, so this pauses before returning
/// instead of letting the caller retry at once.
async fn accept(listener: &TcpListener) -> Option<stellarator::net::TcpStream> {
    match listener.accept().await {
        Ok(stream) => Some(stream),
        Err(error) => {
            tracing::warn!(%error, "accept failed, backing off");
            stellarator::sleep(core::time::Duration::from_millis(500)).await;
            None
        }
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

impl Sink {
    /// Registers one input port with the database.
    ///
    /// A frame port registers its vtable and component metadata. A message
    /// port registers its message metadata. If the database already holds a
    /// different vtable under the frame's table id, the port is refused and a
    /// fault is logged.
    fn register(db: &DB, def: &PortDef, log: &mut Log) -> Sink {
        match &def.schema {
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
                    return Sink::Refused;
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
        }
    }
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
    use core::time::Duration;
    use std::net::SocketAddr;

    use metor_fsw_3_ring::RingBuffer;
    use metor_proto::types::{IntoLenPacket, Msg, msg_id};
    use metor_proto_wkt::{GetDbInfo, SequenceCommand};
    use stellarator::io::AsyncWrite;
    use stellarator::net::TcpStream;

    use crate::DefCx;
    use crate::port::Input;
    use crate::record::{Payload, Record};
    use crate::system::InputBinding;
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
        DynInputs::new(
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
        let sinks = sinks(&db, &mut inputs);
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
        let sinks = sinks(&db, &mut inputs);
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

    /// Binds a listener on a free port and returns it with its address.
    fn listener() -> (TcpListener, SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a free port");
        let addr = listener.local_addr().expect("a listener binds");
        (listener, addr)
    }

    /// Reads one byte from the peer, or returns the error the socket closed
    /// with.
    async fn read_byte(stream: &TcpStream) -> Result<usize, stellarator::Error> {
        stream.read(vec![0u8; 1]).await.0
    }

    /// Returns true if the peer closes `stream` within a second. Bytes still
    /// in flight are read off first.
    async fn closes(stream: &TcpStream) -> bool {
        let drained = async {
            while let Ok(1) = read_byte(stream).await {}
            true
        };
        let late = async {
            stellarator::sleep(Duration::from_secs(1)).await;
            false
        };
        future::or(drained, late).await
    }

    /// Connects a client and waits for the db to answer a `GetDbInfo`.
    async fn served_client(addr: SocketAddr) -> TcpStream {
        let client = TcpStream::connect(addr).await.expect("the db accepts");
        let request = (&GetDbInfo).into_len_packet().inner;
        client
            .write_all(request)
            .await
            .0
            .expect("the request is sent");
        assert_eq!(read_byte(&client).await.ok(), Some(1), "the db answers");
        client
    }

    #[stellarator::test]
    async fn test_serve_every_client_until_dropped() {
        let (_dir, db) = store();
        let (listener, addr) = listener();
        let (_handle, stop) = Stop::pair();
        let served = stellarator::spawn(serve(listener, db, stop)).drop_guard();

        let mut clients = Vec::new();
        for _ in 0..12 {
            clients.push(served_client(addr).await);
        }
        drop(clients.remove(0));
        clients.push(served_client(addr).await);

        drop(served);
        for client in &clients {
            assert!(
                closes(client).await,
                "dropping the coordinator's guard closes what it served"
            );
        }
    }

    #[test]
    fn test_open_store_at_path() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let params = DbParams {
            listen: "127.0.0.1:0".into(),
            path: Some(dir.path().join("store")),
            namespace: Some("gw".into()),
            commands: Vec::new(),
        };
        Db::new(params).expect("a free port and a writable dir");
        assert!(dir.path().join("store").is_dir());
    }

    #[test]
    fn test_reject_an_unassignable_address() {
        let params = DbParams {
            listen: "203.0.113.1:9".into(),
            path: None,
            namespace: None,
            commands: Vec::new(),
        };
        let Err(MakeError::Bind { addr, .. }) = Db::new(params) else {
            panic!("an unassignable address is an error")
        };
        assert_eq!(addr, "203.0.113.1:9");
    }

    /// The records a host knows: `sequence_command`, whose id hashes its type
    /// name, a `Fixed` message, and an `Imu` frame.
    fn records() -> Records {
        let cx = DefCx::empty();
        Records::from_port_defs(
            [
                Input::<SequenceCommand>::def(&cx, "cmd"),
                Input::<Fixed>::def(&cx, "fixed"),
                Input::<Imu>::def(&cx, "imu"),
            ]
            .iter(),
        )
    }

    /// Registers each of `inputs` with `db`.
    fn sinks(db: &DB, inputs: &mut DynInputs<Notifier>) -> Vec<Sink> {
        inputs
            .iter_mut()
            .map(|(def, _)| Sink::register(db, def, &mut Log))
            .collect()
    }

    fn command_ids(records: &Records, names: &[&str]) -> Result<Vec<PacketId>, RecordError> {
        names.iter().map(|name| records.msg_id(name)).collect()
    }

    #[test]
    fn test_commands_resolve_to_record_ids() {
        let names = [SequenceCommand::NAME, Fixed::NAME];
        let ids = command_ids(&records(), &names).expect("known messages");
        assert_eq!(
            ids,
            [<SequenceCommand as Msg>::ID, Fixed::schema().packet_id()]
        );
        assert_ne!(
            ids[0],
            msg_id(SequenceCommand::NAME),
            "the id is the record's, not a hash of its name"
        );
        assert_eq!(command_ids(&records(), &[]), Ok(Vec::new()));
    }

    #[test]
    fn test_unknown_or_frame_command_is_an_error() {
        let unknown = command_ids(&records(), &["nope"]);
        assert_eq!(unknown, Err(RecordError::Unknown("nope".into())));
        let frame = command_ids(&records(), &[Imu::NAME]);
        assert_eq!(frame, Err(RecordError::Frame(Imu::NAME.into())));
    }
}
