use core::cell::RefCell;
use std::rc::Rc;

use metor_proto::types::Timestamp;
use metor_proto_wkt::LogEvent;

use crate::async_system::Stop;
use crate::coordinator::{BuildError, CoordinatorConfig, InputConfig, SystemConfig, SystemTable};
use crate::def::{DefCx, DefError};
use crate::fn_system::{AsyncSystemFn, FnSystem, SystemFn};
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

/// Closes its uart once, on the first cycle.
#[system]
fn closer(#[state] closing: &mut Option<Pending<Close>>, uart: &Cap<Uart>) {
    if closing.is_none() {
        *closing = Some(uart.close());
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
    let config = CoordinatorConfig {
        systems,
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
    let mut coordinator = build(vec![granted(vec![grant("uart")]), probe], &seen).unwrap();
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
    let (system, param) = error("uart");
    assert_eq!(err(vec![]), Some(BuildError::Ungranted { system, param }));
    let (system, param) = error("uart");
    let twice = vec![grant("uart"), grant("uart")];
    assert_eq!(
        err(twice),
        Some(BuildError::DuplicateGrant { system, param })
    );
    let (system, param) = error("gps");
    let stray = vec![grant("uart"), grant("gps")];
    assert_eq!(err(stray), Some(BuildError::UnknownGrant { system, param }));
}

#[system]
fn sq_clash(io_sq: &mut Output<Imu>, uart: &Cap<Uart>) {
    let _ = (io_sq, uart);
}

#[test]
fn port_named_io_sq_clashes_with_a_cap() {
    let mut table = SystemTable::new();
    table
        .register("sq_clash", sq_clash(), Default::default)
        .unwrap();
    let config = CoordinatorConfig {
        systems: vec![SystemConfig {
            grants: vec![grant("uart")],
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
