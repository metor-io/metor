//! Convert a metor-panel component view into a Bevy-frame [`Vec3`] or
//! [`Quat`].
//!
//! metor-panel components are expected to use a Z-down convention:
//!
//! - **Position**: a 3-element tensor laid out as `[x, y, z]`.
//! - **Attitude**: a 4-element tensor laid out as `[i, j, k, w]` (XYZW
//!   quaternion).
//!
//! Bevy is Y-up, so a fixed swizzle is applied on every read:
//!
//! - Position `[x, y, z]` → Bevy `(x, z, -y)`
//! - Attitude `[i, j, k, w]` → Bevy `(i, k, -j, w)`
//!
//! This used to be user-configurable per binding, but the picker complexity
//! wasn't worth it — bindings now just pick a component and the swizzle is
//! standard.

use glam::{Quat, Vec3};
use metor_proto::types::ComponentView;

/// Read a position component (`[x, y, z]` in metor-panel frame) and return
/// the Bevy-frame `Vec3`. Missing elements default to `0.0`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Vec3Pick;

impl Vec3Pick {
    pub fn pick(&self, view: &ComponentView<'_>) -> Vec3 {
        let pick = |i: usize| view.get(i).map(|v| v.as_f64() as f32).unwrap_or(0.0);
        let x = pick(0);
        let y = pick(1);
        let z = pick(2);
        Vec3::new(x, z, -y)
    }
}

/// Read an attitude component (`[i, j, k, w]` in metor-panel frame) and
/// return the Bevy-frame `Quat`. Missing elements default to `0.0`. Does
/// not re-normalize — the caller's data is expected to be a unit
/// quaternion.
#[derive(Clone, Copy, Debug, Default)]
pub struct QuatPick;

impl QuatPick {
    pub fn pick(&self, view: &ComponentView<'_>) -> Quat {
        let pick = |i: usize| view.get(i).map(|v| v.as_f64() as f32).unwrap_or(0.0);
        let i = pick(0);
        let j = pick(1);
        let k = pick(2);
        let w = pick(3);
        Quat::from_xyzw(i, k, -j, w)
    }
}
