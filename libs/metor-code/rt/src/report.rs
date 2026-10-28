//! What an instance reports on its `log` after a call: one fault per fault
//! kind it raised, and one per alert that rose.

use metor_fsw_3::Log;

use crate::cx::Fault;
use crate::instance::Instance;

/// The alert states of the previous cycle, to find rising edges.
pub struct AlertEdges {
    active: Vec<bool>,
}

/// One alert that rose this cycle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rising<'a> {
    pub name: &'a str,
    pub severity: &'a str,
}

impl AlertEdges {
    pub fn new(instance: &Instance) -> AlertEdges {
        AlertEdges {
            active: vec![false; instance.desc().alerts.len()],
        }
    }

    /// Reads the alerts output and returns the alerts that rose since the
    /// last call.
    pub fn rising<'a>(&mut self, instance: &'a Instance) -> Vec<Rising<'a>> {
        let mut rose = Vec::new();
        let desc = instance.desc();
        let Some(port) = desc.alerts_port else {
            return rose;
        };
        let Some(bytes) = instance.output(port as usize) else {
            return rose;
        };
        let Some(frame) = desc
            .outputs
            .get(port as usize)
            .and_then(|p| instance.program().desc.frame_of(p))
        else {
            return rose;
        };
        for ((alert, leaf), was) in desc.alerts.iter().zip(&frame.leaves).zip(&mut self.active) {
            let now = bytes.get(leaf.offset as usize).is_some_and(|b| *b != 0);
            if now && !*was {
                rose.push(Rising {
                    name: &alert.name,
                    severity: &alert.severity,
                });
            }
            *was = now;
        }
        rose
    }
}

/// Logs the faults `instance` raised and the alerts that rose, on the
/// current thread's log port.
pub fn log_faults_and_alerts(instance: &mut Instance, edges: &mut AlertEdges) {
    for fault in Fault::of(instance.take_faults()) {
        Log.fault(fault.kind(), fault.message());
    }
    for rising in edges.rising(instance) {
        Log.fault("alert", format!("{} ({})", rising.name, rising.severity));
    }
}
