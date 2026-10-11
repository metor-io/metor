//! `imu -> nav -> control` as `#[system]` blocks, with a trait-path monitor
//! reading the three status rings, run on the stellarator runtime.

use core::cell::RefCell;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::rc::Rc;

use metor_fsw_3::{
    Clock, CoordinatorConfig, DefCx, DefError, Frame, Input, InputConfig, Output, PortRef, System,
    SystemConfig, SystemDef, SystemInputs, SystemStatus, SystemTable, Timestamp, system,
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

#[derive(SystemInputs)]
struct MonitorIn {
    control: Input<Control>,
    imu_status: Input<SystemStatus>,
    nav_status: Input<SystemStatus>,
    control_status: Input<SystemStatus>,
}

/// Counts cycles and publishes the count as a rate.
#[derive(Default)]
struct Gyro {
    tick: i64,
}

#[system]
impl Gyro {
    fn execute(&mut self, imu: &mut Output<Imu>) {
        self.tick += 1;
        let _ = imu.write(&Imu {
            timestamp: Timestamp(self.tick),
            omega: self.tick as f64,
        });
    }
}

/// Halves the newest rate into an attitude.
struct NavFilter;

#[system]
impl NavFilter {
    fn execute(&mut self, imu: &mut Input<Imu>, nav: &mut Output<Nav>) {
        let Ok(Some(imu)) = imu.latest() else {
            return;
        };
        let out = Nav {
            timestamp: imu.timestamp,
            attitude: imu.omega * 0.5,
        };
        let _ = nav.write(&out);
    }
}

/// Commands the opposite of the attitude.
struct ControlLaw;

#[system]
impl ControlLaw {
    fn execute(&mut self, nav: &mut Input<Nav>, control: &mut Output<Control>) {
        let Ok(Some(nav)) = nav.latest() else {
            return;
        };
        let out = Control {
            timestamp: nav.timestamp,
            torque: -nav.attitude,
        };
        let _ = control.write(&out);
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

    fn def(cx: &DefCx<'_>) -> Result<SystemDef, DefError> {
        SystemDef::new::<MonitorIn, ()>("monitor", cx)
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
    table
        .register("gyro", Gyro::default)
        .expect("valid records");
    table.register("nav", || NavFilter).expect("valid records");
    table
        .register("control", || ControlLaw)
        .expect("valid records");
    let report = report.clone();
    table
        .register_system("monitor", move |_| Ok((Monitor, report.clone())))
        .expect("valid records");
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
fn test_pipeline_ten_cycles() {
    let report = Rc::new(RefCell::new(Report::default()));
    let mut coordinator = config().build(&table(&report)).expect("valid config");
    // Three outputs, three fn-system logs, four status rings.
    assert_eq!(coordinator.rings(), 10);

    stellarator::run(|| async move {
        coordinator.run(AfterCycles { left: 10 }).await;
        assert_eq!(coordinator.cycle(), 10);
    });

    let report = report.borrow();
    // Tick 10 through `* 0.5` and a sign flip.
    assert_eq!(report.torque, -5.0);
    assert_eq!(report.statuses, [10, 10, 10]);
}
