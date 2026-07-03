//! A minimal flight-software sequence executor: a fixed set of channels (slots), each able
//! to run one async "sequence" at a time. Sequences drive [`FSW`] state through timed
//! transitions and report their lifecycle back to the operator panel as
//! [`SequenceChannelEvent`]s; they are commanded via [`SequenceCommand`].
//!
//! The executor is deliberately limited and synchronous to the control loop: it is polled
//! exactly once per cycle from `main`, advancing a [maitake timer wheel] on **simulation
//! time** (a tick per cycle) rather than wall-clock. A `sleep` of N seconds therefore
//! resolves after exactly `N / DT` cycles, independent of how fast the host runs — the same
//! determinism the rest of the simulator relies on.
//!
//! Sequences see the whole [`FSW`] struct (read), and command the spacecraft by writing
//! `mode` and `overrides.disable_reaction_wheels`; they never touch simulation state.
//! `Abort` is cooperative (a [`Cancel`] token the sequence observes at each `wait` point so
//! it can run its safing branch), while `Stop` is a hard drop of the future.
//!
//! [maitake timer wheel]: maitake::time::Timer

use std::cell::{Cell, RefCell, RefMut};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use maitake::time::{Clock, Sleep, Timer};
use metor_proto_wkt::{
    SequenceChannelEvent, SequenceChannelSpec, SequenceCommand, SequenceCommandKind,
    SequenceEventKind, SequenceRegistry, SequenceRunState,
};

use crate::{DT, FSW, Mode};

/// Simulation-time tick counter, advanced once per cycle by [`Sequencer::step`]. This is the
/// timer wheel's clock source; it must be a `static` because [`Clock::new`] takes a bare `fn`
/// pointer, which cannot capture state.
static SIM_TICKS: AtomicU64 = AtomicU64::new(0);

fn sim_now() -> u64 {
    SIM_TICKS.load(Ordering::Relaxed)
}

fn sim_clock() -> Clock {
    Clock::new(Duration::from_secs_f64(DT), sim_now).named("sim")
}

/// Shared command surface for sequences: the spacecraft's [`FSW`] struct. Sequences read all
/// of it and write the commandable fields; the main loop applies and re-publishes it.
pub type FswHandle = Rc<RefCell<FSW>>;

/// Cooperative cancellation token. `Abort` flips it; a sequence observes it at its `wait`
/// points (see [`SequenceCtx::wait`]) and resolves through its safing branch.
#[derive(Clone, Default)]
pub struct Cancel(Rc<Cell<bool>>);

impl Cancel {
    fn request(&self) {
        self.0.set(true);
    }

    pub fn requested(&self) -> bool {
        self.0.get()
    }
}

/// Everything a sequence needs while it runs: the timer wheel, the shared FSW handle, its
/// cancellation token, and a sink for `Progress` updates. Cloned into each sequence future at
/// load time; all fields are `'static` so the future can be boxed and stored in a slot.
#[derive(Clone)]
pub struct SequenceCtx {
    timer: &'static Timer,
    fsw: FswHandle,
    cancel: Cancel,
    progress: Rc<RefCell<Vec<String>>>,
}

impl SequenceCtx {
    /// Mutable access to the whole FSW. Sequences conventionally write `mode` and
    /// `overrides.disable_reaction_wheels`.
    pub fn fsw(&self) -> RefMut<'_, FSW> {
        self.fsw.borrow_mut()
    }

    /// Record a `Progress` update; drained into a [`SequenceChannelEvent`] by the executor.
    pub fn progress(&self, detail: impl Into<String>) {
        self.progress.borrow_mut().push(detail.into());
    }

    /// Sleep on simulation time, resolving early if `Abort` was requested. Returns
    /// [`Step::Aborted`] in that case so the caller can run its safing branch.
    pub fn wait(&self, dur: Duration) -> Wait {
        Wait {
            sleep: Box::pin(self.timer.sleep(dur)),
            cancel: self.cancel.clone(),
        }
    }
}

/// Outcome of a single `wait`: the duration elapsed, or the sequence was asked to abort.
pub enum Step {
    Elapsed,
    Aborted,
}

/// A [`SequenceCtx::wait`] future: an abort-aware sleep. Because the executor re-polls every
/// cycle, a pending abort is observed within one cycle of the request.
pub struct Wait {
    sleep: Pin<Box<Sleep<'static>>>,
    cancel: Cancel,
}

impl Future for Wait {
    type Output = Step;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Step> {
        let this = self.get_mut();
        if this.cancel.requested() {
            return Poll::Ready(Step::Aborted);
        }
        match this.sleep.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(Step::Elapsed),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// How a sequence finished. Mapped to terminal [`SequenceEventKind`]s by the executor. (The
/// wire protocol also has a `Failed` kind; add it here once a fallible sequence needs it.)
pub enum Outcome {
    Completed,
    Aborted,
}

type SeqFuture = Pin<Box<dyn Future<Output = Outcome>>>;

/// Resolve a sequence name to a fresh future bound to `ctx`. The set of names here is what a
/// channel's `available` list advertises to the panel.
fn build(name: &str, ctx: SequenceCtx) -> Option<SeqFuture> {
    match name {
        "commissioning" => Some(Box::pin(commissioning(ctx))),
        "safe_mode" => Some(Box::pin(safe_mode(ctx))),
        _ => None,
    }
}

/// Bring the spacecraft up: settle, enable the reaction wheels, then slew to velocity-vector
/// pointing — each transition gated on a timer and on cooperative abort.
async fn commissioning(ctx: SequenceCtx) -> Outcome {
    ctx.progress("warming up");
    if let Step::Aborted = ctx.wait(Duration::from_secs(2)).await {
        return safing(&ctx);
    }
    ctx.fsw().overrides.disable_reaction_wheels = [false; 3];
    ctx.progress("reaction wheels enabled");
    if let Step::Aborted = ctx.wait(Duration::from_secs(3)).await {
        return safing(&ctx);
    }
    ctx.fsw().mode = Mode::HilPoint;
    ctx.progress("slewing to velocity vector");
    Outcome::Completed
}

/// Command the spacecraft into a safe configuration and complete. Also serves as the abort
/// target for other sequences via [`safing`].
async fn safe_mode(ctx: SequenceCtx) -> Outcome {
    ctx.progress("entering safe mode");
    if let Step::Aborted = ctx.wait(Duration::from_secs(1)).await {
        return safing(&ctx);
    }
    safing(&ctx);
    Outcome::Completed
}

/// Shared safing action: disable the reaction wheels and point nadir. Returns
/// [`Outcome::Aborted`] so it doubles as the cooperative-abort branch.
fn safing(ctx: &SequenceCtx) -> Outcome {
    {
        let mut fsw = ctx.fsw();
        fsw.overrides.disable_reaction_wheels = [true; 3];
        fsw.mode = Mode::NadirPoint;
    }
    ctx.progress("safed");
    Outcome::Aborted
}

/// One executor channel: holds at most one loaded/running sequence. The channel's **name**
/// is its identity — the address every [`SequenceCommand::channel`] carries. `run_state`
/// mirrors what the panel folds from this channel's events, so the executor can gate
/// `Reset` to terminal states without re-deriving it.
struct Slot {
    name: String,
    available: Vec<String>,
    loaded: Option<String>,
    running: bool,
    run_state: SequenceRunState,
    future: Option<SeqFuture>,
    cancel: Cancel,
    progress: Rc<RefCell<Vec<String>>>,
}

impl Slot {
    fn new(name: &str, available: Vec<String>) -> Self {
        Self {
            name: name.to_string(),
            available,
            loaded: None,
            running: false,
            run_state: SequenceRunState::Idle,
            future: None,
            cancel: Cancel::default(),
            progress: Rc::new(RefCell::new(Vec::new())),
        }
    }
}

/// The limited futures executor. Owns the channels and the simulation-time timer wheel,
/// folds [`SequenceCommand`]s into slot state, and emits [`SequenceChannelEvent`]s that the
/// main loop forwards to the panel.
pub struct Sequencer {
    timer: &'static Timer,
    fsw: FswHandle,
    slots: Vec<Slot>,
    pending: Vec<SequenceChannelEvent>,
}

impl Sequencer {
    pub fn new(fsw: FswHandle) -> Self {
        // Leaked so slot futures can hold `Sleep<'static>`; the timer lives for the whole run.
        let timer: &'static Timer = Box::leak(Box::new(Timer::new(sim_clock())));
        let available = vec!["commissioning".to_string(), "safe_mode".to_string()];
        let slots = vec![
            Slot::new("ADCS", available.clone()),
            Slot::new("Recovery", available),
        ];
        Self {
            timer,
            fsw,
            slots,
            pending: Vec::new(),
        }
    }

    /// The channel declaration to publish to the panel (on boot and on reload).
    pub fn registry(&self) -> SequenceRegistry {
        SequenceRegistry {
            channels: self
                .slots
                .iter()
                .map(|s| SequenceChannelSpec {
                    name: s.name.clone(),
                    available: s.available.clone(),
                })
                .collect(),
        }
    }

    pub fn handle_command(&mut self, cmd: SequenceCommand) {
        // Channels are addressed by name; a command naming no channel is dropped.
        let Some(idx) = self.slots.iter().position(|s| s.name == cmd.channel) else {
            return;
        };
        match cmd.command {
            SequenceCommandKind::Load { name } => {
                if self.slots[idx].running {
                    return;
                }
                if !self.slots[idx].available.iter().any(|a| a == &name) {
                    return;
                }
                self.load_slot(idx, name);
            }
            SequenceCommandKind::Start => {
                if self.slots[idx].future.is_some() && !self.slots[idx].running {
                    self.slots[idx].running = true;
                    self.slots[idx].run_state = SequenceRunState::Running;
                    let channel = self.slots[idx].name.clone();
                    self.emit(channel, SequenceEventKind::Started);
                }
            }
            SequenceCommandKind::Abort => {
                // Cooperative: flag it and let the sequence resolve through its safing branch;
                // the terminal `Aborted` event is emitted from `step` when the future finishes.
                if self.slots[idx].running {
                    self.slots[idx].cancel.request();
                }
            }
            SequenceCommandKind::Stop => {
                // Hard drop: cancel the future immediately, no async cleanup.
                if self.slots[idx].future.is_some() {
                    self.slots[idx].future = None;
                    self.slots[idx].running = false;
                    self.slots[idx].run_state = SequenceRunState::Stopped;
                    let channel = self.slots[idx].name.clone();
                    self.emit(channel, SequenceEventKind::Stopped);
                }
            }
            SequenceCommandKind::Reset => {
                // Only from a terminal state: rebuild the loaded sequence from the beginning.
                if !matches!(
                    self.slots[idx].run_state,
                    SequenceRunState::Completed | SequenceRunState::Aborted
                ) {
                    return;
                }
                let Some(name) = self.slots[idx].loaded.clone() else {
                    return;
                };
                self.load_slot(idx, name);
            }
        }
    }

    /// Build `name`'s future into slot `idx` from the beginning and report it as `Loaded`,
    /// leaving the channel idle and ready to start. Shared by `Load` and `Reset`.
    fn load_slot(&mut self, idx: usize, name: String) {
        let cancel = Cancel::default();
        self.slots[idx].progress.borrow_mut().clear();
        let ctx = SequenceCtx {
            timer: self.timer,
            fsw: self.fsw.clone(),
            cancel: cancel.clone(),
            progress: self.slots[idx].progress.clone(),
        };
        let Some(future) = build(&name, ctx) else {
            return;
        };
        let channel = self.slots[idx].name.clone();
        self.slots[idx].future = Some(future);
        self.slots[idx].cancel = cancel;
        self.slots[idx].loaded = Some(name.clone());
        self.slots[idx].running = false;
        self.slots[idx].run_state = SequenceRunState::Idle;
        self.emit(channel, SequenceEventKind::Loaded { name });
    }

    /// Poll every running sequence once. Called exactly once per control cycle, before the
    /// FSW update reads the (possibly mutated) command surface.
    pub fn step(&mut self) {
        SIM_TICKS.fetch_add(1, Ordering::Relaxed);
        self.timer.try_turn();
        let mut cx = Context::from_waker(Waker::noop());
        for i in 0..self.slots.len() {
            if !self.slots[i].running {
                continue;
            }
            let result = match self.slots[i].future.as_mut() {
                Some(future) => future.as_mut().poll(&mut cx),
                None => continue,
            };
            let channel = self.slots[i].name.clone();
            let drained: Vec<String> = self.slots[i].progress.borrow_mut().drain(..).collect();
            for detail in drained {
                self.emit(channel.clone(), SequenceEventKind::Progress { detail });
            }
            if let Poll::Ready(outcome) = result {
                self.slots[i].future = None;
                self.slots[i].running = false;
                let kind = match outcome {
                    Outcome::Completed => {
                        self.slots[i].run_state = SequenceRunState::Completed;
                        SequenceEventKind::Completed
                    }
                    Outcome::Aborted => {
                        self.slots[i].run_state = SequenceRunState::Aborted;
                        SequenceEventKind::Aborted
                    }
                };
                self.emit(channel, kind);
            }
        }
    }

    /// Take the events accumulated since the last drain, for the main loop to publish.
    pub fn drain_events(&mut self) -> Vec<SequenceChannelEvent> {
        std::mem::take(&mut self.pending)
    }

    fn emit(&mut self, channel: String, kind: SequenceEventKind) {
        self.pending.push(SequenceChannelEvent { channel, kind });
    }
}
