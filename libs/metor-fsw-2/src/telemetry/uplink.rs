//! Relay configured command messages from the link to graph outputs.

use metor_fsw_2_core::log::{LogLevel, LogPort};
use metor_fsw_2_core::{
    BindPorts, BuildCtx, BuildSystem, ConfigureError, CyclicSystem, Declarations, LogOutput,
    MsgFanOut, NamedMsg, PortDesc, RingSource, Shared, System, SystemDescriptor, SystemOutput,
};
use metor_proto::types::{PacketId, Timestamp};

use super::LinkState;

/// Wiring parameters for the built-in uplink (`type="Uplink"`): the message
/// types to relay off the link. Each `msgs` token is a [`NamedMsg::NAME`]
/// resolved against the registry's [`MsgTable`](crate::MsgTable); the uplink
/// mints one ordinary message output port per msg, so
/// `m.route(uplink, …, msg="…")` edges resolve like any other. An empty
/// `msgs` list means the uplink relays nothing (and warns); there is no
/// built-in default set.
///
/// ```python
/// m.add("uplink", Uplink(msgs=["SequenceCommand", "AlarmAck"]))
/// ```
#[derive(serde::Serialize, serde::Deserialize, postcard_schema::Schema, Debug, Clone, Default)]
pub struct UplinkParams {
    /// The [`NamedMsg::NAME`] tokens of the messages to relay.
    #[serde(default)]
    pub msgs: Option<Vec<String>>,
}

impl BuildSystem for UplinkSystem {
    type Params = UplinkParams;

    /// Construct detached, like the downlink's.
    fn new(params: UplinkParams) -> Self {
        Self {
            unresolved: params.msgs.unwrap_or_default(),
            ..Self::new()
        }
    }

    fn configure(&mut self, ctx: &BuildCtx) -> Result<(), ConfigureError> {
        for token in std::mem::take(&mut self.unresolved) {
            let Some((name, id)) = ctx.msgs.get(&token) else {
                return Err(ConfigureError::UnknownMsg {
                    name: token,
                    available: ctx.msgs.names(),
                });
            };
            // A duplicate token folds into one port instead of a duplicate-key
            // error.
            if !self.msgs.iter().any(|&(_, existing)| existing == id) {
                self.msgs.push((name, id));
            }
        }
        Ok(())
    }
}

/// Configured message outputs and the uplink log.
///
/// The log binds first because [`MsgFanOut::bind`] consumes all remaining
/// outputs. [`UplinkSystem::instance_descriptor`] appends message ports in
/// the same order used for dispatch. They are untelemetered to avoid echoing
/// inbound commands.
pub struct UplinkOut {
    fan: MsgFanOut,
    log: LogPort,
}

impl SystemOutput for UplinkOut {
    fn decls() -> Declarations {
        vec![PortDesc::msg_named::<crate::LogEvent>("log")].into()
    }
}

impl LogOutput for UplinkOut {
    fn log(&mut self) -> &mut LogPort {
        &mut self.log
    }
}

impl BindPorts for UplinkOut {
    fn bind<S: RingSource>(src: &mut S) -> Self {
        let log = metor_fsw_2_core::MsgOut::bind(src);
        let fan = MsgFanOut::bind(src);
        let mut log = LogPort::new(log);
        log.set_instance(src.instance_name());
        Self { fan, log }
    }
}

/// Relay configured messages from [`LinkState`] to graph outputs.
///
/// Register before its consumers to deliver commands in the same cycle.
/// Payloads are forwarded unchanged; consumers decode and validate them.
#[derive(Default)]
pub struct UplinkSystem {
    /// The shared link whose inbound queue feeds the ports. `None` on a
    /// detached instance ([`BuildSystem::new`]); the builtin link pack's
    /// ctor attaches it.
    link: Option<Shared<LinkState>>,
    /// The forward set, in config order: one `(NAME, ID)` per msg. Index k is
    /// minted output port k and bound writer k, so this one list keys the
    /// dispatch and the ports.
    msgs: Vec<(&'static str, PacketId)>,
    /// Config name tokens awaiting resolution in
    /// [`configure`](BuildSystem::configure); [`with_msg`](Self::with_msg)
    /// resolves statically instead.
    unresolved: Vec<String>,
}

impl UplinkSystem {
    /// A detached uplink with an empty forward set; add msgs via
    /// [`with_msg`](Self::with_msg) or the registry's `msgs` params, and
    /// attach the link via the builtin pack (or [`attach`](Self::attach)).
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach the shared link server this uplink drains.
    pub fn attach(mut self, link: Shared<LinkState>) -> Self {
        self.link = Some(link);
        self
    }

    /// Add `M` to the forward set, the typed twin of the `msgs` config list:
    /// mints an `M`-keyed output port. Idempotent.
    pub fn with_msg<M: NamedMsg>(mut self) -> Self {
        if !self.msgs.iter().any(|&(_, id)| id == M::ID) {
            self.msgs.push((M::NAME, M::ID));
        }
        self
    }
}

impl System for UplinkSystem {
    type Input = ();
    type Output = UplinkOut;
    const NAME: &'static str = "uplink";

    /// Advertise the forward set in the link's identity packet, so a ground
    /// client knows which msg ids to send up. Init order makes this land
    /// before the downlink freezes the replay (the downlink is deferred
    /// last by its `ReceiveAll` capability); a late registration is a
    /// config defect reported here rather than silently unadvertised.
    fn init(&mut self, output: &mut UplinkOut) {
        let UplinkOut { fan, log } = output;
        if self.msgs.is_empty() {
            log.log(
                LogLevel::Warn,
                "uplink has no msgs configured; it will relay nothing",
            );
        } else if fan.len() != self.msgs.len() {
            // One writer per configured msg is the bind contract; a mismatch
            // means the registered descriptor and this instance diverged.
            log.fault(
                LogLevel::Error,
                "uplink_bind_mismatch",
                "uplink ports and msg list diverged",
                &[],
            );
        }

        let link = self
            .link
            .as_ref()
            .expect("uplink attached to a TcpServer state (the builtin link pack's ctor)");
        let ids: Vec<PacketId> = self.msgs.iter().map(|&(_, id)| id).collect();
        if link.get().add_uplink_msgs(&ids).is_err() {
            output.log().fault(
                LogLevel::Warn,
                "uplink_announced_late",
                "uplink registered after the downlink; its command set is not advertised to clients",
                &[],
            );
        }
    }
}

impl CyclicSystem for UplinkSystem {
    /// The static log shape plus one untelemetered message port per
    /// configured msg, in config order. The dispatch and the wired ports
    /// both derive from the one `msgs` list, so they cannot diverge.
    fn instance_descriptor(&self) -> SystemDescriptor {
        let mut desc = Self::descriptor();
        desc.outputs.extend(
            self.msgs
                .iter()
                .map(|&(name, id)| PortDesc::msg_dynamic(name, id).untelemetered()),
        );
        desc
    }

    /// Drain the link's inbound queue onto the minted outputs. A msg outside
    /// the configured set counts as `uplink_unroutable` (a sender/config
    /// mismatch), a full ring as `uplink_dropped`; either way the queue
    /// drains fully so a bad sender cannot wedge it, and each kind logs one
    /// line per cycle with its count.
    fn execute(&mut self, _now: Timestamp, _input: &mut (), output: &mut UplinkOut) {
        let UplinkOut { fan, log } = output;
        let link = self
            .link
            .as_ref()
            .expect("uplink attached to a TcpServer state (the builtin link pack's ctor)");
        let msgs = &self.msgs;
        let (mut dropped, mut unroutable) = (0u64, 0u64);
        link.get().drain_inbound(
            |id, payload| match msgs.iter().position(|&(_, mid)| mid == id) {
                Some(idx) => {
                    if fan.write_raw(idx, id, payload).is_err() {
                        dropped += 1;
                    }
                }
                None => unroutable += 1,
            },
        );
        if dropped > 0 {
            log.fault(
                LogLevel::Warn,
                "uplink_dropped",
                "uplink output ring full",
                &[("dropped", &dropped)],
            );
        }
        if unroutable > 0 {
            log.fault(
                LogLevel::Warn,
                "uplink_unroutable",
                "uplink msg outside the configured set",
                &[("dropped", &unroutable)],
            );
        }
    }
}
