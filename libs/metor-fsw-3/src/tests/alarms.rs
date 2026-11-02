//! A feeder and a sink for tests of alarms run in a coordinator.

use core::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use metor_proto::types::Timestamp;
use metor_proto_wkt::LogEvent;

use crate::Severity;
use crate::alarm::{ClearedMsg, RaisedMsg};
use crate::port::{Input, Output};
use crate::tests::utils::Imu;

/// An edge a sink saw, with the cycle it saw it in.
#[derive(Clone, Debug, PartialEq)]
pub enum Seen {
    Raised(i64, String, u64, Severity, String),
    Cleared(i64, String, u64),
}

/// What the test drives and what the systems saw.
#[derive(Default)]
pub struct World {
    pub temps: VecDeque<f64>,
    pub seen: Vec<Seen>,
    /// Each fault line's kind and message.
    pub faults: Vec<(String, String)>,
}

#[derive(Clone, Default)]
pub struct Shared(pub Rc<RefCell<World>>);

impl Shared {
    pub fn scripted(temps: &[f64]) -> Self {
        let shared = Self::default();
        shared.0.borrow_mut().temps = temps.iter().copied().collect();
        shared
    }

    /// The edges seen since the last call.
    pub fn take(&self) -> Vec<Seen> {
        core::mem::take(&mut self.0.borrow_mut().seen)
    }

    /// The messages of the faults of `kind` seen so far.
    pub fn faults(&self, kind: &str) -> Vec<String> {
        let world = self.0.borrow();
        let of_kind = world.faults.iter().filter(|(k, _)| k == kind);
        of_kind.map(|(_, m)| m.clone()).collect()
    }
}

/// Writes the next scripted value, or nothing once the script ends.
#[crate::system]
pub fn feeder(#[state] s: &mut Shared, batt: &mut Output<Imu>, now: Timestamp) {
    if let Some(t) = s.0.borrow_mut().temps.pop_front() {
        let _ = batt.write(&Imu::new(now.0, t));
    }
}

/// Records every alarm edge and every fault line.
#[crate::system]
pub fn sink(
    #[state] s: &mut Shared,
    raised: &mut Input<RaisedMsg>,
    cleared: &mut Input<ClearedMsg>,
    log: &mut Input<LogEvent>,
    now: Timestamp,
) {
    let mut w = s.0.borrow_mut();
    for r in raised.drain().flatten() {
        let msg = r.message.as_str().to_string();
        let id = r.def_id.to_string();
        w.seen
            .push(Seen::Raised(now.0, id, r.occurrence, r.severity, msg));
    }
    for c in cleared.drain().flatten() {
        let id = c.def_id.to_string();
        w.seen.push(Seen::Cleared(now.0, id, c.occurrence));
    }
    for line in log.drain().flatten() {
        if let Some((_, kind)) = line.fields.iter().find(|(k, _)| k == "kind") {
            w.faults.push((kind.to_string(), line.message.to_string()));
        }
    }
}

/// The occurrence an alarm `id` raised at `t` carries.
pub fn occ(id: &str, t: i64) -> u64 {
    crate::alarm::occurrence(id, Timestamp(t))
}
