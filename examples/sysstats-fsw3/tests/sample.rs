//! Runs `sim.py`'s samplers on this host for a few seconds and checks that
//! every frame arrives with its maps filled, and that nothing was dropped.

use core::cell::RefCell;
use core::future::{Future, poll_fn};
use core::task::Poll;
use std::path::Path;
use std::rc::Rc;

use metor_fsw_3::builtins::register_builtins;
use metor_fsw_3::cli::run::prepare;
use metor_fsw_3::coordinator::{
    Coordinator, CoordinatorConfig, EndReason, InputConfig, PortRef, SystemConfig,
};
use metor_fsw_3::{Input, system};
use metor_proto_wkt::LogEvent;
use stats_systems::{Cpu, Disks, Mem, Nics, Procs};

/// Cycles to run, 3 s at 100 Hz: three disk samples and thirty process
/// samples.
const CYCLES: u64 = 300;

/// The most entries each map held, and every frame and log line seen.
#[derive(Default, Debug)]
struct Seen {
    frames: [usize; 5],
    cores: usize,
    mem_total: u64,
    ifaces: usize,
    ethtool: usize,
    pids: usize,
    named_pids: usize,
    devices: usize,
    smart: usize,
    logs: Vec<String>,
}

type Shared = Rc<RefCell<Seen>>;

struct Watch(Shared);

#[system]
fn watch(
    #[state] state: &mut Watch,
    cpu: &mut Input<Cpu>,
    mem: &mut Input<Mem>,
    nics: &mut Input<Nics>,
    procs: &mut Input<Procs>,
    disks: &mut Input<Disks>,
    log: &mut Input<LogEvent>,
) {
    let mut seen = state.0.borrow_mut();
    for cpu in cpu.drain().flatten() {
        seen.frames[0] += 1;
        seen.cores = seen.cores.max(cpu.map(|f| &f.cores).len());
    }
    for mem in mem.drain().flatten() {
        seen.frames[1] += 1;
        seen.mem_total = mem.total;
    }
    for nics in nics.drain().flatten() {
        seen.frames[2] += 1;
        let ifaces = nics.map(|f| &f.ifaces);
        let ethtool = ifaces
            .iter()
            .map(|(_, n)| n.map(|s| &s.ethtool).len())
            .sum();
        seen.ifaces = seen.ifaces.max(ifaces.len());
        seen.ethtool = seen.ethtool.max(ethtool);
    }
    for procs in procs.drain().flatten() {
        seen.frames[3] += 1;
        let pids = procs.map(|f| &f.pids);
        let named = pids
            .iter()
            .filter(|(_, p)| p.name.to_str().is_ok_and(|n| !n.is_empty()))
            .count();
        seen.pids = seen.pids.max(pids.len());
        seen.named_pids = seen.named_pids.max(named);
    }
    for disks in disks.drain().flatten() {
        seen.frames[4] += 1;
        let devices = disks.map(|f| &f.devices);
        let smart = devices.iter().map(|(_, d)| d.map(|s| &s.smart).len()).sum();
        seen.devices = seen.devices.max(devices.len());
        seen.smart = seen.smart.max(smart);
    }
    for line in log.drain().flatten() {
        seen.logs
            .push(format!("{}: {:?}", line.message, line.fields));
    }
}

const SAMPLERS: [&str; 5] = ["cpu", "mem", "nics", "procs", "disks"];

/// `sim.py`'s target with its publish link and db swapped for [`watch`].
fn config() -> CoordinatorConfig {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let deployment = prepare(Some(&manifest.join("sim.py"))).expect("sim.py evaluates");
    let mut config = deployment
        .targets
        .into_iter()
        .next()
        .expect("one target")
        .coordinator;
    config
        .systems
        .retain(|system| !["fsw.publish", "fsw.db"].contains(&system.ty.as_str()));
    for system in &mut config.systems {
        if let Some(ty) = system.ty.strip_prefix("stats.") {
            system.ty = ty.into();
        }
    }
    let mut inputs: Vec<InputConfig> = SAMPLERS
        .iter()
        .map(|&id| InputConfig {
            port: id.into(),
            from: vec![PortRef::new(id, id)],
        })
        .collect();
    inputs.push(InputConfig {
        port: "log".into(),
        from: SAMPLERS.iter().map(|&id| PortRef::new(id, "log")).collect(),
    });
    config.systems.push(SystemConfig {
        inputs,
        ..SystemConfig::new("watch", "watch")
    });
    config
}

fn build(shared: &Shared) -> Coordinator {
    let mut table = stats_systems::pack();
    let watched = shared.clone();
    table
        .register("watch", watch(), move || Watch(watched.clone()))
        .expect("valid records");
    register_builtins(&mut table).expect("valid records");
    config().build(&table).expect("the config builds")
}

/// Returns a future that resolves on its `cycles`th poll.
fn stop(cycles: u64) -> impl Future<Output = ()> {
    let mut polls = 0;
    poll_fn(move |_| {
        polls += 1;
        match polls >= cycles {
            true => Poll::Ready(()),
            false => Poll::Pending,
        }
    })
}

#[test]
fn test_samplers_fill_their_maps() {
    let shared = Shared::default();
    let run = shared.clone();
    stellarator::run(move || async move {
        let mut coordinator = build(&run);
        coordinator.run(stop(CYCLES)).await;
        let latched: Vec<&str> = coordinator
            .systems()
            .filter(|(_, why)| *why == Some(&EndReason::Panicked))
            .map(|(id, _)| id)
            .collect();
        assert!(latched.is_empty(), "latched off: {latched:?}");
    });
    let seen = shared.take();
    println!("{seen:#?}");
    let [cpu, mem, nics, procs, disks] = seen.frames;
    assert!(
        cpu > 200 && mem > 200 && nics > 200,
        "fast frames: {:?}",
        seen.frames
    );
    assert!(procs >= 20, "process frames: {procs}");
    assert!(disks >= 2, "disk frames: {disks}");
    assert!(seen.cores > 0, "per-core entries");
    assert!(seen.mem_total > 0, "total memory");
    assert!(seen.ifaces > 0, "interface entries, at least loopback");
    assert!(seen.pids > 0, "process entries");
    assert!(seen.named_pids > 0, "process names");
    assert!(seen.devices > 0, "block devices");
    let bad: Vec<&String> = seen
        .logs
        .iter()
        .filter(|l| l.contains("dropped") || l.contains("failed"))
        .collect();
    assert!(bad.is_empty(), "faults: {bad:?}");
}
