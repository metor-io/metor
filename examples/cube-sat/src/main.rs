use std::{
    net::SocketAddr,
    ops::Add,
    time::{Duration, Instant, SystemTime},
};

use impeller2::types::{LenPacket, PacketId, Timestamp};
use impeller2_stellar::Client;
use impeller2_wkt::SetDbConfig;
use nox::{
    Body, DU, Quaternion, Scalar, SpatialForce, SpatialInertia, SpatialMotion, SpatialTransform,
    TensorItem, Vec3, Vector, Vector3, array::Quat, rk4, tensor,
};
use rand_distr::Distribution;
use roci::{AsVTable, Metadatatize, tcp::SinkExt};
use roci_adcs::{mekf, yang_lqr::YangLQR};
use stellarator::{rent, struc_con::stellar};
use zerocopy::{Immutable, IntoBytes, KnownLayout};

const G: f64 = 6.6743e-11; // Gravitational constant
const M: f64 = 5.972e24; // Mass of Earth
const MASS: f64 = 2825.2 / 1000.0;
const J: Vec3<f64> = Vec3::from_buf([15204079.70002e-9, 14621352.61765e-9, 6237758.3131e-9]);
const EARTH_RADIUS: f64 = 6378.1e3;
const ALTITUDE: f64 = 400.0e3;
const DT: f64 = 1.0 / 120.0;
const K_0: Vec3<f64> = Vec3::from_buf([-30926.00e-9, 5817.00e-9, -2318.00e-9]);

#[derive(AsVTable, Debug, Clone, Immutable, KnownLayout, Metadatatize, IntoBytes)]
#[repr(C)]
#[roci(parent = "cube_sat")]
pub struct CubeSat {
    pub sim: Sim,
    pub fsw: FSW,
}

#[derive(AsVTable, Debug, Clone, Immutable, KnownLayout, Metadatatize, IntoBytes)]
#[repr(C)]
pub struct Sim {
    body: Body,
    reaction_wheels: [ReactionWheel; 3],
    sensors: Sensors,
    control_torque: Vec3<f64>,
}

#[derive(AsVTable, Debug, Clone, Immutable, KnownLayout, Metadatatize, IntoBytes, Default)]
#[repr(C)]
pub struct Sensors {
    imu: IMU,
    mag: Mag,
    css: CSS,
    gps: GPS,
}

impl Sensors {
    pub fn update(&mut self, body: &Body) {
        self.imu.update(&body);
        self.mag = Mag::from_body(body);
        self.css = CSS::from_body(body);
        self.gps = GPS::from_body(body);
    }
}

#[derive(AsVTable, Debug, Clone, Immutable, KnownLayout, Metadatatize, IntoBytes, Default)]
#[repr(C)]
pub struct IMU {
    gyro: Vec3<f64>,
    bias: Vec3<f64>,
}

impl IMU {
    pub fn update(&mut self, body: &Body) {
        let bias_dist = rand_distr::Normal::new(0.0, 3.16e-7).expect("dist failed to create");
        let dist = rand_distr::Normal::new(0.0, 3.16e-4).expect("dist failed to create");
        let mut rng = rand::rng();
        self.bias = self.bias + bias_dist.sample_tensor(&mut rng);
        let noise = dist.sample_tensor(&mut rng) + self.bias;
        self.gyro = body.pos.angular().inverse() * body.vel.angular();
    }
}

#[derive(AsVTable, Debug, Clone, Immutable, KnownLayout, Metadatatize, IntoBytes, Default)]
#[repr(C)]
pub struct Mag {
    mag: Vec3<f64>,
}

impl Mag {
    pub fn from_body(body: &Body) -> Self {
        let pos = body.pos.linear();
        let pos_norm = pos.norm().into_buf();
        let e_hat = pos.normalize();
        let b = ((EARTH_RADIUS / pos_norm).powi(3)) * (3.0 * K_0.dot(&e_hat) * e_hat - K_0);
        let dist = rand_distr::Normal::new(0.0, 1e-10).expect("dist failed to create");
        let mag = body.pos.angular().inverse() * b; // + dist.sample_tensor(&mut rand::rng());
        let mag = mag.normalize();

        Mag { mag }
    }
}

#[derive(AsVTable, Debug, Clone, Immutable, KnownLayout, Metadatatize, IntoBytes, Default)]
#[repr(C)]
pub struct CSS {
    css_readings: Vector<f64, 6>,
    sun_vec: Vec3<f64>,
}

impl CSS {
    pub fn from_body(body: &Body) -> Self {
        let att_ecef_body = body.pos.angular().inverse();
        let sun_pos = tensor![0.0, 0.0, 1.0]; // NOTE(sphw): this is super fake make this more real
        let sun_pos_b = att_ecef_body * sun_pos;
        let dist = rand_distr::Normal::new(0.0, 0.01).expect("dist failed to create");
        let mut rng = rand::rng();
        let mut css_reading = |normal: Vector3<f64>| -> f64 {
            let cos = normal.dot(&sun_pos_b).into_buf() + dist.sample(&mut rng);
            if cos.acos().abs() < 90f64.to_radians() {
                cos
            } else {
                0.0
            }
        };

        let css_readings = [
            css_reading(Vector3::new(1.0, 0.0, 0.0)),
            css_reading(Vector3::new(0.0, 1.0, 0.0)),
            css_reading(Vector3::new(0.0, 0.0, 1.0)),
            css_reading(Vector3::new(-1.0, 0.0, 0.0)),
            css_reading(Vector3::new(0.0, -1.0, 0.0)),
            css_reading(Vector3::new(0.0, 0.0, -1.0)),
        ];
        let css_readings = Vector::from_buf(css_readings);

        CSS {
            css_readings,
            sun_vec: sun_pos_b, // TODO: do this more legit
        }
    }
}
#[derive(AsVTable, Debug, Clone, Immutable, KnownLayout, Metadatatize, IntoBytes, Default)]
#[repr(C)]
pub struct GPS {
    pos: Vector<f64, 3>,
    vel: Vector<f64, 3>,
}

impl GPS {
    pub fn from_body(body: &Body) -> Self {
        Self {
            pos: body.pos.linear(), // TODO: add noise
            vel: body.vel.linear(), // TODO: add noise
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
    torque: Vec3<f64>,
}

impl ReactionWheel {
    pub fn new(axis: Vec3<f64>) -> Self {
        Self {
            axis,
            speed: 0.0.into(),
            ang_momentum: Vec3::zeros(),
            torque_set_point: Vec3::zeros(),
            friction: 0.0.into(),
            torque: Vec3::zeros(),
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

    /// Update the reaction wheel state for the next time step
    pub fn update(&mut self) {
        let rw_force_clamp = 0.002;

        let new_ang_momentum = self.ang_momentum + self.torque_set_point * DT;

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

        self.ang_momentum = self.ang_momentum + clamped_torque * DT;
        self.friction = self.friction_torque().into();
        self.torque = clamped_torque; // TODO: add friction
        self.update_speed();
    }
}

#[derive(AsVTable, Debug, Clone, Immutable, KnownLayout, Metadatatize, IntoBytes)]
#[repr(C)]
pub struct FSW {
    pub mekf: mekf::State,
    pub sensors: Sensors,
    pub nav: Nav,
    pub control: Control,
    pub mode: Mode,
    pub start_epoch: Timestamp,
}

#[derive(AsVTable, Metadatatize, Debug, Clone, Copy, IntoBytes, KnownLayout, Immutable)]
#[repr(u64)]
pub enum Mode {
    NadirPoint,
    HilPoint,
}

#[derive(AsVTable, Debug, Clone, Immutable, KnownLayout, Metadatatize, IntoBytes)]
#[repr(C)]
pub struct Control {
    yang_lqr: YangLQR,
    pub torque_set_point: Vec3<f64>,
    pub error_term: Vec3<f64>,
    pub ang_vel_term: Vec3<f64>,
    pub att_set_point: Quaternion<f64>,
}

impl Default for Control {
    fn default() -> Self {
        Self {
            yang_lqr: YangLQR::new(
                J,
                tensor![5., 5., 5.],
                tensor![5., 5., 5.],
                tensor![8., 8., 8.],
            ),
            torque_set_point: Vec3::zeros(),
            error_term: Vec3::zeros(),
            ang_vel_term: Vec3::zeros(),
            att_set_point: Quaternion::identity(),
        }
    }
}

#[derive(AsVTable, Debug, Clone, Immutable, KnownLayout, Metadatatize, IntoBytes, Default)]
#[repr(C)]
pub struct Nav {
    mag: Vec3<f64>,
    sun_pos: Vec3<f64>,
}

impl Nav {
    pub fn from_sensors(sensors: &Sensors) -> Self {
        let pos_norm = sensors.gps.pos.norm().into_buf();
        let e_hat = sensors.gps.pos.normalize();
        let sun_pos = tensor![0.0, 0.0, 1.0]; // NOTE(sphw): this is super fake make this more real
        let mag = ((EARTH_RADIUS / pos_norm).powi(3)) * (3.0 * K_0.dot(&e_hat) * e_hat - K_0);
        let mag = mag.normalize();
        Self { mag, sun_pos }
    }
}

impl FSW {
    pub fn new() -> Self {
        Self {
            mekf: mekf::State::new(tensor![0.01, 0.01, 0.01], tensor![0.01, 0.01, 0.01], DT),
            sensors: Sensors::default(),
            nav: Nav::default(),
            control: Control::default(),
            mode: Mode::HilPoint,
            start_epoch: Timestamp::now(),
        }
    }

    pub fn nadir_point(&self) -> Quat<f64> {
        let pos = self.sensors.gps.pos;
        let r = pos.normalize();
        let body_axis = tensor![0.0, -1.0, 0.0];
        let [x, y, z] = body_axis.cross(&r).into_buf();
        let w = 1.0 + body_axis.dot(&r).into_buf();
        Quat::new(w, x, y, z).normalize()
    }

    pub fn hil_point(&self) -> Quat<f64> {
        let pos = self.sensors.gps.vel.normalize();
        let r = pos.normalize();
        let body_axis = tensor![0.0, -1.0, 0.0];
        let [x, y, z] = body_axis.cross(&r).into_buf();
        let w = 1.0 + body_axis.dot(&r).into_buf();
        Quat::new(w, x, y, z).normalize()
    }

    pub fn update(mut self, sensors: &Sensors) -> Self {
        self.nav = Nav::from_sensors(&sensors);
        self.mekf.omega = sensors.imu.gyro;
        self.mekf = self.mekf.estimate_attitude(
            [sensors.css.sun_vec, sensors.mag.mag.normalize()],
            [self.nav.sun_pos, self.nav.mag.normalize()],
            [0.01, 0.01],
        );
        self.sensors = sensors.clone();

        let elapsed = Timestamp::now() - self.start_epoch;
        self.mode = match elapsed.as_secs() % 60 {
            ..30 => Mode::NadirPoint,
            _ => Mode::HilPoint,
        };

        self.control.att_set_point = match self.mode {
            Mode::NadirPoint => self.nadir_point(),
            Mode::HilPoint => self.hil_point(),
        };

        if self
            .control
            .att_set_point
            .0
            .into_buf()
            .iter()
            .any(|f| !f.is_finite())
        {
            self.control.att_set_point = Quat::identity();
        }

        self.control.torque_set_point = self.control.yang_lqr.control(
            self.mekf.q_hat,
            self.mekf.q_hat * sensors.imu.gyro,
            self.control.att_set_point,
        );

        self
    }
}

impl<'a> Add<DU> for &'a Sim {
    type Output = Sim;

    fn add(self, du: DU) -> Self::Output {
        let mut new = self.clone();
        new.body = &new.body + du;
        new
    }
}

impl CubeSat {
    pub fn new() -> Self {
        let radius = EARTH_RADIUS + ALTITUDE;
        let initial_velocity = (G * M / radius).sqrt();
        let body = Body {
            pos: SpatialTransform::from_linear(tensor![1.0, 0.0, 0.0] * radius),
            vel: SpatialMotion::new(tensor![0.0, 2.0, 0.0], tensor![0.0, initial_velocity, 0.0]),
            accel: SpatialMotion::zero(),
            inertia: SpatialInertia::new(J, Vec3::zeros(), MASS),
            force: SpatialForce::zero(),
        };
        let sim = Sim {
            sensors: Sensors::default(),
            reaction_wheels: [
                ReactionWheel::new(tensor![1.0, 0.0, 0.0]),
                ReactionWheel::new(tensor![0.0, 1.0, 0.0]),
                ReactionWheel::new(tensor![0.0, 0.0, 1.0]),
            ],
            control_torque: Vec3::zeros(),
            body,
        };
        let control = FSW::new();
        Self { sim, fsw: control }
    }
}

impl Sim {
    pub fn gravity(&self) -> Vector3<f64> {
        // f = G*M*m/r^3 * r
        let r = self.body.pos.linear();
        let r_mag = r.norm().into_buf();
        -G * M * self.body.inertia.mass() / Scalar::from(r_mag.powi(3)) * r
    }

    pub fn set_reaction_wheel_torque(&mut self, torque: Vec3<f64>) {
        self.control_torque = torque;
        for wheel in &mut self.reaction_wheels {
            wheel.torque_set_point = self.control_torque.dot(&wheel.axis) * wheel.axis;
        }
    }

    pub fn reaction_wheel_torque(&self) -> Vec3<f64> {
        //Vec3::from_buf(self.control_torque.into_buf().map(|x| x.clamp(-0.1, 0.1)))
        //Vec3::from_buf(self.control_torque.into_buf())
        self.reaction_wheels.iter().map(|wheel| wheel.torque).sum()
    }

    pub fn update(mut self) -> Self {
        for rw in &mut self.reaction_wheels {
            rw.update();
        }
        self.sensors.update(&self.body);
        self
    }

    pub fn du(&self) -> DU {
        let gravity_force = SpatialForce::from_linear(self.gravity());
        let rw_torque = self.reaction_wheel_torque();
        let rw_spatial_force = SpatialForce::from_torque(rw_torque);
        let force = gravity_force + rw_spatial_force;
        DU::from_body_force(&self.body, force)
    }
}

fn tick(mut cubesat: CubeSat) -> CubeSat {
    cubesat.sim = cubesat.sim.update();
    cubesat.sim = rk4::<f64, Sim, DU, _>(DT, &cubesat.sim, |sim: &Sim| -> DU { sim.du() });
    cubesat.fsw = cubesat.fsw.update(&cubesat.sim.sensors);
    cubesat
        .sim
        .set_reaction_wheel_torque(cubesat.fsw.control.torque_set_point);
    cubesat
}

#[stellarator::main]
pub async fn main() -> anyhow::Result<()> {
    stellar(move || metor_db::serve_tmp_db(SocketAddr::new([127, 0, 0, 1].into(), 2240)));
    stellarator::sleep(Duration::from_millis(50)).await;
    let mut client = Client::connect(SocketAddr::new([127, 0, 0, 1].into(), 2240))
        .await
        .map_err(anyhow::Error::from)?;
    let id: PacketId = fastrand::u16(..).to_le_bytes();
    client.init_world::<CubeSat>(id).await?;
    client
        .send(&SetDbConfig::schematic_content(
            include_str!("./schematic.kdl").to_string(),
        ))
        .await
        .0?;
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

pub trait NormalExt<T> {
    fn sample_tensor(&self, rand: &mut impl rand::Rng) -> T;
}
impl NormalExt<Vec3<f64>> for rand_distr::Normal<f64> {
    fn sample_tensor(&self, rand: &mut impl rand::Rng) -> Vec3<f64> {
        Vec3::new(self.sample(rand), self.sample(rand), self.sample(rand))
    }
}
