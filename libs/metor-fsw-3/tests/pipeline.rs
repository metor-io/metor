//! `imu -> nav -> control`, with a fourth system reading the three status
//! rings, run on the stellarator runtime.

use core::cell::RefCell;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::rc::Rc;

use metor_fsw_3::{
    Clock, CoordinatorConfig, Frame, Input, InputConfig, Output, PortRef, System, SystemConfig,
    SystemDef, SystemInputs, SystemOutputs, SystemStatus, SystemTable, Timestamp,
};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[frame(name = "imu")]
#[repr(C)]
struct Imu {
    #[frame(timestamp)]
    timestamp: Timestamp,
    omega: f64,
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[frame(name = "nav")]
#[repr(C)]
struct Nav {
    #[frame(timestamp)]
    timestamp: Timestamp,
    attitude: f64,
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[frame(name = "control")]
#[repr(C)]
struct Control {
    #[frame(timestamp)]
    timestamp: Timestamp,
    torque: f64,
}

#[derive(SystemOutputs)]
struct ImuOut {
    imu: Output<Imu>,
}

#[derive(SystemInputs)]
struct NavIn {
    imu: Input<Imu>,
}

#[derive(SystemOutputs)]
struct NavOut {
    nav: Output<Nav>,
}

#[derive(SystemInputs)]
struct ControlIn {
    nav: Input<Nav>,
}

#[derive(SystemOutputs)]
struct ControlOut {
    control: Output<Control>,
}

#[derive(SystemInputs)]
struct MonitorIn {
    control: Input<Control>,
    imu_status: Input<SystemStatus>,
    nav_status: Input<SystemStatus>,
    control_status: Input<SystemStatus>,
}

struct Gyro;

impl System for Gyro {
    type State = i64;
    type Inputs = ();
    type Outputs = ImuOut;

    fn def() -> SystemDef {
        SystemDef::new::<(), ImuOut>("gyro")
    }

    fn execute(&self, _now: Timestamp, tick: &mut i64, _inputs: &mut (), outputs: &mut ImuOut) {
        *tick += 1;
        let _ = outputs.imu.write(&Imu {
            timestamp: Timestamp(*tick),
            omega: *tick as f64,
        });
    }
}

struct NavFilter;

impl System for NavFilter {
    type State = ();
    type Inputs = NavIn;
    type Outputs = NavOut;

    fn def() -> SystemDef {
        SystemDef::new::<NavIn, NavOut>("nav_filter")
    }

    fn execute(&self, _now: Timestamp, _state: &mut (), inputs: &mut NavIn, outputs: &mut NavOut) {
        let Ok(Some(imu)) = inputs.imu.latest() else {
            return;
        };
        let nav = Nav {
            timestamp: imu.timestamp,
            attitude: imu.omega * 0.5,
        };
        drop(imu);
        let _ = outputs.nav.write(&nav);
    }
}

struct ControlLaw;

impl System for ControlLaw {
    type State = ();
    type Inputs = ControlIn;
    type Outputs = ControlOut;

    fn def() -> SystemDef {
        SystemDef::new::<ControlIn, ControlOut>("control_law")
    }

    fn execute(
        &self,
        _now: Timestamp,
        _state: &mut (),
        inputs: &mut ControlIn,
        outputs: &mut ControlOut,
    ) {
        let Ok(Some(nav)) = inputs.nav.latest() else {
            return;
        };
        let control = Control {
            timestamp: nav.timestamp,
            torque: -nav.attitude,
        };
        drop(nav);
        let _ = outputs.control.write(&control);
    }
}

/// Reads the pipeline's product and the three status rings behind it.
struct Monitor;

#[derive(Default)]
struct Report {
    torque: f64,
    statuses: [usize; 3],
}

impl System for Monitor {
    type State = Rc<RefCell<Report>>;
    type Inputs = MonitorIn;
    type Outputs = ();

    fn def() -> SystemDef {
        SystemDef::new::<MonitorIn, ()>("monitor")
    }

    fn execute(
        &self,
        _now: Timestamp,
        report: &mut Self::State,
        inputs: &mut MonitorIn,
        _outputs: &mut (),
    ) {
        if let Ok(Some(control)) = inputs.control.latest() {
            report.borrow_mut().torque = control.torque;
        }
        for (i, port) in [
            &mut inputs.imu_status,
            &mut inputs.nav_status,
            &mut inputs.control_status,
        ]
        .into_iter()
        .enumerate()
        {
            report.borrow_mut().statuses[i] += port.drain().flat_map(|r| r.ok()).count();
        }
    }
}

fn config() -> CoordinatorConfig {
    CoordinatorConfig {
        clock: Clock::Wall { rate: 5_000.0 },
        systems: vec![
            SystemConfig::new("imu", "gyro"),
            SystemConfig {
                inputs: vec![input("imu", PortRef::new("imu", "imu"))],
                ..SystemConfig::new("nav", "nav")
            },
            SystemConfig {
                inputs: vec![input("nav", PortRef::new("nav", "nav"))],
                ..SystemConfig::new("control", "control")
            },
            SystemConfig {
                inputs: vec![
                    input("control", PortRef::new("control", "control")),
                    input("imu_status", PortRef::new("imu", "status")),
                    input("nav_status", PortRef::new("nav", "status")),
                    input("control_status", PortRef::new("control", "status")),
                ],
                ..SystemConfig::new("monitor", "monitor")
            },
        ],
        ..Default::default()
    }
}

fn input(port: &str, from: PortRef) -> InputConfig {
    InputConfig {
        port: port.into(),
        from: vec![from],
    }
}

fn table(report: &Rc<RefCell<Report>>) -> SystemTable {
    let mut table = SystemTable::new();
    table.register_system("gyro", |_| Ok((Gyro, 0)));
    table.register_system("nav", |_| Ok((NavFilter, ())));
    table.register_system("control", |_| Ok((ControlLaw, ())));
    let report = report.clone();
    table.register_system("monitor", move |_| Ok((Monitor, report.clone())));
    table
}

/// Resolves on its `cycles`-th poll, and [`Coordinator::run`] polls its stop
/// future once per cycle.
struct AfterCycles {
    left: usize,
}

impl Future for AfterCycles {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        self.left = self.left.saturating_sub(1);
        if self.left == 0 {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

#[test]
fn ten_cycles_of_the_pipeline() {
    let report = Rc::new(RefCell::new(Report::default()));
    let mut coordinator = config().build(&table(&report)).expect("valid config");
    assert_eq!(coordinator.rings(), 7);

    stellarator::run(|| async move {
        coordinator.run(AfterCycles { left: 10 }).await;
        assert_eq!(coordinator.cycle(), 10);
    });

    let report = report.borrow();
    // Tick 10 through `* 0.5` and a sign flip.
    assert_eq!(report.torque, -5.0);
    assert_eq!(report.statuses, [10, 10, 10]);
}
