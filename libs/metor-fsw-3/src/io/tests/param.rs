use core::cell::RefCell;
use std::rc::Rc;

use metor_proto::types::Timestamp;
use metor_proto_wkt::LogEvent;

use crate::async_system::Stop;
use crate::coordinator::{BuildError, CoordinatorConfig, InputConfig, SystemConfig, SystemTable};
use crate::def::{DefCx, DefError};
use crate::fn_system::{AsyncSystemFn, FnSystem, SystemFn};
use crate::io::SharedIo;
use crate::io::tests::system::{io_steps, script, with_io};
use crate::io::*;
use crate::port::{DynInputs, Input, Output};
use crate::system::{PortDef, System, SystemDef};
use crate::tests::utils::Imu;
use crate::{AsyncSystem, Record, system};

fn def_of<S: SystemFn>(_: S, cx: &DefCx<'_>) -> SystemDef {
    <FnSystem<S> as System>::def(cx).expect("a definition")
}

fn names(ports: &[PortDef]) -> Vec<&str> {
    ports.iter().map(|p| p.name.as_ref()).collect()
}

#[system]
fn two_uarts(imu: &mut Input<Imu>, a: &Cap<Uart>, b: &Cap<Uart>, out: &mut Output<Imu>) {
    let _ = (imu, a, b, out);
}

#[test]
fn caps_share_one_queue_pair_before_log() {
    let def = def_of(two_uarts(), &DefCx::empty());
    assert_eq!(names(&def.inputs), ["imu", IO_CQ]);
    assert_eq!(names(&def.outputs), ["out", IO_SQ, "log"]);
    assert_eq!(def.inputs[1].depth, MAX_OPS);
    assert_eq!(def.outputs[1].depth, MAX_OPS);
    assert_eq!(def.outputs[1].record, Sqe::NAME);
}

#[test]
fn grants_list_each_cap_in_order() {
    let def = def_of(two_uarts(), &DefCx::empty());
    let grant = |param: &'static str| GrantDef {
        param: param.into(),
        interface: "uart".into(),
        end: End::Host,
    };
    assert_eq!(def.grants, [grant("a"), grant("b")]);
}

#[system]
fn dyn_with_cap(ins: &mut DynInputs, uart: &Cap<Uart>) {
    let _ = (ins, uart);
}

#[test]
fn dynamic_inputs_leave_the_cq_edge_alone() {
    let cqe = Input::<Cqe>::def(&DefCx::empty(), IO_CQ);
    let imu = Output::<Imu>::def(&DefCx::empty(), "imu");
    let edges = [(IO_CQ, &cqe), ("plant.imu", &imu)];
    let cx = DefCx {
        inputs: &edges,
        ..DefCx::empty()
    };
    let def = def_of(dyn_with_cap(), &cx);
    assert_eq!(names(&def.inputs), ["plant.imu", IO_CQ]);
}

#[system]
fn model(dev: &mut DeviceSide<Uart>) {
    let _ = dev;
}

#[test]
fn device_side_takes_request_and_reply_ports() {
    let def = def_of(model(), &DefCx::empty());
    assert_eq!(names(&def.inputs), ["dev.req"]);
    assert_eq!(names(&def.outputs), ["dev.rep", "log"]);
    assert_eq!(def.inputs[0].record, Sqe::NAME);
    assert_eq!(def.outputs[0].record, Cqe::NAME);
    assert_eq!(def.grants[0].end, End::Device);
}

#[system]
async fn async_cap(uart: &Cap<Uart>, stop: Stop) {
    let _ = uart;
    stop.wait().await;
}

#[system]
async fn async_device(dev: &mut DeviceSide<Uart>, stop: Stop) {
    let _ = dev;
    stop.wait().await;
}

fn async_def<S: AsyncSystemFn>(_: S) -> Result<SystemDef, DefError> {
    <FnSystem<S> as AsyncSystem>::def(&DefCx::empty())
}

#[test]
fn async_systems_refuse_io() {
    let refused = |system: &str| {
        Err(DefError::IoOnAsync {
            system: system.into(),
        })
    };
    assert_eq!(async_def(async_cap()), refused("async_cap"));
    assert_eq!(async_def(async_device()), refused("async_device"));
}

/// Closes its socket once, on the first cycle.
#[system]
fn closer(#[state] closing: &mut Option<Pending<Close>>, udp: &Cap<Udp>) {
    if closing.is_none() {
        *closing = Some(udp.close());
    }
}

#[system]
fn sq_probe(#[state] seen: &mut Rc<RefCell<Vec<Sqe>>>, sq: &mut Input<Sqe>) {
    seen.borrow_mut().extend(sq.drain().flatten());
}

fn granted(grants: Vec<Grant>) -> SystemConfig {
    SystemConfig {
        grants,
        ..SystemConfig::new("closer", "closer")
    }
}

/// The socket every test grant names.
fn gps1() -> ResourceConfig {
    ResourceConfig {
        id: "gps1".into(),
        backing: Backing::Real(RealKind::Udp {
            bind: "127.0.0.1:0".into(),
            connect: None,
        }),
    }
}

fn grant(param: &str) -> Grant {
    Grant {
        param: param.into(),
        resource: "gps1".into(),
    }
}

fn build(
    systems: Vec<SystemConfig>,
    seen: &Rc<RefCell<Vec<Sqe>>>,
) -> Result<crate::Coordinator, BuildError> {
    let mut table = SystemTable::new();
    table.register("closer", closer(), || None).unwrap();
    let seen = seen.clone();
    table
        .register("sq_probe", sq_probe(), move || seen.clone())
        .unwrap();
    table
        .register("two_uarts", two_uarts(), Default::default)
        .unwrap();
    io_steps(&mut table);
    let config = CoordinatorConfig {
        systems: with_io(systems),
        resources: vec![gps1()],
        ..CoordinatorConfig::default()
    };
    config.build(&table)
}

#[test]
fn granted_cap_writes_its_queue() {
    let seen = Rc::default();
    let probe = SystemConfig {
        inputs: vec![InputConfig::new("sq", "closer", IO_SQ)],
        ..SystemConfig::new("probe", "sq_probe")
    };
    let mut coordinator = build(vec![granted(vec![grant("udp")]), probe], &seen).unwrap();
    coordinator.step(Timestamp(0));
    let seen = seen.borrow();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].op, Op::CLOSE);
    assert_eq!(seen[0].handle, RawHandle::new(0, 0));
}

#[test]
fn grants_are_checked_at_build() {
    let seen = Rc::default();
    let err = |grants| build(vec![granted(grants)], &seen).err();
    let error = |param: &str| ("closer".to_string(), param.to_string());
    let (system, param) = error("udp");
    assert_eq!(err(vec![]), Some(BuildError::Ungranted { system, param }));
    let (system, param) = error("udp");
    let twice = vec![grant("udp"), grant("udp")];
    assert_eq!(
        err(twice),
        Some(BuildError::DuplicateGrant { system, param })
    );
    let (system, param) = error("gps");
    let stray = vec![grant("udp"), grant("gps")];
    assert_eq!(err(stray), Some(BuildError::UnknownGrant { system, param }));
}

#[system]
fn sq_clash(io_sq: &mut Output<Imu>, udp: &Cap<Udp>) {
    let _ = (io_sq, udp);
}

#[test]
fn port_named_io_sq_clashes_with_a_cap() {
    let mut table = SystemTable::new();
    table
        .register("sq_clash", sq_clash(), Default::default)
        .unwrap();
    let config = CoordinatorConfig {
        systems: vec![SystemConfig {
            grants: vec![grant("udp")],
            ..SystemConfig::new("clash", "sq_clash")
        }],
        ..CoordinatorConfig::default()
    };
    assert_eq!(
        config.build(&table).err(),
        Some(BuildError::DuplicateOutput {
            system: "clash".into(),
            port: IO_SQ.into(),
        })
    );
}

#[test]
fn io_errors_log_by_name() {
    let (_ring, output, mut input) = crate::tests::utils::pair::<LogEvent>(4);
    let mut port = crate::log::LogPort::new(output);
    crate::log::with_log_port(&mut port, Timestamp(1), || {
        crate::log::Log.fault("gps_read", IoError::Stale);
    });
    let line = input.drain().next().expect("a line").expect("decodes");
    assert_eq!(&*line.message, "stale handle");
}

/// Writes its greeting whenever its buffer is idle.
struct Greeter {
    tx: Slot<Send>,
}

#[system]
fn greeter(#[state] s: &mut Greeter, udp: &Cap<Udp>) {
    let _ = s.tx.poll();
    if let Some(buf) = s.tx.take_idle() {
        s.tx = Slot::Busy(udp.send(buf));
    }
}

/// The slot each constructor allocated, and whether the next one fails.
#[derive(Default)]
struct Makes {
    slots: Vec<u16>,
    fail_next: bool,
}

#[derive(Clone, Default)]
struct Made(Rc<RefCell<Makes>>);

impl Made {
    fn ctor(
        &self,
    ) -> impl Fn(
        crate::tests::utils::NoParams,
        &IoInit<'_>,
    ) -> Result<Greeter, crate::coordinator::ParamError>
    + use<> {
        let made = self.clone();
        move |_, io| {
            if core::mem::take(&mut made.0.borrow_mut().fail_next) {
                return Err(crate::coordinator::ParamError::Decode("not today".into()));
            }
            let buf = io.alloc_from(b"hi");
            made.0.borrow_mut().slots.push(buf.buf_ref().slot());
            Ok(Greeter {
                tx: Slot::Idle(buf),
            })
        }
    }
}

fn greeter_table(made: &Made, seen: &Rc<RefCell<Vec<Sqe>>>) -> SystemTable {
    let mut table = SystemTable::new();
    table.register("greeter", greeter(), made.ctor()).unwrap();
    let seen = seen.clone();
    table
        .register("sq_probe", sq_probe(), move || seen.clone())
        .unwrap();
    io_steps(&mut table);
    table
}

fn greeter_config(adapter: Option<crate::coordinator::Adapter>, cmds: bool) -> CoordinatorConfig {
    let mut systems = vec![];
    let mut inputs = vec![];
    if cmds {
        systems.push(SystemConfig::new("cmds", "cmd_source"));
        inputs.push(InputConfig::new("commands", "cmds", "commands"));
    }
    systems.push(SystemConfig {
        inputs,
        adapter,
        grants: vec![grant("udp")],
        ..SystemConfig::new("gps", "greeter")
    });
    systems.push(SystemConfig {
        inputs: vec![InputConfig::new("sq", "gps", IO_SQ)],
        ..SystemConfig::new("probe", "sq_probe")
    });
    CoordinatorConfig {
        systems: with_io(systems),
        resources: vec![gps1()],
        ..CoordinatorConfig::default()
    }
}

#[test]
fn constructor_buffer_goes_out_on_its_own_queue() {
    let (made, seen) = (Made::default(), Rc::default());
    let mut coordinator = greeter_config(None, false)
        .build(&greeter_table(&made, &seen))
        .unwrap();
    coordinator.step(Timestamp(0));
    let slot = made.0.borrow().slots[0];
    let seen = seen.borrow();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].op, Op::send(BufRef::new(slot)));
}

#[system]
fn hoarder(#[state] s: &mut Option<Buf>, imu: &mut Output<Imu>) {
    let _ = (s, imu);
}

#[test]
fn allocating_without_a_cap_fails_the_make() {
    let mut table = SystemTable::new();
    table
        .register("hoarder", hoarder(), |io: &IoInit<'_>| Some(io.alloc(4)))
        .unwrap();
    let config = CoordinatorConfig {
        systems: vec![SystemConfig::new("stash", "hoarder")],
        ..CoordinatorConfig::default()
    };
    let Err(BuildError::Params { id, source }) = config.build(&table) else {
        panic!("expected a params error");
    };
    assert_eq!(id, "stash");
    let crate::coordinator::ParamError::IoMemory { need, reason } = source else {
        panic!("expected IoMemory");
    };
    assert_eq!(need, 4);
    assert!(reason.contains("`stash`"), "{reason}");
}

#[system]
fn cmd_source(
    #[state] queue: &mut Rc<RefCell<Vec<metor_proto_wkt::SequenceCommand>>>,
    commands: &mut Output<metor_proto_wkt::SequenceCommand>,
) {
    for command in queue.borrow_mut().drain(..) {
        let _ = commands.write(&command);
    }
}

#[test]
fn reset_remakes_an_io_system_over_its_rings() {
    use crate::coordinator::{Adapter, ChannelConfig, RunState};
    use metor_proto_wkt::{SequenceCommand, SequenceCommandKind};

    let (made, seen) = (Made::default(), Rc::<RefCell<Vec<Sqe>>>::default());
    let queue = Rc::<RefCell<Vec<SequenceCommand>>>::default();
    // The old system's ops must finish before the remake, so the Os answers.
    script().borrow_mut().answer = Some(Box::new(|sqe| Some(Cqe::ok(sqe.token, Out::UNIT))));
    let mut table = greeter_table(&made, &seen);
    let q = queue.clone();
    table
        .register("cmd_source", cmd_source(), move || q.clone())
        .unwrap();
    let channel = Adapter::Channel(ChannelConfig {
        name: "gps".into(),
        start: RunState::Running,
    });
    let mut coordinator = greeter_config(Some(channel), true).build(&table).unwrap();
    let command = |command| SequenceCommand {
        channel: "gps".into(),
        command,
    };
    coordinator.step(Timestamp(0));
    assert_eq!(seen.borrow_mut().drain(..).count(), 1);

    // A failed make must release the rings, or the second make cannot bind.
    // An IO system's reset takes effect on a later cycle, once it is idle.
    made.0.borrow_mut().fail_next = true;
    queue.borrow_mut().push(command(SequenceCommandKind::Stop));
    queue.borrow_mut().push(command(SequenceCommandKind::Reset));
    coordinator.step(Timestamp(1));
    coordinator.step(Timestamp(2));
    queue.borrow_mut().push(command(SequenceCommandKind::Reset));
    coordinator.step(Timestamp(3));
    coordinator.step(Timestamp(4));
    queue.borrow_mut().push(command(SequenceCommandKind::Start));
    coordinator.step(Timestamp(5));
    coordinator.step(Timestamp(6));
    let seen = seen.borrow();
    assert!(seen.iter().all(|s| s.op.kind() == OpKind::Send));
    assert!(!seen.is_empty(), "the remade system submits");
    assert_eq!(made.0.borrow().slots.len(), 2);
}

/// Registers `S` through the `System` trait path.
fn via_trait<S: SystemFn>(_: S) -> Result<(), BuildError>
where
    S::State: Default,
{
    let mut table = SystemTable::new();
    table.register_system::<FnSystem<S>>("io", |_| Ok((FnSystem::default(), Default::default())))
}

#[test]
fn io_system_through_register_system_is_refused() {
    let refused = via_trait(closer()).unwrap_err();
    assert!(matches!(
        refused,
        BuildError::Def {
            source: DefError::IoOnTraitPath { .. },
            ..
        }
    ));
    assert!(via_trait(sq_probe()).is_ok());
}

#[test]
fn failed_make_frees_its_ring_slots() {
    use crate::coordinator::{MakeCx, Params};
    use crate::tests::utils::ring_of;

    let (made, seen) = (Made::default(), Rc::default());
    let table = greeter_table(&made, &seen);
    let entry = table.get("greeter").expect("registered");
    let def = &entry.def;
    let ring = |p: &PortDef| ring_of(p.max_len, p.depth, 1);
    let inputs: Vec<_> = def.inputs.iter().map(ring).collect();
    let outputs: Vec<_> = def.outputs.iter().map(ring).collect();
    let make = || {
        (entry.make)(MakeCx {
            id: "gps",
            adapter: None,
            def,
            params: Params(&serde_json::Value::Null),
            inputs: inputs.iter().map(|r| vec![r]).collect(),
            outputs: outputs.iter().collect(),
            threads: &mut crate::thread::Threads::new(),
            io: &crate::io::SharedIo::default(),
            seed: Some(0),
            namespace: "",
        })
    };
    made.0.borrow_mut().fail_next = true;
    assert!(make().is_err());
    assert!(make().is_ok(), "the failed make left its ring slots free");
}

/// Binds `S` over fresh rings through `io`, as system `id`.
fn bound<S: SystemFn>(
    _: S,
    io: &SharedIo,
    id: &str,
) -> (
    crate::fn_system::OutSet<S>,
    Option<Io>,
    Vec<metor_fsw_3_ring::RingBuffer>,
) {
    use crate::tests::utils::ring_of;
    let def = def_of_fn::<S>();
    let ring = |p: &PortDef| ring_of(p.max_len, p.depth, 1);
    let rings: Vec<_> = def.inputs.iter().chain(&def.outputs).map(ring).collect();
    let (ins, outs) = rings.split_at(def.inputs.len());
    let inputs = ins.iter().map(|r| vec![r]).collect();
    let (views, writers) = crate::coordinator::bindings(&def, inputs, outs.iter().collect());
    let instance = crate::fn_system::Instance {
        io,
        id,
        namespace: "",
        seed: Some(0),
    };
    let (_ins, outs, io) = crate::fn_system::bind::<S>(views, writers, instance).unwrap();
    (outs, io, rings)
}

fn def_of_fn<S: SystemFn>() -> SystemDef {
    <FnSystem<S> as System>::def(&DefCx::empty()).expect("a definition")
}

/// A model with one `DeviceSide`, with a nameable parameter tuple.
struct Serve;

impl crate::fn_system::Ports for Serve {
    type State = ();
    type Params = (DeviceSide<Uart>,);
    const NAME: &'static str = "serve";
    const NAMES: &'static [&'static str] = &["dev"];
}

impl SystemFn for Serve {
    fn call(
        _: &mut (),
        _: <Self::Params as crate::fn_system::Param>::Item<'_, metor_fsw_3_ring::NoWake>,
    ) {
    }
}

#[test]
fn device_side_resolves_tables_added_later_once_frozen() {
    let io = SharedIo::default();
    let (mut outs, _, _rings) = bound(Serve, &io, "dev");
    let member = io.borrow_mut().member("drv", vec!["uart".into()]).unwrap();
    let slot = member.table.alloc_from(b"abc").unwrap();
    let mut buf = BufRef::new(slot);
    buf.stamp(member.table.bufs.id().unwrap());
    let dev = &mut outs.outs.0;
    assert_eq!(dev.bytes(buf), Err(IoError::BadBuf));
    member.table.bufs.freeze();
    assert_eq!(dev.bytes(buf).unwrap(), b"abc");
}

#[test]
fn two_makes_share_one_member() {
    let io = SharedIo::default();
    let (_, first, _a) = bound(closer(), &io, "closer");
    let (_, second, _b) = bound(closer(), &io, "closer");
    let (first, second) = (first.unwrap(), second.unwrap());
    assert!(Rc::ptr_eq(first.table(), second.table()));
    assert_eq!(io.borrow().members.len(), 1);
    let (_, other, _c) = bound(closer(), &io, "other");
    assert!(!Rc::ptr_eq(first.table(), other.unwrap().table()));
}

#[system]
fn stream_user(link: &Cap<Stream>) {
    let _ = link;
}

fn checked(
    resources: Vec<ResourceConfig>,
    ty: &str,
    param: &str,
    resource: &str,
) -> Option<BuildError> {
    let mut table = SystemTable::new();
    table.register("closer", closer(), || None).unwrap();
    table
        .register("stream_user", stream_user(), Default::default)
        .unwrap();
    let config = CoordinatorConfig {
        systems: vec![SystemConfig {
            grants: vec![Grant {
                param: param.into(),
                resource: resource.into(),
            }],
            ..SystemConfig::new("sys", ty)
        }],
        resources,
        ..CoordinatorConfig::default()
    };
    config.build(&table).err()
}

fn real(id: &str, kind: RealKind) -> ResourceConfig {
    ResourceConfig {
        id: id.into(),
        backing: Backing::Real(kind),
    }
}

#[test]
fn granted_resource_must_provide_the_interface() {
    let dir = real(
        "logs",
        RealKind::Dir {
            path: "/tmp".into(),
        },
    );
    assert_eq!(
        checked(vec![dir], "closer", "udp", "logs"),
        Some(BuildError::Interface {
            cap: "sys.udp".into(),
            resource: "logs".into(),
            expected: "udp".into(),
            found: "dir".into(),
        })
    );
}

#[test]
fn no_kind_provides_a_stream() {
    let listen = real(
        "srv",
        RealKind::TcpListen {
            addr: "127.0.0.1:0".into(),
        },
    );
    let err = checked(vec![listen], "stream_user", "link", "srv");
    assert!(matches!(
        err,
        Some(BuildError::Interface { ref cap, ref expected, .. }) if cap == "sys.link" && expected == "stream"
    ));
}

#[test]
fn unknown_and_duplicate_resources_fail() {
    assert_eq!(
        checked(vec![gps1()], "closer", "udp", "gps9"),
        Some(BuildError::UnknownResource {
            system: "sys".into(),
            param: "udp".into(),
            resource: "gps9".into(),
        })
    );
    assert_eq!(
        checked(vec![gps1(), gps1()], "closer", "udp", "gps1"),
        Some(BuildError::DuplicateResource { id: "gps1".into() })
    );
}

#[test]
fn peer_backing_is_refused() {
    let peer = ResourceConfig {
        id: "down".into(),
        backing: Backing::Peer("plant.down".into()),
    };
    assert_eq!(
        checked(vec![peer], "closer", "udp", "down"),
        Some(BuildError::Backing {
            resource: "down".into(),
            backing: "peer".into(),
        })
    );
}

#[test]
fn config_without_resources_decodes() {
    let text = r#"{"clock":{"Wall":{"rate":10.0}},"ring_depth":8,"systems":[]}"#;
    let config: CoordinatorConfig = serde_json::from_str(text).unwrap();
    assert!(config.resources.is_empty());
    assert!(
        !serde_json::to_string(&config)
            .unwrap()
            .contains("resources")
    );
}
