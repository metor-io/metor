//! Validate timing, port contracts, and graph execution order.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use metor_fsw_2_core::{Delivery, FanIn, NAME_CAP, PortConn, SystemKind, compatible};

use crate::coordinator::{ClockMode, WireError};

use super::{ConsEdges, InitGraph, SystemBind};

impl InitGraph {
    /// Validate the actual wall-clock period, including rounding and range.
    pub(super) fn cycle_budget(&self) -> Result<Duration, WireError> {
        if matches!(self.config.clock, ClockMode::Simulated { .. }) {
            return Ok(Duration::ZERO);
        }
        let rate = self.config.cycle_rate;
        let invalid = || WireError::InvalidCycleRate { rate };
        if !rate.is_finite() || rate <= 0.0 {
            return Err(invalid());
        }
        let period = Duration::try_from_secs_f64(1.0 / rate).map_err(|_| invalid())?;
        // Stellarator stores nanosecond deadlines in a u64 on Unix. Keep
        // half that range free for the monotonic clock's existing uptime.
        if period.is_zero()
            || period.as_nanos() > u128::from(u64::MAX / 2)
            || Instant::now().checked_add(period).is_none()
        {
            return Err(invalid());
        }
        Ok(period)
    }

    pub(super) fn validate_simulated_step(&self) -> Result<(), WireError> {
        if let ClockMode::Simulated { dt } = self.config.clock
            && dt.is_zero()
        {
            return Err(WireError::InvalidSimulatedStep { dt });
        }
        Ok(())
    }

    /// Receive-all systems must observe the other systems' completed steps.
    pub(super) fn validate_receive_all_last(&self) -> Result<(), WireError> {
        let mut first_receive_all: Option<usize> = None;
        for (s, sys) in self.systems.iter().enumerate() {
            if !matches!(sys.desc.kind, SystemKind::Cyclic | SystemKind::Async) {
                continue;
            }
            let has_receive_all = sys
                .desc
                .capabilities
                .contains(&crate::Capability::ReceiveAll);
            if has_receive_all {
                first_receive_all.get_or_insert(s);
            } else if let Some(t) = first_receive_all {
                return Err(WireError::ReceiveAllNotLast {
                    system: sys.name.clone(),
                    receive_all: self.systems[t].name.clone(),
                });
            }
        }
        Ok(())
    }

    /// Slot instance names are wire addresses: enforce the [`NAME_CAP`]. A
    /// `SequenceCommand` addresses a slot by its instance name, and the same
    /// name packs into fixed-size status frames; a longer name would telemeter
    /// truncated while addressing untruncated, so it is a build error, never a
    /// truncation.
    pub(super) fn validate_slot_name_caps(&self) -> Result<(), WireError> {
        for sys in &self.systems {
            if matches!(sys.bind, SystemBind::Slot(_)) && sys.desc.name.len() > NAME_CAP {
                return Err(WireError::SlotNameTooLong {
                    name: sys.desc.name.to_string(),
                    len: sys.desc.name.len(),
                });
            }
        }
        Ok(())
    }

    /// Per-descriptor axis validation, needing no edges: FanIn::Many with
    /// Delivery::Snapshot is rejected (latest-wins across producers is
    /// ill-defined).
    pub(super) fn validate_port_axes(&self) -> Result<(), WireError> {
        for sys in &self.systems {
            for port in &sys.desc.inputs {
                if port.fan_in == FanIn::Many && port.delivery == Delivery::Snapshot {
                    return Err(WireError::SnapshotFanIn {
                        system: sys.desc.name.clone(),
                        port: port.id(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Validate every edge and build the one connection map,
    /// `(cons_id, in_idx) -> [(prod_id, out_idx)]`. Every rule branches on a
    /// descriptor axis, never on frame-vs-message. Also runs the graph-shape
    /// checks: every feedback loop broken by a `connect_delayed`, registration
    /// order agreeing with the dataflow, every FanIn::One input connected.
    pub(crate) fn solve_edges(&self) -> Result<ConsEdges, WireError> {
        let n = self.systems.len();
        let mut cons_edges: ConsEdges = HashMap::new();
        // System-level adjacency over the non-delayed SNAPSHOT edges only, for
        // cycle detection: a remaining cycle is an unbroken feedback loop. Log
        // edges are excluded; a log is a decoupled event/command stream, not a
        // same-cycle dependency.
        let mut forward_adj: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (p, c, delayed) in &self.edges {
            let prod = &self.systems[p.system.id].desc;
            let cons = &self.systems[c.system.id].desc;
            let out_idx = prod.outputs.iter().position(|d| d.id() == p.port).ok_or(
                WireError::UnknownPort {
                    system: p.system.id,
                    port: p.port,
                },
            )?;
            let in_idx = cons.inputs.iter().position(|d| d.id() == c.port).ok_or(
                WireError::UnknownPort {
                    system: c.system.id,
                    port: c.port,
                },
            )?;
            if !compatible(&prod.outputs[out_idx], &cons.inputs[in_idx]) {
                return Err(WireError::Incompatible {
                    producer: prod.name.clone(),
                    consumer: cons.name.clone(),
                    port: c.port,
                });
            }
            let in_desc = &cons.inputs[in_idx];
            // A non-Edge input's counterpart is held by its runner, so an edge
            // into it is rejected; Host *outputs* keep accepting consumer edges.
            if in_desc.conn != PortConn::Edge {
                return Err(WireError::HostPort {
                    system: cons.name.clone(),
                    port: c.port,
                });
            }
            // `delayed` marks a one-cycle-late snapshot sample; on a log input
            // it is meaningless and rejected instead of silently ignored.
            if *delayed && in_desc.delivery == Delivery::Log {
                return Err(WireError::DelayedLogEdge {
                    producer: prod.name.clone(),
                    consumer: cons.name.clone(),
                    port: c.port,
                });
            }
            let producers = cons_edges.entry((c.system.id, in_idx)).or_default();
            match in_desc.fan_in {
                // Exactly one edge per input.
                FanIn::One => {
                    if !producers.is_empty() {
                        return Err(WireError::DoubleConnect {
                            system: cons.name.clone(),
                            port: c.port,
                        });
                    }
                    producers.push((p.system.id, out_idx));
                }
                // Fan-in (append). Distinct producers may fan in freely, but an
                // exact duplicate of one edge would deliver every record twice.
                FanIn::Many => {
                    if producers.contains(&(p.system.id, out_idx)) {
                        return Err(WireError::DuplicateEdge {
                            producer: prod.name.clone(),
                            consumer: cons.name.clone(),
                            port: c.port,
                        });
                    }
                    producers.push((p.system.id, out_idx));
                }
            }
            // Self-edges included: a system plainly connected to itself is the
            // tightest feedback loop (it can only ever read its own previous
            // cycle's value), so it must be declared with `connect_delayed` like
            // any other loop; the DFS reports it as a one-member `FeedbackCycle`.
            if in_desc.delivery == Delivery::Snapshot && !delayed {
                forward_adj[p.system.id].push(c.system.id);
            }
        }

        // Every feedback loop must be broken by a `connect_delayed`
        if let Some(cycle) = find_cycle(&forward_adj) {
            return Err(WireError::FeedbackCycle {
                systems: cycle
                    .into_iter()
                    .map(|id| self.systems[id].desc.name.clone())
                    .collect(),
            });
        }

        // Registration order must agree with the dataflow
        // A backward non-delayed snapshot edge between scheduled systems would
        // read last cycle's value forever: silent staleness that must be
        // declared with `connect_delayed`. Checked after cycle detection so an
        // unbroken loop reports the clearer `FeedbackCycle`. Log edges remain
        // order-independent; self-edges never reach here (rejected above as a
        // one-member cycle).
        for (p, c, delayed) in &self.edges {
            if *delayed {
                continue;
            }
            let prod = &self.systems[p.system.id].desc;
            let cons = &self.systems[c.system.id].desc;
            let in_delivery = cons
                .inputs
                .iter()
                .find(|d| d.id() == c.port)
                .map(|d| d.delivery);
            if in_delivery != Some(Delivery::Snapshot) {
                continue;
            }
            let scheduled = |kind| matches!(kind, SystemKind::Cyclic | SystemKind::Async);
            if scheduled(prod.kind) && scheduled(cons.kind) && c.system.id < p.system.id {
                return Err(WireError::StaleFrameEdge {
                    producer: prod.name.clone(),
                    consumer: cons.name.clone(),
                    port: c.port,
                });
            }
        }

        // Input coverage: every FanIn::One Edge input has its one edge
        for (sid, sys) in self.systems.iter().enumerate() {
            for (in_idx, port) in sys.desc.inputs.iter().enumerate() {
                if port.conn == PortConn::Edge
                    && port.fan_in == FanIn::One
                    && !cons_edges.contains_key(&(sid, in_idx))
                {
                    return Err(WireError::UnconnectedInput {
                        system: sys.desc.name.clone(),
                        port: port.id(),
                    });
                }
            }
        }

        Ok(cons_edges)
    }
}

/// Find any directed cycle in the system graph (over the non-delayed edges),
/// returning its members in loop order, or `None` if the graph is acyclic. A
/// plain depth-first search colouring nodes white/grey/black; a back-edge to a
/// grey (on-stack) node closes a cycle, reconstructed from the DFS stack.
fn find_cycle(adj: &[Vec<usize>]) -> Option<Vec<usize>> {
    const WHITE: u8 = 0;
    const GREY: u8 = 1;
    const BLACK: u8 = 2;
    let n = adj.len();
    let mut color = vec![WHITE; n];
    let mut stack: Vec<usize> = Vec::new();

    fn dfs(
        u: usize,
        adj: &[Vec<usize>],
        color: &mut [u8],
        stack: &mut Vec<usize>,
    ) -> Option<Vec<usize>> {
        color[u] = GREY;
        stack.push(u);
        for &v in &adj[u] {
            match color[v] {
                GREY => {
                    // Back-edge: the cycle is the stack tail from `v` onward.
                    let start = stack.iter().position(|&x| x == v).unwrap_or(0);
                    return Some(stack[start..].to_vec());
                }
                WHITE => {
                    if let Some(c) = dfs(v, adj, color, stack) {
                        return Some(c);
                    }
                }
                _ => {}
            }
        }
        stack.pop();
        color[u] = BLACK;
        None
    }

    for s in 0..n {
        if color[s] == WHITE
            && let Some(c) = dfs(s, adj, &mut color, &mut stack)
        {
            return Some(c);
        }
    }
    None
}
