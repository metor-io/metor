//! TEMPORARY repro of "can't stop the mode sequence from the panel": the full live leg —
//! an in-process metor-db broker (exactly how metor-panel embeds one), the mission's real
//! `TcpUplink`/`TcpDownlink` dialing it over real TCP, and a panel-exact
//! `db.push_msg(SequenceCommand::Abort)`.
//!
//! The commissioning warm-up gate is patched unpassable with a huge timeout so the occupant
//! polls forever until aborted; the coordinator is paced in chunks with wall sleeps so the
//! async uplink task gets real time to dial/subscribe/relay.

#![cfg(not(miri))]

use std::cell::RefCell;
use std::rc::Rc;

use adcs_contracts::ModeCmd;
use metor_fsw_2::metor_proto::types::{ComponentId, Msg, Timestamp};
use metor_fsw_2::metor_proto_wkt::{SequenceCommand, SequenceCommandKind};
use metor_fsw_2::wiring::Registry;
use metor_fsw_2::wiring::{build_artifacts, parse, resolve};
use metor_fsw_2::{BuildOptions, Input, SequenceStatus};

const MISSION_KDL: &str = include_str!("../mission.kdl");
const BROKER: &str = "127.0.0.1:23240";

#[test]
fn autorun_abort_via_real_uplink() {
    // Park the occupant in warm-up forever (gate unpassable, timeout enormous).
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
    let seq: Input<SequenceStatus> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("mode.sequence"))
            .expect("registered")
            .expect("reader slot"),
    );
    let mode: Input<ModeCmd> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("mode.mode_cmd"))
            .expect("registered")
            .expect("reader slot"),
    );

    // The broker, embedded panel-style: TCP server + the same DB handle push_msg writes to.
    let db_path = std::env::temp_dir().join(format!("abort-repro-{}.db", std::process::id()));
    let _ = std::fs::remove_dir_all(&db_path);
    let server = metor_db::Server::new(&db_path, BROKER.parse().unwrap()).expect("bind the broker");
    let db = server.db.clone();

    let captured = stellarator::run(|| async move {
        let broker = stellarator::spawn(server.run()).drop_guard();

        let run_states = Rc::new(RefCell::new(Vec::<u8>::new()));
        let modes = Rc::new(RefCell::new(Vec::<u8>::new()));
        let (rs, ms) = (run_states.clone(), modes.clone());
        let watch = run_states.clone();
        let sampler = stellarator::spawn(async move {
            let (mut seq, mut mode) = (seq, mode);
            loop {
                stellarator::yield_now().await;
                if let Some(r) = seq.latest() {
                    rs.borrow_mut().push(r.run_state);
                }
                let _ = mode.drain(|f| ms.borrow_mut().push(f.get().mode));
            }
        })
        .drop_guard();

        // One long run (run_for is single-shot); a concurrent wall-clock task waits for
        // the uplink to dial + subscribe, pushes the panel-exact Abort, then watches.
        let aborted = Rc::new(RefCell::new(false));
        let aborted_flag = aborted.clone();
        let pusher = stellarator::spawn(async move {
            stellarator::sleep(core::time::Duration::from_secs(2)).await;
            // The panel's Abort button, verbatim (sequences/mod.rs `publish`).
            let cmd = SequenceCommand {
                channel: "mode".to_string(),
                command: SequenceCommandKind::Abort,
            };
            let bytes = postcard::to_stdvec(&cmd).expect("postcard");
            db.push_msg(Timestamp::now(), SequenceCommand::ID, &bytes)
                .expect("push_msg");
            eprintln!("[repro] abort pushed into the db");
            for _ in 0..80 {
                stellarator::sleep(core::time::Duration::from_millis(100)).await;
                if watch.borrow().last() == Some(&2) {
                    *aborted_flag.borrow_mut() = true;
                    eprintln!("[repro] occupant reached Aborted");
                    break;
                }
            }
        })
        .drop_guard();

        coord.run_for(15_000).await;
        drop((sampler, pusher, broker));
        let aborted = *aborted.borrow();
        (run_states, modes, aborted)
    });
    let _ = std::fs::remove_dir_all(&db_path);
    let (run_states, modes, aborted) = captured;
    let run_states = run_states.borrow();
    let modes = modes.borrow();
    eprintln!(
        "[repro] aborted={aborted}, last run_states {:?}, modes {:?}",
        &run_states[run_states.len().saturating_sub(5)..],
        &*modes
    );
    assert!(aborted, "the panel-path abort reached the occupant");
    assert!(modes.contains(&ModeCmd::SAFE), "safed on abort: {modes:?}");
}
