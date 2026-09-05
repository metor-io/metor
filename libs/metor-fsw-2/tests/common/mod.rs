//! Shared fixture build/locate helpers and the host-side mirrors of the
//! dl fixture's frames, for the integration tests.

// Each integration target compiles this module independently and uses a
// different subset of it.
#![allow(dead_code)]

use std::path::PathBuf;
use std::process::Command;

use metor_fsw_2::metor_proto::types::{Msg, Timestamp};
use metor_fsw_2::ring::{NoWake, View};
use metor_fsw_2::{
    BuildSystem, CyclicSystem, Frame, Out, Output, System, SystemInput, SystemOutput, split_record,
};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

// The dl fixture's frames, params, and message, byte for byte: that layout
// agreement is the contract `compatible()` checks against the descriptor
// reconstructed from the shared object.

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "tick_in")]
pub struct TickIn {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    pub value: u64,
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "tick_out")]
pub struct TickOut {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    pub count: u64,
}

#[derive(serde::Serialize, Default)]
pub struct CounterParams {
    pub start: u64,
    pub scale: f64,
}

/// The shared schema name hashes to the same `PacketId` as the fixture's, so
/// the host decodes the loaded system's records from the id alone.
#[derive(serde::Serialize, serde::Deserialize, postcard_schema::Schema, Debug)]
pub struct TickEvent {
    pub count: u64,
}

/// A statically linked producer feeding the loaded consumer: emits
/// `tick_in.value = 1, 2, 3, ...`, so after `n` cycles the freshest value is `n`.
pub struct Ticker {
    n: u64,
}

#[derive(SystemInput)]
pub struct TickerIn {}

#[derive(SystemOutput)]
pub struct TickerOut {
    tick: Output<TickIn>,
}

impl System for Ticker {
    type Input = TickerIn;
    type Output = Out<TickerOut>;
    const NAME: &'static str = "ticker";
}

impl CyclicSystem for Ticker {
    fn execute(&mut self, now: Timestamp, _input: &mut TickerIn, output: &mut Out<TickerOut>) {
        self.n += 1;
        let _ = output.tick.write(&TickIn {
            timestamp: now,
            value: self.n,
        });
    }
}

impl BuildSystem for Ticker {
    type Params = ();
    fn new(_params: ()) -> Self {
        Ticker { n: 0 }
    }
}

/// The platform file name of a `cdylib` with library stem `stem`.
pub fn fixture_lib_name(stem: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("lib{stem}.dylib")
    } else if cfg!(target_os = "windows") {
        format!("{stem}.dll")
    } else {
        format!("lib{stem}.so")
    }
}

/// Build a required fixture and locate the cdylib in Cargo's JSON output.
/// Build failures fail the test, including the compiler diagnostics.
pub fn locate_fixture(package: &str, stem: &str) -> PathBuf {
    let output = Command::new(env!("CARGO"))
        .args(["build", "-p", package, "--message-format=json"])
        .output()
        .expect("run cargo for the required fixture");
    assert!(
        output.status.success(),
        "fixture {package} failed to build:\n{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    let want = fixture_lib_name(stem);
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Ok(message) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if message["reason"] != "compiler-artifact" {
            continue;
        }
        if let Some(files) = message["filenames"].as_array() {
            for file in files.iter().filter_map(|file| file.as_str()) {
                let path = PathBuf::from(file);
                if path.file_name().is_some_and(|name| name == want.as_str()) && path.is_file() {
                    return path;
                }
            }
        }
    }
    panic!("cargo built {package} but did not report the required artifact {want}");
}

/// A stalled status tap must only drop telemetry, never stop the occupant.
pub async fn assert_status_backpressure(mut coord: metor_fsw_2::Coordinator, name: &str) {
    use metor_fsw_2::metor_proto::types::ComponentId;
    let tap = coord
        .registry()
        .view(ComponentId::new(&format!("{name}.slot_status")))
        .expect("status output registered")
        .expect("reader slot available");
    let log_view = coord
        .registry()
        .view(ComponentId::new("coordinator.log"))
        .expect("log registered")
        .expect("reader slot available");
    let mut logs = metor_fsw_2::MsgIn::<metor_fsw_2::LogEvent>::new(log_view);
    coord.run_for(100).await;
    assert!(
        coord.stopped().is_empty(),
        "status backpressure stopped a healthy slot: {:?}",
        coord.stopped()
    );
    let mut reported = false;
    logs.drain(|event| {
        reported |= event
            .fields
            .iter()
            .any(|(key, value)| key == "kind" && value == "status_publish_failed");
    })
    .expect("read log");
    assert!(reported, "status drops must be reported");
    drop(tap);
}

/// Drain a message ring, checking and decoding every record as `M`.
pub fn drain_msgs<M: Msg + serde::de::DeserializeOwned>(view: &mut View<NoWake>) -> Vec<M> {
    let mut out = Vec::new();
    let mut buf = Vec::new();
    while view
        .try_read_into(&mut buf)
        .expect("no lap on the message tap")
    {
        let (id, payload) = split_record(&buf).expect("a 2-byte-id record");
        assert_eq!(id, M::ID, "every record on this channel carries M::ID");
        out.push(postcard::from_bytes(payload).expect("postcard round-trip"));
    }
    out
}
