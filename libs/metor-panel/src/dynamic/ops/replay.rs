//! Run a compiled system over its inputs' stored history.
//!
//! The live runtime in [`program`](super::program) evaluates a system at each
//! driving sample as it arrives and stamps the output with that sample's own
//! timestamp, so the same rule run over samples read back from the database
//! produces the history the system would have published had it been running
//! then. That is what a plot over a window older than the expression needs,
//! and it costs nothing in the language: the only clock a system sees is the
//! timestamp handed to each evaluation.
//!
//! A replay is a cold start. State the live instance carries — `window`
//! buffers, declared `State`, the generator's word — begins from its defaults
//! at the range's start, and the generator is seeded from the range so two
//! replays of one stretch agree with each other, if not with the live run.

use std::ops::Range;
use std::sync::Arc;

use metor_db::time_series::TimeSeriesNodeSlice;
use metor_db::{Component, ComponentSchema};
use metor_expr::Ty;
use metor_proto::types::{ComponentId, Timestamp};

use crate::dynamic::BuildError;
use crate::dynamic::ops::db_source;
use crate::dynamic::ops::program::{Compiled, Held, Running, read_field, schema_of};

/// Where one system's replay reads and what it publishes: the wiring
/// `program::system` is given, minus the tasks.
///
/// Ports are the components themselves rather than their ids because a
/// replay reads their schemas and time series, and a plot reads the same time
/// series to size its window — resolving them once here serves both.
#[derive(Clone)]
pub struct ReplayPlan {
    pub compiled: Arc<Compiled>,
    pub system: usize,
    /// One component per input port, in the manifest's order.
    pub ports: Vec<Component>,
    /// Each output field and the component it publishes into.
    pub outputs: Vec<(usize, ComponentId)>,
}

impl ReplayPlan {
    /// The bytes one output field carries, cut out of a whole frame.
    pub fn field(&self, field: usize, frame: &[u8], out: &mut Vec<u8>) {
        let spec = &self.compiled.manifest.systems[self.system].output.fields[field];
        read_field(&frame[spec.offset as usize..], &spec.ty, out);
    }

    /// The component schema one output field publishes as.
    pub fn field_schema(&self, field: usize) -> ComponentSchema {
        schema_of(&self.field_ty(field))
    }

    fn field_ty(&self, field: usize) -> Ty {
        self.compiled.manifest.systems[self.system].output.fields[field]
            .ty
            .clone()
    }
}

/// What a replay got through before it returned.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReplayStats {
    /// Input samples read across every port.
    pub read: usize,
    /// Frames handed to the sink.
    pub emitted: usize,
    /// Whether the sink asked to stop before the range was exhausted.
    pub stopped: bool,
}

/// Replay `range` of the plan's inputs through a fresh instance, handing each
/// output frame to `sink` in timestamp order. The sink returns `false` to
/// stop early.
///
/// Synchronous and self-contained: it spawns nothing and touches no ring, so
/// it belongs on a background thread, never on the node worker.
pub fn replay(
    plan: &ReplayPlan,
    range: Range<Timestamp>,
    fuel: u64,
    sink: &mut dyn FnMut(Timestamp, &[u8]) -> bool,
) -> Result<ReplayStats, BuildError> {
    let desc = plan
        .compiled
        .manifest
        .systems
        .get(plan.system)
        .ok_or(BuildError::Expr("no such system".into()))?;
    if plan.ports.len() != desc.inputs.len() {
        return Err(BuildError::WrongArity {
            op: "expr.replay",
            expected: desc.inputs.len(),
            got: plan.ports.len(),
        });
    }
    let driven_port = match desc.rate {
        Some(_) => None,
        None if plan.ports.is_empty() => {
            return Err(BuildError::Expr(
                "a system with no inputs needs @system(rate=) to fire it".into(),
            ));
        }
        None => Some(desc.driving.unwrap_or(0)),
    };

    let schemas: Vec<ComponentSchema> = plan.ports.iter().map(|c| c.schema.clone()).collect();
    let mut held = Held::new(desc, &schemas, driven_port)?;
    let mut instance = Running::new(&plan.compiled, plan.system, fuel)?;
    let port_ids: Vec<_> = plan
        .ports
        .iter()
        .map(|c| db_source::from_db_id(c.component_id))
        .collect();
    let id = plan.compiled.system_hash(plan.system, &port_ids);
    instance.seed_rng(id.0 ^ range.start.0 as u64);

    let mut stats = ReplayStats::default();
    let mut cursors = Vec::with_capacity(plan.ports.len());
    for (i, port) in plan.ports.iter().enumerate() {
        // A held port begins with what it last said before the range, as a
        // live system begins with what the host already knows.
        if Some(i) != driven_port
            && let Some(value) = last_before(port, range.start)
        {
            held.hold(i, &value);
        }
        cursors.push(Cursor::new(port, range.clone()));
    }

    let mut emit = |ts: Timestamp, frame: &[u8], stats: &mut ReplayStats| -> bool {
        stats.emitted += 1;
        sink(ts, frame)
    };

    match desc.rate {
        None => {
            let driven = driven_port.expect("an input fires an unclocked system");
            loop {
                // The oldest waiting sample fires or holds. Among equals a
                // held port goes first, so a same-instant input is visible to
                // the evaluation — the replay's reading of "drain after the
                // driving sample arrives".
                let Some(next) = cursors
                    .iter()
                    .enumerate()
                    .filter_map(|(i, c)| c.peek().map(|ts| (ts, i == driven, i)))
                    .min()
                else {
                    break;
                };
                let (ts, _, port) = next;
                stats.read += 1;
                if port != driven {
                    held.hold(port, cursors[port].current());
                    cursors[port].advance();
                    continue;
                }
                let fired = held
                    .fire(&mut instance, ts, Some(cursors[port].current()))
                    .map_err(BuildError::Expr)?;
                cursors[port].advance();
                if let Some(frame) = fired
                    && !emit(ts, frame, &mut stats)
                {
                    stats.stopped = true;
                    break;
                }
            }
        }
        Some(hz) => {
            for tick in ticks(hz, range.clone()) {
                for (i, cursor) in cursors.iter_mut().enumerate() {
                    while cursor.peek().is_some_and(|ts| ts <= tick) {
                        held.hold(i, cursor.current());
                        cursor.advance();
                        stats.read += 1;
                    }
                }
                if let Some(frame) = held
                    .fire(&mut instance, tick, None)
                    .map_err(BuildError::Expr)?
                    && !emit(tick, frame, &mut stats)
                {
                    stats.stopped = true;
                    break;
                }
            }
        }
    }
    Ok(stats)
}

/// The instants a source system fires at over `range`, on a grid anchored to
/// the epoch rather than to the range so adjacent stretches replayed
/// separately meet without a seam.
pub fn ticks(hz: f64, range: Range<Timestamp>) -> impl Iterator<Item = Timestamp> {
    let period = ((1_000_000.0 / hz).round() as i64).max(1);
    let first = range.start.0.div_euclid(period) * period;
    let first = if first < range.start.0 {
        first + period
    } else {
        first
    };
    (first..range.end.0)
        .step_by(period as usize)
        .map(Timestamp)
}

/// The last sample a component holds from strictly before `ts`.
fn last_before(component: &Component, ts: Timestamp) -> Option<Vec<u8>> {
    let size = component.schema.size();
    // Newest node first, so the first one with anything older is the answer.
    for node in component.time_series.iter_node_slices() {
        let at = node.timestamps().partition_point(|t| t.0 < ts.0);
        if at == 0 {
            continue;
        }
        let data = node.data();
        return data.get((at - 1) * size..at * size).map(<[u8]>::to_vec);
    }
    None
}

/// One port's samples inside the range, oldest first.
struct Cursor {
    /// Node slices oldest-last, so the current one is always at the back.
    nodes: Vec<TimeSeriesNodeSlice>,
    at: usize,
    end: Timestamp,
    size: usize,
}

impl Cursor {
    fn new(component: &Component, range: Range<Timestamp>) -> Self {
        let mut nodes: Vec<_> = component
            .time_series
            .iter_node_slices()
            .filter(|node| {
                let timestamps = node.timestamps();
                match (timestamps.first(), timestamps.last()) {
                    (Some(first), Some(last)) => first.0 < range.end.0 && last.0 >= range.start.0,
                    _ => false,
                }
            })
            .collect();
        nodes.reverse();
        let at = nodes
            .last()
            .map_or(0, |node| node.timestamps().partition_point(|t| t.0 < range.start.0));
        Cursor {
            nodes,
            at,
            end: range.end,
            size: component.schema.size(),
        }
    }

    fn peek(&self) -> Option<Timestamp> {
        let ts = *self.nodes.last()?.timestamps().get(self.at)?;
        (ts.0 < self.end.0).then_some(ts)
    }

    fn current(&self) -> &[u8] {
        let node = self.nodes.last().expect("peeked before reading");
        &node.data()[self.at * self.size..(self.at + 1) * self.size]
    }

    fn advance(&mut self) {
        self.at += 1;
        if let Some(node) = self.nodes.last()
            && self.at >= node.timestamps().len()
        {
            self.nodes.pop();
            self.at = 0;
        }
    }
}
