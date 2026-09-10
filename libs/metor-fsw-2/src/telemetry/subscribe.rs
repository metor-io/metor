//! Mirror one peer member's instance into this target's graph.
//!
//! A `Subscribe` records the peer type's outputs as its own ports
//! ([`resolve`](crate::wiring::resolve)) and runs the built-in subscriber
//! instead of the type itself: a free-running client of the peer's link that
//! writes arriving records into those ports' rings. Consumers read a mirrored
//! port exactly as they read a local one, one cycle plus network time late.
//!
//! The client dials its candidates in turn ([`direct_candidates`], then
//! mDNS), checks the peer's identity, binds the announce replay to its ports
//! ([`bind`]), and copies records across until the socket drops. Packet ids
//! are per connection, so the map is rebuilt on every connect.

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::time::Duration;

use metor_fsw_2_core::log::{LogLevel, LogPort};
use metor_fsw_2_core::{
    BindPorts, Declarations, LogOutput, MsgOut, Output, PortDesc, PortSchema, RingSource, System,
    SystemDescriptor, SystemOutput, announced_covers,
};
use metor_fsw_ring::{NoWake, Writer};
use metor_proto::types::{Msg, OwnedPacket, PacketId, Timestamp};
use metor_proto::vtable::VTable;
use metor_proto_stellar::{Peer, identify};
use metor_proto_wkt::{LinkInfo, SetComponentMetadata, SetMsgMetadata, VTableMsg};
use postcard_schema::schema::owned::OwnedNamedType;
use stellarator::buf::Slice;
use stellarator::struc_con::Joinable;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::async_system::{AsyncContext, AsyncSystem};
use crate::ir::PeerSpec;

/// The wait between candidate rounds, doubling to [`BACKOFF_MAX`]; a verified
/// connection resets it. The panel's link loop uses the same pair.
const BACKOFF_INITIAL: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(10);

/// How long one mDNS round listens before the client gives up on it and
/// backs off. Paid only when the direct candidates all failed.
const BROWSE_TIMEOUT: Duration = Duration::from_secs(2);

/// The lowest link protocol the client speaks: version 2 is the one that
/// carries the namespace and link name the identity check reads.
const MIN_PROTOCOL_VERSION: u32 = 2;

/// The mirror's own gauge, published when it changes: whether the peer is
/// reachable, and what has crossed. Timestamps here are this target's; a peer
/// record keeps the peer's, and the two clocks are unrelated.
#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default, Clone, PartialEq)]
#[repr(C)]
#[metor_fsw(name = "peer_status")]
pub struct PeerStatus {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    /// 1 while a verified connection is live, 0 otherwise. A gauge like the
    /// counters beside it, and one word wide for the same reason.
    pub connected: u64,
    /// Connections established over the run; a peer restart is visible as +1.
    pub sessions: u64,
    /// Records written into mirror rings over the run.
    pub records: u64,
    /// This target's own cycle count when the last record was written.
    pub last_rx_cycle: u64,
    /// Records the mirror could not write (a full ring) or map (unannounced).
    pub dropped: u64,
}

/// The mirror's outputs: its status, its log, then one raw writer per
/// mirrored port, in [`SubscribeSystem::instance_descriptor`] order.
///
/// The two static ports bind first because the tail loop consumes every
/// remaining ring, the [`UplinkOut`](super::UplinkSystem) shape.
pub struct SubscribeOut {
    status: Output<PeerStatus>,
    log: LogPort,
    ports: Vec<Writer<NoWake>>,
}

impl SystemOutput for SubscribeOut {
    fn decls() -> Declarations {
        vec![
            PortDesc::of::<PeerStatus>(),
            PortDesc::msg_named::<crate::LogEvent>("log"),
        ]
        .into()
    }
}

impl LogOutput for SubscribeOut {
    fn log(&mut self) -> &mut LogPort {
        &mut self.log
    }
}

impl BindPorts for SubscribeOut {
    fn bind<S: RingSource>(src: &mut S) -> Self {
        let status = Output::bind(src);
        let mut log = LogPort::new(MsgOut::bind(src));
        log.set_instance(src.instance_name());
        let mut ports = Vec::new();
        while let Some((ring, data)) = src.try_next_output::<NoWake>() {
            ports.push(
                ring.writer(data)
                    .expect("a mirror ring is bound to exactly one writer at build"),
            );
        }
        Self { status, log, ports }
    }
}

/// A mirror of one peer instance: the peer it dials and the ports it fills.
///
/// Registered as an [`AsyncSystem`], so its client runs between cycles and the
/// coordinator exports what arrived at the mirror's position in the graph.
pub struct SubscribeSystem {
    peer: PeerSpec,
    ports: Vec<PortDesc>,
}

impl SubscribeSystem {
    /// A mirror of `peer` publishing `ports`, the peer type's telemetered
    /// outputs as [`resolve`](crate::wiring::resolve) selected them.
    pub(crate) fn new(peer: PeerSpec, ports: Vec<PortDesc>) -> Self {
        Self { peer, ports }
    }

    /// Where this mirror's records come from.
    pub fn peer(&self) -> &PeerSpec {
        &self.peer
    }
}

impl System for SubscribeSystem {
    type Input = ();
    type Output = SubscribeOut;
    const NAME: &'static str = "subscribe";
}

impl AsyncSystem for SubscribeSystem {
    /// The static status and log ports plus the mirrored ones, in the order
    /// [`SubscribeOut::bind`] takes them.
    fn instance_descriptor(&self) -> SystemDescriptor {
        let mut desc = Self::descriptor();
        desc.outputs.extend(self.ports.iter().cloned());
        desc
    }

    /// Dials the peer and mirrors its records for as long as the coordinator
    /// runs: one connection per verified candidate, a rebuilt packet map per
    /// connection, and the panel's backoff between rounds.
    async fn run(
        &mut self,
        context: &mut AsyncContext,
        _input: &mut Self::Input,
        output: &mut SubscribeOut,
    ) {
        if output.ports.len() != self.ports.len() {
            // One writer per mirrored port is the bind contract; a mismatch
            // means the registered descriptor and this instance diverged.
            output.log().fault(
                LogLevel::Error,
                "peer_bind_mismatch",
                "mirror ports and bound rings diverged",
                &[],
            );
        }
        let mut gauge = Gauge::default();
        gauge.publish(output);
        context.status().tick(0);

        let mut backoff = BACKOFF_INITIAL;
        let mut reported_unreachable = false;
        loop {
            let mut addrs = direct_candidates(self.peer.host.as_deref(), self.peer.port);
            // mDNS is the last candidate and costs its whole timeout, so the
            // round only browses once the direct addresses are spent.
            let mut browsed = self.peer.host.is_some();
            let mut connected = false;
            let mut next = 0;
            while next < addrs.len() {
                match session(
                    &self.peer,
                    &self.ports,
                    addrs[next],
                    context,
                    output,
                    &mut gauge,
                )
                .await
                {
                    Session::Cancelled => return,
                    Session::Rejected => next += 1,
                    Session::Dropped => {
                        connected = true;
                        break;
                    }
                }
                if next == addrs.len() && !browsed {
                    browsed = true;
                    let Some(found) = browse(&self.peer.namespace, &self.peer.link, context).await
                    else {
                        return;
                    };
                    addrs.extend(found);
                }
            }
            if connected {
                backoff = BACKOFF_INITIAL;
                reported_unreachable = false;
            } else if !reported_unreachable {
                reported_unreachable = true;
                output.log().fault(
                    LogLevel::Warn,
                    "peer_unreachable",
                    "no peer answered; retrying",
                    &[
                        (
                            "peer",
                            &format_args!("{}/{}", self.peer.namespace, self.peer.link),
                        ),
                        ("port", &self.peer.port),
                    ],
                );
                output.log().flush(Timestamp::now());
                tracing::warn!(
                    peer = %self.peer.namespace,
                    link = %self.peer.link,
                    port = self.peer.port,
                    "no peer answered; retrying"
                );
            }
            if context
                .until_cancelled(stellarator::sleep(backoff))
                .await
                .is_none()
            {
                return;
            }
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }
}

/// The mirror's counters and the copy last published, so the status frame
/// goes out on change only. The live copy keeps a zero timestamp; publishing
/// stamps it.
#[derive(Default)]
struct Gauge {
    live: PeerStatus,
    published: Option<PeerStatus>,
}

impl Gauge {
    fn publish(&mut self, output: &mut SubscribeOut) {
        if self.published.as_ref() == Some(&self.live) {
            return;
        }
        output.status.publish(&PeerStatus {
            timestamp: Timestamp::now(),
            ..self.live.clone()
        });
        self.published = Some(self.live.clone());
    }
}

/// How one connection attempt ended.
enum Session {
    /// Nothing answered, or what answered is not this mirror's peer.
    Rejected,
    /// A verified connection ended.
    Dropped,
    /// The coordinator is shutting down.
    Cancelled,
}

/// One announced table channel: its per-connection packet id, the vtable the
/// peer sends under it, and the component names that name its instance and
/// frame.
struct TableAnnounce {
    id: PacketId,
    vtable: VTable,
    components: Vec<String>,
}

/// One announced message channel: its id and the schema of its payload.
struct MsgAnnounce {
    id: PacketId,
    schema: OwnedNamedType,
}

/// One connection's announce replay, in arrival order.
#[derive(Default)]
struct Announced {
    tables: Vec<TableAnnounce>,
    msgs: Vec<MsgAnnounce>,
}

/// Where an arriving packet goes: the mirror port each announced id fills.
#[derive(Default)]
struct Routes {
    tables: HashMap<PacketId, usize>,
    msgs: HashMap<PacketId, usize>,
    /// Channels a port refused this connection; their packets are counted
    /// rather than passing unnoticed.
    refused: HashSet<PacketId>,
}

/// One connection: identity, the announce map, then records until the socket
/// ends.
async fn session(
    peer: &PeerSpec,
    ports: &[PortDesc],
    addr: SocketAddr,
    context: &mut AsyncContext,
    output: &mut SubscribeOut,
    gauge: &mut Gauge,
) -> Session {
    let dialed = match context.until_cancelled(identify(addr)).await {
        None => return Session::Cancelled,
        Some(Ok(dialed)) => dialed,
        Some(Err(err)) => {
            tracing::debug!(%addr, %err, "peer did not answer");
            return Session::Rejected;
        }
    };
    let (info, mut rx, _tx, mut buf) = match dialed {
        Peer::Fsw { info, rx, tx, buf } => (info, rx, tx, buf),
        Peer::Db(_) => {
            reject(output, addr, "a metor-db answered at the peer's address");
            return Session::Rejected;
        }
    };
    if let Err(detail) = check_identity(&info, &peer.namespace, &peer.link) {
        reject(output, addr, &detail);
        return Session::Rejected;
    }

    let mut announced = Announced::default();
    let mut routes: Option<Routes> = None;
    let mut record = Vec::new();
    loop {
        let pkt = match context.until_cancelled(rx.next_grow(buf)).await {
            None => return Session::Cancelled,
            Some(Ok(pkt)) => pkt,
            Some(Err(err)) => {
                if routes.is_none() {
                    tracing::debug!(%addr, %err, "peer closed during its announce replay");
                    return Session::Rejected;
                }
                output.log().fault(
                    LogLevel::Info,
                    "peer_disconnect",
                    "peer link dropped; reconnecting",
                    &[("addr", &addr)],
                );
                output.log().flush(Timestamp::now());
                tracing::info!(%addr, %err, "peer link dropped; reconnecting");
                gauge.live.connected = 0;
                gauge.publish(output);
                context.status().tick(0);
                return Session::Dropped;
            }
        };
        // The replay runs until the first packet that is not part of it; that
        // packet is data and is routed below like any other.
        if routes.is_none() && !take_announce(&pkt, &mut announced) {
            routes = Some(bind(peer, ports, &announced, output.log()));
            gauge.live.sessions += 1;
            gauge.live.connected = 1;
            gauge.publish(output);
            output.log().fault(
                LogLevel::Info,
                "peer_connect",
                "peer connected",
                &[("addr", &addr)],
            );
            output.log().flush(Timestamp::now());
            tracing::info!(%addr, peer = %peer.namespace, link = %peer.link, "peer connected");
            context.status().tick(0);
        }
        if let Some(routes) = &routes {
            route(&pkt, routes, output, gauge, context.cycle(), &mut record);
            gauge.publish(output);
        }
        buf = pkt.into_buf().into_inner();
    }
}

/// Refuse one candidate: the identity fault every wrong answer shares.
fn reject(output: &mut SubscribeOut, addr: SocketAddr, detail: &str) {
    output
        .log()
        .fault(LogLevel::Warn, "peer_identity", detail, &[("addr", &addr)]);
    output.log().flush(Timestamp::now());
}

/// Fold one replay packet into `announced`; `false` means the packet is not
/// part of the replay, which ends it.
fn take_announce(pkt: &OwnedPacket<Slice<Vec<u8>>>, announced: &mut Announced) -> bool {
    let OwnedPacket::Msg(m) = pkt else {
        return false;
    };
    if m.id == VTableMsg::ID {
        if let Ok(msg) = m.parse::<VTableMsg>() {
            announced.tables.push(TableAnnounce {
                id: msg.id,
                vtable: msg.vtable,
                components: Vec::new(),
            });
        }
        true
    } else if m.id == SetComponentMetadata::ID {
        // The server sends each component's metadata right behind its vtable.
        if let Ok(msg) = m.parse::<SetComponentMetadata>()
            && let Some(table) = announced.tables.last_mut()
        {
            table.components.push(msg.0.name);
        }
        true
    } else if m.id == SetMsgMetadata::ID {
        if let Ok(msg) = m.parse::<SetMsgMetadata>() {
            announced.msgs.push(MsgAnnounce {
                id: msg.id,
                schema: msg.metadata.schema,
            });
        }
        true
    } else {
        false
    }
}

/// Bind this connection's announced channels to the mirror's ports.
///
/// A table port takes the group whose components sit under
/// `<peer-ns>.<instance>.<port>`, when the announced vtable covers the port's
/// own announce form; a message port takes the announced id whose schema is
/// exactly its own. Every port that binds to nothing says so once, here.
fn bind(peer: &PeerSpec, ports: &[PortDesc], announced: &Announced, log: &mut LogPort) -> Routes {
    let prefix = format!("{}.{}", peer.namespace, peer.instance);
    let mut routes = Routes::default();
    for (index, port) in ports.iter().enumerate() {
        match &port.schema {
            PortSchema::Table { .. } => {
                let (expected, metadata) = port.announce(&prefix).expect("a table port announces");
                let head = format!("{prefix}.{}", port.name);
                // The trailing dot keeps `gps` from taking `gps_raw`.
                let nested = format!("{head}.");
                let group = announced.tables.iter().find(|table| {
                    table
                        .components
                        .first()
                        .is_some_and(|name| *name == head || name.starts_with(&nested))
                });
                let Some(group) = group else {
                    missing(log, &port.name);
                    continue;
                };
                if announced_covers(&group.vtable, &expected) {
                    routes.tables.insert(group.id, index);
                } else {
                    let differing = metadata
                        .iter()
                        .map(|m| m.name.as_str())
                        .find(|name| !group.components.iter().any(|c| c == name))
                        .unwrap_or(head.as_str());
                    mismatch(log, &port.name, differing);
                    routes.refused.insert(group.id);
                }
            }
            PortSchema::Postcard { id, schema } => {
                let Some(announce) = announced.msgs.iter().find(|msg| msg.id == *id) else {
                    missing(log, &port.name);
                    continue;
                };
                // Postcard is not self-describing and the peer is a separate
                // build, so the payload schema is compared exactly.
                match schema {
                    Some(expected) if **expected != announce.schema => {
                        mismatch(log, &port.name, &announce.schema.name);
                        routes.refused.insert(*id);
                    }
                    _ => {
                        routes.msgs.insert(*id, index);
                    }
                }
            }
        }
    }
    routes
}

/// A port the peer's announce does not carry: it looks like a peer that is
/// down for this connection, and the rest of the ports flow.
fn missing(log: &mut LogPort, port: &str) {
    log.fault(
        LogLevel::Warn,
        "peer_channel_missing",
        "peer announces no channel for this port",
        &[("port", &port)],
    );
}

/// A port the peer announces with a different shape, named by the first
/// component that differs.
fn mismatch(log: &mut LogPort, port: &str, component: &str) {
    log.fault(
        LogLevel::Warn,
        "peer_schema_mismatch",
        "peer announces this port with a different schema",
        &[("port", &port), ("component", &component)],
    );
}

/// Copy one packet into the port its id binds: a table record verbatim, a
/// message record as `id ++ payload`, the framing a local writer would have
/// left in the ring.
fn route(
    pkt: &OwnedPacket<Slice<Vec<u8>>>,
    routes: &Routes,
    output: &mut SubscribeOut,
    gauge: &mut Gauge,
    cycle: u64,
    record: &mut Vec<u8>,
) {
    let (index, bytes) = match pkt {
        OwnedPacket::Table(table) => match routes.tables.get(&table.id) {
            Some(index) => (*index, &table.buf[..]),
            None => {
                if routes.refused.contains(&table.id) {
                    gauge.live.dropped += 1;
                }
                return;
            }
        },
        OwnedPacket::Msg(msg) => match routes.msgs.get(&msg.id) {
            Some(index) => {
                record.clear();
                record.extend_from_slice(&msg.id);
                record.extend_from_slice(&msg.buf);
                (*index, &record[..])
            }
            None => {
                if routes.refused.contains(&msg.id) {
                    gauge.live.dropped += 1;
                }
                return;
            }
        },
        OwnedPacket::TimeSeries(_) => return,
    };
    let Some(writer) = output.ports.get_mut(index) else {
        return;
    };
    if writer.try_write(bytes).is_ok() {
        gauge.live.records += 1;
        gauge.live.last_rx_cycle = cycle;
    } else {
        gauge.live.dropped += 1;
    }
}

/// Whether the answering link is the one the peer spec names. `Err` carries
/// the `peer_identity` detail for a wrong version, namespace, or link.
pub(crate) fn check_identity(info: &LinkInfo, namespace: &str, link: &str) -> Result<(), String> {
    if info.protocol_version < MIN_PROTOCOL_VERSION {
        return Err(format!(
            "peer speaks link protocol {}; {MIN_PROTOCOL_VERSION} is the first with an identity",
            info.protocol_version
        ));
    }
    if info.namespace.as_deref() != Some(namespace) || info.link != link {
        return Err(format!(
            "peer here is `{}/{}`, not `{namespace}/{link}`",
            info.namespace.as_deref().unwrap_or(""),
            info.link,
        ));
    }
    Ok(())
}

/// Where to dial before mDNS: the `--peer` override alone when it is set,
/// else both loopback families on the port.
pub(crate) fn direct_candidates(host: Option<&str>, port: u16) -> Vec<SocketAddr> {
    let Some(host) = host else {
        return vec![
            SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
            SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        ];
    };
    if let Ok(addr) = host.parse::<SocketAddr>() {
        return vec![addr];
    }
    match (host, port).to_socket_addrs() {
        Ok(addrs) => addrs.collect(),
        Err(err) => {
            tracing::warn!(%host, %err, "peer host does not resolve");
            Vec::new()
        }
    }
}

/// One bounded mDNS round for the peer's link, on its own thread so a silent
/// multicast link never holds the cycle.
pub(crate) async fn browse_round(namespace: &str, link: &str) -> Vec<SocketAddr> {
    let (namespace, link) = (namespace.to_string(), link.to_string());
    let round = stellarator::struc_con::thread(move |_| {
        super::discovery::browse_peer(&namespace, &link, BROWSE_TIMEOUT)
    });
    round.join().await.unwrap_or_default()
}

/// [`browse_round`] under the system's cancellation. `None` means shutdown
/// began; a task cancelled by dropping its guard calls the round directly.
pub(crate) async fn browse(
    namespace: &str,
    link: &str,
    context: &AsyncContext,
) -> Option<Vec<SocketAddr>> {
    context.until_cancelled(browse_round(namespace, link)).await
}

#[cfg(test)]
mod tests {
    use metor_proto::types::ComponentId;

    use crate::ir::PeerSpec;
    use crate::wiring::{LoadError, Registry, WiringBuilder, resolve};
    use crate::{Coordinator, Wiring};

    /// A peer at `plant.peer:2242` offering `alarms`.
    fn peer(telemetered: bool) -> PeerSpec {
        PeerSpec {
            namespace: "plant".into(),
            link: "peer".into(),
            port: 2242,
            instance: "alarms".into(),
            telemetered,
            host: None,
        }
    }

    /// Member `fsw` mirroring the peer's built-in `Alarms`.
    fn mirror(telemetered: bool) -> Wiring {
        let mut wiring = WiringBuilder::new()
            .subscribe("alarms", "Alarms", None, peer(telemetered))
            .build();
        wiring.coordinator.namespace = Some("fsw".into());
        wiring
    }

    fn resolved(wiring: &Wiring) -> Coordinator {
        resolve(wiring, &Registry::with_builtins()).expect("the mirror resolves")
    }

    /// The mirror of a static type carries that type's message outputs plus
    /// its own status, and nothing the peer never announces.
    #[test]
    fn a_static_type_mirrors_its_outputs() {
        let coord = resolved(&mirror(true));
        let registry = coord.registry();
        for port in ["AlarmDefs", "AlarmRaised", "AlarmCleared", "peer_status"] {
            let id = ComponentId::new(&format!("fsw.alarms.{port}"));
            assert!(registry.get(id).is_some(), "the mirror publishes `{port}`");
        }
        // The framework's own ports are the mirror's, not the peer's: `log`
        // comes from this bundle and `system_status` from `push_node`.
        assert!(registry.get(ComponentId::new("fsw.alarms.log")).is_some());
        assert!(
            registry
                .get(ComponentId::new("fsw.alarms.system_status"))
                .is_some()
        );
    }

    /// `telemetered=False` keeps the mirrored ports out of this target's own
    /// downlink; the mirror's own status stays telemetered.
    #[test]
    fn an_untelemetered_mirror_clears_its_ports() {
        let coord = resolved(&mirror(false));
        let registry = coord.registry();
        let flag = |port: &str| {
            registry
                .get(ComponentId::new(&format!("fsw.alarms.{port}")))
                .expect("registered")
                .desc
                .telemetered
        };
        assert!(!flag("AlarmDefs"));
        assert!(!flag("AlarmRaised"));
        assert!(flag("peer_status"));
    }

    /// A type the registry does not declare has no descriptor to mirror.
    #[test]
    fn an_unknown_peer_type_is_rejected() {
        let mut wiring = mirror(true);
        wiring.systems[0].ty = Some("Nope".into());
        let Err(err) = resolve(&wiring, &Registry::with_builtins()) else {
            panic!("a mirror of an unregistered type cannot resolve")
        };
        assert!(
            matches!(&err, LoadError::PeerType { system, ty, artifact }
                if system == "alarms" && ty == "Nope" && artifact.is_none()),
            "{err}"
        );
    }
}

#[cfg(test)]
mod client_tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    use metor_fsw_2_core::{LogPort, MsgOut, NamedMsg, StatusPort, capacity_for};
    use metor_fsw_ring::{Config, RingBuffer, View};
    use metor_proto::types::{IntoLenPacket, LenPacket, table_id};
    use metor_proto_wkt::{LINK_PROTOCOL_VERSION, LinkInfo, LogEvent, MsgMetadata};
    use stellarator::io::{AsyncWrite as _, SplitExt as _};
    use stellarator::net::TcpListener;
    use zerocopy::IntoBytes;

    use super::*;

    /// The peer's frame, as this target mirrors it.
    #[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default, Clone)]
    #[repr(C)]
    #[metor_fsw(name = "tick")]
    struct Tick {
        #[metor_fsw(timestamp)]
        timestamp: Timestamp,
        count: f64,
    }

    /// The same frame with its one field renamed: a peer built against a
    /// different definition.
    #[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
    #[repr(C)]
    #[metor_fsw(name = "tick")]
    struct TickRenamed {
        #[metor_fsw(timestamp)]
        timestamp: Timestamp,
        ticks: f64,
    }

    #[derive(serde::Serialize, serde::Deserialize, postcard_schema::Schema, Debug, PartialEq)]
    struct Beat {
        n: u32,
    }

    impl NamedMsg for Beat {
        const NAME: &'static str = "Beat";
    }

    /// The peer's other payload, for an id announced with the wrong schema.
    #[derive(serde::Serialize, serde::Deserialize, postcard_schema::Schema)]
    struct WideBeat {
        n: u64,
    }

    const UNKNOWN_ID: PacketId = [9, 9];

    /// The id the peer announces a table port under: the hash of its prefixed vtable.
    fn tick_id(port: &PortDesc) -> PacketId {
        let (vtable, _) = port.announce("a.counter").expect("a table port");
        table_id(&vtable)
    }

    /// A peer at `a.counter`, dialed through the `--peer` override so the
    /// test's ephemeral port is the only candidate.
    fn peer_at(addr: SocketAddr) -> PeerSpec {
        PeerSpec {
            namespace: "a".into(),
            link: "peer".into(),
            port: addr.port(),
            instance: "counter".into(),
            telemetered: true,
            host: Some(addr.to_string()),
        }
    }

    fn identity(namespace: Option<&str>, link: &str, version: u32) -> Vec<u8> {
        (&LinkInfo {
            protocol_version: version,
            features: 0,
            command_ids: Vec::new(),
            namespace: namespace.map(str::to_string),
            link: link.into(),
        })
            .into_len_packet()
            .inner
    }

    /// The server's announce for one table channel: the vtable under the
    /// peer's prefix, then one metadata packet per component.
    fn table_announce(port: &PortDesc) -> Vec<u8> {
        let (vtable, metadata) = port.announce("a.counter").expect("a table port");
        let mut blob = (&VTableMsg {
            id: table_id(&vtable),
            vtable,
        })
            .into_len_packet()
            .inner;
        for m in metadata {
            blob.extend_from_slice(&(&SetComponentMetadata(m)).into_len_packet().inner);
        }
        blob
    }

    fn msg_announce(id: PacketId, schema: OwnedNamedType) -> Vec<u8> {
        (&SetMsgMetadata {
            id,
            metadata: MsgMetadata {
                name: schema.name.clone(),
                schema,
                metadata: Default::default(),
            },
        })
            .into_len_packet()
            .inner
    }

    fn schema_of<S: postcard_schema::Schema>() -> OwnedNamedType {
        OwnedNamedType::from(S::SCHEMA)
    }

    fn table_packet(id: PacketId, payload: &[u8]) -> Vec<u8> {
        let mut pkt = LenPacket::table(id, payload.len());
        pkt.extend_from_slice(payload);
        pkt.inner
    }

    fn msg_packet(id: PacketId, payload: &[u8]) -> Vec<u8> {
        let mut pkt = LenPacket::msg(id, payload.len());
        pkt.extend_from_slice(payload);
        pkt.inner
    }

    /// A hand-written peer: write `blob` to the first connection and hold the
    /// socket open, the shape `metor-proto-stellar`'s own tests use. The
    /// returned handle owns the task, so the test holds it for as long as the
    /// peer is meant to stay up.
    fn fake_peer(listener: TcpListener, blob: Vec<u8>) -> stellarator::JoinHandle<()> {
        stellarator::spawn(async move {
            let stream = listener.accept().await.expect("accept");
            let (_rx, tx) = stream.split();
            tx.write_all(blob).await.0.expect("replay");
            std::future::pending::<()>().await
        })
    }

    fn listener_on(ip: std::net::IpAddr, port: u16) -> (TcpListener, SocketAddr) {
        let listener = TcpListener::bind(SocketAddr::new(ip, port)).expect("bind");
        let addr = listener.local_addr().expect("bound");
        (listener, addr)
    }

    fn listener() -> (TcpListener, SocketAddr) {
        listener_on(Ipv4Addr::LOCALHOST.into(), 0)
    }

    fn ring_for(port: &PortDesc) -> RingBuffer {
        RingBuffer::create_in_memory(Config {
            capacity: capacity_for(port.max_size, 8),
            max_readers: 2,
        })
    }

    /// The reader's half of a mirror: the port rings, the status frame, and
    /// the log, all readable while the client runs.
    struct Probe {
        ports: Vec<View<NoWake>>,
        status_view: View<NoWake>,
        status: PeerStatus,
        log_view: View<NoWake>,
        log: Vec<LogEvent>,
    }

    impl Probe {
        /// The newest status the gauge published.
        fn status(&mut self) -> PeerStatus {
            let mut bytes = Vec::new();
            while self.status_view.try_read_into(&mut bytes).expect("ring") {
                self.status = PeerStatus::read_from_bytes(&bytes).expect("a status record");
            }
            self.status.clone()
        }

        /// Every log line the client has flushed so far.
        fn log(&mut self) -> &[LogEvent] {
            let mut bytes = Vec::new();
            while self.log_view.try_read_into(&mut bytes).expect("ring") {
                self.log
                    .push(postcard::from_bytes(&bytes[2..]).expect("a log event"));
            }
            &self.log
        }

        /// The fault line of `kind`, if the client has raised one.
        fn fault(&mut self, kind: &str) -> Option<LogEvent> {
            self.log()
                .iter()
                .find(|ev| ev.fields.iter().any(|(k, v)| k == "kind" && v == kind))
                .cloned()
        }

        /// One record off a port ring, or `None` when it is caught up.
        fn record(&mut self, index: usize) -> Option<Vec<u8>> {
            let mut bytes = Vec::new();
            self.ports[index]
                .try_read_into(&mut bytes)
                .expect("ring")
                .then_some(bytes)
        }
    }

    /// A mirror bound over in-memory rings, with no coordinator involved.
    struct Mirror {
        system: SubscribeSystem,
        output: SubscribeOut,
        probe: Probe,
    }

    fn mirror(peer: PeerSpec, ports: Vec<PortDesc>) -> Mirror {
        let rings: Vec<RingBuffer> = ports.iter().map(ring_for).collect();
        let status_ring = ring_for(&PortDesc::of::<PeerStatus>());
        let log_ring = ring_for(&PortDesc::msg_named::<LogEvent>("log"));
        let mut log = LogPort::new(MsgOut::<LogEvent, NoWake>::new(
            log_ring.writer(NoWake).expect("log writer"),
        ));
        log.set_instance("mirror");
        Mirror {
            output: SubscribeOut {
                status: Output::new(status_ring.writer(NoWake).expect("status writer")),
                log,
                ports: rings
                    .iter()
                    .map(|ring| ring.writer(NoWake).expect("port writer"))
                    .collect(),
            },
            system: SubscribeSystem::new(peer, ports),
            probe: Probe {
                ports: rings
                    .iter()
                    .map(|ring| ring.view(NoWake).expect("port view"))
                    .collect(),
                status_view: status_ring.view(NoWake).expect("status view"),
                status: PeerStatus::default(),
                log_view: log_ring.view(NoWake).expect("log view"),
                log: Vec::new(),
            },
        }
    }

    impl Mirror {
        /// Run the client until `done` holds, then cancel it: `run` returns
        /// only on shutdown, so the check races it. The cycle counter reads 7
        /// throughout, so a record's stamp is checkable.
        async fn drive(&mut self, mut done: impl FnMut(&mut Probe) -> bool) {
            let status_ring = ring_for(&PortDesc::of::<crate::SystemStatus>());
            let mut context = AsyncContext {
                cancel: stellarator::util::CancelToken::new(),
                status: StatusPort::new(Output::new(
                    status_ring.writer(NoWake).expect("system status writer"),
                )),
                cycle: Arc::new(AtomicU64::new(7)),
            };
            let mut input = ();
            let (system, output, probe) = (&mut self.system, &mut self.output, &mut self.probe);
            futures_lite::future::or(
                async { system.run(&mut context, &mut input, output).await },
                async {
                    for _ in 0..500 {
                        if done(probe) {
                            return;
                        }
                        stellarator::sleep(Duration::from_millis(4)).await;
                    }
                    panic!("the client never reached the expected state");
                },
            )
            .await;
        }
    }

    /// A verified peer's table and message records land in the mirror's rings
    /// byte for byte, and the gauge says what crossed.
    #[stellarator::test]
    async fn a_connected_peer_fills_the_mirror() {
        let (server, addr) = listener();
        let tick = Tick {
            timestamp: Timestamp(11),
            count: 3.0,
        };
        let beat = postcard::to_allocvec(&Beat { n: 5 }).expect("encodes");
        let mut blob = identity(Some("a"), "peer", LINK_PROTOCOL_VERSION);
        let port = PortDesc::of::<Tick>();
        blob.extend_from_slice(&table_announce(&port));
        blob.extend_from_slice(&msg_announce(Beat::ID, schema_of::<Beat>()));
        blob.extend_from_slice(&table_packet(tick_id(&port), tick.as_bytes()));
        blob.extend_from_slice(&msg_packet(Beat::ID, &beat));
        // A retained snapshot behind the replay, for an id no port takes.
        blob.extend_from_slice(&msg_packet(UNKNOWN_ID, b"manifest"));
        let _peer = fake_peer(server, blob);

        let mut mirror = mirror(
            peer_at(addr),
            vec![PortDesc::of::<Tick>(), PortDesc::msg::<Beat>()],
        );
        mirror.drive(|probe| probe.status().records == 2).await;

        assert_eq!(
            mirror.probe.record(0).as_deref(),
            Some(tick.as_bytes()),
            "a table record is the ring record, copied verbatim"
        );
        let mut expected = Beat::ID.to_vec();
        expected.extend_from_slice(&beat);
        assert_eq!(
            mirror.probe.record(1),
            Some(expected),
            "a message record is `id ++ payload`"
        );
        let status = mirror.probe.status();
        assert_eq!(status.connected, 1);
        assert_eq!(status.sessions, 1);
        assert_eq!(
            status.dropped, 0,
            "an unbound id behind the replay is another target's telemetry, not a drop"
        );
        assert_eq!(status.last_rx_cycle, 7, "stamped with this target's cycle");
        assert!(
            mirror.probe.fault("peer_connect").is_some(),
            "the connection is on the log"
        );
    }

    /// A peer whose frame carries a different field refuses that port, names
    /// the component, and counts what it would have taken.
    #[stellarator::test]
    async fn a_renamed_field_refuses_the_port() {
        let (server, addr) = listener();
        let mut blob = identity(Some("a"), "peer", LINK_PROTOCOL_VERSION);
        let port = PortDesc::of::<TickRenamed>();
        blob.extend_from_slice(&table_announce(&port));
        blob.extend_from_slice(&table_packet(
            tick_id(&port),
            TickRenamed::default().as_bytes(),
        ));
        let _peer = fake_peer(server, blob);

        let mut mirror = mirror(peer_at(addr), vec![PortDesc::of::<Tick>()]);
        mirror.drive(|probe| probe.status().dropped == 1).await;

        let fault = mirror
            .probe
            .fault("peer_schema_mismatch")
            .expect("the port is refused");
        assert!(
            fault
                .fields
                .contains(&("component".into(), "a.counter.tick.count".into())),
            "the fault names the first differing component: {:?}",
            fault.fields
        );
        assert_eq!(mirror.probe.status().records, 0);
        assert!(mirror.probe.record(0).is_none(), "nothing crossed");
    }

    /// A port the peer never announces is refused once and the connection
    /// stands: the other ports still flow.
    #[stellarator::test]
    async fn an_unannounced_port_is_refused_once() {
        let (server, addr) = listener();
        let beat = postcard::to_allocvec(&Beat { n: 1 }).expect("encodes");
        let mut blob = identity(Some("a"), "peer", LINK_PROTOCOL_VERSION);
        blob.extend_from_slice(&msg_announce(Beat::ID, schema_of::<Beat>()));
        blob.extend_from_slice(&msg_packet(Beat::ID, &beat));
        blob.extend_from_slice(&msg_packet(Beat::ID, &beat));
        let _peer = fake_peer(server, blob);

        let mut mirror = mirror(
            peer_at(addr),
            vec![PortDesc::of::<Tick>(), PortDesc::msg::<Beat>()],
        );
        mirror.drive(|probe| probe.status().records == 2).await;

        let missing: Vec<_> = mirror
            .probe
            .log()
            .iter()
            .filter(|ev| {
                ev.fields
                    .contains(&("kind".into(), "peer_channel_missing".into()))
            })
            .cloned()
            .collect();
        assert_eq!(missing.len(), 1, "once per connection, not per packet");
        assert!(missing[0].fields.contains(&("port".into(), "tick".into())));
        assert_eq!(mirror.probe.status().connected, 1);
    }

    /// A message id announced with a different payload schema is refused:
    /// postcard is not self-describing, so the check is exact.
    #[stellarator::test]
    async fn a_message_schema_mismatch_refuses_the_port() {
        let (server, addr) = listener();
        let mut blob = identity(Some("a"), "peer", LINK_PROTOCOL_VERSION);
        blob.extend_from_slice(&msg_announce(Beat::ID, schema_of::<WideBeat>()));
        blob.extend_from_slice(&msg_packet(
            Beat::ID,
            &postcard::to_allocvec(&WideBeat { n: 1 }).expect("encodes"),
        ));
        let _peer = fake_peer(server, blob);

        let mut mirror = mirror(peer_at(addr), vec![PortDesc::msg::<Beat>()]);
        mirror.drive(|probe| probe.status().dropped == 1).await;

        assert!(mirror.probe.fault("peer_schema_mismatch").is_some());
        assert_eq!(mirror.probe.status().records, 0);
    }

    /// A foreign deployment on the dialed port is caught by the identity, and
    /// the loop moves to the next candidate.
    #[stellarator::test]
    async fn a_wrong_identity_moves_to_the_next_candidate() {
        // Both loopback families on one port: the v6 candidate is dialed
        // first and answers for the wrong member.
        let (wrong, addr) = listener_on(Ipv6Addr::LOCALHOST.into(), 0);
        let (right, _) = listener_on(Ipv4Addr::LOCALHOST.into(), addr.port());
        let _wrong_peer = fake_peer(wrong, identity(Some("b"), "peer", LINK_PROTOCOL_VERSION));
        let tick = Tick {
            timestamp: Timestamp(3),
            count: 1.0,
        };
        let mut blob = identity(Some("a"), "peer", LINK_PROTOCOL_VERSION);
        let port = PortDesc::of::<Tick>();
        blob.extend_from_slice(&table_announce(&port));
        blob.extend_from_slice(&table_packet(tick_id(&port), tick.as_bytes()));
        let _peer = fake_peer(right, blob);

        let mut peer = peer_at(addr);
        peer.host = None;
        let mut mirror = mirror(peer, vec![PortDesc::of::<Tick>()]);
        mirror.drive(|probe| probe.status().records == 1).await;

        assert!(mirror.probe.fault("peer_identity").is_some());
        assert_eq!(mirror.probe.record(0).as_deref(), Some(tick.as_bytes()));
    }

    /// A link answers for one namespace, one link name, and a protocol new
    /// enough to carry an identity; anything else is a rejection with its
    /// reason.
    #[test]
    fn identity_takes_one_namespace_link_and_version() {
        let info = |version, namespace: Option<&str>, link: &str| LinkInfo {
            protocol_version: version,
            features: 0,
            command_ids: Vec::new(),
            namespace: namespace.map(str::to_string),
            link: link.into(),
        };
        assert!(
            check_identity(&info(LINK_PROTOCOL_VERSION, Some("a"), "peer"), "a", "peer").is_ok()
        );

        let old = check_identity(&info(1, Some("a"), "peer"), "a", "peer").unwrap_err();
        assert!(old.contains("link protocol 1"), "{old}");

        let wrong_ns = check_identity(&info(LINK_PROTOCOL_VERSION, Some("b"), "peer"), "a", "peer")
            .unwrap_err();
        assert!(wrong_ns.contains("`b/peer`"), "{wrong_ns}");

        let wrong_link = check_identity(
            &info(LINK_PROTOCOL_VERSION, Some("a"), "ground"),
            "a",
            "peer",
        )
        .unwrap_err();
        assert!(wrong_link.contains("`a/ground`"), "{wrong_link}");
    }

    /// The override is the whole list when it is set; without one both
    /// loopback families come first, in that order.
    #[test]
    fn candidates_take_the_override_alone() {
        let mut peer = peer_at(SocketAddr::from((Ipv4Addr::LOCALHOST, 2242)));
        peer.host = Some("10.0.0.5:2300".into());
        assert_eq!(
            direct_candidates(peer.host.as_deref(), peer.port),
            vec![SocketAddr::from(([10, 0, 0, 5], 2300))]
        );

        peer.host = Some("10.0.0.5".into());
        assert_eq!(
            direct_candidates(peer.host.as_deref(), peer.port),
            vec![SocketAddr::from(([10, 0, 0, 5], 2242))],
            "a bare host keeps the peer's own port"
        );

        peer.host = None;
        assert_eq!(
            direct_candidates(peer.host.as_deref(), peer.port),
            vec![
                SocketAddr::from((Ipv6Addr::LOCALHOST, 2242)),
                SocketAddr::from((Ipv4Addr::LOCALHOST, 2242)),
            ]
        );
    }
}
