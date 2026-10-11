//! Records, fixtures, and wait helpers shared by the integration tests.

#![allow(dead_code)]

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::fmt::Display;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use metor_fsw_3::cli::cargo::cargo_build;
use metor_fsw_3::coordinator::{CoordinatorConfig, SystemTable};
use metor_fsw_3::{Frame, Output, Record, Timestamp, system};
use metor_proto_stellar::{PacketSink, PacketStream, Peer, identify};
use metor_proto_wkt::LinkInfo;
use serde::{Deserialize, Serialize};
use stellarator::io::{OwnedReader, OwnedWriter};
use stellarator::net::TcpStream;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// How long a test waits on a target before it fails.
pub const DEADLINE: Duration = Duration::from_secs(20);

/// The fixture pack's `Ping` record, declared again here.
///
/// The host matches records by a hash of the name, so this copy is the same
/// record as the pack's.
#[derive(Record, metor_fsw_3::Schema, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[postcard(crate = metor_fsw_3::postcard_schema)]
#[record(max_len = 8)]
pub struct Ping {
    pub n: u32,
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug)]
#[frame(name = "imu")]
#[repr(C)]
pub struct Imu {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    pub sample: f64,
}

/// Publishes one sample per cycle, counting up.
///
/// If it has a gate, it publishes nothing until the gate is set.
#[derive(Default)]
pub struct Source(pub i64, pub Option<Arc<AtomicBool>>);

#[system]
pub fn source(#[state] s: &mut Source, imu: &mut Output<Imu>) {
    if s.1
        .as_ref()
        .is_some_and(|gate| !gate.load(Ordering::Acquire))
    {
        return;
    }
    s.0 += 1;
    let _ = imu.write(&Imu {
        timestamp: Timestamp(s.0),
        sample: s.0 as f64,
    });
}

/// Returns the `echo-pack` fixture's crate directory.
pub fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/echo-pack")
}

/// Builds the fixture once per test binary and returns its cdylib.
pub fn fixture_lib() -> &'static Path {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT.get_or_init(|| {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        cargo_build("echo-pack", root).expect("the fixture builds")
    })
}

pub fn metor() -> Command {
    Command::new(env!("CARGO_BIN_EXE_metor"))
}

/// A running `metor` child process. Dropping it kills the process.
pub struct Running(pub Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Returns `N` ports that were free when this was called.
pub fn free_ports<const N: usize>() -> [u16; N] {
    let held: [TcpListener; N] =
        core::array::from_fn(|_| TcpListener::bind("127.0.0.1:0").expect("a free port"));
    held.map(|listener| listener.local_addr().expect("bound").port())
}

/// Joins `ports` into the comma-separated list a fixture's ports variable
/// expects.
pub fn ports_env(ports: &[u16]) -> String {
    ports
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

pub fn local(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// A deadline [`DEADLINE`] from now, for polling loops.
pub struct Deadline(Instant);

impl Deadline {
    pub fn new() -> Self {
        Self(Instant::now() + DEADLINE)
    }

    /// Sleeps for a short tick. Fails the test with `what` if the deadline has
    /// passed.
    pub async fn tick(&self, what: impl Display) {
        assert!(Instant::now() < self.0, "{what}");
        stellarator::sleep(Duration::from_millis(5)).await;
    }
}

/// Awaits `fut`, failing the test with `what` if it takes longer than
/// [`DEADLINE`].
pub async fn within<T>(what: &str, fut: impl Future<Output = T>) -> T {
    let late = async {
        stellarator::sleep(DEADLINE).await;
        panic!("{what}")
    };
    futures_lite::future::or(fut, late).await
}

/// A connection to an fsw link, after its `LinkInfo` has been read.
pub struct Fsw {
    pub info: LinkInfo,
    pub rx: PacketStream<OwnedReader<TcpStream>>,
    pub tx: PacketSink<OwnedWriter<TcpStream>>,
    pub buf: Vec<u8>,
}

/// Dials `addr` until an fsw link answers, and returns the connection.
pub async fn dial_fsw(addr: SocketAddr) -> Fsw {
    let deadline = Deadline::new();
    loop {
        match identify(addr).await {
            Ok(Peer::Fsw { info, rx, tx, buf }) => return Fsw { info, rx, tx, buf },
            Ok(Peer::Db(_)) => panic!("{addr} is a db, not an fsw link"),
            Err(e) => deadline.tick(format!("{addr} never came up: {e}")).await,
        }
    }
}

/// A target running on its own thread. Dropping it stops and joins the
/// thread.
pub struct Target {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Target {
    /// Builds `config` and runs it on a new thread.
    ///
    /// `table` is called on that thread to make the system table. Each cycle
    /// increments `cycles`.
    pub fn spawn(
        config: CoordinatorConfig,
        table: impl FnOnce() -> SystemTable + Send + 'static,
        cycles: Arc<AtomicU64>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut coordinator = config.build(&table()).expect("valid config");
            stellarator::run(|| async move { coordinator.run(Until(flag, cycles)).await });
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Target {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A stop future that completes once the `Target` handle is dropped. Each
/// poll counts one cycle.
struct Until(Arc<AtomicBool>, Arc<AtomicU64>);

impl Future for Until {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        self.1.fetch_add(1, Ordering::Relaxed);
        match self.0.load(Ordering::Acquire) {
            true => Poll::Ready(()),
            false => Poll::Pending,
        }
    }
}
