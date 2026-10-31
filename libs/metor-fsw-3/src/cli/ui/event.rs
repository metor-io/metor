//! What commands report. Renderers decide how it looks.

use core::time::Duration;
use std::borrow::Cow;

use metor_proto_wkt::LogLevel;

/// One thing a command reports.
#[derive(Clone, Copy, Debug)]
pub enum Event<'a> {
    Phase(Phase<'a>),
    /// The targets about to run, in deployment order. A target's position
    /// picks its hue.
    Running(&'a [&'a str]),
    Diagnostic(&'a dyn miette::Diagnostic),
    Log(&'a LogLine<'a>),
    /// The widest log source the run can produce, so every line, from every
    /// target, aligns.
    SourceWidth(usize),
    /// Another program's output, shown as it was written, such as cargo's
    /// errors or a target file's traceback.
    Output(&'a [u8]),
    /// Time passed; a live line may redraw.
    Tick,
    /// The run is over.
    Ended(&'a Summary),
}

/// A step before the first cycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Phase<'a> {
    pub verb: Verb,
    pub subject: &'a str,
    pub state: PhaseState<'a>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verb {
    Build,
    Evaluate,
    Resolve,
}

impl Verb {
    /// The present participle, shown while the phase runs.
    pub fn doing(self) -> &'static str {
        match self {
            Self::Build => "Building",
            Self::Evaluate => "Evaluating",
            Self::Resolve => "Resolving",
        }
    }

    /// The past tense, shown once the phase is done.
    pub fn done(self) -> &'static str {
        match self {
            Self::Build => "Built",
            Self::Evaluate => "Evaluated",
            Self::Resolve => "Resolved",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhaseState<'a> {
    Started,
    /// `done` units finished; `current` is the last one.
    Progress {
        done: u32,
        current: &'a str,
    },
    Done(Duration),
    Failed,
}

/// Where a log line comes from: `target.system`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Source<'a> {
    /// The target's namespace and hue index, or `None` in a deployment of
    /// one target.
    pub target: Option<(&'a str, usize)>,
    pub system: &'a str,
}

/// One log line.
#[derive(Clone, Copy, Debug)]
pub struct LogLine<'a> {
    /// Time since the first cycle, or `None` for a line from outside the
    /// run.
    pub elapsed: Option<Duration>,
    pub level: LogLevel,
    pub source: Source<'a>,
    pub message: &'a str,
    pub fields: &'a [(Cow<'static, str>, Cow<'static, str>)],
}

/// How a run went.
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    /// Wall time from the first cycle to the stop.
    pub took: Duration,
    /// Whether SIGINT stopped the run, rather than its cycle count.
    pub interrupted: bool,
    /// One row per target this process ran.
    pub targets: Vec<TargetSummary>,
}

/// How one target's cycles went.
#[derive(Clone, Debug, PartialEq)]
pub struct TargetSummary {
    pub name: String,
    pub cycles: u64,
    pub max_cycle: Duration,
    /// Cycles past their period. A simulated clock has none.
    pub overruns: u64,
}
