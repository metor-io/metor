//! The mux's per-cycle state and logic.

use metor_proto::types::Timestamp;

use crate::coordinator::MakeError;
use crate::fn_system::Bound;
use crate::log::Log;
use crate::port::{DynInputs, DynOutputs};

use super::params::MuxParams;
use super::plan::{Plan, Stamp, Trigger, apply};

/// A mux's plan and what it has seen.
///
/// `max_age` compares the cycle time with input stamps, so it assumes every
/// stamped input shares this target's clock.
pub struct Mux {
    plan: Plan,
    /// Inputs delivered at least once, one bit each.
    seen: u64,
    /// Inputs that trigger an output, one bit each.
    triggers: u64,
    /// Each input's last stamp, or the cycle its last record arrived in.
    last: Vec<Option<Timestamp>>,
    outs: Vec<OutState>,
}

/// One output's staged record.
struct OutState {
    /// Inputs delivered since the last emit, one bit each.
    fresh: u64,
    staging: Box<[u8]>,
}

impl Mux {
    /// Resolves `params` against the bound ports and allocates staging.
    pub fn new(params: MuxParams, bound: &Bound<'_>) -> Result<Self, MakeError> {
        let plan = Plan::resolve(&params, bound)?;
        let triggers = plan.outs.iter().fold(0, |mask, out| match out.on {
            Trigger::Input(i) => mask | bit(usize::from(i)),
            _ => mask,
        });
        let outs = plan
            .outs
            .iter()
            .map(|out| OutState {
                fresh: 0,
                staging: vec![0; out.len as usize].into_boxed_slice(),
            })
            .collect();
        Ok(Self {
            last: vec![None; plan.ins.len()],
            plan,
            seen: 0,
            triggers,
            outs,
        })
    }

    /// Drains the inputs in the plan's order, then emits the outputs whose
    /// rule holds this cycle.
    ///
    /// Records that cannot be read are skipped, with one fault per cycle.
    pub(crate) fn step(&mut self, now: Timestamp, ins: &mut DynInputs, outs: &mut DynOutputs) {
        let mut unread = 0u32;
        for index in 0..self.plan.order.len() {
            unread += self.drain(self.plan.order[index], now, ins, outs);
        }
        if unread > 0 {
            Log.fault("mux", "records could not be read");
        }
        for out in 0..self.outs.len() {
            if self.rule_holds(out) && self.ready(out, now) {
                self.emit(out, self.stamp(out, now), outs);
            }
        }
    }

    /// Takes every record of input `i`, firing its port-triggered outputs
    /// after each one. Returns how many records could not be read.
    fn drain(
        &mut self,
        i: usize,
        now: Timestamp,
        ins: &mut DynInputs,
        outs: &mut DynOutputs,
    ) -> u32 {
        let Some((_, input)) = ins.get_mut(self.plan.ins[i].port) else {
            return 0;
        };
        let mut unread = 0;
        for record in input.drain() {
            let Ok(record) = record else {
                unread += 1;
                continue;
            };
            if self.take(i, record, now) && self.triggers & bit(i) != 0 {
                self.fire(i, now, outs);
            }
        }
        unread
    }

    /// Copies `record` into every output's staging and records its stamp.
    ///
    /// Returns `false` for a record too short for its input, which is skipped
    /// with a fault.
    fn take(&mut self, input: usize, record: &[u8], now: Timestamp) -> bool {
        let spec = &self.plan.ins[input];
        if record.len() < spec.len as usize {
            Log.fault("mux", "a record is shorter than its input's frame");
            return false;
        }
        for (out, state) in self.plan.outs.iter().zip(&mut self.outs) {
            let mine = out.copies.iter().filter(|c| usize::from(c.input) == input);
            for copy in mine {
                apply(copy, &self.plan.cals, record, &mut state.staging);
            }
            state.fresh |= bit(input);
        }
        self.seen |= bit(input);
        self.last[input] = Some(
            spec.stamp_at
                .and_then(|at| read_stamp(record, at))
                .unwrap_or(now),
        );
        true
    }

    /// Emits every ready output that input `input` triggers.
    fn fire(&mut self, input: usize, now: Timestamp, outs: &mut DynOutputs) {
        for out in 0..self.outs.len() {
            let on = self.plan.outs[out].on;
            let fired = matches!(on, Trigger::Input(t) if usize::from(t) == input);
            if fired && self.ready(out, now) {
                self.emit(out, self.stamp(out, now), outs);
            }
        }
    }

    /// Whether an `any`, `all`, or `cycle` output's rule holds.
    fn rule_holds(&self, out: usize) -> bool {
        let spec = &self.plan.outs[out];
        let fresh = self.outs[out].fresh & spec.deps;
        match spec.on {
            Trigger::Input(_) => false,
            Trigger::Any => fresh != 0,
            Trigger::All => fresh == spec.deps,
            Trigger::Cycle => true,
        }
    }

    /// Whether every mapped input has delivered, and none is older than
    /// `max_age`.
    fn ready(&self, out: usize, now: Timestamp) -> bool {
        let spec = &self.plan.outs[out];
        if self.seen & spec.deps != spec.deps {
            return false;
        }
        let Some(max_age) = spec.max_age else {
            return true;
        };
        (0..self.last.len())
            .filter(|&i| spec.deps & bit(i) != 0)
            .all(|i| self.last[i].is_some_and(|at| now.0.saturating_sub(at.0) <= max_age))
    }

    fn stamp(&self, out: usize, now: Timestamp) -> Timestamp {
        match self.plan.outs[out].stamp {
            Stamp::Input(i) => self.last[usize::from(i)].unwrap_or(now),
            Stamp::Now => now,
        }
    }

    /// Stamps and writes output `out`'s staging. A failed write is counted
    /// by `DynOutputs`.
    fn emit(&mut self, out: usize, stamp: Timestamp, outs: &mut DynOutputs) {
        let spec = &self.plan.outs[out];
        let state = &mut self.outs[out];
        if let Some(at) = spec.stamp_at {
            write_stamp(&mut state.staging, at, stamp);
        }
        state.fresh = 0;
        if let Some((_, output)) = outs.get_mut(spec.port) {
            let _ = output.write(&state.staging);
        }
    }
}

fn bit(i: usize) -> u64 {
    1u64.checked_shl(i as u32).unwrap_or(0)
}

fn read_stamp(record: &[u8], at: u32) -> Option<Timestamp> {
    let at = at as usize;
    let bytes = record.get(at..at.checked_add(8)?)?;
    Some(Timestamp(i64::from_le_bytes(bytes.try_into().ok()?)))
}

fn write_stamp(staging: &mut [u8], at: u32, stamp: Timestamp) {
    let at = at as usize;
    if let Some(bytes) = at.checked_add(8).and_then(|end| staging.get_mut(at..end)) {
        bytes.copy_from_slice(&stamp.0.to_le_bytes());
    }
}

#[cfg(all(test, feature = "host"))]
mod tests;
