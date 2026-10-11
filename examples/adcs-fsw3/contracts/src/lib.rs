//! Frames, params, and physics shared by the `adcs-fsw3` systems.
//!
//! The plant simulates a cube-sat in a 400 km orbit and writes what its sensors
//! measure each cycle. The flight software (nav, ctrl, and mode) reads those
//! measurements and sends torque and dipole commands back. Both sides evaluate
//! the same environment models, the sun direction and the WMM magnetic field, so
//! those models live here with the frames.
//!
//! The flight software sees only [`Sensors`] and [`Gps`]. Nav reconstructs the
//! sun direction from the raw sun sensor readings and models its inertial
//! references at the GPS fix. [`BodyState`], [`World`], and [`Disturbances`]
//! carry the plant's ground truth for plotting and for scoring convergence, and
//! no flight system reads them.
//!
//! Vector names end in the frame they are expressed in: `_b` for the body frame
//! and `_eci` for Earth-centered inertial.

use nox::{ArrayRepr, Quaternion, Vector};

mod environment;
mod frames;
mod mtq;
mod orbit;
mod params;
mod pointing;
mod spacecraft;

pub use environment::*;
pub use frames::*;
pub use mtq::*;
pub use orbit::*;
pub use params::*;
pub use pointing::*;
pub use spacecraft::*;

pub use hifitime::Epoch;
pub use wmm::MagneticModel;

/// A 3-vector of `f64`.
pub type V3 = Vector<f64, 3, ArrayRepr>;
/// A quaternion of `f64`.
pub type Quat = Quaternion<f64, ArrayRepr>;

/// Cycle period (s), 120 Hz.
pub const DT: f64 = 1.0 / 120.0;
