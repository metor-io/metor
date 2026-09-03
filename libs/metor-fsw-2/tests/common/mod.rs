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

/// Build the cargo package `package` and return its `cdylib` path (library stem
/// `stem`), parsed from cargo's JSON artifact output so a custom target dir or
/// profile still resolves. Returns `None`, after a skip note on stderr, when the
/// build plumbing is unavailable, so the caller skips instead of failing.
pub fn locate_fixture(package: &str, stem: &str) -> Option<PathBuf> {
    let output = Command::new(env!("CARGO"))
        .args(["build", "-p", package, "--message-format=json"])
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
    let want = fixture_lib_name(stem);
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
