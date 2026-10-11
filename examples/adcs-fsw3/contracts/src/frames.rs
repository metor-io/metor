//! The frames the systems exchange.

use metor_fsw_3::Timestamp;
use nox::{ArrayRepr, Matrix6};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::{Quat, V3};

/// The plant's sensor measurements for one cycle.
#[derive(metor_fsw_3::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug)]
#[repr(C)]
#[frame(name = "sensors")]
pub struct Sensors {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    /// Measured body rate (rad/s).
    pub gyro_b: V3,
    /// Coarse sun sensor readings, one head per face in the order
    /// `[+X, +Y, +Z, −X, −Y, −Z]`. A lit head reads the cosine of its sun angle,
    /// clamped at zero, and a dark head reads its noise floor.
    pub css: [f64; 6],
    /// Measured magnetic field (T).
    pub mag_b: V3,
}

/// A GPS fix: the plant's, or the GPS driver's from the receiver's logs.
#[derive(metor_fsw_3::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug)]
#[repr(C)]
#[frame(name = "gps")]
pub struct Gps {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    /// Measured position (m).
    pub pos_eci: V3,
    /// Measured velocity (m/s).
    pub vel_eci: V3,
    /// Measured position as `[latitude_deg, longitude_deg, altitude_m]`.
    pub lla: V3,
}

/// A fault the GPS receiver model applies, as a scenario or the panel sends
/// it.
#[derive(metor_fsw_3::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug)]
#[repr(C)]
#[frame(name = "gps_fault")]
pub struct GpsFault {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    /// Nonzero while the receiver has no solution.
    pub outage: u64,
}

/// The navigation filter's attitude estimate.
#[derive(metor_fsw_3::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug)]
#[repr(C)]
#[frame(name = "attitude_estimate")]
pub struct AttitudeEstimate {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    /// Estimated attitude, body←ECI.
    pub q_hat_b_eci: Quat,
    /// Estimated body rate (rad/s).
    pub omega_b: V3,
    /// Estimated gyro bias (rad/s).
    pub b_hat_b: V3,
    /// Error covariance over attitude and gyro bias.
    pub p: Matrix6<f64, ArrayRepr>,
}

/// The spacecraft's true attitude, body rate, and orbit state.
#[derive(metor_fsw_3::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug)]
#[repr(C)]
#[frame(name = "body")]
pub struct BodyState {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    /// Attitude, body←ECI.
    pub q_b_eci: Quat,
    /// Body rate (rad/s).
    pub omega_b: V3,
    /// Position (m).
    pub pos_eci: V3,
    /// Velocity (m/s).
    pub vel_eci: V3,
}

/// A reaction wheel spinning about one body axis.
///
/// `ang_momentum` is the wheel's own momentum. The motor spins the wheel
/// against the commanded body torque, and the body receives the reaction `−ḣ`.
#[derive(
    metor_component::AsVTable,
    metor_component::Metadatatize,
    metor_component::Componentize,
    metor_component::Decomponentize,
    IntoBytes,
    Immutable,
    KnownLayout,
    FromBytes,
    Clone,
    Copy,
    Debug,
)]
#[repr(C)]
pub struct ReactionWheel {
    /// Spin axis, a unit vector in the body frame.
    pub axis: V3,
    /// Stored momentum `s·axis` (N·m·s).
    pub ang_momentum: V3,
    /// Commanded body torque projected onto `axis` (N·m).
    pub torque_set_point: V3,
    /// Torque delivered to the body this step, `−ḣ·axis` (N·m).
    pub torque: V3,
    /// Signed spin rate about `axis` (rad/s).
    pub speed: f64,
    /// Bearing friction torque this step (N·m).
    pub friction: f64,
    /// `1` when armed and `0` when disarmed. A disarmed wheel applies no motor torque.
    pub arm: u8,
    _pad: [u8; 7],
}

impl ReactionWheel {
    /// Returns a wheel at rest on `axis`.
    pub fn new(axis: V3, arm: bool) -> Self {
        Self {
            axis,
            ang_momentum: V3::zeros(),
            torque_set_point: V3::zeros(),
            torque: V3::zeros(),
            speed: 0.0,
            friction: 0.0,
            arm: arm as u8,
            _pad: [0; 7],
        }
    }

    /// Returns whether the wheel is armed.
    pub fn armed(&self) -> bool {
        self.arm != 0
    }
}

/// The three reaction wheels, with wheel `i` on body axis `i`.
#[derive(metor_fsw_3::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug)]
#[repr(C)]
#[frame(name = "wheels")]
pub struct Wheels {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    #[frame(nest)]
    pub wheels: [ReactionWheel; 3],
}

/// The true sun direction and magnetic field at the spacecraft.
#[derive(metor_fsw_3::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug)]
#[repr(C)]
#[frame(name = "world")]
pub struct World {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    /// Unit vector toward the sun.
    pub sun_eci: V3,
    /// Magnetic field (T).
    pub mag_eci: V3,
    /// `1` in sunlight and `0` in Earth shadow.
    pub illuminated: u8,
    _pad: [u8; 7],
}

impl World {
    /// Returns the environment for one cycle.
    pub fn new(timestamp: Timestamp, sun_eci: V3, mag_eci: V3, illuminated: bool) -> Self {
        Self {
            timestamp,
            sun_eci,
            mag_eci,
            illuminated: illuminated as u8,
            _pad: [0; 7],
        }
    }
}

/// The environmental torques on the body for one cycle, with the magnetorquer
/// torque alongside for comparison.
#[derive(metor_fsw_3::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug)]
#[repr(C)]
#[frame(name = "disturb")]
pub struct Disturbances {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    /// Gravity-gradient torque (N·m).
    pub gg_b: V3,
    /// Aerodynamic drag torque (N·m).
    pub aero_b: V3,
    /// Residual magnetic dipole torque (N·m).
    pub mag_b: V3,
    /// Solar radiation pressure torque (N·m).
    pub srp_b: V3,
    /// Magnetorquer torque `m × B` (N·m), left out of `total_b`.
    pub mtq_b: V3,
    /// Sum of the environmental torques (N·m).
    pub total_b: V3,
}

/// The commanded magnetorquer dipole. The plant clamps each axis to its
/// `mtq_max_dipole` and applies `m × B`.
#[derive(metor_fsw_3::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug)]
#[repr(C)]
#[frame(name = "mtq_cmd")]
pub struct MtqCmd {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    /// Commanded dipole (A·m²).
    pub dipole_b: V3,
}

/// The commanded ADCS phase and the pointing law the controller follows.
#[derive(metor_fsw_3::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug)]
#[repr(C)]
#[frame(name = "mode_cmd")]
pub struct ModeCmd {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    /// Phase: `0` idle, `1` settling, `2` pointing, `3` safe, `4` detumble.
    pub mode: u8,
    /// Pointing law: `0` nadir, `1` velocity vector, `2` detumble.
    pub law: u8,
    _pad: [u8; 6],
}

impl ModeCmd {
    pub const IDLE: u8 = 0;
    pub const SETTLING: u8 = 1;
    pub const POINTING: u8 = 2;
    pub const SAFE: u8 = 3;
    /// Rate damping on the magnetorquers, for boot rates too high for the wheels.
    pub const DETUMBLE: u8 = 4;

    pub const LAW_NADIR: u8 = 0;
    pub const LAW_HIL: u8 = 1;
    /// Rate damping on the magnetorquers while the wheels idle.
    pub const LAW_DETUMBLE: u8 = 2;

    /// Returns the command for `mode` and `law` with a zero timestamp.
    pub const fn new(mode: u8, law: u8) -> Self {
        Self {
            timestamp: Timestamp(0),
            mode,
            law,
            _pad: [0; 6],
        }
    }

    /// Returns the idle command, which holds nadir.
    pub const fn idle() -> Self {
        Self::new(Self::IDLE, Self::LAW_NADIR)
    }
    /// Returns the settling command, which slews toward the velocity vector.
    pub const fn settling() -> Self {
        Self::new(Self::SETTLING, Self::LAW_HIL)
    }
    /// Returns the pointing command, which holds the velocity vector.
    pub const fn pointing() -> Self {
        Self::new(Self::POINTING, Self::LAW_HIL)
    }
    /// Returns the safe command, which holds nadir.
    pub const fn safe() -> Self {
        Self::new(Self::SAFE, Self::LAW_NADIR)
    }
    /// Returns the detumble command.
    pub const fn detumble() -> Self {
        Self::new(Self::DETUMBLE, Self::LAW_DETUMBLE)
    }

    /// Returns the command stamped `now`.
    pub const fn stamped(mut self, now: Timestamp) -> Self {
        self.timestamp = now;
        self
    }
}

/// The commanded control torque.
#[derive(metor_fsw_3::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug)]
#[repr(C)]
#[frame(name = "torque_cmd")]
pub struct TorqueCmd {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    /// Commanded torque (N·m).
    pub torque_b: V3,
}
