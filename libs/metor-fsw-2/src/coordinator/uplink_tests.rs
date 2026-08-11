//! Uplinked command dispatch off the shared link server.
//!
//! These exercise the command path from an [`UplinkSystem`] into a slot and
//! the coordinator bundle: same-cycle dispatch of an uplinked `Load`+`Start`,
//! the reload re-emission of the [`SequenceRegistry`], and per-id output
//! routing with garbage tolerance. Each seeds the shared [`LinkState`]'s
//! inbound queue directly (the server's reader tasks are covered in the
//! `telemetry::link` tests), registering the uplink ahead of its consumers —
//! the placement that makes a queued command land the same cycle it is
//! republished.
//!
//! The two slot tests open the `metor-fsw-2-seq-fixture` `waiter` entry over a
//! real shared-object boundary; if the fixture cannot be built the body skips.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Duration;

use metor_proto::types::{ComponentId, Msg};
use metor_proto_wkt::{
    ReloadSequences, SequenceChannelEvent, SequenceCommand, SequenceCommandKind, SequenceEventKind,
    SequenceRegistry,
};

use crate::{
    AllowedOccupant, ClockMode, CoordinatorConfig, CyclicSystem, DlPack, DlSystem, Input,
    LinkState, MsgIn, Out, Shared, SlotStatus, System, SystemHealth, SystemInput, SystemOutput,
    Timestamp, UplinkSystem, split_record,
};

use super::PortRef;
use super::init::{Node, SystemBind, cyclic_node};
use super::slot::{SlotReg, plan_slot};
use metor_fsw_2_core::PortId;

// ---------------------------------------------------------------------------
// Fixture build/locate and shared config
// ---------------------------------------------------------------------------

fn fixture_lib_name() -> String {
    let stem = "metor_fsw_2_seq_fixture";
    if cfg!(target_os = "macos") {
        format!("lib{stem}.dylib")
    } else if cfg!(target_os = "windows") {
        format!("{stem}.dll")
    } else {
        format!("lib{stem}.so")
    }
}

/// Build the seq fixture cdylib and return the shared object's path, or `None`
/// (after a skip note) when the build plumbing is unavailable.
fn locate_fixture() -> Option<PathBuf> {
    let output = Command::new(env!("CARGO"))
        .args([
            "build",
            "-p",
            "metor-fsw-2-seq-fixture",
            "--message-format=json",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        eprintln!(
            "skipping: fixture build failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let want = fixture_lib_name();
    for line in stdout.lines() {
        if !line.contains("compiler-artifact") || !line.contains(&want) {
            continue;
        }
        for tok in line.split('"') {
            if tok.ends_with(&want) {
                let path = PathBuf::from(tok);
                if path.exists() {
                    return Some(path);
                }
            }
        }
    }
    eprintln!("skipping: built the fixture but could not locate {want} in cargo output");
    None
}

fn sim_config() -> CoordinatorConfig {
    CoordinatorConfig {
        cycle_rate: 1000.0,
        default_depth: 8,
        clock: ClockMode::Simulated {
            dt: Duration::from_micros(2),
        },
        ..CoordinatorConfig::default()
    }
}

/// Open the fixture's `waiter` entry as a paramless allowed occupant.
fn open_waiter(lib: &PathBuf) -> DlSystem {
    DlPack::open(lib)
        .expect("DlPack::open the sequence .so")
        .system("waiter")
        .expect("select the waiter entry")
}

fn occ(name: &str, system: DlSystem) -> AllowedOccupant {
    AllowedOccupant::dl(name, system, Vec::new())
}

fn load(ch: &str, occupant: &str) -> SequenceCommand {
    SequenceCommand {
        channel: ch.to_string(),
        command: SequenceCommandKind::Load {
            name: occupant.to_string(),
        },
    }
}

// ---------------------------------------------------------------------------
// The seeded link
// ---------------------------------------------------------------------------

/// A constructed link state, never started: these tests feed the inbound
/// queue directly rather than through a socket.
fn test_link() -> Shared<LinkState> {
    let link = Shared::new("TcpServer");
    link.set(LinkState::bind("127.0.0.1:0".parse().unwrap()).expect("bind"))
        .ok();
    link
}

/// Queue one typed msg on the link as its postcard payload under `M::ID` —
/// the bytes a wire `Msg` packet's payload carries.
fn queue<M: Msg + serde::Serialize>(link: &Shared<LinkState>, msg: &M) {
    link.get()
        .push_inbound(M::ID, &postcard::to_allocvec(msg).expect("encode"));
}

/// Drain a message ring, decoding every record as `M` after checking its
/// 2-byte id.
fn drain_msgs<M: Msg + serde::de::DeserializeOwned>(
    view: &mut crate::ring::View<crate::ring::NoWake>,
) -> Vec<M> {
    let mut out = Vec::new();
    let mut buf = Vec::new();
    while view
        .try_read_into(&mut buf)
        .expect("no lap on the message tap")
    {
        let (id, payload) = split_record(&buf).expect("a 2-byte-id record");
        assert_eq!(id, M::ID, "every record on this channel carries M::ID");
        out.push(postcard::from_bytes::<M>(payload).expect("postcard round-trip"));
    }
    out
}

// SlotState wire codes: Empty=0, Loaded=1, Loading=2, Running=3, Done=4.
const LOADED: u8 = 1;
const RUNNING: u8 = 3;
const DONE: u8 = 4;

fn slot_phases(view: &mut Input<SlotStatus>) -> Vec<u8> {
    let mut phases = Vec::new();
    view.drain(|f| phases.push(f.get().phase)).expect("no lap");
    phases
}

// ---------------------------------------------------------------------------
// Same-cycle dispatch of an uplinked command
// ---------------------------------------------------------------------------

/// A command received over the uplink lands in the slot's command ring at the
/// head of a cycle and dispatches that same cycle, with no off-by-one.
#[test]
fn uplink_command_loads_and_starts_same_cycle() {
    let Some(lib) = locate_fixture() else {
        return;
    };
    let loaded = open_waiter(&lib);

    let link = test_link();
    queue(&link, &load("adcs", "waiter"));
    queue(
        &link,
        &SequenceCommand {
            channel: "adcs".to_string(),
            command: SequenceCommandKind::Start,
        },
    );

    let mut b = crate::coordinator::init::InitGraph::new(sim_config());
    // The uplink steps first, so the queued commands hit the slot's command
    // ring at the head of the cycle.
    let uplink = b.push_node(cyclic_node(
        "uplink".into(),
        UplinkSystem::new()
            .attach(link.clone())
            .with_msg::<SequenceCommand>(),
    ));
    // The slot starts empty; the uplink alone drives it.
    let allowed = vec![occ("waiter", loaded)];
    let (desc, ports, process) = plan_slot("adcs", &allowed).unwrap();
    let slot = b.push_node(Node {
        name: "adcs".into(),
        desc,
        bind: SystemBind::Slot(SlotReg {
            allowed,
            initial: None,
            ports,
            process,
        }),
    });
    b.connect(
        PortRef {
            system: uplink,
            port: PortId::Packet(SequenceCommand::ID),
        },
        PortRef {
            system: slot,
            port: PortId::Packet(SequenceCommand::ID),
        },
    );
    let mut coord = b.build().expect("the slot + uplink graph builds");

    let mut slot_view: Input<SlotStatus> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("adcs.slot_status"))
            .expect("slot status is registered")
            .expect("reader slot available"),
    );

    let coord = stellarator::run(|| async move {
        coord.run_for(6).await;
        coord
    });

    let phases = slot_phases(&mut slot_view);
    assert!(
        phases.contains(&RUNNING),
        "the uplink drove the slot to Running: {phases:?}"
    );
    assert_eq!(
        phases.last(),
        Some(&DONE),
        "the started occupant ran to Done: {phases:?}"
    );
    // Load and Start dispatched in the same cycle. A one-cycle-late Start would
    // have let the slot publish a bare Loaded phase.
    assert!(
        !phases.contains(&LOADED),
        "Load and Start landed the same cycle (no bare Loaded): {phases:?}"
    );

    drop((coord, slot_view));
}

/// A [`ReloadSequences`] request wired into the coordinator's #0 bundle
/// re-emits the [`SequenceRegistry`], so a consumer that missed the one-shot
/// boot message (a late-started panel) can recover the channel list.
#[test]
fn reload_request_reemits_registry() {
    let Some(lib) = locate_fixture() else {
        return;
    };
    let loaded = open_waiter(&lib);

    let link = test_link();
    queue(&link, &ReloadSequences {});

    let mut b = crate::coordinator::init::InitGraph::new(sim_config());
    let uplink = b.push_node(cyclic_node(
        "uplink".into(),
        UplinkSystem::new()
            .attach(link.clone())
            .with_msg::<ReloadSequences>(),
    ));
    let allowed = vec![occ("waiter", loaded)];
    let (desc, ports, process) = plan_slot("adcs", &allowed).unwrap();
    let _slot = b.push_node(Node {
        name: "adcs".into(),
        desc,
        bind: SystemBind::Slot(SlotReg {
            allowed,
            initial: None,
            ports,
            process,
        }),
    });
    b.connect(
        PortRef {
            system: uplink,
            port: PortId::Packet(ReloadSequences::ID),
        },
        PortRef {
            system: b.coordinator_handle(),
            port: PortId::Packet(ReloadSequences::ID),
        },
    );
    let mut coord = b.build().expect("the slot + uplink graph builds");

    let mut boot_view = coord
        .registry()
        .view(ComponentId::new("coordinator.sequences"))
        .expect("the coordinator registry channel is registered")
        .expect("reader slot available");

    let coord = stellarator::run(|| async move {
        coord.run_for(6).await;
        coord
    });

    let registries = drain_msgs::<SequenceRegistry>(&mut boot_view);
    assert!(
        registries.len() >= 2,
        "boot emission plus at least one reload re-emission: {} seen",
        registries.len()
    );
    for registry in &registries {
        assert_eq!(registry.channels.len(), 1);
        assert_eq!(registry.channels[0].name, "adcs");
        assert_eq!(registry.channels[0].available, vec!["waiter".to_string()]);
    }

    drop((coord, boot_view));
}

// ---------------------------------------------------------------------------
// Per-id output routing and garbage tolerance (no slot)
// ---------------------------------------------------------------------------

static RELOADS_SEEN: AtomicU64 = AtomicU64::new(0);
static CMDS_SEEN: AtomicU64 = AtomicU64::new(0);

/// A cyclic consumer of both uplink command types, each over its own edge.
struct CmdTap;

#[derive(SystemInput)]
struct CmdTapIn {
    reloads: MsgIn<ReloadSequences>,
    commands: MsgIn<SequenceCommand>,
}

#[derive(SystemOutput)]
struct CmdTapOut {}

impl System for CmdTap {
    type Input = CmdTapIn;
    type Output = Out<CmdTapOut>;
    const NAME: &'static str = "cmd_tap";
}

impl CyclicSystem for CmdTap {
    fn execute(&mut self, _now: Timestamp, input: &mut CmdTapIn, _o: &mut Self::Output) {
        input
            .reloads
            .drain(|_| {
                RELOADS_SEEN.fetch_add(1, Relaxed);
            })
            .unwrap();
        input
            .commands
            .drain(|_| {
                CMDS_SEEN.fetch_add(1, Relaxed);
            })
            .unwrap();
    }
}

/// Queued msgs route by their declared output id. A second declared command
/// type gets its own output, an unknown id counts on the uplink's health, and
/// a malformed payload under a known id is dropped by the consumer's own
/// drain with everything else still flowing.
#[test]
fn uplink_routes_by_declared_output_and_survives_garbage() {
    RELOADS_SEEN.store(0, Relaxed);
    CMDS_SEEN.store(0, Relaxed);

    // The queue is a second-type command (routes to `reloads`), an id the
    // uplink declares no output for (SequenceChannelEvent is downlink traffic),
    // a malformed payload under a known id, and finally a valid
    // SequenceCommand, which arrives only if the garbage wedged nothing.
    let link = test_link();
    queue(&link, &ReloadSequences {});
    queue(
        &link,
        &SequenceChannelEvent {
            channel: "adcs".to_string(),
            kind: SequenceEventKind::Started,
        },
    );
    link.get()
        .push_inbound(SequenceCommand::ID, &[0xFF, 0xFF, 0xFF, 0xFF]);
    queue(
        &link,
        &SequenceCommand {
            channel: "anything".to_string(),
            command: SequenceCommandKind::Start,
        },
    );

    let mut b = crate::coordinator::init::InitGraph::new(sim_config());
    let uplink = b.push_node(cyclic_node(
        "uplink".into(),
        UplinkSystem::new()
            .attach(link.clone())
            .with_msg::<ReloadSequences>()
            .with_msg::<SequenceCommand>(),
    ));
    let tap = b.push_node(cyclic_node(CmdTap::NAME.into(), CmdTap));
    b.connect(
        PortRef {
            system: uplink,
            port: PortId::Packet(ReloadSequences::ID),
        },
        PortRef {
            system: tap,
            port: PortId::Packet(ReloadSequences::ID),
        },
    );
    b.connect(
        PortRef {
            system: uplink,
            port: PortId::Packet(SequenceCommand::ID),
        },
        PortRef {
            system: tap,
            port: PortId::Packet(SequenceCommand::ID),
        },
    );
    let mut coord = b.build().expect("the uplink + tap graph builds");

    let mut health: Input<SystemHealth> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("uplink.health"))
            .expect("the uplink's health is registered")
            .expect("reader slot available"),
    );

    let coord = stellarator::run(|| async move {
        coord.run_for(5).await;
        coord
    });

    assert_eq!(
        RELOADS_SEEN.load(Relaxed),
        1,
        "the second declared command type routed to its own output"
    );
    assert_eq!(
        CMDS_SEEN.load(Relaxed),
        1,
        "the valid command after the malformed one arrived; the garbage \
         payload was dropped at the consumer's drain"
    );
    let mut errors = 0;
    health
        .drain(|f| errors = errors.max(f.get().errors))
        .unwrap();
    assert!(
        errors >= 1,
        "the unknown-id Msg bumped the uplink's unroutable counter: {errors}"
    );

    drop((coord, health));
}
