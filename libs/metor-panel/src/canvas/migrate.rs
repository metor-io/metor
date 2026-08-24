//! Turning a saved node graph into a program.
//!
//! The node editor's graph and a Python module say the same things in
//! different shapes, so this is a translation rather than an interpretation:
//! one declaration per node, edges become names, `FromDb` becomes a binding,
//! `Persist` becomes the declaration's name, and a node's canvas position
//! becomes its `@node` annotation. What comes out is source an operator could
//! have typed, which is the only standard worth holding a converter to.
//!
//! ## What must be identical, and what may not be
//!
//! The bar the plan sets is that a converted graph publishes the *same
//! components*: same ids, same schemas. Ids are the easy half — a component's
//! id is `ComponentId::new(name)` and the converter keeps every `Persist`
//! node's name, so ids match by construction.
//!
//! Schemas are where the two vocabularies actually differ, and the difference
//! is not this converter's to paper over. The legacy ops preserve or promote
//! their input's element type — an `f32` channel through `Scale` stays `f32`,
//! an `i32` channel through `Window` stays `i32`. The language has one numeric
//! type: everything reads as `f64` on the way in and publishes as `f64`
//! (`dynamic/tensor.rs` computed in `f64` and cast at write time even under
//! the old ops, so this changes what a schema *says*, not what a plot draws).
//! Over `f64` sources the two agree exactly, which is what
//! `migrate_tests` pins op by op.
//!
//! ## What does not convert
//!
//! Two of the twenty-one ops have no expression in the language, and the
//! converter says so rather than emitting something that merely looks right:
//!
//! - **`Pack`** builds a rank-1 tensor out of N scalars. The subset has no
//!   list literal and no tensor constructor, deliberately — there is no
//!   spelling for it.
//! - **`DeltaT`** is the interval between arrivals, which needs the previous
//!   timestamp. `now()` gives the current one and a `State` field could hold
//!   the last, but only inside a `@system` with a declared state class, and
//!   the converter does not invent one.
//!
//! Both are reported per node, so a graph containing them converts as far as
//! it can and names what it could not.

use std::collections::HashMap;

use metor_db::DB;
use metor_proto::types::{ComponentId, PrimType};

use crate::dynamic::ops::compose::BinaryOp;
use crate::dynamic::ops::derive::{AffineOp, ThresholdOp, UnaryOp};
use crate::dynamic::ops::generators::Waveform;
use crate::dynamic::ops::resample::ResampleMode;
use crate::dynamic::tensor::TypedScalar;
use crate::node_editor::config::{NodeEditorConfig, SerializedNode};
use crate::node_editor::spec::NodeSpec;

/// What a conversion produced.
pub struct Converted {
    /// The program, ready to compile.
    pub source: String,
    /// One line per node that could not be expressed, for the prompt to show
    /// before anything is applied.
    pub refused: Vec<String>,
}

/// Convert a saved graph, resolving component ids through the db the way the
/// editor resolved them.
pub fn convert(config: &NodeEditorConfig, db: &DB) -> Converted {
    convert_with(config, &|id| component_name(db, id))
}

/// The same, against any id-to-name mapping — which is what makes this
/// testable without a database.
pub fn convert_with(
    config: &NodeEditorConfig,
    resolve: &dyn Fn(ComponentId) -> Option<String>,
) -> Converted {
    let by_id: HashMap<&str, &SerializedNode> = config
        .nodes
        .iter()
        .map(|n| (n.flow_id.as_str(), n))
        .collect();
    // Parents in socket order, which is the order every op's constructor
    // expects and therefore the order its expression reads.
    let mut parents: HashMap<&str, Vec<&str>> = HashMap::new();
    for edge in &config.edges {
        let slot = parents.entry(edge.target.as_str()).or_default();
        let at = edge.target_socket as usize;
        if slot.len() <= at {
            slot.resize(at + 1, "");
        }
        slot[at] = edge.source.as_str();
    }

    let mut state = Conversion {
        by_id,
        parents,
        resolve,
        named: HashMap::new(),
        lines: Vec::new(),
        refused: Vec::new(),
        taken: Vec::new(),
    };

    // A `Persist` node is what makes a value public, so each one is a
    // declaration named exactly what it published — which is what keeps the
    // component ids identical.
    let mut order: Vec<&SerializedNode> = config.nodes.iter().collect();
    order.sort_by(|a, b| a.flow_id.cmp(&b.flow_id));
    for node in &order {
        if let NodeSpec::Persist { name } = &node.spec {
            let Some(parent) = state.parents.get(node.flow_id.as_str()).and_then(|p| p.first())
            else {
                state
                    .refused
                    .push(format!("`{name}` publishes nothing: it has no input"));
                continue;
            };
            state.declare(parent, Some(name.clone()));
        }
    }
    // Anything not reachable from a `Persist` is still a node the operator
    // drew, so it converts too — as an unpublished intermediate.
    for node in &order {
        if !matches!(node.spec, NodeSpec::Persist { .. } | NodeSpec::FromDb { .. })
            && !state.is_clock(node.flow_id.as_str())
        {
            state.declare(node.flow_id.as_str(), None);
        }
    }

    Converted {
        source: state.lines.join(""),
        refused: state.refused,
    }
}

struct Conversion<'a> {
    by_id: HashMap<&'a str, &'a SerializedNode>,
    parents: HashMap<&'a str, Vec<&'a str>>,
    resolve: &'a dyn Fn(ComponentId) -> Option<String>,
    /// Nodes already given a declaration, by flow id.
    named: HashMap<String, String>,
    lines: Vec<String>,
    refused: Vec<String>,
    taken: Vec<String>,
}

impl<'a> Conversion<'a> {
    /// A clock is not a value, so it never becomes a declaration — it becomes
    /// the `rate=` of whatever it drives.
    fn is_clock(&self, flow: &str) -> bool {
        matches!(
            self.by_id.get(flow).map(|n| &n.spec),
            Some(NodeSpec::FixedRate { .. } | NodeSpec::ClockOf)
        )
    }

    /// The hertz a clock ticks at, following `ClockOf` to whatever it wraps.
    fn rate_of(&self, flow: &str) -> Option<f64> {
        match self.by_id.get(flow).map(|n| &n.spec) {
            Some(NodeSpec::FixedRate { hz }) => Some(*hz),
            // `clock_of` borrows another node's timestamps, which a source
            // system cannot do — it names a rate. The node's own rate is not
            // knowable here, so the conversion says what it assumed.
            Some(NodeSpec::ClockOf) => None,
            _ => None,
        }
    }

    /// Emit a declaration for `flow`, returning the name that reads it.
    ///
    /// `want` is the name a `Persist` asked for; without one the declaration
    /// is an intermediate and gets a name derived from what it does.
    fn declare(&mut self, flow: &str, want: Option<String>) -> Option<String> {
        if let Some(existing) = self.named.get(flow) {
            return Some(existing.clone());
        }
        let node = *self.by_id.get(flow)?;

        // A `FromDb` node is not a declaration at all: it is the component
        // path its consumers name directly.
        if let NodeSpec::FromDb { component_id } = &node.spec {
            let path = (self.resolve)(ComponentId(*component_id)).or_else(|| {
                self.refused
                    .push(format!("component {component_id:#x} is not in this database"));
                None
            })?;
            self.named.insert(flow.to_string(), path.clone());
            return Some(path);
        }

        let inputs: Vec<&str> = self.parents.get(flow).cloned().unwrap_or_default();
        let mut read = Vec::with_capacity(inputs.len());
        for input in &inputs {
            match self.is_clock(input) {
                true => read.push(String::new()),
                false => read.push(self.declare(input, None)?),
            }
        }

        let name = self.fresh(want.unwrap_or_else(|| stem(&node.spec).to_string()));
        let rate = inputs.iter().find(|i| self.is_clock(i)).and_then(|c| self.rate_of(c));
        let body = self.expression(node, &read, rate, &name)?;

        // The position rides the declaration, in whichever form that
        // declaration can carry.
        let placed = match body.starts_with('@') {
            true => format!("@node(x={}, y={})\n{body}", node.x.round(), node.y.round()),
            false => format!("{body}  # @node(x={}, y={})\n", node.x.round(), node.y.round()),
        };
        self.lines.push(placed);
        self.named.insert(flow.to_string(), name.clone());
        Some(name)
    }

    /// One node as one declaration's text.
    fn expression(
        &mut self,
        node: &SerializedNode,
        read: &[String],
        rate: Option<f64>,
        name: &str,
    ) -> Option<String> {
        use NodeSpec::*;
        let arg = |at: usize| read.get(at).cloned().unwrap_or_default();
        let binding = |body: String| Some(format!("{name} = {body}"));

        match &node.spec {
            // Generators become source systems: a clock is not something a
            // system reads, it is how often it runs.
            Waveform {
                shape,
                freq,
                amplitude,
                phase,
                ..
            } => {
                if *phase != 0.0 {
                    self.refused.push(format!(
                        "`{name}` had a phase offset, which the waveform functions do not take"
                    ));
                }
                let call = match shape {
                    self::Waveform::Sin => "sine",
                    self::Waveform::Cos => "cosine",
                    self::Waveform::Square => "square",
                    self::Waveform::Sawtooth => "sawtooth",
                };
                Some(self.source_system(name, &format!("{call}({freq:?}, {amplitude:?})"), rate))
            }
            Random { .. } => Some(self.source_system(name, "random()", rate)),
            Constant { value, .. } => {
                Some(self.source_system(name, &format!("constant({})", scalar(value)), rate))
            }

            Affine { op, k } => binding(match op {
                AffineOp::Scale => format!("{} * {}", arg(0), scalar(k)),
                AffineOp::Offset => format!("{} + {}", arg(0), scalar(k)),
            }),
            Unary { op } => binding(match op {
                UnaryOp::Abs => format!("abs({})", arg(0)),
                UnaryOp::Neg => format!("-{}", arg(0)),
                UnaryOp::Log => format!("log({})", arg(0)),
                UnaryOp::Sqrt => format!("sqrt({})", arg(0)),
                UnaryOp::Exp => format!("exp({})", arg(0)),
                UnaryOp::Floor => format!("floor({})", arg(0)),
            }),
            Window { size } => binding(format!("window({}, {size})", arg(0))),
            Fft => binding(format!("fft({})", arg(0))),
            Magnitude => binding(format!("sqrt({} @ {})", arg(0), arg(0))),
            Index { index } => binding(format!("{}[{index}]", arg(0))),
            // A threshold published `1.0`/`0.0`, not a bool, so the faithful
            // translation is the conditional rather than the comparison — the
            // component it publishes has to keep its element type.
            Threshold { k, op } => binding(format!(
                "1.0 if {} {} {} else 0.0",
                arg(0),
                match op {
                    ThresholdOp::Gt => ">",
                    ThresholdOp::Ge => ">=",
                    ThresholdOp::Lt => "<",
                    ThresholdOp::Le => "<=",
                },
                scalar(k)
            )),
            // The first difference is the last two samples, which is what a
            // length-two window holds.
            Delta => binding(format!(
                "window({}, 2)[1] - window({}, 2)[0]",
                arg(0),
                arg(0)
            )),
            Binary { op } => binding(format!(
                "{} {} {}",
                arg(0),
                match op {
                    BinaryOp::Add => "+",
                    BinaryOp::Sub => "-",
                    BinaryOp::Mul => "*",
                    BinaryOp::Div => "/",
                },
                arg(1)
            )),
            Mean => {
                let n = read.len().max(1);
                binding(format!("({}) / {n}.0", read.join(" + ")))
            }
            Dot => binding(format!("{} @ {}", arg(0), arg(1))),
            Resample { mode } => {
                let hz = rate.unwrap_or(10.0);
                let call = match mode {
                    ResampleMode::Zoh => "resample_zoh",
                    ResampleMode::Linear => "resample_linear",
                };
                binding(format!("{call}({}, {hz:?})", arg(0)))
            }

            Pack => {
                self.refused.push(format!(
                    "`{name}` is a Pack: the subset has no tensor literal to build one with"
                ));
                None
            }
            DeltaT => {
                self.refused.push(format!(
                    "`{name}` is a DeltaT: the interval between arrivals needs a state field the \
                     converter does not invent"
                ));
                None
            }
            FixedRate { .. } | ClockOf | FromDb { .. } | Persist { .. } => None,
        }
    }

    /// A generator, as the self-clocked system it always was.
    fn source_system(&mut self, name: &str, call: &str, rate: Option<f64>) -> String {
        let hz = rate.unwrap_or_else(|| {
            self.refused.push(format!(
                "`{name}` was driven by another node's timestamps; it converts as a 100 Hz source"
            ));
            100.0
        });
        format!("@system(rate={hz:?})\ndef {name}() -> f64:\n    return {call}\n")
    }

    /// A name nothing has taken yet.
    fn fresh(&mut self, want: String) -> String {
        let mut name = sanitize(&want);
        if self.taken.contains(&name) {
            name = (2..)
                .map(|n| format!("{name}{n}"))
                .find(|n| !self.taken.contains(n))
                .expect("some suffix is free");
        }
        self.taken.push(name.clone());
        name
    }
}

/// A `Persist` name is a component id, which may contain dots and other things
/// a Python name may not.
fn sanitize(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    if !out.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_') {
        out.insert(0, '_');
    }
    out
}

/// What an unpublished intermediate is called, when nothing named it.
fn stem(spec: &NodeSpec) -> &'static str {
    use NodeSpec::*;
    match spec {
        Waveform { .. } | Random { .. } | Constant { .. } => "signal",
        Affine { .. } => "scaled",
        Unary { .. } => "mapped",
        Window { .. } => "windowed",
        Fft => "spectrum",
        Magnitude => "magnitude",
        Index { .. } => "element",
        Threshold { .. } => "flag",
        Delta => "delta",
        Binary { .. } => "combined",
        Mean => "mean",
        Dot => "inner",
        Resample { .. } => "resampled",
        _ => "value",
    }
}

/// A literal the compiler will read back as the same number.
fn scalar(value: &TypedScalar) -> String {
    format!("{:?}", value.as_f64())
}

fn component_name(db: &DB, id: ComponentId) -> Option<String> {
    db.with_state(|state| {
        state
            .get_component_metadata(id)
            .map(|metadata| metadata.name.clone())
    })
}

/// The element type a legacy component carried, for the report a review
/// prompt shows.
pub fn widens(prim: PrimType) -> bool {
    prim != PrimType::F64 && prim != PrimType::Bool
}

#[cfg(test)]
mod tests;
