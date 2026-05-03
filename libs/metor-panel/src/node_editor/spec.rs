//! `NodeSpec`: serializable description of a single node in the graph.
//!
//! A `NodeSpec` carries only the *args* of an op; parent node ids come from
//! graph edges and are mixed into the [`NodeId`] hash by [`compute_node_id`].
//! Every variant's `op_tag()` and `hash_args()` mirror the corresponding
//! constructor in `crate::dynamic::ops::*` byte-for-byte — drift here breaks
//! reconciliation silently (every rebuild recreates everything; in-flight
//! tasks die). The `tests::node_id_matches_constructor` test enforces this.

use std::hash::Hash;
use std::sync::Arc;

use metor_db::DB;
use metor_proto::types::ComponentId;

use crate::dynamic::{BuildError, DynamicNode, NodeId, hash_id, op_tag, ops};

#[derive(Clone, Debug, PartialEq, facet::Facet)]
#[repr(u8)]
pub enum NodeSpec {
    FixedRate { hz: f64 },
    ClockOf,
    Sin { freq: f64, amplitude: f64, phase: f64 },
    Square { freq: f64, amplitude: f64, phase: f64 },
    Random { seed: u64 },
    Constant { value: f64 },
    Scale { k: f64 },
    Offset { k: f64 },
    Abs,
    Neg,
    Log,
    Add,
    Sub,
    Mul,
    Mean,
    Zoh,
    Linear,
    LatestAt,
    FromDb { component_id: u64 },
    Persist { name: String },
}

/// Compact discriminant used to key things off a spec without comparing args.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NodeSpecKind {
    FixedRate, ClockOf,
    Sin, Square, Random, Constant,
    Scale, Offset, Abs, Neg, Log,
    Add, Sub, Mul, Mean,
    Zoh, Linear, LatestAt,
    FromDb, Persist,
}

impl NodeSpec {
    pub fn kind(&self) -> NodeSpecKind {
        use NodeSpec::*;
        match self {
            FixedRate { .. } => NodeSpecKind::FixedRate,
            ClockOf => NodeSpecKind::ClockOf,
            Sin { .. } => NodeSpecKind::Sin,
            Square { .. } => NodeSpecKind::Square,
            Random { .. } => NodeSpecKind::Random,
            Constant { .. } => NodeSpecKind::Constant,
            Scale { .. } => NodeSpecKind::Scale,
            Offset { .. } => NodeSpecKind::Offset,
            Abs => NodeSpecKind::Abs,
            Neg => NodeSpecKind::Neg,
            Log => NodeSpecKind::Log,
            Add => NodeSpecKind::Add,
            Sub => NodeSpecKind::Sub,
            Mul => NodeSpecKind::Mul,
            Mean => NodeSpecKind::Mean,
            Zoh => NodeSpecKind::Zoh,
            Linear => NodeSpecKind::Linear,
            LatestAt => NodeSpecKind::LatestAt,
            FromDb { .. } => NodeSpecKind::FromDb,
            Persist { .. } => NodeSpecKind::Persist,
        }
    }

    pub fn op_tag(&self) -> &'static [u8] {
        use NodeSpec::*;
        match self {
            FixedRate { .. } => op_tag::FIXED_RATE_CLOCK,
            ClockOf => op_tag::CLOCK_OF,
            Sin { .. } => op_tag::SIN,
            Square { .. } => op_tag::SQUARE,
            Random { .. } => op_tag::RANDOM,
            Constant { .. } => op_tag::CONSTANT,
            Scale { .. } => op_tag::SCALE,
            Offset { .. } => op_tag::OFFSET,
            Abs => op_tag::ABS,
            Neg => op_tag::NEG,
            Log => op_tag::LOG,
            Add => op_tag::ADD,
            Sub => op_tag::SUB,
            Mul => op_tag::MUL,
            Mean => op_tag::MEAN,
            Zoh => op_tag::ZOH,
            Linear => op_tag::LINEAR,
            LatestAt => op_tag::LATEST_AT,
            FromDb { .. } => op_tag::FROM_DB,
            Persist { .. } => op_tag::PERSIST,
        }
    }

    /// Mirror the `args` closure each constructor passes to
    /// [`hash_id`](crate::dynamic::node::hash_id). Drift breaks reconciliation.
    fn hash_args(&self, h: &mut std::collections::hash_map::DefaultHasher) {
        use NodeSpec::*;
        match self {
            FixedRate { hz } => {
                hz.to_bits().hash(h);
            }
            ClockOf => {}
            Sin { freq, amplitude, phase }
            | Square { freq, amplitude, phase } => {
                freq.to_bits().hash(h);
                amplitude.to_bits().hash(h);
                phase.to_bits().hash(h);
            }
            Random { seed } => {
                seed.hash(h);
            }
            Constant { value } => {
                value.to_bits().hash(h);
            }
            Scale { k } | Offset { k } => {
                k.to_bits().hash(h);
            }
            Abs | Neg | Log => {}
            Add | Sub | Mul | Mean => {}
            Zoh | Linear | LatestAt => {}
            FromDb { component_id } => {
                component_id.hash(h);
            }
            Persist { name } => {
                name.hash(h);
            }
        }
    }
}

/// Compute the `NodeId` a built node would have, given its spec and the ids
/// of its parents in the canonical order this op expects.
pub fn compute_node_id(spec: &NodeSpec, parents: &[NodeId]) -> NodeId {
    hash_id(spec.op_tag(), parents, |h| spec.hash_args(h))
}

/// Build a runtime node from a spec and its already-built parents. The caller
/// is responsible for passing parents in the canonical order — same order
/// used by [`compute_node_id`].
pub fn build(
    spec: &NodeSpec,
    parents: Vec<Arc<dyn DynamicNode>>,
    db: &DB,
) -> Result<Arc<dyn DynamicNode>, BuildError> {
    use NodeSpec::*;

    fn p1(
        op: &'static str,
        mut parents: Vec<Arc<dyn DynamicNode>>,
    ) -> Result<Arc<dyn DynamicNode>, BuildError> {
        if parents.len() != 1 {
            return Err(BuildError::WrongArity {
                op,
                expected: 1,
                got: parents.len(),
            });
        }
        Ok(parents.pop().unwrap())
    }

    fn p2(
        op: &'static str,
        mut parents: Vec<Arc<dyn DynamicNode>>,
    ) -> Result<(Arc<dyn DynamicNode>, Arc<dyn DynamicNode>), BuildError> {
        if parents.len() != 2 {
            return Err(BuildError::WrongArity {
                op,
                expected: 2,
                got: parents.len(),
            });
        }
        let b = parents.pop().unwrap();
        let a = parents.pop().unwrap();
        Ok((a, b))
    }

    fn p0(op: &'static str, parents: Vec<Arc<dyn DynamicNode>>) -> Result<(), BuildError> {
        if !parents.is_empty() {
            return Err(BuildError::WrongArity {
                op,
                expected: 0,
                got: parents.len(),
            });
        }
        Ok(())
    }

    match spec {
        FixedRate { hz } => {
            p0("fixed_rate", parents)?;
            ops::clock::fixed_rate(*hz)
        }
        ClockOf => Ok(ops::clock::clock_of(p1("clock_of", parents)?)),
        Sin { freq, amplitude, phase } => {
            ops::generators::sin(p1("sin", parents)?, *freq, *amplitude, *phase)
        }
        Square { freq, amplitude, phase } => {
            ops::generators::square(p1("square", parents)?, *freq, *amplitude, *phase)
        }
        Random { seed } => ops::generators::random(p1("random", parents)?, *seed),
        Constant { value } => ops::generators::constant(p1("constant", parents)?, *value),
        Scale { k } => ops::derive::scale(p1("scale", parents)?, *k),
        Offset { k } => ops::derive::offset(p1("offset", parents)?, *k),
        Abs => ops::derive::abs(p1("abs", parents)?),
        Neg => ops::derive::neg(p1("neg", parents)?),
        Log => ops::derive::log(p1("log", parents)?),
        Add => {
            let (a, b) = p2("add", parents)?;
            ops::compose::add(a, b)
        }
        Sub => {
            let (a, b) = p2("sub", parents)?;
            ops::compose::sub(a, b)
        }
        Mul => {
            let (a, b) = p2("mul", parents)?;
            ops::compose::mul(a, b)
        }
        Mean => {
            if parents.is_empty() {
                return Err(BuildError::EmptyInputs);
            }
            ops::compose::mean(parents)
        }
        Zoh => {
            let (input, clock) = p2("zoh", parents)?;
            ops::resample::zoh(input, clock)
        }
        Linear => {
            let (input, clock) = p2("linear", parents)?;
            ops::resample::linear(input, clock)
        }
        LatestAt => {
            let (input, clock) = p2("latest_at", parents)?;
            ops::resample::latest_at(input, clock)
        }
        FromDb { component_id } => {
            p0("from_db", parents)?;
            ops::db_source::from_db(db, ComponentId(*component_id))
        }
        Persist { name } => ops::persist::persist(db, name.clone(), p1("persist", parents)?),
    }
}
