//! Steps one compiled instance without a coordinator.
//!
//! A test sets input fields, steps cycles, and reads output fields, faults
//! and alerts. A sequence follows the fsw-3 step's order: an abort request
//! or a tripped redline aborts before `resume` runs, and a failure runs
//! `on abort:` before the sequence ends `Failed`.

use serde_json::Value;

use crate::cx::{Fault, ResumeWord};
use crate::desc::{FrameDesc, Kind, PortDesc};
use crate::instance::{Instance, read_leaf, write_leaf};
use crate::program::Program;
use crate::report::AlertEdges;

/// How a harnessed sequence ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ended {
    Completed,
    Aborted,
    Failed(String),
}

/// One line the instance would have logged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub kind: String,
    pub message: String,
}

/// An error setting up or driving a harness.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum HarnessError {
    #[error("no type `{0}`")]
    UnknownType(String),
    #[error("params: {0}")]
    Params(String),
    #[error("no port `{0}`")]
    UnknownPort(String),
    #[error("no field `{0}`")]
    UnknownField(String),
}

/// One instance under test.
pub struct Harness {
    instance: Instance,
    edges: AlertEdges,
    /// Which outputs the last step wrote.
    flushed: Vec<bool>,
    log: Vec<Event>,
    abort_requested: bool,
    started: bool,
    ended: Option<Ended>,
}

impl Harness {
    /// Makes an instance of type `ty` with `params`.
    pub fn new(program: &Program, ty: &str, params: Value) -> Result<Harness, HarnessError> {
        let index = program
            .desc
            .types
            .iter()
            .position(|t| t.name == ty)
            .ok_or_else(|| HarnessError::UnknownType(ty.to_string()))?;
        let instance = Instance::new(program.clone(), index, &params)
            .map_err(|e| HarnessError::Params(e.to_string()))?;
        Ok(Harness {
            edges: AlertEdges::new(&instance),
            flushed: vec![false; instance.desc().outputs.len()],
            instance,
            log: Vec::new(),
            abort_requested: false,
            started: false,
            ended: None,
        })
    }

    /// Sets field `path` of input `port`, marking the input present.
    pub fn set(&mut self, port: &str, path: &str, value: f64) -> Result<(), HarnessError> {
        let (index, frame) = self.port(true, port)?;
        let bytes = self
            .instance
            .input_mut(index)
            .ok_or_else(|| unknown_port(port))?;
        write_leaf(&frame, bytes, path, value)
            .ok_or_else(|| HarnessError::UnknownField(path.into()))
    }

    /// Reads field `path` of output `port`.
    pub fn get(&self, port: &str, path: &str) -> Result<f64, HarnessError> {
        let (index, frame) = self.port(false, port)?;
        let bytes = self
            .instance
            .output(index)
            .ok_or_else(|| unknown_port(port))?;
        read_leaf(&frame, bytes, path).ok_or_else(|| HarnessError::UnknownField(path.into()))
    }

    /// Returns whether the last step wrote output `port`.
    pub fn wrote(&self, port: &str) -> bool {
        self.port(false, port)
            .is_ok_and(|(index, _)| self.flushed.get(index).copied().unwrap_or(false))
    }

    /// Asks a sequence to abort on its next step.
    pub fn abort(&mut self) {
        self.abort_requested = true;
    }

    pub fn ended(&self) -> Option<&Ended> {
        self.ended.as_ref()
    }

    /// Every line logged so far.
    pub fn log(&self) -> &[Event] {
        &self.log
    }

    /// Returns whether a line of `kind` has been logged.
    pub fn logged(&self, kind: &str) -> bool {
        self.log.iter().any(|event| event.kind == kind)
    }

    /// Steps one cycle at `now`, in microseconds.
    pub fn step(&mut self, now: i64) {
        self.flushed.fill(false);
        if self.ended.is_some() {
            return;
        }
        match self.instance.desc().kind {
            Kind::Cyclic => {
                self.instance.step(now);
                self.finish_cycle(now);
            }
            Kind::Sequence => self.step_sequence(now),
        }
    }

    /// Steps `cycles` cycles `period` microseconds apart, from `start`.
    pub fn run(&mut self, start: i64, period: i64, cycles: usize) -> i64 {
        let mut now = start;
        for _ in 0..cycles {
            self.step(now);
            now += period;
        }
        now
    }

    fn step_sequence(&mut self, now: i64) {
        if !self.abort_requested
            && let Some(name) = self.tripped(now)
        {
            self.push("redline", name);
            self.abort_requested = true;
        }
        if self.abort_requested {
            if self.started {
                self.instance.on_abort(now);
                self.finish_cycle(now);
            }
            self.ended = Some(Ended::Aborted);
            return;
        }
        self.started = true;
        match self.instance.step(now) {
            ResumeWord::Running => self.finish_cycle(now),
            ResumeWord::Completed => {
                self.finish_cycle(now);
                self.ended = Some(Ended::Completed);
            }
            ResumeWord::Failed => {
                let message = self.instance.fail_message().to_string();
                self.instance.on_abort(now);
                self.finish_cycle(now);
                self.push("failed", message.clone());
                self.ended = Some(Ended::Failed(message));
            }
        }
    }

    /// Returns the name of the first redline that holds.
    fn tripped(&mut self, now: i64) -> Option<String> {
        let count = self.instance.desc().entry.redlines.len();
        let index = (0..count).find(|&i| self.instance.redline(i, now))?;
        Some(self.instance.desc().entry.redlines[index].name.clone())
    }

    /// Flushes the marked outputs and logs the cycle's faults and alerts.
    fn finish_cycle(&mut self, now: i64) {
        for index in 0..self.flushed.len() {
            if self.instance.take_written(index) {
                self.instance.stamp(index, now);
                self.flushed[index] = true;
            }
        }
        for fault in Fault::of(self.instance.take_faults()) {
            self.push(fault.kind(), fault.message());
        }
        let rising: Vec<Event> = self
            .edges
            .rising(&self.instance)
            .into_iter()
            .map(|r| Event {
                kind: "alert".into(),
                message: format!("{} ({})", r.name, r.severity),
            })
            .collect();
        self.log.extend(rising);
    }

    fn push(&mut self, kind: &str, message: impl Into<String>) {
        self.log.push(Event {
            kind: kind.into(),
            message: message.into(),
        });
    }

    /// Returns the index and frame of an input or output port.
    fn port(&self, input: bool, name: &str) -> Result<(usize, FrameDesc), HarnessError> {
        let ty = self.instance.desc();
        let ports: &[PortDesc] = if input { &ty.inputs } else { &ty.outputs };
        let index = ports
            .iter()
            .position(|p| p.name == name)
            .ok_or_else(|| unknown_port(name))?;
        let frame = self
            .instance
            .program()
            .desc
            .frame_of(&ports[index])
            .ok_or_else(|| unknown_port(name))?;
        Ok((index, frame.clone()))
    }
}

fn unknown_port(name: &str) -> HarnessError {
    HarnessError::UnknownPort(name.to_string())
}
