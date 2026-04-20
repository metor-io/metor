//! Orbit-camera state and input math, kept on the gpui side so the
//! Bevy world only ever sees the final resolved [`Transform`].

use glam::Vec3;

/// Orbit camera: spherical coordinates around a world-space target.
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

/// Pitch clamp; avoids gimbal flip at the poles.
const PITCH_LIMIT: f32 = std::f32::consts::FRAC_PI_2 - 0.01;

impl OrbitCamera {
    /// Apply a pixel-space rotation drag. Positive `dx` yaws right; positive
    /// `dy` pitches up (matches Blender/Maya conventions).
    pub fn rotate(&mut self, dx: f32, dy: f32) {
        const RADIANS_PER_PIXEL: f32 = 0.005;
        self.yaw -= dx * RADIANS_PER_PIXEL;
        self.pitch = (self.pitch + dy * RADIANS_PER_PIXEL).clamp(-PITCH_LIMIT, PITCH_LIMIT);
    }

    /// Translate the target in camera-local right/up.
    ///
    /// Pan speed scales with orbit distance so far-away views pan at a
    /// comparable on-screen rate to close-ups.
    pub fn pan(&mut self, dx: f32, dy: f32) {
        let pan_speed = self.distance * 0.0015;
        let (right, up) = self.right_up();
        self.target += -right * dx * pan_speed + up * dy * pan_speed;
    }

    /// Multiplicatively adjust orbit distance. Positive `delta` zooms out.
    pub fn zoom(&mut self, delta: f32) {
        let factor = (1.0 + delta * 0.0015).clamp(0.5, 2.0);
        self.distance = (self.distance * factor).clamp(0.1, 10_000.0);
    }

    /// World-space eye position implied by the current orbit state.
    pub fn eye(&self) -> Vec3 {
        let cp = self.pitch.cos();
        let offset = Vec3::new(
            self.distance * cp * self.yaw.sin(),
            self.distance * self.pitch.sin(),
            self.distance * cp * self.yaw.cos(),
        );
        self.target + offset
    }

    /// Camera-local right and up axes; used to resolve pan offsets.
    fn right_up(&self) -> (Vec3, Vec3) {
        let forward = (self.target - self.eye()).normalize_or_zero();
        let world_up = Vec3::Y;
        let right = forward.cross(world_up).normalize_or_zero();
        let up = right.cross(forward).normalize_or_zero();
        (right, up)
    }
}
