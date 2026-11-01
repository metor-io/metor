//! The two outputs an alarm publishes its edges on.

use core::fmt;
use core::sync::atomic::AtomicU32;
use std::sync::Arc;

use super::text::{REDLINE, compose, occurrence, prefix_fits};
use super::{AlarmSpec, ClearedMsg, Edge, Id, Msg, RaisedMsg, Severity};
use crate::coordinator::MakeError;
use crate::def::DefCx;
use crate::fn_system::Instance;
use crate::port::Output;
use crate::system::OutputBinding;

pub const ALARM_RAISED_PORT: &str = "alarm_raised";
pub const ALARM_CLEARED_PORT: &str = "alarm_cleared";

/// An alarm's two outputs, its id prefix, and the records it fills on an edge.
pub struct AlarmPorts {
    raised: Output<RaisedMsg>,
    cleared: Output<ClearedMsg>,
    prefix: Id,
    raised_msg: RaisedMsg,
    cleared_msg: ClearedMsg,
}

impl AlarmPorts {
    /// Fails if `{ns}.{id}` leaves no room for a name and its hash suffix.
    pub(crate) fn bind(
        instance: &Instance,
        raised: OutputBinding,
        cleared: OutputBinding,
        dropped: &Arc<AtomicU32>,
    ) -> Result<Self, MakeError> {
        let prefix = DefCx::path(instance.namespace.as_deref(), &instance.id, "");
        Self::new(
            Output::counting(raised.writer, dropped.clone()),
            Output::counting(cleared.writer, dropped.clone()),
            &prefix,
        )
    }

    pub(crate) fn new(
        raised: Output<RaisedMsg>,
        cleared: Output<ClearedMsg>,
        prefix: &str,
    ) -> Result<Self, MakeError> {
        let too_long = || MakeError::AlarmPrefixTooLong {
            prefix: prefix.to_string(),
        };
        if !prefix_fits(prefix) {
            return Err(too_long());
        }
        Ok(Self {
            raised,
            cleared,
            prefix: Id::try_from(prefix).map_err(|_| too_long())?,
            raised_msg: RaisedMsg {
                def_id: Id::new(),
                occurrence: 0,
                severity: Severity::Info,
                value: None,
                message: Msg::default(),
            },
            cleared_msg: ClearedMsg::default(),
        })
    }

    /// Writes `edge` for `spec`. `fmt` runs only on `Raised`.
    pub(crate) fn publish_alarm(
        &mut self,
        spec: &AlarmSpec,
        edge: Edge,
        fmt: impl FnOnce(&mut Msg) -> fmt::Result,
    ) {
        self.publish(".", spec, edge, fmt);
    }

    /// As `publish_alarm`, under the id `{prefix}.redline.{id}`.
    pub(crate) fn publish_redline(
        &mut self,
        spec: &AlarmSpec,
        edge: Edge,
        fmt: impl FnOnce(&mut Msg) -> fmt::Result,
    ) {
        self.publish(REDLINE, spec, edge, fmt);
    }

    /// A failed write is counted in the shared `dropped` counter.
    fn publish(
        &mut self,
        infix: &str,
        spec: &AlarmSpec,
        edge: Edge,
        fmt: impl FnOnce(&mut Msg) -> fmt::Result,
    ) {
        let parts = [self.prefix.as_str(), infix, spec.id];
        match edge {
            Edge::Raised { raised_at } => {
                let msg = &mut self.raised_msg;
                compose(&mut msg.def_id, &parts);
                msg.occurrence = occurrence(&msg.def_id, raised_at);
                msg.severity = spec.severity;
                msg.message.clear();
                let _ = fmt(&mut msg.message);
                let _ = self.raised.write(msg);
            }
            Edge::Cleared { raised_at } => {
                let msg = &mut self.cleared_msg;
                compose(&mut msg.def_id, &parts);
                msg.occurrence = occurrence(&msg.def_id, raised_at);
                let _ = self.cleared.write(msg);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use metor_fsw_3_ring::{Config, NoWake, RingBuffer};
    use metor_proto::types::Timestamp;

    use super::*;
    use crate::port::Input;
    use crate::tests::utils::{input, pair, ring};

    struct Rig {
        ports: AlarmPorts,
        raised: Input<RaisedMsg>,
        cleared: Input<ClearedMsg>,
        _rings: (RingBuffer, RingBuffer),
    }

    fn rig(prefix: &str, depth: usize) -> Rig {
        let (r1, raised_out, raised) = pair::<RaisedMsg>(depth);
        let (r2, cleared_out, cleared) = pair::<ClearedMsg>(depth);
        Rig {
            ports: AlarmPorts::new(raised_out, cleared_out, prefix).expect("fits"),
            raised,
            cleared,
            _rings: (r1, r2),
        }
    }

    const BATT_HOT: AlarmSpec = AlarmSpec::warning("batt_hot");
    const A: AlarmSpec = AlarmSpec::info("a");

    fn raise(t: i64) -> Edge {
        Edge::Raised {
            raised_at: Timestamp(t),
        }
    }

    fn clear(t: i64) -> Edge {
        Edge::Cleared {
            raised_at: Timestamp(t),
        }
    }

    fn raised(r: &mut Rig) -> Vec<RaisedMsg> {
        r.raised.drain().flatten().collect()
    }

    fn cleared(r: &mut Rig) -> Vec<ClearedMsg> {
        r.cleared.drain().flatten().collect()
    }

    #[test]
    fn raise_then_clear() {
        let mut r = rig("ns.sys", 4);
        r.ports
            .publish_alarm(&BATT_HOT, raise(5), |m| write!(m, "batt {:.1} C", 61.0));
        r.ports.publish_alarm(&BATT_HOT, clear(5), |_| Ok(()));
        let (raised, cleared) = (raised(&mut r), cleared(&mut r));
        let occ = occurrence("ns.sys.batt_hot", Timestamp(5));
        assert_eq!(raised.len(), 1);
        assert_eq!(raised[0].def_id.as_str(), "ns.sys.batt_hot");
        assert_eq!(raised[0].occurrence, occ);
        assert_eq!(raised[0].severity, Severity::Warning);
        assert_eq!(raised[0].message.as_str(), "batt 61.0 C");
        assert_eq!(cleared.len(), 1);
        assert_eq!(
            (cleared[0].def_id.as_str(), cleared[0].occurrence),
            ("ns.sys.batt_hot", occ)
        );
    }

    #[test]
    fn redline_id() {
        let mut r = rig("t", 4);
        r.ports
            .publish_redline(&AlarmSpec::critical("x > 1"), raise(1), |_| Ok(()));
        assert_eq!(raised(&mut r)[0].def_id.as_str(), "t.redline.x > 1");
    }

    #[test]
    fn fmt_is_not_called_on_clear() {
        let mut r = rig("sys", 4);
        r.ports
            .publish_alarm(&A, clear(1), |_| panic!("formatted a clear"));
        assert_eq!(cleared(&mut r).len(), 1);
    }

    #[test]
    fn overflowing_message_is_cut() {
        let mut r = rig("sys", 4);
        let long = "x".repeat(300);
        r.ports.publish_alarm(&A, raise(1), |m| write!(m, "{long}"));
        assert_eq!(raised(&mut r)[0].message.as_str().len(), 128);
    }

    #[test]
    fn full_ring_counts_a_drop() {
        let ring_r = RingBuffer::create_in_memory(Config {
            capacity: 64,
            max_readers: 1,
        })
        .expect("a valid ring config");
        let ring_c = ring::<ClearedMsg>(1, 1);
        let dropped = Arc::new(AtomicU32::new(0));
        let instance = Instance {
            id: "sys".into(),
            ..Default::default()
        };
        let raised = OutputBinding {
            def: Output::<RaisedMsg>::def(&DefCx::empty(), ALARM_RAISED_PORT),
            writer: ring_r.writer(NoWake).expect("free writer"),
        };
        let cleared = OutputBinding {
            def: Output::<ClearedMsg>::def(&DefCx::empty(), ALARM_CLEARED_PORT),
            writer: ring_c.writer(NoWake).expect("free writer"),
        };
        let mut ports = AlarmPorts::bind(&instance, raised, cleared, &dropped).expect("fits");
        let _reader = input::<RaisedMsg>(&ring_r);
        for t in 0..8 {
            ports.publish_alarm(&A, raise(t), |_| Ok(()));
        }
        assert!(dropped.load(core::sync::atomic::Ordering::Relaxed) > 0);
    }

    #[test]
    fn long_prefix_is_refused() {
        let (_r1, raised, _) = pair::<RaisedMsg>(1);
        let (_r2, cleared, _) = pair::<ClearedMsg>(1);
        let prefix = "p".repeat(90);
        let err = AlarmPorts::new(raised, cleared, &prefix).err();
        assert_eq!(err, Some(MakeError::AlarmPrefixTooLong { prefix }));
    }
}
