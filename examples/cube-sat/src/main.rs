use std::{
    net::SocketAddr,
    ops::Add,
    time::{Duration, Instant},
};

use impeller2::types::{LenPacket, PacketId};
use impeller2_stellar::Client;
use nox::{
    Body, DU, ReprMonad, Scalar, SpatialForce, SpatialInertia, SpatialMotion, SpatialTransform,
    Vec3, Vector3, rk4, tensor,
};
use roci::{AsVTable, Metadatatize, tcp::SinkExt};
use roci_adcs::mekf;
use stellarator::{rent, struc_con::stellar};
use zerocopy::{Immutable, IntoBytes, KnownLayout};

#[derive(AsVTable, Debug, Clone, Immutable, KnownLayout, Metadatatize, IntoBytes)]
#[repr(C)]
#[roci(parent = "cube_sat")]
pub struct CubeSat {
    #[roci(nest = true)]
    pub sim: Sim,
    #[roci(nest = true)]
    pub control: Control,
}

#[derive(AsVTable, Debug, Clone, Immutable, KnownLayout, Metadatatize, IntoBytes)]
#[repr(C)]
pub struct Sim {
    #[roci(nest = true)]
    body: Body,
    #[roci(nest = true)]
    reaction_wheels: [ReactionWheel; 3],
}

#[derive(AsVTable, Debug, Clone, Immutable, KnownLayout, Metadatatize, IntoBytes)]
#[repr(C)]
pub struct Control {
    #[roci(nest = true)]
    pub mekf: mekf::State,
}

impl Control {
    pub fn new() -> Self {
        Self {
            mekf: mekf::State::new(tensor![0.01, 0.01, 0.01], tensor![0.01, 0.01, 0.01], DT),
        }
    }
}

#[derive(AsVTable, Debug, Clone, Immutable, KnownLayout, Metadatatize, IntoBytes)]
#[repr(C)]
pub struct ReactionWheel {
    axis: Vec3<f64>,
    speed: Scalar<f64>,
    ang_momentum: Vec3<f64>,
    torque_set_point: Vec3<f64>,
    friction: Scalar<f64>,
}

impl ReactionWheel {
    pub fn new(axis: Vec3<f64>) -> Self {
        Self {
            axis,
            speed: 0.0.into(),
            ang_momentum: Vec3::zeros(),
            torque_set_point: Vec3::zeros(),
            friction: 0.0.into(),
        }
    }

    pub fn moment_of_inertia(&self) -> f64 {
        0.185 * (0.05 / 2.0_f64).powi(2) / 2.0
    }

    /// Update the wheel speed based on angular momentum
    pub fn update_speed(&mut self) {
        let i = self.moment_of_inertia();
        let momentum_norm: f64 = self.ang_momentum.norm().into_buf();
        self.speed = (momentum_norm / i).into();
    }

    /// Calculate friction torque based on the Python rw_drag function
    pub fn friction_torque(&self) -> f64 {
        let static_fric = 0.0005;
        let columb_fric = 0.0005;
        let stribeck_coef = 0.0005;
        let cv = 0.00005;
        let omega_limit = 0.1;
        let speed: f64 = self.speed.into_buf();

        let stribeck_torque = -(2.0 * std::f64::consts::E).sqrt()
            * (static_fric - columb_fric)
            * (-((speed / stribeck_coef).powi(2))).exp()
            - columb_fric * (10.0 * speed / stribeck_coef).tanh()
            - cv * speed;

        let torque_norm: f64 = self.torque_set_point.norm().into_buf();
        let use_stribeck =
            speed.abs() < 0.01 * omega_limit && speed.signum() == torque_norm.signum();

        if use_stribeck {
            stribeck_torque
        } else {
            -columb_fric * speed.signum() - cv * speed
        }
    }

    /// Calculate the net torque on the body from this reaction wheel
    /// This is the reaction torque from accelerating/decelerating the wheel
    pub fn net_torque(&self) -> Vec3<f64> {
        let rw_force_clamp = 0.002;

        // Calculate new angular momentum after applying force
        let new_ang_momentum = self.ang_momentum + self.torque_set_point * DT;

        // Saturate the torque based on momentum limits
        let new_momentum_norm: f64 = new_ang_momentum.norm().into_buf();
        let torque = if new_momentum_norm < 0.04 {
            self.torque_set_point
        } else {
            Vec3::zeros()
        };

        // Clamp the torque
        let clamped_torque = Vec3::from_buf(
            torque
                .into_buf()
                .map(|t| t.clamp(-rw_force_clamp, rw_force_clamp)),
        );

        // Add friction effects
        let friction_torque = self.friction_torque();
        let friction_vec = self.axis * friction_torque;

        // The net torque on the body is the negative of the torque applied to the wheel
        // (Newton's third law) plus friction effects
        -(clamped_torque + friction_vec)
    }

    /// Update the reaction wheel state for the next time step
    pub fn update(&mut self, dt: f64) {
        let rw_force_clamp = 0.002;

        let new_ang_momentum = self.ang_momentum + self.torque_set_point * dt;

        let new_momentum_norm: f64 = new_ang_momentum.norm().into_buf();
        let torque = if new_momentum_norm < 0.04 {
            self.torque_set_point
        } else {
            Vec3::zeros()
        };

        let clamped_torque = Vec3::from_buf(
            torque
                .into_buf()
                .map(|t| t.clamp(-rw_force_clamp, rw_force_clamp)),
        );

        self.ang_momentum = self.ang_momentum + clamped_torque * dt;
        self.friction = self.friction_torque().into();
        self.update_speed();
    }
}

impl<'a> Add<DU> for &'a CubeSat {
    type Output = CubeSat;

    fn add(self, du: DU) -> Self::Output {
        let Sim {
            body,
            mut reaction_wheels,
        } = self.sim.clone();
        let body = &body + du;

        // Update each reaction wheel
        for rw in &mut reaction_wheels {
            rw.update(DT);
        }

        CubeSat {
            sim: Sim {
                body,
                reaction_wheels,
            },
            control: self.control.clone(),
        }
    }
}

const G: f64 = 6.6743e-11; // Gravitational constant
const M: f64 = 5.972e24; // Mass of Earth
const EARTH_RADIUS: f64 = 6378.1e3;
const ALTITUDE: f64 = 400.0e3;
const DT: f64 = 1.0 / 120.0;

impl CubeSat {
    pub fn new() -> Self {
        let radius = EARTH_RADIUS + ALTITUDE;
        let initial_velocity = (G * M / radius).sqrt();
        let body = Body {
            pos: SpatialTransform::from_linear(tensor![1.0, 0.0, 0.0] * radius),
            vel: SpatialMotion::new(tensor![0.0, 1.0, 0.0], tensor![0.0, initial_velocity, 0.0]),
            accel: SpatialMotion::zero(),
            inertia: SpatialInertia::from_mass(2825.2 / 1000.0),
        };
        let sim = Sim {
            body,
            reaction_wheels: [
                ReactionWheel::new(tensor![1.0, 0.0, 0.0]),
                ReactionWheel::new(tensor![0.0, 1.0, 0.0]),
                ReactionWheel::new(tensor![0.0, 0.0, 1.0]),
            ],
        };
        let control = Control::new();
        Self { sim, control }
    }
}

impl Sim {
    pub fn gravity(&self) -> Vector3<f64> {
        // f = G*M*m/r^3 * r
        let r = self.body.pos.linear();
        let r_mag = r.norm().into_buf();
        -G * M * self.body.inertia.mass() / Scalar::from(r_mag.powi(3)) * r
    }

    pub fn reaction_wheel_torque(&self) -> Vec3<f64> {
        self.reaction_wheels
            .iter()
            .map(|wheel| wheel.net_torque())
            .sum()
    }

    pub fn du(&self) -> DU {
        let gravity_force = SpatialForce::from_linear(self.gravity());
        let rw_torque = self.reaction_wheel_torque();
        let rw_spatial_force = SpatialForce::new(Vec3::zeros(), rw_torque);
        let total_force = gravity_force + rw_spatial_force;
        nox::DU::from_body_force(&self.body, total_force)
    }
}

fn tick(cubesat: CubeSat) -> CubeSat {
    rk4::<f64, CubeSat, DU, _>(DT, &cubesat, |cubesat: &CubeSat| -> DU { cubesat.sim.du() })
}

#[stellarator::main]
pub async fn main() -> anyhow::Result<()> {
    stellar(move || metor_db::serve_tmp_db(SocketAddr::new([127, 0, 0, 1].into(), 2240)));
    stellarator::sleep(Duration::from_millis(50)).await;
    let mut client = Client::connect(SocketAddr::new([127, 0, 0, 1].into(), 2240))
        .await
        .map_err(anyhow::Error::from)?;
    let id: PacketId = fastrand::u16(..).to_le_bytes();
    println!("init world");
    client.init_world::<CubeSat>(id).await?;
    let mut cube_sat = CubeSat::new();
    let mut pkt = LenPacket::new(impeller2::types::PacketTy::Table, id, size_of::<CubeSat>());
    loop {
        let start = Instant::now();
        cube_sat = tick(cube_sat);
        pkt.extend_from_slice(cube_sat.as_bytes());
        rent!(client.send(pkt).await, pkt)?;
        pkt.clear();

        let sleep = Duration::from_secs_f64(DT).saturating_sub(start.elapsed());
        if sleep > Duration::ZERO {
            stellarator::sleep(sleep).await;
        }
    }
}
