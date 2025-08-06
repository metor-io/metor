use core::ops::{Add, Mul};

use crate::{Dim, Field, Scalar, Tensor};

pub fn rk4<DT: Field, U, DU, F>(dt: impl Into<Scalar<DT>>, state: &U, func: F) -> U
where
    F: for<'a> Fn(&'a U) -> DU,
    for<'a> &'a U: Add<DU, Output = U>,
    DU: Add<DU, Output = DU>,
    Scalar<DT>: for<'a> Mul<&'a DU, Output = DU>,
{
    let dt = dt.into();
    let two = DT::two();
    let k1: DU = func(state);
    let k2 = func(&(state + dt / &two * &k1));
    let k3 = func(&(state + dt / &two * &k2));
    let k4 = func(&(state + dt * &k3));

    state + dt / DT::six() * &(k1 + (two * &k2) + two * &k3 + k4)
}

#[cfg(test)]
mod tests {
    use approx::assert_relative_eq;

    use crate::{Vector, tensor};

    use super::*;

    #[test]
    fn test_rk4_simple() {
        let func = |u: &Vector<f64, 2>| {
            let [_x, v] = u.parts();
            Vector::from_arr([v, 0.0.into()])
        };
        let state = Vector::<f64, 2>::from([0.0, 2.0]);

        let result = rk4(0.1, &state, func);
        assert_eq!(result, tensor![0.2, 2.0]);

        let func = |u: &Vector<f64, 2>| {
            let [_x, v] = u.parts();
            Vector::from_arr([v, 1.0.into()])
        };
        let mut state = Vector::<f64, 2>::from([0.0, 2.0]);

        for _ in 0..10 {
            state = rk4(0.1, &state, func);
        }
        assert_relative_eq!(state, tensor![2.5, 3.0], epsilon = 1e-6);
    }
}
