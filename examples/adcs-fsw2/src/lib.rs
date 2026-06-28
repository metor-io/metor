//! A closed-loop spacecraft attitude-determination-and-control (ADCS) mission built
//! on the `metor-fsw-2` flight-software framework.
//!
//! Three cyclic systems form a feedback loop:
//!
//! ```text
//!   Plant ──Sensors──▶ Nav ──AttitudeEstimate──▶ Ctrl
//!     ▲                                            │
//!     └──────────────── TorqueCmd ─────────────────┘   (one-cycle delay back-edge)
//! ```
//!
//! * [`PlantSystem`] owns a rigid-body [`Body`] (nox `six_dof`); each cycle it
//!   integrates one `rk4` step under the latest commanded torque and emits simulated
//!   gyro + sun-sensor + magnetometer measurements.
//! * [`NavSystem`] wraps [`metor_fsw_adcs::mekf::State`] (a multiplicative EKF) and
//!   estimates attitude from the two vector observations + gyro.
//! * [`CtrlSystem`] wraps [`metor_fsw_adcs::yang_lqr::YangLQR`] and produces the
//!   body torque that drives the spacecraft toward the target attitude.
//!
//! Registration order is Plant → Nav → Ctrl, so Plant→Nav→Ctrl resolve in the same
//! cycle; the Ctrl→Plant torque edge is declared `connect_delayed` (KDL `delayed=#true`),
//! the explicit one-cycle-delayed feedback back-edge that breaks the loop's cycle at
//! build time. On the first cycle Plant has no torque yet and defaults to zero.
//!
//! ## A note on frame field types
//!
//! The mission brief asked for **plain `[f64; N]` array** frame fields. Those do not
//! compile: `#[derive(Frame)]` requires every leaf field to implement
//! `AsComponentView` / `FromComponentView`, and arrays implement neither (only
//! scalars and nox tensor types do). The brief's suggested fallback — that nox types
//! "may not satisfy the Frame derive bounds" — is the opposite of reality: nox
//! `Vector` / `Quaternion` satisfy *all* the bounds (they derive the zerocopy traits
//! and have `AsComponentView`/`FromComponentView`), so they are the only ergonomic
//! way to put a 3- or 4-vector in a frame. We use them here and the ADCS math (which
//! already speaks nox) flows in and out with zero hand-written conversions.

use std::sync::{LazyLock, Mutex};

use metor_proto::types::Timestamp;
use nox::{
    ArrayRepr, Body, Quaternion, SpatialForce, SpatialInertia, SpatialMotion, SpatialTransform,
    Vector, six_dof_rk4, tensor,
};
use rand::SeedableRng;
use rand::rngs::StdRng;
use rand_distr::{Distribution, Normal};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use std::time::Duration;

use metor_fsw_2::{
    ClockMode, Coordinator, CoordinatorConfig, CyclicSystem, Input, Out, Output, PortRef, System,
    SystemInput, SystemOutput, WireError,
    wiring::{FromKdlNode, RegisteredSystem, Registry},
};

/// A body-frame / world-frame 3-vector in nox's array representation.
type V3 = Vector<f64, 3, ArrayRepr>;
/// A quaternion in nox's array representation.
type Quat = Quaternion<f64, ArrayRepr>;

/// Physics / FSW step (matches the existing cube-sat example: 120 Hz).
pub const DT: f64 = 1.0 / 120.0;

/// Principal moments of inertia (kg·m²) — the cube-sat values.
fn inertia_diag() -> V3 {
    tensor![15204079.70002e-9, 14621352.61765e-9, 6237758.3131e-9]
}

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

/// Simulated sensor measurements produced by the plant each cycle: the gyro rate
/// (body frame) plus two normalized vector observations and their inertial
/// references (sun + magnetometer) feeding the MEKF.
#[derive(metor_fsw_2::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone)]
#[repr(C)]
#[metor_fsw(name = "sensors")]
pub struct Sensors {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    pub gyro: V3,
    pub sun_body: V3,
    pub mag_body: V3,
    pub sun_ref: V3,
    pub mag_ref: V3,
}

/// The navigation filter's attitude estimate.
#[derive(metor_fsw_2::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone)]
#[repr(C)]
#[metor_fsw(name = "attitude_estimate")]
pub struct AttitudeEstimate {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    pub q_hat: Quat,
    pub omega: V3,
    pub b_hat: V3,
}

/// The commanded body-frame control torque (the feedback back-edge into the plant).
#[derive(metor_fsw_2::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone)]
#[repr(C)]
#[metor_fsw(name = "torque_cmd")]
pub struct TorqueCmd {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    pub torque: V3,
}

/// Ground-truth attitude + body rate the plant knows (for the convergence
/// assertion). Produced but, in this graph, not consumed by another system.
#[derive(metor_fsw_2::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone)]
#[repr(C)]
#[metor_fsw(name = "truth")]
pub struct Truth {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    pub q_true: Quat,
    pub omega_true: V3,
}

// ---------------------------------------------------------------------------
// Convergence log (shared out of the closed loop for the assertion / demo)
// ---------------------------------------------------------------------------

/// Per-cycle convergence samples the plant records: the attitude error angle to the
/// target (rad) and the body-rate magnitude (rad/s). A process-global because the
/// KDL-wiring path constructs systems from params alone and cannot be handed an
/// `Arc` — mirroring how `tests_wiring.rs` observes its async logger through statics.
#[derive(Default)]
pub struct SimLog {
    pub err_angle: Vec<f64>,
    pub rate: Vec<f64>,
}

pub static SIM_LOG: LazyLock<Mutex<SimLog>> = LazyLock::new(|| Mutex::new(SimLog::default()));

/// Clear the shared convergence log before a run.
pub fn reset_sim_log() {
    let mut log = SIM_LOG.lock().unwrap();
    log.err_angle.clear();
    log.rate.clear();
}

// ---------------------------------------------------------------------------
// Plant / dynamics system
// ---------------------------------------------------------------------------

/// Plant parameters (also the KDL params).
#[derive(FromKdlNode)]
pub struct PlantParams {
    /// Initial attitude offset from the target, radians about the [1,1,1] axis.
    pub init_angle: f64,
    /// Initial body-rate magnitude, rad/s about [1,1,1].
    pub init_rate: f64,
    /// 1-sigma sensor noise (rad/s for gyro, unitless for the normalized vectors).
    pub meas_sigma: f64,
    /// RNG seed, so a run is reproducible.
    pub seed: i64,
}

/// The rigid-body plant: integrates rotational dynamics under the commanded torque
/// and emits noisy sensor measurements.
pub struct PlantSystem {
    body: Body,
    sun_ref: V3,
    mag_ref: V3,
    bias: V3,
    meas_sigma: f64,
    target: Quat,
    rng: StdRng,
}

#[derive(SystemInput)]
pub struct PlantIn {
    pub torque: Input<TorqueCmd>,
}

#[derive(SystemOutput)]
pub struct PlantOut {
    pub sensors: Output<Sensors>,
    pub truth: Output<Truth>,
}

impl PlantSystem {
    pub fn new(p: PlantParams) -> Self {
        // Start rotated `init_angle` about [1,1,1] from the (identity) target, with a
        // small initial tumble about the same axis.
        let axis: V3 = tensor![1.0, 1.0, 1.0];
        let q0 = Quaternion::from_axis_angle(axis, p.init_angle);
        let omega0_world = axis.normalize() * p.init_rate;
        let body = Body {
            pos: SpatialTransform::new(q0, tensor![1.0, 0.0, 0.0]),
            vel: SpatialMotion::new(omega0_world, tensor![0.0, 0.0, 0.0]),
            accel: SpatialMotion::zero(),
            inertia: SpatialInertia::new(inertia_diag(), tensor![0.0, 0.0, 0.0], 1.0),
            force: SpatialForce::zero(),
        };
        Self {
            body,
            sun_ref: tensor![0.0, 0.0, 1.0],
            mag_ref: tensor![1.0, 0.0, 0.0],
            bias: V3::zeros(),
            meas_sigma: p.meas_sigma,
            target: Quaternion::identity(),
            rng: StdRng::seed_from_u64(p.seed as u64),
        }
    }

    fn noise(&mut self, sigma: f64) -> V3 {
        let d = Normal::new(0.0, sigma).unwrap();
        tensor![
            d.sample(&mut self.rng),
            d.sample(&mut self.rng),
            d.sample(&mut self.rng)
        ]
    }
}

impl System for PlantSystem {
    type Input = PlantIn;
    type Output = Out<PlantOut>;
    const NAME: &'static str = "plant";
}

impl CyclicSystem for PlantSystem {
    fn execute(&mut self, now: Timestamp, input: &mut PlantIn, o: &mut Self::Output) {
        // Latest commanded torque (zero on the first cycle / if none has arrived).
        let torque: V3 = match input.torque.latest() {
            Ok(Some(cmd)) => cmd.get().torque,
            _ => V3::zeros(),
        };

        // One rk4 step of rigid-body dynamics under that (constant-over-step) torque.
        self.body = six_dof_rk4(DT, self.body.clone(), |_| SpatialForce::from_torque(torque));

        let att = self.body.pos.angular();
        let att_inv = att.inverse();
        // Gyro: world-frame rate brought into the body frame, plus a slow bias walk.
        let gyro_body = &att_inv * self.body.vel.angular();
        self.bias = self.bias + self.noise(self.meas_sigma * 1e-2);
        let gyro = gyro_body.clone() + self.bias + self.noise(self.meas_sigma);
        // Two normalized vector observations in the body frame.
        let sun_body = (&att_inv * self.sun_ref).normalize() + self.noise(self.meas_sigma);
        let mag_body = (&att_inv * self.mag_ref).normalize() + self.noise(self.meas_sigma);

        let _ = o.sensors.write(&Sensors {
            timestamp: now,
            gyro,
            sun_body,
            mag_body,
            sun_ref: self.sun_ref,
            mag_ref: self.mag_ref,
        });
        let _ = o.truth.write(&Truth {
            timestamp: now,
            q_true: att.clone(),
            omega_true: gyro_body.clone(),
        });

        // Record convergence: angle to target and body-rate magnitude.
        let err = att.angular_distance(&self.target).into_buf().abs();
        let rate = gyro_body.norm().into_buf();
        let mut log = SIM_LOG.lock().unwrap();
        log.err_angle.push(err);
        log.rate.push(rate);
    }
}

impl RegisteredSystem for PlantSystem {
    type Params = PlantParams;
    fn new(params: Self::Params) -> Self {
        PlantSystem::new(params)
    }
}

// ---------------------------------------------------------------------------
// Navigation filter system (MEKF)
// ---------------------------------------------------------------------------

#[derive(FromKdlNode)]
pub struct NavParams {
    /// MEKF measurement 1-sigma for the two vector observations.
    pub meas_sigma: f64,
}

pub struct NavSystem {
    state: metor_fsw_adcs::mekf::State,
    sigma: f64,
}

#[derive(SystemInput)]
pub struct NavIn {
    pub sensors: Input<Sensors>,
}

#[derive(SystemOutput)]
pub struct NavOut {
    pub estimate: Output<AttitudeEstimate>,
}

impl NavSystem {
    pub fn new(p: NavParams) -> Self {
        let state =
            metor_fsw_adcs::mekf::State::new(tensor![0.01, 0.01, 0.01], tensor![0.01, 0.01, 0.01], DT);
        Self { state, sigma: p.meas_sigma }
    }
}

impl System for NavSystem {
    type Input = NavIn;
    type Output = Out<NavOut>;
    const NAME: &'static str = "nav";
}

impl CyclicSystem for NavSystem {
    fn execute(&mut self, now: Timestamp, input: &mut NavIn, o: &mut Self::Output) {
        let Ok(Some(s)) = input.sensors.latest() else {
            return; // no sample yet
        };
        let s = s.get().clone();

        self.state.omega = s.gyro;
        self.state = self.state.clone().estimate_attitude(
            [s.sun_body, s.mag_body],
            [s.sun_ref, s.mag_ref],
            [self.sigma, self.sigma],
        );
        self.state.reset_if_invalid();

        let _ = o.estimate.write(&AttitudeEstimate {
            timestamp: now,
            q_hat: self.state.q_hat.clone(),
            omega: s.gyro, // pass the measured body rate through to the controller
            b_hat: self.state.b_hat,
        });
    }
}

impl RegisteredSystem for NavSystem {
    type Params = NavParams;
    fn new(params: Self::Params) -> Self {
        NavSystem::new(params)
    }
}

// ---------------------------------------------------------------------------
// Controller system (Yang LQR)
// ---------------------------------------------------------------------------

#[derive(FromKdlNode)]
pub struct CtrlParams {
    /// LQR attitude/rate state weight (q) and control weight (r).
    pub q_weight: f64,
    pub r_weight: f64,
}

pub struct CtrlSystem {
    lqr: metor_fsw_adcs::yang_lqr::YangLQR,
    target: Quat,
}

#[derive(SystemInput)]
pub struct CtrlIn {
    pub estimate: Input<AttitudeEstimate>,
}

#[derive(SystemOutput)]
pub struct CtrlOut {
    pub torque: Output<TorqueCmd>,
}

impl CtrlSystem {
    pub fn new(p: CtrlParams) -> Self {
        let q = tensor![p.q_weight, p.q_weight, p.q_weight];
        let r = tensor![p.r_weight, p.r_weight, p.r_weight];
        let lqr = metor_fsw_adcs::yang_lqr::YangLQR::new(inertia_diag(), q, q, r);
        Self { lqr, target: Quaternion::identity() }
    }
}

impl System for CtrlSystem {
    type Input = CtrlIn;
    type Output = Out<CtrlOut>;
    const NAME: &'static str = "ctrl";
}

impl CyclicSystem for CtrlSystem {
    fn execute(&mut self, now: Timestamp, input: &mut CtrlIn, o: &mut Self::Output) {
        let Ok(Some(e)) = input.estimate.latest() else {
            return;
        };
        let e = e.get();
        let q_hat = e.q_hat.clone();
        // Body rate rotated into the world frame, matching the cube-sat recipe.
        let ang_vel = &q_hat * e.omega;
        let torque = self.lqr.control(q_hat, ang_vel, self.target.clone());

        let _ = o.torque.write(&TorqueCmd {
            timestamp: now,
            torque,
        });
    }
}

impl RegisteredSystem for CtrlSystem {
    type Params = CtrlParams;
    fn new(params: Self::Params) -> Self {
        CtrlSystem::new(params)
    }
}

// ---------------------------------------------------------------------------
// Wiring — registry for the KDL path + the KDL document
// ---------------------------------------------------------------------------

/// The app's auditable list of loadable systems.
pub fn registry() -> Registry {
    let mut r = Registry::new();
    r.register::<PlantSystem, _>("Plant");
    r.register::<NavSystem, _>("Nav");
    r.register::<CtrlSystem, _>("Ctrl");
    r
}

/// Wire the three-system closed loop **code-first** via the coordinator builder.
///
/// Registration order Plant → Nav → Ctrl makes Plant→Nav→Ctrl resolve in the same
/// cycle; the Ctrl→Plant `torque_cmd` edge is wired with [`connect_delayed`] as the
/// explicit one-cycle-delayed feedback back-edge (so the loop's cycle is broken at
/// build time, not by registration luck). The clock is [`ClockMode::Simulated`]
/// with `dt = DT` (1/120 s): the loop free-runs (no wall pacing) and every system's
/// `now`, and the physics integrator, advance by `DT` per cycle.
pub fn build_code_first() -> Result<Coordinator, WireError> {
    let mut b = Coordinator::builder(CoordinatorConfig {
        cycle_rate: 120.0,
        default_depth: metor_fsw_2::DEFAULT_DEPTH,
        clock: ClockMode::Simulated {
            dt: Duration::from_secs_f64(DT),
        },
    });
    let plant = b.add_cyclic(PlantSystem::new(PlantParams {
        init_angle: 0.5,
        init_rate: 0.15,
        meas_sigma: 0.002,
        seed: 42,
    }));
    let nav = b.add_cyclic(NavSystem::new(NavParams { meas_sigma: 0.02 }));
    let ctrl = b.add_cyclic(CtrlSystem::new(CtrlParams { q_weight: 5.0, r_weight: 8.0 }));

    b.connect(PortRef::new::<Sensors>(plant), PortRef::new::<Sensors>(nav))?;
    b.connect(
        PortRef::new::<AttitudeEstimate>(nav),
        PortRef::new::<AttitudeEstimate>(ctrl),
    )?;
    b.connect_delayed(PortRef::new::<TorqueCmd>(ctrl), PortRef::new::<TorqueCmd>(plant))?;
    b.build()
}

/// The same three-system closed loop, expressed as a KDL wiring document. `sim_dt`
/// selects the free-running simulated clock (1/120 s logical step) and the Ctrl →
/// Plant torque edge is `delayed=#true` — the KDL twin of `connect_delayed`.
///
/// The trailing `telemetry` node (WP7) streams *every* output frame — the ADCS
/// `sensors`/`attitude_estimate`/`torque_cmd` plus each system's health/log — out to a
/// metor-db/ground endpoint in metor-proto's wire format. If nothing is listening, the
/// sender just fails to connect and stops downlinking; the control loop is unaffected.
pub const KDL: &str = r#"
coordinator cycle_rate=120.0 sim_dt=0.008333333333333333

system "plant" type="Plant" init_angle=0.5 init_rate=0.15 meas_sigma=0.002 seed=42
system "nav"   type="Nav"   meas_sigma=0.02
system "ctrl"  type="Ctrl"  q_weight=5.0 r_weight=8.0

connect "plant" -> "nav"  frame="sensors"
connect "nav"   -> "ctrl" frame="attitude_estimate"
connect "ctrl"  -> "plant" frame="torque_cmd" delayed=#true

telemetry {
    transport "tcp" addr="127.0.0.1:2240"
    mode "all"
}
"#;
