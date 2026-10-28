//! The step of a compiled system: copy the inputs in, call `step`, flush
//! the outputs it marked, and report its faults and alerts.

use metor_fsw_3::coordinator::StepCx;
use metor_fsw_3::log::{LogPort, with_log_port};
use metor_fsw_3::{Log, Step, Timestamp};

use crate::instance::Instance;
use crate::ports::Rings;
use crate::report::{AlertEdges, log_faults_and_alerts};

/// A compiled system bound to its rings.
pub struct CyclicStep {
    instance: Instance,
    rings: Rings,
    log: LogPort,
    edges: AlertEdges,
}

impl CyclicStep {
    pub fn new(instance: Instance, rings: Rings, log: LogPort) -> CyclicStep {
        CyclicStep {
            edges: AlertEdges::new(&instance),
            instance,
            rings,
            log,
        }
    }
}

impl Step for CyclicStep {
    fn execute(&mut self, now: Timestamp, _cx: &mut StepCx<'_>) {
        let Self {
            instance,
            rings,
            log,
            edges,
        } = self;
        rings.refresh(instance);
        instance.step(now.0);
        rings.flush(instance, now.0);
        with_log_port(log, now, || {
            log_faults_and_alerts(instance, edges);
            report_dropped(rings);
        });
    }

    fn fault(&mut self, now: Timestamp, message: &str) {
        with_log_port(&mut self.log, now, || {
            Log.fault("panic", message.to_string())
        });
    }
}

/// Logs one `publish_dropped` fault if any write failed this cycle.
pub(crate) fn report_dropped(rings: &Rings) {
    let count = rings.dropped.swap(0, core::sync::atomic::Ordering::Relaxed);
    if count != 0 {
        Log.fault("publish_dropped", format!("{count} writes dropped"));
    }
}
