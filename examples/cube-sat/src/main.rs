use std::{
    cell::RefCell,
    net::SocketAddr,
    ops::Add,
    rc::Rc,
    time::{Duration, Instant},
};

use metor_fsw::{AsVTable, Metadatatize, tcp::SinkExt};
use metor_fsw_adcs::{mekf, yang_lqr::YangLQR};
use metor_proto::types::{ComponentId, LenPacket, Msg, OwnedPacket, PacketId, Timestamp};
use metor_proto_bbq::RxExt;
use metor_proto_stellar::{PacketSink, PacketStream, queue::spawn_recv};
use metor_proto_wkt::{
    AlarmCleared, AlarmDef, AlarmLimit, AlarmRaised, AlarmTarget, ComponentValue, LimitKind,
    MsgStream, ReloadSequences, SequenceCommand, SetDbConfig, Severity, UpdateComponent,
};
use nox::{
    ArrayBuf, Body, DU, Quaternion, Scalar, SpatialForce, SpatialInertia, SpatialMotion,
    SpatialTransform, Vec3, Vector, Vector3, array::Quat, rk4, tensor,
};
use rand_distr::Distribution;
use stellarator::{io::SplitExt, net::TcpStream, rent};
use tracing_subscriber::EnvFilter;
use zerocopy::{Immutable, IntoBytes, KnownLayout};

mod sequencer;
use sequencer::Sequencer;

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
#[metor_fsw(parent = "cube_sat")]
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
    tick_time: f64,
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
        self.imu.update(body);
        self.mag = Mag::from_body(body);
        self.css = CSS::from_body(body);
        self.gps = GPS::from_body(body);
    }
}

#[derive(AsVTable, Debug, Clone, Immutable, KnownLayout, Metadatatize, IntoBytes, Default)]
#[repr(C)]
#[metor_fsw(group)]
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
        self.gyro = body.pos.angular().inverse() * body.vel.angular() + noise;
    }
}

#[derive(AsVTable, Debug, Clone, Immutable, KnownLayout, Metadatatize, IntoBytes, Default)]
#[repr(C)]
#[metor_fsw(group)]
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
        let mag = body.pos.angular().inverse() * b + dist.sample_tensor(&mut rand::rng());
        let mag = mag.normalize();

        Mag { mag }
    }
}

#[derive(AsVTable, Debug, Clone, Immutable, KnownLayout, Metadatatize, IntoBytes, Default)]
#[repr(C)]
#[metor_fsw(group)]
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
#[metor_fsw(group)]
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

#[derive(AsVTable, Debug, Clone, Immutable, KnownLayout, Metadatatize, IntoBytes, Default)]
#[repr(C)]
#[metor_fsw(group)]
pub struct ReactionWheel {
    axis: Vec3<f64>,
    speed: Scalar<f64>,
    ang_momentum: Vec3<f64>,
    torque_set_point: Vec3<f64>,
    friction: Scalar<f64>,
    torque: Vec3<f64>,
    healthy: bool,
    arm: bool,
    _pad1: u16,
    _pad: u32,
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
            healthy: true,
            arm: true,
            _pad: 0,
            _pad1: 0,
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
        // A disarmed wheel is offline: it applies no control torque and ignores
        // its set point, though its stored momentum is retained.
        if !self.arm {
            self.torque = Vec3::zeros();
            self.friction = self.friction_torque().into();
            self.update_speed();
            return;
        }

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
    pub tick_time: f64,
    pub overrides: Overrides,
}

#[derive(AsVTable, Debug, Clone, Immutable, KnownLayout, Metadatatize, IntoBytes)]
#[repr(C)]
pub struct Overrides {
    pub _pad: [u8; 5],
    pub disable_reaction_wheels: [bool; 3],
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

impl Default for FSW {
    fn default() -> Self {
        Self {
            mekf: mekf::State::new(tensor![0.01, 0.01, 0.01], tensor![0.01, 0.01, 0.01], DT),
            sensors: Sensors::default(),
            nav: Nav::default(),
            control: Control::default(),
            mode: Mode::HilPoint,
            start_epoch: Timestamp::now(),
            tick_time: 0.0,
            overrides: Overrides {
                _pad: [0; 5],
                disable_reaction_wheels: [false; 3],
            },
        }
    }
}

impl FSW {
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
        self.nav = Nav::from_sensors(sensors);
        self.mekf.omega = sensors.imu.gyro;
        self.mekf = self.mekf.estimate_attitude(
            [sensors.css.sun_vec, sensors.mag.mag.normalize()],
            [self.nav.sun_pos, self.nav.mag.normalize()],
            [0.01, 0.01],
        );
        self.sensors = sensors.clone();

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

impl Add<DU> for &'_ Sim {
    type Output = Sim;

    fn add(self, du: DU) -> Self::Output {
        let mut new = self.clone();
        new.body = &new.body + du;
        new
    }
}

impl Default for CubeSat {
    fn default() -> Self {
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
            tick_time: 0.0,
        };
        let control = FSW::default();
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

// The FSW lives behind a shared cell so sequences can command it; `tick` updates it in place
// and returns the advanced sim. The FSW wheel-enable surface
// (`overrides.disable_reaction_wheels`) is mirrored onto the sim's per-wheel arm gate, so both
// operators and sequences command the wheels through FSW alone.
fn tick(mut sim: Sim, fsw: &RefCell<FSW>) -> Sim {
    let sim_start = Instant::now();
    sim = sim.update();
    sim = rk4::<f64, Sim, DU, _>(DT, &sim, |sim: &Sim| -> DU { sim.du() });
    sim.tick_time = sim_start.elapsed().as_secs_f64();

    let fsw_start = Instant::now();
    let next = fsw.borrow().clone().update(&sim.sensors);
    {
        let mut fsw = fsw.borrow_mut();
        *fsw = next;
        fsw.tick_time = fsw_start.elapsed().as_secs_f64();
    }

    let fsw = fsw.borrow();
    for i in 0..sim.reaction_wheels.len() {
        sim.reaction_wheels[i].arm = !fsw.overrides.disable_reaction_wheels[i];
    }
    sim.set_reaction_wheel_torque(fsw.control.torque_set_point);
    sim
}

#[stellarator::main]
pub async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::fmt()
        .with_target(false)
        .with_env_filter(EnvFilter::from_default_env())
        .try_init()
        .unwrap();

    //stellar(move || metor_db::serve_tmp_db(SocketAddr::new([127, 0, 0, 1].into(), 2240)));
    //stellarator::sleep(Duration::from_millis(50)).await;
    let (rx, tx) = TcpStream::connect(SocketAddr::new([127, 0, 0, 1].into(), 2240))
        .await?
        .split();
    let tx = PacketSink::new(tx);
    let rx = PacketStream::new(rx);
    let mut rx = spawn_recv(rx, 1024 * 1024);
    let id: PacketId = fastrand::u16(..).to_le_bytes();
    tx.init_world::<CubeSat>(id).await?;
    tx.send(&MsgStream {
        msg_id: UpdateComponent::ID,
    })
    .await
    .0?;
    // Receive operator sequence commands (load/start/abort/stop) and reload requests.
    tx.send(&MsgStream {
        msg_id: SequenceCommand::ID,
    })
    .await
    .0?;
    tx.send(&MsgStream {
        msg_id: ReloadSequences::ID,
    })
    .await
    .0?;
    tx.send(&SetDbConfig::schematic_content(
        include_str!("./schematic.kdl").to_string(),
    ))
    .await
    .0?;

    // Declare a body-rate alarm. The limits are informational display lines; the firing
    // decision (with a two-sample debounce) is made below by this "control system".
    const RATE_ALARM_ID: &str = "ADCS_RATE_HIGH";
    const RATE_WARN: f64 = 0.5;
    const RATE_CRIT: f64 = 1.0;
    tx.send(&AlarmDef {
        id: RATE_ALARM_ID.into(),
        name: "Body Rate High".into(),
        description: "Spacecraft angular rate (gyro Y) outside the safe envelope".into(),
        target: Some(AlarmTarget {
            component_id: ComponentId::new("cube_sat.sim.sensors.imu.gyro"),
            // Element 1 (Y) carries the initial tumble (2 rad/s), so the alarm fires
            // on startup and clears once the wheels detumble the spacecraft.
            element_index: Some(1),
        }),
        limits: vec![
            AlarmLimit {
                kind: LimitKind::Upper,
                value: RATE_WARN,
                severity: Severity::Warning,
                label: Some("Warn".into()),
            },
            AlarmLimit {
                kind: LimitKind::Upper,
                value: RATE_CRIT,
                severity: Severity::Critical,
                label: Some("Crit".into()),
            },
            AlarmLimit {
                kind: LimitKind::Lower,
                value: -RATE_WARN,
                severity: Severity::Warning,
                label: Some("Warn".into()),
            },
            AlarmLimit {
                kind: LimitKind::Lower,
                value: -RATE_CRIT,
                severity: Severity::Critical,
                label: Some("Crit".into()),
            },
        ],
        default_severity: Severity::Warning,
    })
    .await
    .0?;

    // Debounced firing state for the rate alarm.
    let mut rate_occurrence: u64 = 0;
    let mut rate_active = false;
    let mut over_count = 0u8;
    let mut under_count = 0u8;

    // FSW state is shared with the sequence executor; sim state stays private to this loop.
    let CubeSat { mut sim, fsw } = CubeSat::default();
    let fsw = Rc::new(RefCell::new(fsw));
    // `--disarmed` brings the spacecraft up with every reaction wheel offline, so the
    // operator must arm them from the panel before attitude control takes effect.
    if std::env::args().any(|arg| arg == "--disarmed") {
        fsw.borrow_mut().overrides.disable_reaction_wheels = [true; 3];
        println!("reaction wheels starting disarmed");
    }

    // The limited futures executor: it declares the sequence channels, runs loaded sequences
    // against the shared FSW, and reports their lifecycle back to the panel.
    let mut sequencer = Sequencer::new(fsw.clone());
    tx.send(&sequencer.registry()).await.0?;

    let mut pkt = LenPacket::new(
        metor_proto::types::PacketTy::Table,
        id,
        size_of::<CubeSat>(),
    );
    loop {
        let start = Instant::now();
        while let Some(pkt) = rx.try_recv_pkt() {
            match pkt {
                OwnedPacket::Msg(m) if m.id == UpdateComponent::ID => {
                    let Ok(update_component) = m.parse::<UpdateComponent>().inspect_err(|err| {
                        println!("{err:?}");
                    }) else {
                        continue;
                    };
                    // Operator commands write the same shared FSW surface as sequences.
                    match &update_component.value {
                        ComponentValue::U64(value) => {
                            if update_component.id == ComponentId::new("cube_sat.fsw.mode") {
                                let mode = value.buf.as_buf()[0];
                                fsw.borrow_mut().mode = match mode {
                                    0 => Mode::NadirPoint,
                                    1 => Mode::HilPoint,
                                    _ => continue,
                                };
                            }
                        }
                        // Operator arm/disarm of a reaction wheel from the panel, expressed as
                        // the FSW wheel-enable override (mirrored onto the sim gate each tick).
                        ComponentValue::Bool(value) => {
                            let armed = value.buf.as_buf()[0];
                            for i in 0..sim.reaction_wheels.len() {
                                if update_component.id
                                    == ComponentId::new(&format!(
                                        "cube_sat.sim.reaction_wheels.{i}.arm"
                                    ))
                                {
                                    fsw.borrow_mut().overrides.disable_reaction_wheels[i] = !armed;
                                    println!(
                                        "reaction wheel {i} {}",
                                        if armed { "armed" } else { "disarmed" }
                                    );
                                }
                            }
                        }
                        _ => {}
                    }
                }
                OwnedPacket::Msg(m) if m.id == SequenceCommand::ID => {
                    let Ok(cmd) = m.parse::<SequenceCommand>().inspect_err(|err| {
                        println!("{err:?}");
                    }) else {
                        continue;
                    };
                    sequencer.handle_command(cmd);
                }
                OwnedPacket::Msg(m) if m.id == ReloadSequences::ID => {
                    // The channel set is static here; reload just re-advertises the registry.
                    tx.send(&sequencer.registry()).await.0?;
                }
                _ => {}
            }
        }

        // Poll the sequence executor once (advances sim-time, may mutate the FSW surface),
        // then advance the dynamics + FSW for this cycle.
        sequencer.step();
        sim = tick(sim, &fsw);

        // Evaluate the body-rate alarm with a two-sample debounce, then raise/clear on
        // transitions only. Y (element 1) holds the initial tumble.
        let rate = sim.sensors.imu.gyro.into_buf()[1];
        if rate.abs() > RATE_WARN {
            over_count = over_count.saturating_add(1);
            under_count = 0;
        } else {
            under_count = under_count.saturating_add(1);
            over_count = 0;
        }
        if !rate_active && over_count >= 2 {
            rate_active = true;
            rate_occurrence += 1;
            let severity = if rate.abs() > RATE_CRIT {
                Severity::Critical
            } else {
                Severity::Warning
            };
            tx.send(&AlarmRaised {
                def_id: RATE_ALARM_ID.into(),
                occurrence: rate_occurrence,
                severity,
                value: Some(rate),
                message: format!("gyro Y = {rate:.3} rad/s"),
            })
            .await
            .0?;
        } else if rate_active && under_count >= 2 {
            rate_active = false;
            tx.send(&AlarmCleared {
                def_id: RATE_ALARM_ID.into(),
                occurrence: rate_occurrence,
            })
            .await
            .0?;
        }

        // Re-assemble the table from the split sim + shared FSW for the wire.
        let cube_sat = CubeSat {
            sim: sim.clone(),
            fsw: fsw.borrow().clone(),
        };
        pkt.extend_from_slice(cube_sat.as_bytes());
        rent!(tx.send(pkt).await, pkt)?;
        pkt.clear();

        // Forward any sequence lifecycle events produced this cycle to the panel.
        for event in sequencer.drain_events() {
            tx.send(&event).await.0?;
        }

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
