//! One member's ground link, streamed into the gateway's embedded db.
//!
//! An `Ingest` is the client half of what a `Downlink` serves: it dials one
//! member the way a [`Subscribe`](crate::telemetry::SubscribeSystem) dials its
//! peer — the `--peer` override, both loopback families, then one bounded mDNS
//! round — checks the answering link's identity, and hands the socket to
//! [`fsw_stream`], which ingests every pushed packet into the db and forwards
//! the configured commands back up.
//!
//! The client is a plain task held as a drop guard, not an
//! [`AsyncSystem`](crate::AsyncSystem): shared state is cyclic-only, and what
//! the task needs out of the state is the `Arc<DB>`, which crosses on its own.
//! The cycle only reads the task's counters and publishes them.

use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use metor_db::{DB, remote::fsw_stream};
use metor_fsw_2_core::log::LogLevel;
use metor_fsw_2_core::{
    BuildCtx, BuildSystem, ConfigureError, CyclicSystem, Out, Output, Shared, System,
};
use metor_proto::types::{PacketId, Timestamp};
use metor_proto_stellar::{Peer, identify};
use stellarator::JoinHandleDropGuard;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use super::DbState;
use crate::telemetry::subscribe::{browse_round, check_identity, direct_candidates};

/// The wait between candidate rounds, doubling to [`BACKOFF_MAX`]; a verified
/// connection resets it. The mirror's pair.
const BACKOFF_INITIAL: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(10);

/// Wiring params of the built-in ingest (`type="Ingest"`, attached to a
/// `Db` state): one member's ground link, as the gateway dials it.
///
/// ```python
/// gw.add("plant", Ingest(db, plant_link))
/// ```
#[derive(serde::Serialize, serde::Deserialize, postcard_schema::Schema, Debug, Clone, Default)]
pub struct IngestParams {
    /// The member's `coordinator.namespace`.
    pub namespace: String,
    /// Its `TcpServer` state, by declaration name.
    pub link: String,
    /// That server's port, from its `addr`.
    pub port: u16,
    /// [`NamedMsg::NAME`](crate::NamedMsg) tokens forwarded up this link, a
    /// subset of the member's advertised set.
    #[serde(default)]
    pub commands: Vec<String>,
    /// Where the member runs, when `--peer <ns>=<host>` named it.
    #[serde(default)]
    pub host: Option<String>,
}

/// One source's gauge, published when it changes: whether the member is
/// reachable, and what has crossed. Timestamps are this target's; an ingested
/// record keeps the member's, and the two clocks are unrelated.
#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default, Clone, PartialEq)]
#[repr(C)]
#[metor_fsw(name = "source_status")]
pub struct SourceStatus {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    /// 1 while a verified connection is live.
    pub connected: u64,
    /// Connections established over the run; a member restart is +1.
    pub sessions: u64,
    /// Packets ingested over the run.
    pub packets: u64,
    /// Packets the db refused: an unknown table id, a schema mismatch.
    pub rejected: u64,
}

/// The source gauge.
#[derive(crate::SystemOutput)]
pub struct IngestOut {
    status: Output<SourceStatus>,
}

/// What the client task reports to the cycle. The two run on one thread, so
/// the atomics are for the shape rather than for contention; the cycle takes
/// the event counts and leaves the totals.
#[derive(Default)]
struct SourceCounters {
    connected: AtomicU64,
    sessions: AtomicU64,
    packets: AtomicU64,
    rejected: AtomicU64,
    /// Candidates refused since the last cycle reported them.
    refused: AtomicU64,
    /// Verified connections lost since the last cycle reported them.
    dropped: AtomicU64,
    /// Configured commands the member did not advertise, reported once per
    /// connection.
    unadvertised: AtomicU64,
}

impl SourceCounters {
    fn bump(&self, field: &AtomicU64) -> u64 {
        field.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Read and clear one event count.
    fn take(&self, field: &AtomicU64) -> u64 {
        field.swap(0, Ordering::Relaxed)
    }
}

/// Streams one member's ground link into the gateway's db.
pub struct IngestSystem {
    /// The shared db this source writes into. `None` on a detached instance
    /// ([`BuildSystem::new`]); the gateway pack's ctor attaches it.
    db: Option<Shared<DbState>>,
    params: IngestParams,
    /// The forward set, resolved from [`IngestParams::commands`] in
    /// [`configure`](BuildSystem::configure).
    commands: Vec<PacketId>,
    counters: Rc<SourceCounters>,
    /// The client task; dropping it at shutdown cancels the dial loop.
    task: Option<JoinHandleDropGuard<()>>,
    /// The last published gauge, so the frame goes out on change only.
    last: SourceStatus,
}

impl IngestSystem {
    /// Attach the shared db this source ingests into.
    pub fn attach(mut self, db: Shared<DbState>) -> Self {
        self.db = Some(db);
        self
    }
}

impl BuildSystem for IngestSystem {
    type Params = IngestParams;

    fn new(params: IngestParams) -> Self {
        Self {
            db: None,
            params,
            commands: Vec::new(),
            counters: Rc::new(SourceCounters::default()),
            task: None,
            last: SourceStatus::default(),
        }
    }

    /// Resolve the command tokens and union them into what the db
    /// advertises. This runs at create time, before the state's `start`
    /// serves the first `GetDbInfo`, so a ground client never sees a partial
    /// command set.
    fn configure(&mut self, ctx: &BuildCtx) -> Result<(), ConfigureError> {
        for token in &self.params.commands {
            let Some((_, id)) = ctx.msgs.get(token) else {
                return Err(ConfigureError::UnknownMsg {
                    name: token.clone(),
                    available: ctx.msgs.names(),
                });
            };
            if !self.commands.contains(&id) {
                self.commands.push(id);
            }
        }
        let db = self
            .db
            .as_ref()
            .expect("ingest attached to a Db state (the gateway pack's ctor)");
        db.get().add_commands(&self.commands);
        Ok(())
    }
}

impl System for IngestSystem {
    type Input = ();
    type Output = Out<IngestOut>;
    const NAME: &'static str = "ingest";

    /// Spawn the client on the coordinator's runtime, holding it as a drop
    /// guard so shutting the system down cancels the dial loop.
    fn init(&mut self, _output: &mut Self::Output) {
        let db = self
            .db
            .as_ref()
            .expect("ingest attached to a Db state (the gateway pack's ctor)")
            .get()
            .db()
            .clone();
        let params = self.params.clone();
        let commands = self.commands.clone();
        let counters = self.counters.clone();
        self.task =
            Some(stellarator::spawn(source_loop(params, commands, db, counters)).drop_guard());
    }
}

impl CyclicSystem for IngestSystem {
    /// Report what the client did since the last cycle and republish the
    /// gauge when it moved.
    fn execute(&mut self, now: Timestamp, _input: &mut (), output: &mut Self::Output) {
        let counters = &self.counters;
        let refused = counters.take(&counters.refused);
        if refused > 0 {
            output.log().fault(
                LogLevel::Warn,
                "source_identity",
                "candidate is not the configured member",
                &[
                    (
                        "source",
                        &format_args!("{}/{}", self.params.namespace, self.params.link),
                    ),
                    ("refused", &refused),
                ],
            );
        }
        let dropped = counters.take(&counters.dropped);
        if dropped > 0 {
            output.log().fault(
                LogLevel::Info,
                "source_disconnect",
                "source link dropped; reconnecting",
                &[(
                    "source",
                    &format_args!("{}/{}", self.params.namespace, self.params.link),
                )],
            );
        }
        let unadvertised = counters.take(&counters.unadvertised);
        if unadvertised > 0 {
            output.log().fault(
                LogLevel::Warn,
                "source_commands",
                "member does not advertise every configured command",
                &[
                    (
                        "source",
                        &format_args!("{}/{}", self.params.namespace, self.params.link),
                    ),
                    ("missing", &unadvertised),
                ],
            );
        }

        let status = SourceStatus {
            timestamp: now,
            connected: counters.connected.load(Ordering::Relaxed),
            sessions: counters.sessions.load(Ordering::Relaxed),
            packets: counters.packets.load(Ordering::Relaxed),
            rejected: counters.rejected.load(Ordering::Relaxed),
        };
        if status.connected != self.last.connected
            || status.sessions != self.last.sessions
            || status.packets != self.last.packets
            || status.rejected != self.last.rejected
        {
            output.status.publish(&status);
            self.last = status;
        }
    }
}

/// The client: candidates in order, an identity check on each, then
/// [`fsw_stream`] until the socket ends. Runs until the guard is dropped.
async fn source_loop(
    params: IngestParams,
    commands: Vec<PacketId>,
    db: Arc<DB>,
    counters: Rc<SourceCounters>,
) {
    let mut backoff = BACKOFF_INITIAL;
    loop {
        let mut addrs = direct_candidates(params.host.as_deref(), params.port);
        // mDNS costs its whole timeout, so the round browses only once the
        // direct addresses are spent — and never when `--peer` named one.
        let mut browsed = params.host.is_some();
        let mut connected = false;
        let mut next = 0;
        while next < addrs.len() {
            if session(&params, &commands, addrs[next], &db, &counters).await {
                connected = true;
                break;
            }
            next += 1;
            if next == addrs.len() && !browsed {
                browsed = true;
                addrs.extend(browse_round(&params.namespace, &params.link).await);
            }
        }
        if connected {
            backoff = BACKOFF_INITIAL;
        }
        stellarator::sleep(backoff).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// One connection attempt: `true` once a verified link ended, `false` when
/// nothing answered or what answered is not this source.
async fn session(
    params: &IngestParams,
    commands: &[PacketId],
    addr: SocketAddr,
    db: &Arc<DB>,
    counters: &SourceCounters,
) -> bool {
    let dialed = match identify(addr).await {
        Ok(dialed) => dialed,
        Err(err) => {
            tracing::debug!(%addr, %err, "source did not answer");
            return false;
        }
    };
    let (info, rx, tx, buf) = match dialed {
        Peer::Fsw { info, rx, tx, buf } => (info, rx, tx, buf),
        Peer::Db(_) => {
            counters.bump(&counters.refused);
            tracing::warn!(%addr, "a metor-db answered at the source's address");
            return false;
        }
    };
    if let Err(detail) = check_identity(&info, &params.namespace, &params.link) {
        counters.bump(&counters.refused);
        tracing::warn!(%addr, %detail, "source candidate refused");
        return false;
    }

    // The member is the authority on what it accepts; a configured token it
    // no longer advertises is a config drift worth one line per connection.
    let forwarded: Vec<PacketId> = commands
        .iter()
        .copied()
        .filter(|id| info.command_ids.contains(id))
        .collect();
    if forwarded.len() != commands.len() {
        counters.bump(&counters.unadvertised);
        tracing::warn!(
            %addr,
            missing = commands.len() - forwarded.len(),
            "source does not advertise every configured command",
        );
    }

    let sessions = counters.bump(&counters.sessions);
    counters.connected.store(1, Ordering::Relaxed);
    tracing::info!(
        %addr,
        source = %format_args!("{}/{}", params.namespace, params.link),
        sessions,
        "source link connected"
    );
    let err = fsw_stream(forwarded, rx, tx, buf, db, |accepted| {
        let field = if accepted {
            &counters.packets
        } else {
            &counters.rejected
        };
        field.fetch_add(1, Ordering::Relaxed);
    })
    .await;
    counters.connected.store(0, Ordering::Relaxed);
    counters.bump(&counters.dropped);
    tracing::info!(%addr, %err, "source link dropped; reconnecting");
    true
}

#[cfg(test)]
mod tests {
    use std::sync::Arc as StdArc;
    use std::sync::atomic::AtomicU64 as StdAtomicU64;

    use metor_proto::types::{IntoLenPacket, LenPacket};
    use metor_proto_stellar::PacketStream;
    use metor_proto_wkt::{LINK_PROTOCOL_VERSION, LinkInfo};
    use stellarator::io::{AsyncWrite, SplitExt};
    use stellarator::net::TcpListener;

    use super::*;
    use crate::SharedLifecycle;

    /// A self-describing msg the fake link pushes, so an ingested packet is
    /// visible in the db's message logs.
    const DATA: PacketId = [0x33, 7];

    fn params(port: u16, namespace: &str, link: &str) -> IngestParams {
        IngestParams {
            namespace: namespace.into(),
            link: link.into(),
            port,
            commands: Vec::new(),
            host: None,
        }
    }

    fn temp_db() -> (tempfile::TempDir, Arc<DB>) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let db = Arc::new(metor_db::DB::create(dir.path().join("db")).expect("the db opens"));
        (dir, db)
    }

    async fn wait_for(pred: impl Fn() -> bool, what: &str) {
        for _ in 0..400 {
            if pred() {
                return;
            }
            stellarator::sleep(Duration::from_millis(25)).await;
        }
        panic!("never saw {what}");
    }

    /// A hand-written fsw link server on its own thread: each accepted
    /// connection gets the identity and one data msg, then closes. The
    /// counter is the number of connections it has served.
    fn fake_link(
        listener: TcpListener,
        namespace: &str,
        link: &str,
        command_ids: Vec<PacketId>,
    ) -> StdArc<StdAtomicU64> {
        let accepted = StdArc::new(StdAtomicU64::new(0));
        let served = accepted.clone();
        let identity = (&LinkInfo {
            protocol_version: LINK_PROTOCOL_VERSION,
            features: 0,
            command_ids,
            namespace: Some(namespace.to_string()),
            link: link.to_string(),
        })
            .into_len_packet()
            .inner;
        stellarator::struc_con::stellar(move || async move {
            loop {
                let Ok(stream) = listener.accept().await else {
                    return;
                };
                let (rx, tx) = stream.split();
                if tx.write_all(identity.clone()).await.0.is_err() {
                    return;
                }
                let mut data = LenPacket::msg(DATA, 8);
                data.extend_from_slice(b"hello");
                let _ = tx.write_all(data.inner).await.0;
                served.fetch_add(1, Ordering::Relaxed);
                // Read one packet so the client's own probe drains, then
                // close: the drop is the disconnect the loop reconnects from.
                let mut packets = PacketStream::new(rx);
                let _ = packets.next_grow(vec![0u8; 1024]).await;
            }
        });
        accepted
    }

    fn ingested(db: &Arc<DB>) -> bool {
        db.with_state_mut(|s| s.get_or_insert_msg_log(DATA, &db.path).cloned())
            .is_ok_and(|log| {
                log.latest()
                    .and_then(|m| m.data().map(|d| d == b"hello"))
                    .unwrap_or(false)
            })
    }

    /// A verified link ingests its pushed packets and counts the session.
    #[cfg(not(miri))]
    #[stellarator::test]
    async fn a_session_ingests_a_verified_link() {
        let (_dir, db) = temp_db();
        let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();
        let _served = fake_link(listener, "plant", "link", Vec::new());

        let counters = SourceCounters::default();
        assert!(
            session(
                &params(addr.port(), "plant", "link"),
                &[],
                addr,
                &db,
                &counters
            )
            .await,
            "a verified connection ended, so the loop reconnects rather than moving on"
        );
        assert_eq!(counters.sessions.load(Ordering::Relaxed), 1);
        assert_eq!(counters.connected.load(Ordering::Relaxed), 0);
        assert_eq!(counters.dropped.load(Ordering::Relaxed), 1);
        assert!(counters.packets.load(Ordering::Relaxed) >= 1);
        let stored = db.clone();
        wait_for(move || ingested(&stored), "the pushed msg in the db").await;
    }

    /// A link answering under another identity is refused and the candidate
    /// is spent, not retried.
    #[cfg(not(miri))]
    #[stellarator::test]
    async fn a_session_refuses_another_member() {
        let (_dir, db) = temp_db();
        let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();
        let _served = fake_link(listener, "other", "link", Vec::new());

        let counters = SourceCounters::default();
        assert!(
            !session(
                &params(addr.port(), "plant", "link"),
                &[],
                addr,
                &db,
                &counters
            )
            .await
        );
        assert_eq!(counters.refused.load(Ordering::Relaxed), 1);
        assert_eq!(counters.sessions.load(Ordering::Relaxed), 0);
        assert!(!ingested(&db));
    }

    /// A metor-db at the source's address is not a link: refused, and the
    /// loop tries the next candidate.
    #[cfg(not(miri))]
    #[stellarator::test]
    async fn a_session_refuses_a_db() {
        let (_dir, db) = temp_db();
        let mut state = super::super::DbState::open(
            super::super::DbParams {
                addr: "127.0.0.1:0".parse().unwrap(),
                path: None,
                name: None,
                store: None,
                max_bytes: None,
                max_age_secs: None,
            },
            Some("gw"),
        )
        .expect("the db opens");
        let addr = state.local_addr();
        let path = state.db().path.clone();
        state.start();

        let counters = SourceCounters::default();
        assert!(
            !session(
                &params(addr.port(), "plant", "link"),
                &[],
                addr,
                &db,
                &counters
            )
            .await
        );
        assert_eq!(counters.refused.load(Ordering::Relaxed), 1);

        state.shutdown();
        let _ = std::fs::remove_dir_all(&path);
    }

    /// A configured command the member does not advertise is reported once
    /// per connection and dropped from the forwarded set.
    #[cfg(not(miri))]
    #[stellarator::test]
    async fn a_session_narrows_to_the_advertised_commands() {
        let (_dir, db) = temp_db();
        let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();
        let _served = fake_link(listener, "plant", "link", vec![[0x51, 0]]);

        let counters = SourceCounters::default();
        session(
            &params(addr.port(), "plant", "link"),
            &[[0x51, 0], [0x52, 0]],
            addr,
            &db,
            &counters,
        )
        .await;
        assert_eq!(counters.unadvertised.load(Ordering::Relaxed), 1);
    }

    /// The dial loop reconnects after a drop, counting one session per
    /// connection.
    #[cfg(not(miri))]
    #[stellarator::test]
    async fn the_loop_reconnects_after_a_drop() {
        let (_dir, db) = temp_db();
        let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();
        let _served = fake_link(listener, "plant", "link", Vec::new());

        let counters = Rc::new(SourceCounters::default());
        let mut source = params(addr.port(), "plant", "link");
        source.host = Some(addr.ip().to_string());
        let _task =
            stellarator::spawn(source_loop(source, Vec::new(), db, counters.clone())).drop_guard();
        let seen = counters.clone();
        wait_for(
            move || seen.sessions.load(Ordering::Relaxed) >= 2,
            "a second session after the drop",
        )
        .await;
    }
}
