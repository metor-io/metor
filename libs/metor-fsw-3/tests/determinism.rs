//! A deterministic sim run gives the same records every time: a driver on a
//! sim resource whose model draws from the target seed, and an async system
//! on a turned group.

mod common;

#[path = "fixtures/echo-pack/src/chatter.rs"]
#[allow(dead_code)]
mod chatter;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use common::Ping;
use metor_fsw_3::coordinator::{
    Adapter, BuildOptions, Clock, Coordinator, CoordinatorConfig, InputConfig, OutputConfig,
    SystemConfig, SystemTable,
};
use metor_fsw_3::io::{Backing, End, Grant, IO_CQ, IO_SQ, IO_TYPE, ResourceConfig, SimKind};
use metor_fsw_3::ring::Notifier;
use metor_fsw_3::{DynInputs, Input, Output, Stop, Timestamp, system};

/// A record one port carried: the cycle time, the port, and its bytes.
type Tapped = Vec<(i64, String, Vec<u8>)>;

/// Keeps every record on its configured ports.
#[system]
fn tap(#[state] seen: &mut Rc<RefCell<Tapped>>, now: Timestamp, taps: &mut DynInputs) {
    for (def, input) in taps.iter_mut() {
        for bytes in input.drain().flatten() {
            let port = def.name.to_string();
            seen.borrow_mut().push((now.0, port, bytes.to_vec()));
        }
    }
}

/// Sleeps 20 ms of cycle time per ping, then echoes it.
#[system]
async fn napper(input: &mut Input<Ping, Notifier>, output: &mut Output<Ping>, stop: Stop) {
    while !stop.is_set() {
        let ping = futures_lite::future::or(async { input.next().await.ok() }, async {
            stop.wait().await;
            None
        })
        .await;
        let Some(ping) = ping else { return };
        stellarator::sleep(Duration::from_millis(20)).await;
        let _ = output.write(&ping);
    }
}

fn table(seen: &Rc<RefCell<Tapped>>) -> SystemTable {
    let mut table = SystemTable::new();
    metor_fsw_3::link::register_builtins(&mut table).unwrap();
    table
        .register("chatter", chatter::chatter(), chatter::Chatter::new)
        .unwrap();
    table
        .register("reader", chatter::reader(), chatter::Reader::new)
        .unwrap();
    table
        .register_async("napper", napper(), Default::default)
        .unwrap();
    let seen = seen.clone();
    table.register("tap", tap(), move || seen.clone()).unwrap();
    table
}

fn grant(param: &str, end: End) -> Vec<Grant> {
    let resource = "line".into();
    vec![Grant {
        param: param.into(),
        resource,
        end,
    }]
}

/// `reader` on sim resource `line`, `chatter` on its other end between the io
/// systems, `napper` on a thread, and `tap` on every IO port and `napper`'s
/// output.
fn config(seed: u64) -> CoordinatorConfig {
    let systems = vec![
        SystemConfig {
            grants: grant("uart", End::Host),
            inputs: vec![InputConfig::new(IO_CQ, "io_complete", "reader")],
            ..SystemConfig::new("reader", "reader")
        },
        SystemConfig {
            inputs: vec![InputConfig::new("input", "reader", "sums")],
            adapter: Some(Adapter::Thread("io".into())),
            ..SystemConfig::new("napper", "napper")
        },
        SystemConfig {
            inputs: vec![InputConfig::new("reader", "reader", IO_SQ)],
            outputs: vec![OutputConfig::record("chatter", "io_cqe")],
            ..SystemConfig::new("io_submit", IO_TYPE)
        },
        SystemConfig {
            grants: grant("uart", End::Device),
            inputs: vec![InputConfig::new(IO_CQ, "io_submit", "chatter")],
            ..SystemConfig::new("chatter", "chatter")
        },
        SystemConfig {
            inputs: vec![InputConfig::new("chatter", "chatter", IO_SQ)],
            outputs: vec![OutputConfig::record("reader", "io_cqe")],
            ..SystemConfig::new("io_complete", IO_TYPE)
        },
        SystemConfig {
            inputs: vec![
                InputConfig::new("sq", "reader", IO_SQ),
                InputConfig::new("cq", "io_complete", "reader"),
                InputConfig::new("model_sq", "chatter", IO_SQ),
                InputConfig::new("model_cq", "io_submit", "chatter"),
                InputConfig::new("echo", "napper", "output"),
            ],
            ..SystemConfig::new("tap", "tap")
        },
    ];
    CoordinatorConfig {
        clock: Clock::Simulated {
            dt: Duration::from_millis(10),
            epoch: Some(Timestamp(0)),
        },
        systems,
        resources: vec![ResourceConfig {
            id: "line".into(),
            backing: Backing::Sim(SimKind::Uart),
        }],
        seed,
        ..CoordinatorConfig::default()
    }
}

const CYCLES: i64 = 500;

/// Runs `CYCLES` cycles and returns every tapped record.
fn run(seed: u64, deterministic: bool) -> (Tapped, Coordinator) {
    let seen = Rc::default();
    let options = BuildOptions { deterministic };
    let mut c = config(seed).build_with(&table(&seen), options).unwrap();
    for k in 0..CYCLES {
        c.step(Timestamp(k * 10_000));
    }
    let tapped = seen.take();
    (tapped, c)
}

fn on(tapped: &Tapped, port: &str) -> Tapped {
    tapped
        .iter()
        .filter(|(_, p, _)| p == port)
        .cloned()
        .collect()
}

#[test]
fn same_seed_gives_the_same_records() {
    let (first, c) = run(7, true);
    assert_eq!(c.latched().count(), 0);
    let echoes = on(&first, "echo");
    assert!(!echoes.is_empty(), "the async system answered");
    assert!(!on(&first, "model_sq").is_empty(), "the model answered");
    let (second, _) = run(7, true);
    assert_eq!(first, second);
}

#[test]
fn another_seed_gives_other_bytes() {
    let (seven, _) = run(7, true);
    let (eight, _) = run(8, true);
    assert_ne!(on(&seven, "model_sq"), on(&eight, "model_sq"));
    let first_sq = |t: &Tapped| on(t, "sq").into_iter().next();
    assert_eq!(
        first_sq(&seven),
        first_sq(&eight),
        "the first read is the same"
    );
}

/// The threaded build runs; its records depend on the OS scheduler, so they
/// are not compared.
#[test]
fn threaded_build_still_runs() {
    let (_, c) = run(7, false);
    assert_eq!(c.latched().count(), 0);
    assert_eq!(c.cycle(), CYCLES as u64);
}
