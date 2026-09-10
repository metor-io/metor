//! Mirror one peer member's instance into this target's graph.
//!
//! A `Subscribe` records the peer type's outputs as its own ports
//! ([`resolve`](crate::wiring::resolve)) and runs the built-in subscriber
//! instead of the type itself: a free-running client of the peer's link that
//! writes arriving records into those ports' rings. Consumers read a mirrored
//! port exactly as they read a local one, one cycle plus network time late.

use metor_fsw_2_core::log::{LogLevel, LogPort};
use metor_fsw_2_core::{
    BindPorts, Declarations, LogOutput, MsgOut, Output, PortDesc, RingSource, System,
    SystemDescriptor, SystemOutput,
};
use metor_fsw_ring::{NoWake, Writer};
use metor_proto::types::Timestamp;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::async_system::{AsyncContext, AsyncSystem};
use crate::ir::PeerSpec;

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

    /// Publishes one disconnected status and waits for shutdown. The client
    /// that fills the mirrored ports is the next work package.
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
        output.status.publish(&PeerStatus {
            timestamp: Timestamp::now(),
            ..PeerStatus::default()
        });
        context.status().tick(0);
        context.until_cancelled(std::future::pending::<()>()).await;
    }
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
