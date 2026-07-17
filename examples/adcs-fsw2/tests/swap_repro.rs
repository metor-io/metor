//! TEMPORARY repro of "load from the panel stays on the existing sequence":
//! the same live leg as `abort_repro` (embedded metor-db broker, real
//! `TcpUplink` over TCP, panel-exact `push_msg`), driven past the terminal:
//! abort commissioning to `Done`, then push `Load { safe_mode }` + `Start`
//! and watch whether the slot actually swaps occupants.

#![cfg(not(miri))]

use std::cell::RefCell;
use std::rc::Rc;

use metor_fsw_2::metor_proto::types::{ComponentId, Msg, Timestamp};
use metor_fsw_2::metor_proto_wkt::{SequenceCommand, SequenceCommandKind};
use metor_fsw_2::wiring::Registry;
use metor_fsw_2::wiring::{build_artifacts, parse, resolve};
use metor_fsw_2::{BuildOptions, Input, SlotStatus};

const MISSION_KDL: &str = include_str!("../mission.kdl");
const BROKER: &str = "127.0.0.1:23241";

fn status_pair(s: &SlotStatus) -> (u8, String) {
    let name = core::str::from_utf8(&s.occupant[..s.occ_len as usize])
        .unwrap_or("<bad>")
        .to_string();
    (s.phase, name)
}

#[test]
fn load_after_done_swaps_occupant_via_real_uplink() {
    // Park commissioning in warm-up forever so the first abort is deterministic.
    let kdl = MISSION_KDL
        .replace("est_delta_rad=0.001", "est_delta_rad=0.0")
        .replace("warmup_timeout_s=10.0", "warmup_timeout_s=1000000.0")
        .replace("127.0.0.1:2240", BROKER);
    assert!(kdl != MISSION_KDL);
    let mut wiring = parse(&kdl).expect("parse the patched mission.kdl");
    for spec in &mut wiring.systems {
        spec.process = false;
    }
    if let Err(e) = build_artifacts(&mut wiring, &BuildOptions::default()) {
        eprintln!("skipping: build_artifacts failed: {e}");
        return;
    }
    let mut coord = resolve(&wiring, &Registry::with_builtins()).expect("resolve the mission");
    let slot_view: Input<SlotStatus> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("mode.slot_status"))
            .expect("registered")
            .expect("reader slot"),
    );

    let db_path = std::env::temp_dir().join(format!("swap-repro-{}.db", std::process::id()));
    let _ = std::fs::remove_dir_all(&db_path);
    let server = metor_db::Server::new(&db_path, BROKER.parse().unwrap()).expect("bind the broker");
    let db = server.db.clone();

    let statuses = stellarator::run(|| async move {
        let broker = stellarator::spawn(server.run()).drop_guard();

        // Sample (phase, occupant) transitions.
        let seen = Rc::new(RefCell::new(Vec::<(u8, String)>::new()));
        let watch = seen.clone();
        let sampler = stellarator::spawn({
            let seen = seen.clone();
            async move {
                let mut slot_view = slot_view;
                loop {
                    stellarator::yield_now().await;
                    if let Some(s) = slot_view.latest() {
                        let pair = status_pair(&s);
                        let mut seen = seen.borrow_mut();
                        if seen.last() != Some(&pair) {
                            seen.push(pair);
                        }
                    }
                }
            }
        })
        .drop_guard();

        let push = {
            let db = db.clone();
            move |command: SequenceCommandKind| {
                let cmd = SequenceCommand {
                    channel: "mode".to_string(),
                    command,
                };
                let bytes = postcard::to_stdvec(&cmd).expect("postcard");
                db.push_msg(Timestamp::now(), SequenceCommand::ID, &bytes)
                    .expect("push_msg");
            }
        };

        // Panel-exact drive: wait for the uplink to dial, abort to Done,
        // then Load(safe_mode) + Start — the flow the panel cannot do today.
        let pusher = stellarator::spawn(async move {
            let wait_for = |want_phase: u8, want_occ: &'static str| {
                let watch = watch.clone();
                async move {
                    for _ in 0..100 {
                        stellarator::sleep(core::time::Duration::from_millis(100)).await;
                        if watch.borrow().last() == Some(&(want_phase, want_occ.to_string())) {
                            return true;
                        }
                    }
                    false
                }
            };
            stellarator::sleep(core::time::Duration::from_secs(2)).await;
            push(SequenceCommandKind::Abort);
            eprintln!("[repro] abort pushed");
            assert!(wait_for(3, "commissioning").await, "reached Done");
            push(SequenceCommandKind::Load {
                name: "safe_mode".to_string(),
            });
            eprintln!("[repro] load(safe_mode) pushed");
            let loaded = wait_for(1, "safe_mode").await;
            eprintln!("[repro] loaded(safe_mode) = {loaded}");
            push(SequenceCommandKind::Start);
            eprintln!("[repro] start pushed");
            let running = wait_for(2, "safe_mode").await;
            eprintln!("[repro] running(safe_mode) = {running}");
        })
        .drop_guard();

        coord.run_for(20_000).await;
        drop((sampler, pusher, broker));
        seen
    });
    let _ = std::fs::remove_dir_all(&db_path);
    let statuses = statuses.borrow();
    eprintln!("[repro] status transitions: {statuses:?}");
    // Done(3) on commissioning, then Loaded(1) and Running(2) on safe_mode.
    assert!(
        statuses.contains(&(3, "commissioning".to_string())),
        "commissioning reached Done: {statuses:?}"
    );
    assert!(
        statuses.contains(&(2, "safe_mode".to_string())),
        "safe_mode swapped in and ran: {statuses:?}"
    );
}
