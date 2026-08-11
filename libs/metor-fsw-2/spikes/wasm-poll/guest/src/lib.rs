//! The guest half of the spike: a commissioning-shaped sequence compiled to
//! `wasm32-unknown-unknown`, driven one poll per cycle by the host.
//!
//! The point is to answer two questions the sequencing plan leaves open, on
//! representative code rather than a microbenchmark:
//!
//! 1. what the ADCS math costs inside a WASM interpreter, and
//! 2. what it costs to move a cycle's ports across the sandbox boundary.
//!
//! So the ladder here runs the *real* predicates — `angular_distance` on the
//! estimate delta, `omega.norm()` for the rate gate, and a `target_for`-shaped
//! look-at quaternion for the tracking error — against `nox`, which compiles to
//! wasm unchanged. Everything else is deliberately minimal: `metor-fsw-2`
//! cannot be a guest dependency (it pulls `stellarator`, `memmap2`,
//! `libloading` and `mdns-sd`, and `errno` refuses the `unknown` OS), so the
//! bits of the sequence runtime the ladder needs are reimplemented here at
//! about thirty lines. That reimplementation is itself a finding, not a
//! shortcut — see the README.
//!
//! ## Boundary
//!
//! The host owns a [`Mailbox`] in guest linear memory: it writes the cycle's
//! inputs, calls `poll`, and reads the outputs back. That copy *is* the port
//! marshalling being measured. State that must survive between cycles lives in
//! guest statics, exactly as a real occupant's future would live in its own
//! linear memory.

use nox::{ArrayRepr, Quaternion, Vector, tensor};

type V3 = Vector<f64, 3, ArrayRepr>;
type Quat = Quaternion<f64, ArrayRepr>;

/// The ports a cycle carries across the boundary, laid out for a single
/// `copy_from_slice` in each direction.
///
/// Inputs are what the `mode` slot declares (`attitude_estimate`, `gps`);
/// outputs are the commanded mode plus whether one was published this cycle,
/// which is how the host distinguishes "no command" from "commanded safe".
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Mailbox {
    // --- inputs, written by the host before each poll ---
    /// `q_hat_b_eci` as `[w, x, y, z]`.
    pub q_hat: [f64; 4],
    /// Estimated body rate (rad/s).
    pub omega_b: [f64; 3],
    /// ECI position (m).
    pub pos_eci: [f64; 3],
    /// ECI velocity (m/s).
    pub vel_eci: [f64; 3],
    /// Whether an attitude estimate has arrived at all.
    pub have_estimate: u32,
    /// Whether a GPS fix has arrived at all.
    pub have_gps: u32,
    /// Latched cancel, the guest-side twin of `SlotControlIn`.
    pub cancel: u32,

    // --- outputs, read by the host after each poll ---
    /// The commanded pointing law, when `published` is set.
    pub mode_law: u32,
    /// Whether the guest published a mode command this cycle.
    pub published: u32,
    /// Which phase the ladder is in, standing in for a progress line.
    pub phase: u32,
}

/// Run states, matching `Outcome::run_state`: zero while pending.
const PENDING: i32 = 0;
const COMPLETED: i32 = 1;
const ABORTED: i32 = 2;
const FAILED: i32 = 3;

/// The pointing laws the ladder commands, mirroring `ModeCmd`.
const LAW_SAFE: u32 = 0;
const LAW_DETUMBLE: u32 = 1;
const LAW_SETTLING: u32 = 2;
const LAW_HIL: u32 = 3;

/// The gates and budgets, fixed here rather than taken as params — the spike
/// measures cost, not configurability. Values track the target's `allow` line.
const EST_DELTA_RAD: f64 = 0.001;
const EST_DWELL_US: i64 = 200_000;
const WARMUP_TIMEOUT_US: i64 = 10_000_000;
const RATE_DETUMBLE_ENTER: f64 = 1.0;
const RATE_DETUMBLE_EXIT: f64 = 0.8;
const DETUMBLE_TIMEOUT_US: i64 = 900_000_000;
const COARSE_ERR_RAD: f64 = 0.2;
const COARSE_DWELL_US: i64 = 500_000;
const SETTLE_TIMEOUT_US: i64 = 60_000_000;
const CONFIRM_DWELL_US: i64 = 1_000_000;
const CONFIRM_TIMEOUT_US: i64 = 30_000_000;

/// The ladder's phases, in order.
const PHASE_WARMUP: u32 = 0;
const PHASE_DETUMBLE: u32 = 1;
const PHASE_COARSE: u32 = 2;
const PHASE_CONFIRM: u32 = 3;

static mut MAILBOX: Mailbox = Mailbox {
    q_hat: [1.0, 0.0, 0.0, 0.0],
    omega_b: [0.0; 3],
    pos_eci: [0.0; 3],
    vel_eci: [0.0; 3],
    have_estimate: 0,
    have_gps: 0,
    cancel: 0,
    mode_law: 0,
    published: 0,
    phase: 0,
};

/// The ladder's cross-cycle state — the guest-side equivalent of the locals a
/// real occupant's future holds across polls.
pub struct State {
    phase: u32,
    /// When the current phase started, for its budget.
    phase_start: i64,
    /// When the current dwell armed, or `None` if the predicate is not holding.
    held_since: Option<i64>,
    /// Previous estimate quaternion, for the warm-up delta.
    last_q: Option<[f64; 4]>,
    /// Set once the ladder has finished, so a done future is not re-run.
    done: Option<i32>,
    started: bool,
}

static mut STATE: State = State {
    phase: PHASE_WARMUP,
    phase_start: 0,
    held_since: None,
    last_q: None,
    done: None,
    started: false,
};

/// The address of the mailbox in linear memory, so the host can find it once
/// and then copy straight in and out.
#[unsafe(no_mangle)]
pub extern "C" fn mailbox_ptr() -> u32 {
    core::ptr::addr_of!(MAILBOX) as u32
}

/// The mailbox's size, so the host can bound its copies without hardcoding the
/// layout.
#[unsafe(no_mangle)]
pub extern "C" fn mailbox_len() -> u32 {
    core::mem::size_of::<Mailbox>() as u32
}

/// Advance the ladder by one cycle and report the run state.
///
/// This is the export the host calls once per cycle under a fuel budget — the
/// WASM shape of `FutureDriver::poll_once`. It returns `PENDING` while the
/// ladder is still running and a terminal run state once it is done.
#[unsafe(no_mangle)]
pub extern "C" fn poll(now_us: i64) -> i32 {
    let mailbox = unsafe { &mut *core::ptr::addr_of_mut!(MAILBOX) };
    let state = unsafe { &mut *core::ptr::addr_of_mut!(STATE) };
    step(mailbox, state, now_us)
}

/// One cycle of the ladder. Split out from `poll` so the host can run the very
/// same code natively for the baseline measurement.
pub fn step(mailbox: &mut Mailbox, state: &mut State, now_us: i64) -> i32 {
    mailbox.published = 0;
    if let Some(outcome) = state.done {
        return outcome;
    }
    if !state.started {
        state.started = true;
        state.phase_start = now_us;
    }
    // Cancel wins over everything, and the ladder gets this one cycle to safe
    // the vehicle before it reports — the single safing site, as in the Rust
    // sequence.
    if mailbox.cancel != 0 {
        publish(mailbox, LAW_SAFE);
        return finish(state, ABORTED);
    }

    let held = predicate(mailbox, state);
    let dwell = dwell_for(state.phase);
    if held {
        let since = *state.held_since.get_or_insert(now_us);
        if now_us - since >= dwell {
            return advance(mailbox, state, now_us);
        }
    } else {
        state.held_since = None;
    }
    if now_us - state.phase_start >= budget_for(state.phase) {
        publish(mailbox, LAW_SAFE);
        return finish(state, FAILED);
    }
    mailbox.phase = state.phase;
    PENDING
}

/// The current phase's gate, evaluated against this cycle's mailbox.
fn predicate(mailbox: &Mailbox, state: &mut State) -> bool {
    match state.phase {
        PHASE_WARMUP => {
            if mailbox.have_estimate == 0 {
                return false;
            }
            let q = mailbox.q_hat;
            let settled = state.last_q.is_some_and(|prev| {
                angular_distance(quat(q), quat(prev)).abs() < EST_DELTA_RAD
            });
            state.last_q = Some(q);
            settled
        }
        PHASE_DETUMBLE => rate(mailbox) < RATE_DETUMBLE_EXIT,
        PHASE_COARSE | PHASE_CONFIRM => {
            tracking_error(mailbox).is_some_and(|err| err < COARSE_ERR_RAD)
        }
        _ => true,
    }
}

/// Move to the next phase, commanding the mode that phase runs under. Skips
/// detumble when the boot tumble is inside wheel capture, exactly as the Rust
/// ladder does.
fn advance(mailbox: &mut Mailbox, state: &mut State, now_us: i64) -> i32 {
    state.held_since = None;
    state.phase_start = now_us;
    state.phase = match state.phase {
        PHASE_WARMUP if rate(mailbox) > RATE_DETUMBLE_ENTER => {
            publish(mailbox, LAW_DETUMBLE);
            PHASE_DETUMBLE
        }
        PHASE_WARMUP | PHASE_DETUMBLE => {
            publish(mailbox, LAW_SETTLING);
            PHASE_COARSE
        }
        PHASE_COARSE => {
            publish(mailbox, LAW_HIL);
            PHASE_CONFIRM
        }
        _ => return finish(state, COMPLETED),
    };
    mailbox.phase = state.phase;
    PENDING
}

fn finish(state: &mut State, outcome: i32) -> i32 {
    state.done = Some(outcome);
    outcome
}

fn publish(mailbox: &mut Mailbox, law: u32) {
    mailbox.mode_law = law;
    mailbox.published = 1;
}

fn dwell_for(phase: u32) -> i64 {
    match phase {
        PHASE_WARMUP => EST_DWELL_US,
        PHASE_DETUMBLE => 0,
        PHASE_COARSE => COARSE_DWELL_US,
        _ => CONFIRM_DWELL_US,
    }
}

fn budget_for(phase: u32) -> i64 {
    match phase {
        PHASE_WARMUP => WARMUP_TIMEOUT_US,
        PHASE_DETUMBLE => DETUMBLE_TIMEOUT_US,
        PHASE_COARSE => SETTLE_TIMEOUT_US,
        _ => CONFIRM_TIMEOUT_US,
    }
}

// ---------------------------------------------------------------------------
// The math under measurement
// ---------------------------------------------------------------------------

fn quat(q: [f64; 4]) -> Quat {
    Quat::new(q[0], q[1], q[2], q[3])
}

fn vec3(v: [f64; 3]) -> V3 {
    tensor![v[0], v[1], v[2]]
}

/// The estimated body rate (rad/s), or zero before the first estimate.
fn rate(mailbox: &Mailbox) -> f64 {
    if mailbox.have_estimate == 0 {
        return 0.0;
    }
    vec3(mailbox.omega_b).norm().into_buf()
}

fn angular_distance(a: Quat, b: Quat) -> f64 {
    a.angular_distance(&b).into_buf()
}

/// The shortest-arc body←ECI quaternion putting `-Y` on `dir_eci`, matching
/// `adcs_contracts::point_minus_y_at`.
fn point_minus_y_at(dir_eci: V3) -> Quat {
    let r = dir_eci.normalize();
    let body_axis: V3 = tensor![0.0, -1.0, 0.0];
    let [x, y, z] = body_axis.cross(&r).into_buf();
    let w = 1.0 + body_axis.dot(&r).into_buf();
    Quat::new(w, x, y, z).normalize()
}

/// The velocity-vector pointing target, with the same non-finite fallback as
/// `adcs_contracts::target_for`.
fn target_for_hil(vel_eci: V3) -> Quat {
    let t = point_minus_y_at(vel_eci.normalize());
    if t.0.into_buf().iter().any(|f| !f.is_finite()) {
        Quat::identity()
    } else {
        t
    }
}

/// The tracking error (rad) to the velocity-vector target — the heaviest
/// predicate in the ladder, and the one this spike is really timing.
fn tracking_error(mailbox: &Mailbox) -> Option<f64> {
    if mailbox.have_estimate == 0 || mailbox.have_gps == 0 {
        return None;
    }
    let target = target_for_hil(vec3(mailbox.vel_eci));
    Some(angular_distance(quat(mailbox.q_hat), target).abs())
}

/// A fresh ladder state, for the host's native baseline.
pub fn new_state() -> State {
    State {
        phase: PHASE_WARMUP,
        phase_start: 0,
        held_since: None,
        last_q: None,
        done: None,
        started: false,
    }
}

/// The velocity-vector target attitude, exported so the host can synthesise an
/// estimate that actually converges onto it — otherwise the ladder never leaves
/// coarse pointing and the measurement misses its own deepest math path.
pub fn hil_target(vel_eci: [f64; 3]) -> [f64; 4] {
    // `Quat::new(w, x, y, z)` stores `[x, y, z, w]`, so reorder on the way out
    // to the `[w, x, y, z]` convention `quat` reads back.
    let [x, y, z, w] = target_for_hil(vec3(vel_eci)).0.into_buf();
    [w, x, y, z]
}

/// Re-arm the ladder from the start.
///
/// The harness calls this the moment a run reaches a terminal state, so every
/// measured cycle is doing real predicate work. Without it a run that completes
/// early spends most of its cycles in `step`'s done-latch early return, and the
/// average reports the cost of doing nothing.
#[unsafe(no_mangle)]
pub extern "C" fn reset() {
    let state = unsafe { &mut *core::ptr::addr_of_mut!(STATE) };
    *state = new_state();
}

/// The confirm-phase gate value, exposed so the harness can see why a phase is
/// or is not holding instead of inferring it from a timeout.
pub fn debug_tracking_error(mailbox: &Mailbox) -> Option<f64> {
    tracking_error(mailbox)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Probe `angular_distance` as two quaternions converge. The spike's first
    /// synthetic feed drove the estimate to *exactly* the target, and the
    /// pointing gate then stopped passing — this pins down whether the culprit
    /// is a NaN from `acos` of a dot product rounded past 1.
    #[test]
    fn angular_distance_near_zero() {
        let a = Quat::new(1.0, 0.0, 0.0, 0.0);
        for eps in [0.0_f64, 1e-12, 1e-9, 1e-7, 1e-4] {
            let n = (1.0 + eps * eps).sqrt();
            let b = Quat::new(1.0 / n, eps / n, 0.0, 0.0);
            let d = angular_distance(a, b);
            println!("eps {eps:e} -> angular_distance {d:e}  nan={}", d.is_nan());
        }
    }
}
