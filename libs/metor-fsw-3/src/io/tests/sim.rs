//! Targets whose drivers use simulated resources, served by model systems.

use core::cell::RefCell;
use std::rc::Rc;

use metor_proto::types::Timestamp;

use super::system::{io_steps, script, with_io_for};
use crate::coordinator::{BuildError, CoordinatorConfig, SystemConfig, SystemTable};
use crate::io::test::ScriptedUart;
use crate::io::*;
use crate::{Coordinator, system};

type Reads = Rc<RefCell<Vec<(Timestamp, Result<Vec<u8>, IoError>)>>>;

struct Reader {
    rx: Slot<Read>,
    reads: Reads,
}

/// Reads whenever its buffer is idle, recording each result.
#[system]
fn reader(#[state] s: &mut Reader, now: Timestamp, uart: &Cap<Uart>) {
    if let Some(result) = s.rx.poll() {
        let bytes = result.map(|_| s.rx.bytes().to_vec());
        s.reads.borrow_mut().push((now, bytes));
    }
    if let Some(buf) = s.rx.take_idle() {
        s.rx = Slot::Busy(uart.read(buf));
    }
}

/// Serves its device end with a shared `ScriptedUart`.
#[system]
fn model(
    #[state] uart: &mut Rc<RefCell<ScriptedUart>>,
    now: Timestamp,
    dev: &mut DeviceSide<Uart>,
) {
    dev.serve(now, &mut *uart.borrow_mut());
}

#[derive(Default)]
struct Seen {
    reads: Reads,
    uart: Rc<RefCell<ScriptedUart>>,
}

fn table(seen: &Seen) -> SystemTable {
    let mut table = SystemTable::new();
    let reads = seen.reads.clone();
    let make_reader = move |io: &IoInit<'_>| Reader {
        rx: Slot::Idle(io.alloc(32)),
        reads: reads.clone(),
    };
    table.register("reader", reader(), make_reader).unwrap();
    let uart = seen.uart.clone();
    table
        .register("model", model(), move || uart.clone())
        .unwrap();
    io_steps(&mut table);
    table
}

fn sim(id: &str) -> ResourceConfig {
    ResourceConfig {
        id: id.into(),
        backing: Backing::Sim(SimKind::Uart),
    }
}

fn tty(id: &str) -> ResourceConfig {
    ResourceConfig {
        id: id.into(),
        backing: Backing::Real(RealKind::Tty {
            path: "/dev/null".into(),
            baud: 9600,
        }),
    }
}

fn granted(id: &str, ty: &str, param: &str, resource: &str) -> SystemConfig {
    SystemConfig {
        grants: vec![Grant {
            param: param.into(),
            resource: resource.into(),
        }],
        ..SystemConfig::new(id, ty)
    }
}

fn driver(id: &str, resource: &str) -> SystemConfig {
    granted(id, "reader", "uart", resource)
}

fn modeled(id: &str, resource: &str) -> SystemConfig {
    granted(id, "model", "dev", resource)
}

fn build(
    seen: &Seen,
    systems: Vec<SystemConfig>,
    resources: Vec<ResourceConfig>,
) -> Result<Coordinator, BuildError> {
    let drivers: Vec<String> = systems
        .iter()
        .filter(|s| s.ty == "reader")
        .map(|s| s.id.clone())
        .collect();
    let config = CoordinatorConfig {
        systems: with_io_for(systems, &drivers),
        resources,
        ..CoordinatorConfig::default()
    };
    config.build(&table(seen))
}

mod checks {
    use super::*;

    #[test]
    fn one_model_and_two_drivers_build() {
        let _os = script();
        let seen = Seen::default();
        let systems = vec![driver("a", "gps"), driver("b", "gps"), modeled("m", "gps")];
        build(&seen, systems, vec![sim("gps")]).unwrap();
    }

    #[test]
    fn sim_kinds_round_trip_through_json() {
        for kind in [SimKind::Uart, SimKind::GpioLine] {
            let backing = Backing::Sim(kind);
            let json = serde_json::to_string(&backing).unwrap();
            assert_eq!(serde_json::from_str::<Backing>(&json).unwrap(), backing);
        }
        let uart = serde_json::to_value(Backing::Sim(SimKind::GpioLine)).unwrap();
        assert_eq!(uart, serde_json::json!({"sim": "gpio_line"}));
    }

    #[test]
    fn sim_resource_without_a_model_is_refused() {
        let _os = script();
        let seen = Seen::default();
        let err = build(&seen, vec![driver("a", "gps")], vec![sim("gps")]).err();
        assert!(
            matches!(&err, Some(BuildError::NoDeviceEnd { resource }) if resource == "gps"),
            "{err:?}"
        );
    }

    #[test]
    fn two_models_are_refused() {
        let _os = script();
        let seen = Seen::default();
        let systems = vec![driver("a", "gps"), modeled("m", "gps"), modeled("n", "gps")];
        let err = build(&seen, systems, vec![sim("gps")]).err();
        let Some(BuildError::TwoDeviceEnds {
            resource,
            first,
            second,
        }) = err
        else {
            panic!("{err:?}");
        };
        assert_eq!((&*resource, &*first, &*second), ("gps", "m", "n"));
    }

    #[test]
    fn model_of_a_real_resource_is_refused() {
        let _os = script();
        let seen = Seen::default();
        let systems = vec![driver("a", "port"), modeled("m", "port")];
        let err = build(&seen, systems, vec![tty("port")]).err();
        assert!(
            matches!(&err, Some(BuildError::DeviceEndOnReal { system, resource })
                if system == "m" && resource == "port"),
            "{err:?}"
        );
    }

    #[test]
    fn drivers_mixing_real_and_sim_are_refused() {
        let _os = script();
        let seen = Seen::default();
        let systems = vec![driver("a", "port"), driver("b", "gps"), modeled("m", "gps")];
        let err = build(&seen, systems, vec![tty("port"), sim("gps")]).err();
        assert!(
            matches!(&err, Some(BuildError::MixedBackings { real, sim })
                if real == "port" && sim == "gps"),
            "{err:?}"
        );
    }

    #[test]
    fn model_of_the_wrong_interface_is_refused() {
        let _os = script();
        let seen = Seen::default();
        let line = ResourceConfig {
            id: "pps".into(),
            backing: Backing::Sim(SimKind::GpioLine),
        };
        let err = build(&seen, vec![modeled("m", "pps")], vec![line]).err();
        assert!(
            matches!(&err, Some(BuildError::Interface { cap, .. }) if cap == "m.dev"),
            "{err:?}"
        );
    }

    #[test]
    fn ungranted_device_side_is_refused() {
        let _os = script();
        let seen = Seen::default();
        let systems = vec![SystemConfig::new("m", "model")];
        let err = build(&seen, systems, vec![]).err();
        assert!(
            matches!(&err, Some(BuildError::Ungranted { param, .. }) if param == "dev"),
            "{err:?}"
        );
    }
}
