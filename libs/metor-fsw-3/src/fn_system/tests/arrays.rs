//! `[P; N]` parameters: names, binding, and items.

use core::cell::RefCell;
use std::rc::Rc;

use metor_fsw_3_ring::Notifier;
use metor_proto::types::Timestamp;

use crate::async_system::Stop;
use crate::coordinator::{BuildError, CoordinatorConfig, InputConfig, SystemConfig, SystemTable};
use crate::port::{Input, Output, Sample};
use crate::tests::utils::{Imu, Nav};

use super::{async_def_in, def_of};
use crate::def::DefCx;

/// Names of `ports`, in order.
fn names(ports: &[crate::PortDef]) -> Vec<&str> {
    ports.iter().map(|port| port.name.as_ref()).collect()
}

#[crate::system]
fn mixed(
    fss: [&mut Input<Imu>; 3],
    now: Timestamp,
    gyro: [&Sample<Imu>; 2],
    est: [&mut Output<Nav>; 2],
    grid: [[&mut Output<Imu>; 2]; 2],
) {
    let _ = (fss, now, gyro, est, grid);
}

#[test]
fn test_array_ports_are_indexed() {
    let def = def_of(mixed());
    let inputs = ["fss.0", "fss.1", "fss.2", "gyro.0", "gyro.1"];
    assert_eq!(names(&def.inputs), inputs);
    let outputs = [
        "est.0", "est.1", "grid.0.0", "grid.0.1", "grid.1.0", "grid.1.1",
    ];
    assert_eq!(names(&def.outputs), [&outputs[..], &["log"]].concat());
}

#[crate::system]
fn empty(fss: [&mut Input<Imu>; 0], est: [&mut Output<Nav>; 0], now: Timestamp) {
    let _ = (fss, est, now);
}

#[test]
fn test_empty_array_takes_no_port() {
    let def = def_of(empty());
    assert!(def.inputs.is_empty());
    assert_eq!(names(&def.outputs), ["log"]);
}

#[crate::system]
async fn feeds(fss: [&mut Input<Imu, Notifier>; 2], est: &mut Output<Nav>, stop: Stop) {
    let _ = (fss, est);
    stop.wait().await;
}

#[test]
fn test_async_array_ports_are_indexed() {
    let def = async_def_in(feeds(), &DefCx::empty()).expect("a static definition");
    assert_eq!(names(&def.inputs), ["fss.0", "fss.1"]);
}

/// Writes its value on `imu` every cycle.
#[crate::system]
fn constant(#[state] value: &mut f64, imu: &mut Output<Imu>, now: Timestamp) {
    let _ = imu.write(&Imu::new(now.0, *value));
}

/// Writes `i` on `outs.i` every cycle.
#[crate::system]
fn spread(outs: [&mut Output<Imu>; 3], now: Timestamp) {
    for (i, out) in outs.into_iter().enumerate() {
        let _ = out.write(&Imu::new(now.0, i as f64));
    }
}

type Seen = Rc<RefCell<Vec<[Option<f64>; 3]>>>;

/// Records the newest sample on each element, as an input and as a sample.
#[crate::system]
fn gather(#[state] seen: &mut Seen, fss: [&mut Input<Imu>; 3], held: [&Sample<Imu>; 3]) {
    let newest = fss.map(|input| input.latest().ok().flatten().map(|imu| imu.sample));
    let sampled = held.map(|sample| sample.get().map(|imu| imu.sample));
    assert_eq!(newest, sampled);
    seen.borrow_mut().push(newest);
}

fn gather_table(seen: &Seen) -> SystemTable {
    let mut table = SystemTable::new();
    let seen = seen.clone();
    table.register("one", constant(), || 1.0).expect("valid");
    table.register("three", constant(), || 3.0).expect("valid");
    table.register("spread", spread(), || ()).expect("valid");
    table
        .register("gather", gather(), move || seen.clone())
        .expect("valid");
    table
}

fn gathering(inputs: Vec<InputConfig>, producers: &[(&str, &str)]) -> Vec<[Option<f64>; 3]> {
    let seen = Seen::default();
    let mut systems: Vec<_> = producers
        .iter()
        .map(|(id, ty)| SystemConfig::new(*id, *ty))
        .collect();
    systems.push(SystemConfig {
        inputs,
        ..SystemConfig::new("gather", "gather")
    });
    let config = CoordinatorConfig {
        systems,
        ..Default::default()
    };
    let mut coordinator = config.build(&gather_table(&seen)).expect("valid");
    coordinator.step(Timestamp(1));
    seen.take()
}

#[test]
fn test_records_reach_their_element() {
    let inputs = vec![
        InputConfig::new("fss.0", "three", "imu"),
        InputConfig::new("held.0", "three", "imu"),
        InputConfig::new("fss.2", "one", "imu"),
        InputConfig::new("held.2", "one", "imu"),
    ];
    let seen = gathering(inputs, &[("one", "one"), ("three", "three")]);
    assert_eq!(seen, [[Some(3.0), None, Some(1.0)]]);
}

#[test]
fn test_output_elements_write_their_own_ports() {
    let inputs = (0..3)
        .flat_map(|i| {
            let from = format!("outs.{}", 2 - i);
            [
                InputConfig::new(format!("fss.{i}"), "spread", from.clone()),
                InputConfig::new(format!("held.{i}"), "spread", from),
            ]
        })
        .collect();
    let seen = gathering(inputs, &[("spread", "spread")]);
    assert_eq!(seen, [[Some(2.0), Some(1.0), Some(0.0)]]);
}

#[test]
fn test_unknown_element_is_refused() {
    let config = CoordinatorConfig {
        systems: vec![
            SystemConfig::new("one", "one"),
            SystemConfig {
                inputs: vec![InputConfig::new("fss.3", "one", "imu")],
                ..SystemConfig::new("gather", "gather")
            },
        ],
        ..Default::default()
    };
    let err = config.build(&gather_table(&Seen::default())).err();
    let unknown = matches!(&err, Some(BuildError::UnknownInput { port, .. }) if port == "fss.3");
    assert!(unknown, "{err:?}");
}
