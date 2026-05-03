//! Per-op metadata used by the palette, the canvas renderer, and the
//! connection validator. Single source of truth for socket counts and types
//! so the three subsystems can't drift.

use crate::node_editor::spec::{NodeSpec, NodeSpecKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SocketKind {
    /// `ValueType::Clock`.
    Clock,
    /// `ValueType::Value(F64 scalar)`.
    F64Scalar,
    /// Any value (clock or value) — used for `ClockOf`'s input, which accepts
    /// either since it just taps the timestamp stream.
    Any,
}

impl SocketKind {
    /// Can data flowing out of `source` be plugged into a socket of kind `target`?
    pub fn compatible_with(self, target: SocketKind) -> bool {
        matches!(
            (self, target),
            (SocketKind::Any, _)
                | (_, SocketKind::Any)
                | (SocketKind::Clock, SocketKind::Clock)
                | (SocketKind::F64Scalar, SocketKind::F64Scalar)
        )
    }
}

/// Variadic-or-fixed input arity. Mean is the only variadic op today.
#[derive(Clone, Copy, Debug)]
pub enum Arity {
    Exact(&'static [SocketKind]),
    Variadic { kind: SocketKind, min: usize },
}

impl Arity {
    pub fn socket_at(&self, index: usize) -> Option<SocketKind> {
        match self {
            Arity::Exact(slots) => slots.get(index).copied(),
            Arity::Variadic { kind, .. } => Some(*kind),
        }
    }

    pub fn min_inputs(&self) -> usize {
        match self {
            Arity::Exact(slots) => slots.len(),
            Arity::Variadic { min, .. } => *min,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct OpDescriptor {
    pub kind: NodeSpecKind,
    pub label: &'static str,
    pub category: &'static str,
    pub inputs: Arity,
    pub output: SocketKind,
    pub default_spec: fn() -> NodeSpec,
}

const F64: SocketKind = SocketKind::F64Scalar;
const CLK: SocketKind = SocketKind::Clock;
const ANY: SocketKind = SocketKind::Any;

const NO_INPUTS: Arity = Arity::Exact(&[]);
const ONE_F64: Arity = Arity::Exact(&[F64]);
const TWO_F64: Arity = Arity::Exact(&[F64, F64]);
const ONE_CLK: Arity = Arity::Exact(&[CLK]);
const VALUE_AND_CLOCK: Arity = Arity::Exact(&[F64, CLK]);
const ONE_ANY: Arity = Arity::Exact(&[ANY]);

pub const ALL: &[OpDescriptor] = &[
    // Clocks
    OpDescriptor {
        kind: NodeSpecKind::FixedRate, label: "Fixed Rate", category: "Clock",
        inputs: NO_INPUTS, output: CLK,
        default_spec: || NodeSpec::FixedRate { hz: 100.0 },
    },
    OpDescriptor {
        kind: NodeSpecKind::ClockOf, label: "Clock Of", category: "Clock",
        inputs: ONE_ANY, output: CLK,
        default_spec: || NodeSpec::ClockOf,
    },
    // Generators
    OpDescriptor {
        kind: NodeSpecKind::Sin, label: "Sin", category: "Generator",
        inputs: ONE_CLK, output: F64,
        default_spec: || NodeSpec::Sin { freq: 1.0, amplitude: 1.0, phase: 0.0 },
    },
    OpDescriptor {
        kind: NodeSpecKind::Square, label: "Square", category: "Generator",
        inputs: ONE_CLK, output: F64,
        default_spec: || NodeSpec::Square { freq: 1.0, amplitude: 1.0, phase: 0.0 },
    },
    OpDescriptor {
        kind: NodeSpecKind::Random, label: "Random", category: "Generator",
        inputs: ONE_CLK, output: F64,
        default_spec: || NodeSpec::Random { seed: 1 },
    },
    OpDescriptor {
        kind: NodeSpecKind::Constant, label: "Constant", category: "Generator",
        inputs: ONE_CLK, output: F64,
        default_spec: || NodeSpec::Constant { value: 0.0 },
    },
    // Derive
    OpDescriptor {
        kind: NodeSpecKind::Scale, label: "Scale", category: "Derive",
        inputs: ONE_F64, output: F64,
        default_spec: || NodeSpec::Scale { k: 1.0 },
    },
    OpDescriptor {
        kind: NodeSpecKind::Offset, label: "Offset", category: "Derive",
        inputs: ONE_F64, output: F64,
        default_spec: || NodeSpec::Offset { k: 0.0 },
    },
    OpDescriptor {
        kind: NodeSpecKind::Abs, label: "Abs", category: "Derive",
        inputs: ONE_F64, output: F64,
        default_spec: || NodeSpec::Abs,
    },
    OpDescriptor {
        kind: NodeSpecKind::Neg, label: "Neg", category: "Derive",
        inputs: ONE_F64, output: F64,
        default_spec: || NodeSpec::Neg,
    },
    OpDescriptor {
        kind: NodeSpecKind::Log, label: "Log", category: "Derive",
        inputs: ONE_F64, output: F64,
        default_spec: || NodeSpec::Log,
    },
    // Compose
    OpDescriptor {
        kind: NodeSpecKind::Add, label: "Add", category: "Compose",
        inputs: TWO_F64, output: F64,
        default_spec: || NodeSpec::Add,
    },
    OpDescriptor {
        kind: NodeSpecKind::Sub, label: "Sub", category: "Compose",
        inputs: TWO_F64, output: F64,
        default_spec: || NodeSpec::Sub,
    },
    OpDescriptor {
        kind: NodeSpecKind::Mul, label: "Mul", category: "Compose",
        inputs: TWO_F64, output: F64,
        default_spec: || NodeSpec::Mul,
    },
    OpDescriptor {
        kind: NodeSpecKind::Mean, label: "Mean", category: "Compose",
        inputs: Arity::Variadic { kind: F64, min: 1 }, output: F64,
        default_spec: || NodeSpec::Mean,
    },
    // Resample
    OpDescriptor {
        kind: NodeSpecKind::Zoh, label: "Zero-Order Hold", category: "Resample",
        inputs: VALUE_AND_CLOCK, output: F64,
        default_spec: || NodeSpec::Zoh,
    },
    OpDescriptor {
        kind: NodeSpecKind::Linear, label: "Linear Interp", category: "Resample",
        inputs: VALUE_AND_CLOCK, output: F64,
        default_spec: || NodeSpec::Linear,
    },
    OpDescriptor {
        kind: NodeSpecKind::LatestAt, label: "Latest At", category: "Resample",
        inputs: VALUE_AND_CLOCK, output: F64,
        default_spec: || NodeSpec::LatestAt,
    },
    // DB bridges
    OpDescriptor {
        kind: NodeSpecKind::FromDb, label: "From DB", category: "DB",
        inputs: NO_INPUTS, output: F64,
        default_spec: || NodeSpec::FromDb { component_id: 0 },
    },
    OpDescriptor {
        kind: NodeSpecKind::Persist, label: "Persist", category: "DB",
        inputs: ONE_F64, output: F64,
        default_spec: || NodeSpec::Persist { name: String::new() },
    },
];

pub fn descriptor(kind: NodeSpecKind) -> &'static OpDescriptor {
    ALL.iter()
        .find(|d| d.kind == kind)
        .expect("every NodeSpecKind has a descriptor")
}

pub fn descriptor_for(spec: &NodeSpec) -> &'static OpDescriptor {
    descriptor(spec.kind())
}
