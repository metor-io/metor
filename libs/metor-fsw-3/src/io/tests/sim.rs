//! Targets whose drivers use simulated resources, served by model systems.

use core::cell::{Cell, RefCell};
use std::rc::Rc;

use metor_proto::types::Timestamp;
use metor_proto_wkt::{SequenceCommand, SequenceCommandKind};

use super::system::{io_steps, script, with_io_for};
use crate::coordinator::{
    Adapter, BuildError, ChannelConfig, CoordinatorConfig, InputConfig, OutputConfig, RunState,
    SystemConfig, SystemTable,
};
use crate::io::test::ScriptedUart;
use crate::io::*;
use crate::port::{Input, Output};
use crate::{Coordinator, system};

type Reads = Rc<RefCell<Vec<(Timestamp, Result<Vec<u8>, IoError>)>>>;

struct Reader {
    rx: Vec<Slot<Read>>,
    reads: Reads,
}

/// Reads on each idle buffer, recording each result.
#[system]
fn reader(#[state] s: &mut Reader, now: Timestamp, uart: &Cap<Uart>) {
    for rx in &mut s.rx {
        if let Some(result) = rx.poll() {
            let bytes = result.map(|_| rx.bytes().to_vec());
            s.reads.borrow_mut().push((now, bytes));
        }
        if let Some(buf) = rx.take_idle() {
            *rx = Slot::Busy(uart.read(buf));
        }
    }
}

struct Model {
    uart: Rc<RefCell<ScriptedUart>>,
    serving: Rc<Cell<bool>>,
}

/// Serves its device end with a shared `ScriptedUart`, while `serving`.
#[system]
fn model(#[state] m: &mut Model, now: Timestamp, dev: &mut DeviceSide<Uart>) {
    if m.serving.get() {
        dev.serve(now, &mut *m.uart.borrow_mut());
    }
}

#[system]
fn replies(#[state] seen: &mut Rc<RefCell<Vec<Cqe>>>, rep: &mut Input<Cqe>) {
    seen.borrow_mut().extend(rep.drain().flatten());
}

#[system]
fn cmds(
    #[state] queue: &mut Rc<RefCell<Vec<SequenceCommandKind>>>,
    commands: &mut Output<SequenceCommand>,
) {
    for command in queue.borrow_mut().drain(..) {
        let channel = "rx".into();
        let _ = commands.write(&SequenceCommand { channel, command });
    }
}

struct Seen {
    reads: Reads,
    uart: Rc<RefCell<ScriptedUart>>,
    serving: Rc<Cell<bool>>,
    replies: Rc<RefCell<Vec<Cqe>>>,
    queue: Rc<RefCell<Vec<SequenceCommandKind>>>,
    /// How many reads each reader keeps in flight.
    width: usize,
}

impl Default for Seen {
    fn default() -> Self {
        Self {
            reads: Reads::default(),
            uart: Rc::default(),
            serving: Rc::new(Cell::new(true)),
            replies: Rc::default(),
            queue: Rc::default(),
            width: 1,
        }
    }
}

fn table(seen: &Seen) -> SystemTable {
    let mut table = SystemTable::new();
    let (reads, width) = (seen.reads.clone(), seen.width);
    let make_reader = move |io: &IoInit<'_>| Reader {
        rx: (0..width).map(|_| Slot::Idle(io.alloc(32))).collect(),
        reads: reads.clone(),
    };
    table.register("reader", reader(), make_reader).unwrap();
    let (uart, serving) = (seen.uart.clone(), seen.serving.clone());
    let make_model = move || Model {
        uart: uart.clone(),
        serving: serving.clone(),
    };
    table.register("model", model(), make_model).unwrap();
    let replies_seen = seen.replies.clone();
    table
        .register("replies", replies(), move || replies_seen.clone())
        .unwrap();
    let queue = seen.queue.clone();
    table
        .register("cmds", cmds(), move || queue.clone())
        .unwrap();
    io_steps(&mut table);
    table
}

/// Wires each model in `models`, `(model, resource)`, to the io steps, and
/// moves it between them.
fn wire(mut systems: Vec<SystemConfig>, models: &[(&str, &str)]) -> Vec<SystemConfig> {
    for &(model, resource) in models {
        let at = systems.iter().position(|s| s.id == model).unwrap();
        let mut model_config = systems.remove(at);
        let req = format!("{resource}.req");
        model_config
            .inputs
            .push(InputConfig::new("dev.req", "io_submit", req.as_str()));
        let submit = systems.iter_mut().find(|s| s.id == "io_submit").unwrap();
        submit
            .outputs
            .push(OutputConfig::record(req.as_str(), "io_sqe"));
        let complete = systems.iter_mut().find(|s| s.id == "io_complete").unwrap();
        let rep = format!("{resource}.rep");
        complete
            .inputs
            .push(InputConfig::new(rep.as_str(), model, "dev.rep"));
        let before = systems.iter().position(|s| s.id == "io_complete").unwrap();
        systems.insert(before, model_config);
    }
    systems
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
        let drivers = ["a".to_string(), "b".to_string()];
        let config = CoordinatorConfig {
            systems: wire(with_io_for(systems, &drivers), &[("m", "gps")]),
            resources: vec![sim("gps")],
            ..CoordinatorConfig::default()
        };
        config.build(&table(&seen)).unwrap();
    }

    #[test]
    fn model_without_edges_fails_the_make() {
        let _os = script();
        let seen = Seen::default();
        let systems = vec![driver("a", "gps"), modeled("m", "gps")];
        let err = build(&seen, systems, vec![sim("gps")]).err();
        assert!(
            matches!(&err, Some(BuildError::Params { id, .. }) if id == "io_submit"),
            "{err:?}"
        );
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

mod routing {
    use super::*;

    const WIDTH: usize = MAX_OPS;

    /// `WIDTH` reads behind channel `rx` on a one-deep ring, served by
    /// `m` once `seen.serving` is set.
    fn flooded(seen: &Seen) -> Coordinator {
        let _os = script();
        let reader = SystemConfig {
            inputs: vec![InputConfig::new("commands", "cmds", "commands")],
            adapter: Some(Adapter::Channel(ChannelConfig {
                name: "rx".into(),
                start: RunState::Running,
            })),
            ..driver("r", "gps")
        };
        let systems = vec![
            SystemConfig::new("cmds", "cmds"),
            reader,
            modeled("m", "gps"),
        ];
        let mut systems = wire(with_io_for(systems, &["r".to_string()]), &[("m", "gps")]);
        systems.push(SystemConfig {
            inputs: vec![InputConfig::new("rep", "m", "dev.rep")],
            ..SystemConfig::new("replies", "replies")
        });
        let config = CoordinatorConfig {
            systems,
            resources: vec![sim("gps")],
            ring_depth: 1,
            ..CoordinatorConfig::default()
        };
        config.build(&table(seen)).unwrap()
    }

    /// The model's replies to the reactor's own cancels.
    fn own_replies(seen: &Seen) -> usize {
        let own = |c: &&Cqe| c.token.get() & 0x8000_0000 != 0;
        seen.replies.borrow().iter().filter(own).count()
    }

    /// The reset drops the system and its buffers, so the model answers each
    /// read it had not seen `BadBuf`, and each late cancel `ENOENT`.
    #[test]
    fn sim_cancel_waits_for_room_on_a_full_ring() {
        let seen = Seen {
            width: WIDTH,
            ..Seen::default()
        };
        seen.serving.set(false);
        let mut c = flooded(&seen);
        c.step(Timestamp(1));
        seen.queue
            .borrow_mut()
            .extend([SequenceCommandKind::Stop, SequenceCommandKind::Reset]);
        for t in 2..5 {
            c.step(Timestamp(t));
        }
        seen.serving.set(true);
        c.step(Timestamp(5));
        assert_eq!(own_replies(&seen), 0, "the reads still fill the ring");
        let bad = |c: &&Cqe| c.status.result() == Err(IoError::BadBuf);
        assert_eq!(seen.replies.borrow().iter().filter(bad).count(), WIDTH);
        c.step(Timestamp(6));
        assert_eq!(
            own_replies(&seen),
            0,
            "a drained ring frees its room at the next read"
        );
        c.step(Timestamp(7));
        assert_eq!(
            own_replies(&seen),
            WIDTH,
            "every cancel arrives once there is room"
        );
        assert!(seen.reads.borrow().is_empty(), "the old system is gone");
    }

    #[test]
    fn read_lands_in_the_next_cycle() {
        let _os = script();
        let seen = Seen::default();
        seen.uart.borrow_mut().push(b"fix");
        let systems = vec![driver("r", "gps"), modeled("m", "gps")];
        let systems = wire(with_io_for(systems, &["r".to_string()]), &[("m", "gps")]);
        let config = CoordinatorConfig {
            systems,
            resources: vec![sim("gps")],
            ..CoordinatorConfig::default()
        };
        let mut c = config.build(&table(&seen)).unwrap();
        for t in 1..4 {
            c.step(Timestamp(t));
        }
        let reads = seen.reads.borrow();
        assert_eq!(reads[0], (Timestamp(2), Ok(b"fix".to_vec())));
    }
}
