//! The frames the systems exchange.

use metor_fsw_3::Timestamp;
use nox::{ArrayRepr, Matrix6};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
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

/// An ADCS phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum Mode {
    /// Holds nadir.
    Idle = 0,
    /// Slews toward the velocity vector.
    Settling = 1,
    /// Holds the velocity vector.
    Pointing = 2,
    /// Holds nadir.
    Safe = 3,
    /// Rate damping on the magnetorquers, for boot rates too high for the wheels.
    Detumble = 4,
}

/// A byte that names no [`Mode`] or [`Law`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnknownCode(pub u8);

impl TryFrom<u8> for Mode {
    type Error = UnknownCode;

    fn try_from(code: u8) -> Result<Self, UnknownCode> {
        match code {
            0 => Ok(Self::Idle),
            1 => Ok(Self::Settling),
            2 => Ok(Self::Pointing),
            3 => Ok(Self::Safe),
            4 => Ok(Self::Detumble),
            _ => Err(UnknownCode(code)),
        }
    }
}

impl Mode {
    /// The pointing law the controller follows in this phase.
    pub const fn law(self) -> Law {
        match self {
            Self::Idle | Self::Safe => Law::Nadir,
            Self::Settling | Self::Pointing => Law::Hil,
            Self::Detumble => Law::Detumble,
        }
    }
}

/// A pointing law.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Law {
    Nadir = 0,
    /// Body `-Y` along the orbital velocity.
    Hil = 1,
    /// Rate damping on the magnetorquers while the wheels idle.
    Detumble = 2,
}

impl TryFrom<u8> for Law {
    type Error = UnknownCode;

    fn try_from(code: u8) -> Result<Self, UnknownCode> {
        match code {
            0 => Ok(Self::Nadir),
            1 => Ok(Self::Hil),
            2 => Ok(Self::Detumble),
            _ => Err(UnknownCode(code)),
        }
    }
}

impl Law {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Nadir => "nadir",
            Self::Hil => "velocity-vector",
            Self::Detumble => "detumble",
        }
    }
}

/// The commanded ADCS phase.
#[derive(metor_fsw_3::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug)]
#[repr(C)]
#[frame(name = "mode_cmd")]
pub struct ModeCmd {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    /// The [`Mode`]'s code.
    mode: u8,
    _pad: [u8; 7],
}

impl ModeCmd {
    /// Returns the command for `mode` with a zero timestamp.
    pub const fn new(mode: Mode) -> Self {
        Self {
            timestamp: Timestamp(0),
            mode: mode as u8,
            _pad: [0; 7],
        }
    }

    /// The commanded phase, or the byte when it names none.
    pub fn mode(&self) -> Result<Mode, UnknownCode> {
        Mode::try_from(self.mode)
    }

    pub const fn idle() -> Self {
        Self::new(Mode::Idle)
    }
    pub const fn settling() -> Self {
        Self::new(Mode::Settling)
    }
    pub const fn pointing() -> Self {
        Self::new(Mode::Pointing)
    }
    pub const fn safe() -> Self {
        Self::new(Mode::Safe)
    }
    pub const fn detumble() -> Self {
        Self::new(Mode::Detumble)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mode_round_trips_its_code() {
        for mode in [
            Mode::Idle,
            Mode::Settling,
            Mode::Pointing,
            Mode::Safe,
            Mode::Detumble,
        ] {
            assert_eq!(ModeCmd::new(mode).mode(), Ok(mode));
        }
        for law in [Law::Nadir, Law::Hil, Law::Detumble] {
            assert_eq!(Law::try_from(law as u8), Ok(law));
        }
    }

    #[test]
    fn test_law_follows_mode() {
        assert_eq!(Mode::Idle.law(), Law::Nadir);
        assert_eq!(Mode::Safe.law(), Law::Nadir);
        assert_eq!(Mode::Settling.law(), Law::Hil);
        assert_eq!(Mode::Pointing.law(), Law::Hil);
        assert_eq!(Mode::Detumble.law(), Law::Detumble);
    }

    #[test]
    fn test_unknown_code_is_refused() {
        assert_eq!(Mode::try_from(5), Err(UnknownCode(5)));
        assert_eq!(Law::try_from(3), Err(UnknownCode(3)));
        let mut cmd = ModeCmd::idle();
        cmd.mode = 0xff;
        assert_eq!(cmd.mode(), Err(UnknownCode(0xff)));
    }
}
