//! Per-viewer orbit camera state and the math for turning mouse/scroll
//! deltas into new yaw/pitch/distance/target values.
//!
//! The camera is purely GPUI-side data; it's serialized into
//! [`ViewerCommand::SetCamera`] and shipped to Bevy each time it changes.
//! Keeping the math out of the Bevy systems makes it easy to unit-test and
//! avoids a round-trip through the command channel for every frame.

use glam::Vec3;

use super::bridge::ViewerCommand;
use super::bridge::ViewerId;

/// An orbit camera rotating around a world-space target point.
#[derive(Clone, Copy, Debug)]
pub struct OrbitCamera {
    pub target: Vec3,
    pub yaw: f32,
    pub pitch: f32,
    pub distance: f32,
    pub fov_y_rad: f32,
}

impl Default for OrbitCamera {
    fn default() -> Self {
        Self {
            target: Vec3::ZERO,
            yaw: std::f32::consts::FRAC_PI_4,
            pitch: std::f32::consts::FRAC_PI_6,
            distance: 5.0,
            fov_y_rad: std::f32::consts::FRAC_PI_3,
        }
    }
}

/// Clamp pitch to avoid gimbal flip at the poles.
const PITCH_LIMIT: f32 = std::f32::consts::FRAC_PI_2 - 0.01;

impl OrbitCamera {
    /// Apply a rotation delta from a mouse drag in pixels. Negative dy looks
    /// down; positive dx yaws right. Matches the convention used by most
    /// 3D tools on first try.
    pub fn rotate(&mut self, dx: f32, dy: f32) {
        const RADIANS_PER_PIXEL: f32 = 0.005;
        self.yaw -= dx * RADIANS_PER_PIXEL;
        self.pitch = (self.pitch + dy * RADIANS_PER_PIXEL).clamp(-PITCH_LIMIT, PITCH_LIMIT);
    }

    /// Pan the target in the camera's local right/up plane. The pan speed
    /// scales with the distance so far-away views don't feel sluggish.
    pub fn pan(&mut self, dx: f32, dy: f32) {
        let pan_speed = self.distance * 0.0015;
        let (right, up) = self.right_up();
        self.target += -right * dx * pan_speed + up * dy * pan_speed;
    }

    /// Zoom in or out by a scroll delta. Positive `delta` moves the camera
    /// away from the target.
    pub fn zoom(&mut self, delta: f32) {
        let factor = (1.0 + delta * 0.0015).clamp(0.5, 2.0);
        self.distance = (self.distance * factor).clamp(0.1, 10_000.0);
    }

    /// World-space eye position implied by the current spherical coords.
    pub fn eye(&self) -> Vec3 {
        let cp = self.pitch.cos();
        let offset = Vec3::new(
            self.distance * cp * self.yaw.sin(),
            self.distance * self.pitch.sin(),
            self.distance * cp * self.yaw.cos(),
        );
        self.target + offset
    }

    /// The camera's local right and up axes, used for panning.
    fn right_up(&self) -> (Vec3, Vec3) {
        let forward = (self.target - self.eye()).normalize_or_zero();
        let world_up = Vec3::Y;
        let right = forward.cross(world_up).normalize_or_zero();
        let up = right.cross(forward).normalize_or_zero();
        (right, up)
    }

    /// Build a [`ViewerCommand::SetCamera`] snapshot for the given viewer.
    pub fn to_command(&self, id: ViewerId) -> ViewerCommand {
        ViewerCommand::SetCamera {
            id,
            target: self.target,
            yaw: self.yaw,
            pitch: self.pitch,
            distance: self.distance,
            fov_y_rad: self.fov_y_rad,
        }
    }
}
