//! What commands report. Renderers decide how it looks.

use core::time::Duration;

/// One thing a command reports.
#[derive(Clone, Copy, Debug)]
pub enum Event<'a> {
    Phase(Phase<'a>),
    /// The targets about to run, in deployment order. A target's position
    /// picks its hue.
    Running(&'a [&'a str]),
    Diagnostic(&'a dyn miette::Diagnostic),
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
