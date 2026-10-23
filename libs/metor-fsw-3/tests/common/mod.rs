//! Records, fixtures, and wait helpers shared by the integration tests.

#![allow(dead_code)]

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::fmt::Display;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use metor_fsw_3::cli::cargo::{cargo_build, wasm_build};
use metor_fsw_3::coordinator::{
    Coordinator, CoordinatorConfig, EndReason, SystemConfig, SystemTable,
};
use metor_fsw_3::io::{Backing, IO_TYPE, RealKind};
use metor_fsw_3::wasm::{Limits, WasmPack};
use metor_fsw_3::{Frame, FrameImage, Mono16, Output, Record, Timestamp, system};
use metor_proto_stellar::{PacketSink, PacketStream, Peer, identify};
use metor_proto_wkt::LinkInfo;
use serde::{Deserialize, Serialize};
use stellarator::io::{OwnedReader, OwnedWriter};
use stellarator::net::TcpStream;
use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout};

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

/// A camera's 1024² 12-bit image.
pub type Pixels = FrameImage<Mono16<12>, 1024, 1024>;

/// One camera frame.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[frame(name = "image_frame")]
#[repr(C)]
pub struct ImageFrame {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    pub frame_id: u64,
    pub exposure_us: u64,
    pub pixels: Pixels,
}

/// The pixels of frame `frame_id`: pixel `i` is `(frame_id + i) & 0xfff`.
pub fn pattern(frame_id: u64) -> &'static [u16] {
    static RAMP: OnceLock<Vec<u16>> = OnceLock::new();
    let ramp = RAMP.get_or_init(|| {
        (0..1024 * 1024 + 4096)
            .map(|j| (j & 0xfff) as u16)
            .collect()
    });
    let start = (frame_id & 0xfff) as usize;
    &ramp[start..start + 1024 * 1024]
}

/// What a [`Camera`] shares with its test.
#[derive(Default)]
pub struct Shots {
    /// The camera writes only while this is set.
    pub on: AtomicBool,
    /// Frames written to the ring.
    pub written: AtomicU64,
    /// Frames the ring refused.
    pub refused: AtomicU64,
}

/// Writes an [`ImageFrame`] every `period_us` from a frame allocated at make.
pub struct Camera {
    frame: Box<ImageFrame>,
    period_us: i64,
    next: Option<i64>,
    shots: Arc<Shots>,
}

impl Camera {
    pub fn new(rate_hz: f64, shots: Arc<Shots>) -> Self {
        Self {
            // PANIC Safety: test setup; a failed 2 MiB allocation fails the test.
            frame: ImageFrame::new_box_zeroed().expect("a frame"),
            period_us: (1e6 / rate_hz) as i64,
            next: None,
            shots,
        }
    }
}

#[system]
pub fn camera(#[state] s: &mut Camera, now: Timestamp, image: &mut Output<ImageFrame>) {
    if !s.shots.on.load(Ordering::Acquire) {
        s.next = None;
        return;
    }
    let due = *s.next.get_or_insert(now.0);
    if now.0 < due {
        return;
    }
    s.next = Some(due + s.period_us);
    let frame = &mut *s.frame;
    frame.frame_id += 1;
    frame.timestamp = now;
    frame.exposure_us = 1000 + frame.frame_id;
    frame
        .pixels
        .pixels_mut()
        .copy_from_slice(pattern(frame.frame_id));
    let count = match image.write(frame) {
        Ok(()) => &s.shots.written,
        Err(_) => &s.shots.refused,
    };
    count.fetch_add(1, Ordering::Relaxed);
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

/// Builds the fixture as a wasm module once per test binary and returns its
/// bytes.
pub fn fixture_wasm() -> &'static [u8] {
    static BUILT: OnceLock<Vec<u8>> = OnceLock::new();
    BUILT.get_or_init(|| {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let path = wasm_build("echo-pack", root).expect("the module builds");
        std::fs::read(&path).expect("the module was written")
    })
}

/// Opens the fixture's wasm module with the default limits.
pub fn open_wasm() -> WasmPack {
    WasmPack::open(fixture_wasm(), &Limits::default()).expect("the module opens")
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

/// The ports [`free_ports`] hands out.
///
/// They sit below the ephemeral ranges of Linux (32768 and up) and macOS
/// (49152 and up), so the OS never gives one to a `bind(0)` or an outgoing
/// connection.
const TEST_PORTS: core::ops::Range<u16> = 20000..32000;

/// How far into [`TEST_PORTS`] this process has handed out ports.
static NEXT_PORT: AtomicU32 = AtomicU32::new(0);

/// Returns `N` ports that nothing listened on when this was called.
///
/// Each call in a process gets different ports, and each process starts at a
/// different place in [`TEST_PORTS`]. A port is probed by dialing it, not by
/// binding it: a socket bound here can leak into a child spawned by another
/// test at the same moment, and the child then holds the port.
pub fn free_ports<const N: usize>() -> [u16; N] {
    let span = u32::from(TEST_PORTS.end - TEST_PORTS.start);
    let start = std::process::id().wrapping_mul(997);
    let ports: Vec<u16> = (0..span)
        .map(|_| start.wrapping_add(NEXT_PORT.fetch_add(1, Ordering::Relaxed)) % span)
        .filter_map(|offset| u16::try_from(offset).ok())
        .map(|offset| TEST_PORTS.start + offset)
        .filter(|&port| std::net::TcpStream::connect(local(port)).is_err())
        .take(N)
        .collect();
    ports.try_into().expect("enough free test ports")
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

/// Moves each system's `ep` param, a real resource, into the resource
/// `{id}_ep` granted to its `ep`, and marks a dialer `dialing`. Adds the io
/// steps if any system has a resource.
pub fn lower_links(config: &mut CoordinatorConfig) {
    for system in &mut config.systems {
        let Some(ep) = system.params.as_object_mut().and_then(|p| p.remove("ep")) else {
            continue;
        };
        let kind: RealKind = serde_json::from_value(ep).expect("a real resource");
        if matches!(kind, RealKind::TcpDial { .. }) {
            system.params["dialing"] = true.into();
        }
        let ep = format!("{}_ep", system.id);
        system.grants.insert("ep".into(), ep.clone());
        config.resources.insert(ep, Backing::Real(kind));
    }
    if config.resources.is_empty() {
        return;
    }
    let status = serde_json::json!({ "status": true });
    config.systems.extend([
        SystemConfig::new("io_submit", IO_TYPE),
        SystemConfig {
            params: status,
            ..SystemConfig::new("io_complete", IO_TYPE)
        },
    ]);
}

/// The ids of `c`'s systems that panicked, in step order.
pub fn panicked(c: &Coordinator) -> Vec<&str> {
    c.systems()
        .filter(|(_, end)| *end == Some(&EndReason::Panicked))
        .map(|(id, _)| id)
        .collect()
}
