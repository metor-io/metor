use metor_component::{AsVTable, Metadatatize};
use nox::{ArrayRepr, Field, Matrix, Matrix3, Matrix3x6, Matrix6, Quaternion, Vector, tensor};
use zerocopy::{Immutable, IntoBytes, KnownLayout};

pub fn calculate_covariance(
    sigma_g: Vector<f64, 3, ArrayRepr>,
    sigma_b: Vector<f64, 3, ArrayRepr>,
    dt: f64,
) -> Matrix<f64, 6, 6, ArrayRepr> {
    let variance_g: Matrix3<f64, ArrayRepr> = Matrix::from_diag(sigma_g * sigma_g * dt);
    let variance_b = Matrix::from_diag(sigma_b * sigma_b * dt);
    let q_00 = variance_g + variance_b * dt * (dt / 3.0);
    let q_01 = variance_b * (dt / 2.0);
    let q_10 = q_01;
    let q_11 = variance_b;
    let q_0: Matrix3x6<f64, ArrayRepr> = Matrix::concat_in_dim([q_00, q_01], 1);
    let q_1: Matrix3x6<f64, ArrayRepr> = Matrix::concat_in_dim([q_10, q_11], 1);
    Matrix::concat_in_dim([q_0, q_1], 0)
}

fn propagate_quaternion(
    q: Quaternion<f64, ArrayRepr>,
    omega: Vector<f64, 3, ArrayRepr>,
    dt: f64,
) -> Quaternion<f64, ArrayRepr> {
    let omega_norm = omega.norm();
    let c = (0.5 * omega_norm * dt).cos();
    let s = (0.5 * omega_norm * dt).sin() / omega_norm;
    let omega_s = s * omega;
    let [x, y, z] = omega_s.parts();
    let big_omega = Matrix::from_scalars([c, z, -y, x, -z, c, x, y, y, -x, c, z, -x, -y, -z, c]);
    if omega_norm.inner().buf > 1e-5 {
        Quaternion(big_omega.dot(&q.0))
    } else {
        q
    }
}

fn propagate_state_covariance(
    big_p: Matrix6<f64, ArrayRepr>,
    omega: Vector<f64, 3, ArrayRepr>,
    yqy: Matrix6<f64, ArrayRepr>,
    dt: f64,
) -> Matrix6<f64, ArrayRepr> {
    let omega_norm_squared = omega.norm_squared();
    let omega_norm = omega.norm();
    let s = (omega_norm * dt).sin();
    let c = (omega_norm * dt).cos();
    let p = s / omega_norm;
    let one = f64::one();
    let q = (one - c) / omega_norm_squared;
    let r = (omega_norm * dt - s) / (omega_norm_squared * omega_norm);
    let omega_cross = omega.skew();
    let omega_cross_square = omega_cross.dot(&omega_cross);
    let eye = Matrix3::eye();
    let phi_00: Matrix3<f64, ArrayRepr> = if omega_norm.into_buf() > 1e-5 {
        eye - omega_cross * p + omega_cross_square * q
    } else {
        eye
    };
    let phi_01: Matrix3<f64, ArrayRepr> = if omega_norm.into_buf() > 1e-5 {
        omega_cross * q - eye * dt - omega_cross_square * r
    } else {
        Matrix::eye() * -dt
    };
    let phi_10: Matrix3<f64, ArrayRepr> = Matrix::zeros();
    let phi_11: Matrix3<f64, ArrayRepr> = Matrix::eye();

    let phi_0: Matrix3x6<f64, ArrayRepr> = Matrix::concat_in_dim([phi_00, phi_01], 1);
    let phi_1: Matrix3x6<f64, ArrayRepr> = Matrix::concat_in_dim([phi_10, phi_11], 1);
    let phi: Matrix6<f64, ArrayRepr> = Matrix::concat_in_dim([phi_0, phi_1], 0);

    phi.dot(&big_p).dot(&phi.transpose()) + yqy
}

pub type V3 = Vector<f64, 3, ArrayRepr>;
type V6 = Vector<f64, 6, ArrayRepr>;
type M6 = Matrix6<f64, ArrayRepr>;
type Quat = Quaternion<f64, ArrayRepr>;

/// A unit vector measured in the body frame, its reference in the inertial
/// frame, and the measurement's standard deviation (rad).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VectorObs {
    pub body: V3,
    pub reference: V3,
    pub sigma: f64,
}

/// How many observations an update fused and how many it gated. An update
/// that rejects the whole frame counts every observation as gated.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UpdateCounts {
    pub used: u32,
    pub gated: u32,
}

#[derive(Debug, Clone, AsVTable, Metadatatize, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct State {
    pub q_hat: Quaternion<f64, ArrayRepr>,
    pub b_hat: Vector<f64, 3, ArrayRepr>,
    pub p: Matrix<f64, 6, 6, ArrayRepr>,
    pub omega: Vector<f64, 3, ArrayRepr>,
    pub yqy: Matrix6<f64, ArrayRepr>,
    pub dt: f64,
}

impl State {
    pub fn new(
        sigma_g: Vector<f64, 3, ArrayRepr>,
        sigma_b: Vector<f64, 3, ArrayRepr>,
        dt: f64,
    ) -> Self {
        let y: Matrix6<f64, ArrayRepr> =
            Matrix::from_diag(tensor![-1.0, -1.0, -1.0, 1.0, 1.0, 1.0]);
        let q = calculate_covariance(sigma_g, sigma_b, dt);
        let yqy = y.dot(&q).dot(&y.transpose());
        Self {
            q_hat: Default::default(),
            b_hat: Default::default(),
            p: Matrix::eye(),
            omega: Default::default(),
            yqy,
            dt,
        }
    }

    /// Propagates and then fuses `N` vectors with no gate.
    pub fn estimate_attitude<const N: usize>(
        mut self,
        measured_bodys: [V3; N],
        references: [V3; N],
        sigma_r: [f64; N],
    ) -> Self {
        self.propagate();
        let obs: [VectorObs; N] = core::array::from_fn(|i| VectorObs {
            body: measured_bodys[i],
            reference: references[i],
            sigma: sigma_r[i],
        });
        self.update_vectors(&obs, f64::INFINITY);
        self
    }

    /// Removes the bias from `omega` and propagates `q_hat` and `p` by `dt`.
    /// The bias is subtracted from `omega` in place, so `omega` must be set
    /// from the gyro before each call; calling it twice corrects twice.
    pub fn propagate(&mut self) {
        self.omega = self.omega - self.b_hat;
        self.q_hat = propagate_quaternion(self.q_hat, self.omega, self.dt);
        self.p = propagate_state_covariance(self.p, self.omega, self.yqy, self.dt);
    }

    /// The trace of the attitude block of `p` (rad²), an upper bound on its
    /// largest eigenvalue.
    pub fn attitude_variance(&self) -> f64 {
        let p = self.p.into_buf();
        p[0][0] + p[1][1] + p[2][2]
    }

    /// Sets the initial gyro bias covariance to `sigma²·I` (rad²/s²).
    pub fn with_bias_sigma(mut self, sigma: f64) -> Self {
        let mut p = self.p.into_buf();
        for (i, row) in p.iter_mut().enumerate().skip(3) {
            row[i] = sigma * sigma;
        }
        self.p = Matrix::from_buf(p);
        self
    }
}

impl State {
    /// Checks if the estimation parameters are all finite (i.e not NaN or infinite)
    pub fn is_non_finite(&self) -> bool {
        self.q_hat.0.into_buf().iter().any(|x| !x.is_finite())
            || self.b_hat.into_buf().iter().any(|x| !x.is_finite())
            || self
                .p
                .into_buf()
                .as_flattened()
                .iter()
                .any(|x| !x.is_finite())
            || self.omega.into_buf().iter().any(|x| !x.is_finite())
    }

    /// Resets the state if any parameter is non-finite
    pub fn reset_if_invalid(&mut self) {
        if self.is_non_finite() {
            self.q_hat = Quaternion::identity();
            self.b_hat = Vector::zeros();
            self.p = Matrix::eye();
            self.omega = Vector::zeros();
        }
    }
}

mod update;

pub use update::{MAX_OBS, valid_count};

#[cfg(test)]
mod tests;
