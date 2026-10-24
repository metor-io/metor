//! Connecting to real resources, and IO through the host's io steps.
#![cfg(unix)]

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use metor_fsw_3::io::{
    Backing,
    host::{Access, Baud, Errno, IoError, OpenError, RealKind, resolve},
};

/// A pty pair and the slave's path.
fn pty() -> (OwnedFd, OwnedFd, String) {
    let (mut master, mut slave) = (0, 0);
    let mut name = [0 as libc::c_char; 128];
    let null = std::ptr::null_mut();
    // SAFETY: `name` has room for any pty path.
    let r = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            name.as_mut_ptr(),
            null as _,
            null as _,
        )
    };
    assert_eq!(r, 0);
    // SAFETY: `openpty` wrote a NUL-terminated name.
    let path = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) };
    let path = path.to_string_lossy().into_owned();
    // SAFETY: `openpty` returned two new descriptors.
    unsafe {
        (
            OwnedFd::from_raw_fd(master),
            OwnedFd::from_raw_fd(slave),
            path,
        )
    }
}

#[test]
fn tty_connects_raw_at_its_baud() {
    let (_master, slave, path) = pty();
    let kind = RealKind::Tty {
        path: path.into(),
        baud: Baud::B115200,
    };
    assert_eq!(connects::uart(kind), Ok(()));
    // SAFETY: an all-zero termios is valid, and `tcgetattr` fills it.
    let mut t = unsafe { std::mem::zeroed::<libc::termios>() };
    assert_eq!(unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut t) }, 0);
    assert_eq!(unsafe { libc::cfgetospeed(&t) }, libc::B115200);
    assert_eq!(t.c_lflag & libc::ICANON, 0, "raw mode");
}

#[test]
fn each_kind_connects() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log.bin");
    let udp = RealKind::Udp {
        bind: "127.0.0.1:0".parse().unwrap(),
        connect: Some("127.0.0.1:9".parse().unwrap()),
    };
    assert_eq!(connects::udp(udp), Ok(()));
    let at = dir.path().into();
    assert_eq!(connects::dir(RealKind::Dir { path: at }), Ok(()));
    let file = RealKind::File {
        path: log.clone(),
        access: Access::ReadWrite,
    };
    assert_eq!(connects::file(file), Ok(()));
    assert!(log.exists(), "a writable file is created");
    let listen = RealKind::TcpListen {
        addr: "127.0.0.1:0".parse().unwrap(),
        allow: Vec::new(),
    };
    assert_eq!(connects::listen(listen), Ok(()));
}

#[test]
fn a_read_only_file_that_is_missing_is_not_made() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ro.bin");
    let file = RealKind::File {
        path: path.clone(),
        access: Access::Read,
    };
    assert_eq!(connects::file(file), Err(IoError::Os(Errno::ENOENT)));
    assert!(!path.exists());
}

#[test]
fn a_missing_device_builds_and_fails_its_connect() {
    let missing = RealKind::Tty {
        path: "/dev/no-such-tty".into(),
        baud: Baud::B9600,
    };
    assert_eq!(connects::uart(missing), Err(IoError::Os(Errno::ENOENT)));
    let tty = |baud: u32| serde_json::json!({ "tty": { "path": "/dev/tty", "baud": baud } });
    assert!(serde_json::from_value::<RealKind>(tty(9600)).is_ok());
    assert!(serde_json::from_value::<RealKind>(tty(12_345)).is_err());
}

#[test]
fn a_tty_that_is_not_a_tty_fails_its_configure() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let kind = RealKind::Tty {
        path: file.path().into(),
        baud: Baud::B9600,
    };
    assert_eq!(connects::uart(kind), Err(IoError::Os(Errno::ENOTTY)));
}

#[test]
fn a_taken_address_fails_its_listen() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let kind = RealKind::TcpListen {
        addr: taken.local_addr().unwrap(),
        allow: Vec::new(),
    };
    assert_eq!(connects::listen(kind), Err(IoError::Os(Errno::EADDRINUSE)));
}

#[test]
fn a_path_with_a_nul_fails_the_build() {
    let kind = RealKind::Tty {
        path: "/dev/tty\0S0".into(),
        baud: Baud::B9600,
    };
    let built = connects::build(kind);
    assert!(matches!(
        built,
        Err(metor_fsw_3::coordinator::BuildError::Open { .. })
    ));
}

#[test]
fn a_dial_resolves_its_host_at_build() {
    let ip = resolve("10.0.0.2:2240").expect("an IP literal");
    assert_eq!(ip, "10.0.0.2:2240".parse().unwrap());
    let named = resolve("localhost:2240").expect("a host name");
    assert!(named.ip().is_loopback() && named.port() == 2240, "{named}");
    assert!(
        matches!(resolve("localhost"), Err(OpenError::Io(_))),
        "no port"
    );
}

#[test]
fn unrunnable_backings_do_not_parse() {
    let dial = serde_json::json!({ "real": { "tcp_connect": { "addr": "127.0.0.1:2240" } } });
    let peer = serde_json::json!({ "peer": "uart" });
    assert!(serde_json::from_value::<Backing>(dial).is_err());
    assert!(serde_json::from_value::<Backing>(peer).is_err());
}

#[cfg(not(target_os = "linux"))]
#[test]
fn can_and_gpio_are_unsupported_off_linux() {
    let can = RealKind::Can {
        iface: "can0".parse().unwrap(),
        filters: vec![],
    };
    assert_eq!(connects::can(can), Err(IoError::Unsupported));
}

/// Runs only where a `vcan0` interface exists.
#[cfg(target_os = "linux")]
#[test]
fn can_connects_on_vcan0() {
    if !std::path::Path::new("/sys/class/net/vcan0").exists() {
        return;
    }
    let can = RealKind::Can {
        iface: "vcan0".parse().unwrap(),
        filters: vec![metor_fsw_3::io::CanFilter {
            id: 0x100,
            mask: 0x7FF,
        }],
    };
    assert_eq!(connects::can(can), Ok(()));
}

/// One connect or listen on a resource, through the io steps.
mod connects {
    use std::cell::RefCell;
    use std::rc::Rc;

    use metor_fsw_3::coordinator::{
        BuildError, Coordinator, CoordinatorConfig, SystemConfig, SystemTable,
    };
    use metor_fsw_3::io::{
        Backing, Can, Cap, ConnectOp, Dir, Endpoint, IO_TYPE, IoError, IoOp, ListenOp, Pending,
        RealKind, RegularFile, Uart, Udp,
    };
    use metor_fsw_3::{Timestamp, system};

    type Seen = Rc<RefCell<Option<Result<(), IoError>>>>;

    /// An op in flight, and where its result goes.
    struct Probe<O: IoOp> {
        op: Option<Pending<O>>,
        seen: Seen,
    }

    macro_rules! probe {
        ($name:ident, $system:ident, $kind:ty, $op:ty, $start:expr) => {
            #[system]
            fn $system(#[state] s: &mut Probe<$op>, cap: &Cap<$kind>) {
                let start: fn(&Cap<$kind>) -> Pending<$op> = $start;
                match s.op.as_mut() {
                    None => s.op = Some(start(cap)),
                    Some(op) => {
                        if let Some(result) = op.poll() {
                            *s.seen.borrow_mut() = Some(result.map(drop));
                        }
                    }
                }
            }

            pub fn $name(kind: RealKind) -> Result<(), IoError> {
                let seen = Seen::default();
                let s = seen.clone();
                let mut table = SystemTable::new();
                metor_fsw_3::builtins::register_builtins(&mut table).unwrap();
                let make = move || Probe::<$op> {
                    op: None,
                    seen: s.clone(),
                };
                table.register("probe", $system(), make).unwrap();
                let mut c = config(kind).build(&table).expect("builds");
                run(&mut c, &seen)
            }
        };
    }

    probe!(uart, uart_probe, Uart, ConnectOp<Uart>, |c| c.connect());
    probe!(udp, udp_probe, Udp, ConnectOp<Udp>, |c| c.connect());
    probe!(dir, dir_probe, Dir, ConnectOp<Dir>, |c| c.connect());
    probe!(file, file_probe, RegularFile, ConnectOp<RegularFile>, |c| c
        .connect());
    probe!(can, can_probe, Can, ConnectOp<Can>, |c| c.connect());
    probe!(listen, listen_probe, Endpoint, ListenOp, |c| c.listen());

    /// A target whose `probe` system holds `kind`.
    fn config(kind: RealKind) -> CoordinatorConfig {
        CoordinatorConfig {
            resources: [("r".to_string(), Backing::Real(kind))]
                .into_iter()
                .collect(),
            systems: vec![
                SystemConfig {
                    grants: [("cap".into(), "r".into())].into(),
                    ..SystemConfig::new("probe", "probe")
                },
                SystemConfig::new("io", IO_TYPE),
            ],
            ..CoordinatorConfig::default()
        }
    }

    /// Builds a target holding `kind` with a `uart` probe.
    pub fn build(kind: RealKind) -> Result<Coordinator, BuildError> {
        let mut table = SystemTable::new();
        metor_fsw_3::builtins::register_builtins(&mut table).unwrap();
        let make = || Probe::<ConnectOp<Uart>> {
            op: None,
            seen: Seen::default(),
        };
        table.register("probe", uart_probe(), make).unwrap();
        config(kind).build(&table)
    }

    fn run(c: &mut Coordinator, seen: &Seen) -> Result<(), IoError> {
        for t in 0..1_000 {
            c.step(Timestamp(t));
            if let Some(result) = *seen.borrow() {
                return result;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("the probe's op never landed")
    }
}

mod through_the_io_steps {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Duration;

    use metor_fsw_3::coordinator::{CoordinatorConfig, InputConfig, SystemConfig, SystemTable};
    use metor_fsw_3::io::{
        Backing, Cap, IO_STATUS_PORT, IO_TYPE, RealKind, Reconnect, RecvOp, SendOp, Udp,
    };
    use metor_fsw_3::{Input, IoInit, ResourceStatus, Slot, Timestamp, system};

    type Got = Rc<RefCell<Vec<Vec<u8>>>>;

    /// Connects, sends `ping` once, and receives whatever arrives.
    struct Pinger {
        link: Reconnect<Udp>,
        tx: Slot<SendOp>,
        rx: Slot<RecvOp>,
        sent: bool,
        got: Got,
    }

    #[system]
    fn pinger(#[state] s: &mut Pinger, udp: &Cap<Udp>) {
        s.link.poll(udp);
        let _ = s.tx.poll();
        if let Some(Ok(_)) = s.rx.poll() {
            s.got.borrow_mut().push(s.rx.bytes().to_vec());
        }
        let Some(udp) = s.link.handle() else {
            return;
        };
        if !s.sent
            && let Some(buf) = s.tx.take_idle()
        {
            s.sent = true;
            s.tx = Slot::Busy(udp.send(buf));
        }
        if let Some(buf) = s.rx.take_idle() {
            s.rx = Slot::Busy(udp.recv(buf));
        }
    }

    #[system]
    fn status_probe(
        #[state] seen: &mut Rc<RefCell<Vec<ResourceStatus>>>,
        st: &mut Input<ResourceStatus>,
    ) {
        seen.borrow_mut().extend(st.drain().flatten().copied());
    }

    /// A free local UDP port.
    fn free_port() -> u16 {
        std::net::UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[test]
    fn datagram_to_itself_completes_a_recv() {
        let (got, status) = (
            Got::default(),
            Rc::<RefCell<Vec<ResourceStatus>>>::default(),
        );
        let mut table = SystemTable::new();
        metor_fsw_3::builtins::register_builtins(&mut table).unwrap();
        let g = got.clone();
        let make = move |io: &mut IoInit<'_>| {
            Ok(Pinger {
                link: Reconnect::new(),
                tx: Slot::Idle(io.alloc_from(b"ping")?),
                rx: Slot::Idle(io.alloc(64)?),
                sent: false,
                got: g.clone(),
            })
        };
        table.register("pinger", pinger(), make).unwrap();
        let st = status.clone();
        table
            .register("status_probe", status_probe(), move || st.clone())
            .unwrap();

        let addr: std::net::SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
        let config = CoordinatorConfig {
            resources: [(
                "loop".to_string(),
                Backing::Real(RealKind::Udp {
                    bind: addr,
                    connect: Some(addr),
                }),
            )]
            .into_iter()
            .collect(),
            systems: vec![
                SystemConfig {
                    grants: [("udp".into(), "loop".into())].into(),
                    ..SystemConfig::new("p", "pinger")
                },
                SystemConfig::new("io_submit", IO_TYPE),
                SystemConfig {
                    params: serde_json::json!({ "status": true }),
                    ..SystemConfig::new("io_complete", IO_TYPE)
                },
                SystemConfig {
                    inputs: vec![InputConfig::new("st", "io_complete", IO_STATUS_PORT)],
                    ..SystemConfig::new("probe", "status_probe")
                },
            ],
            ..CoordinatorConfig::default()
        };
        let mut coordinator = config.build(&table).unwrap();
        for t in 0..500 {
            coordinator.step(Timestamp(t));
            if !got.borrow().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(got.borrow().as_slice(), [b"ping".to_vec()]);
        let last = *status.borrow().last().expect("an io_status record");
        assert_eq!(last.resource.to_str().unwrap(), "loop");
        assert_eq!((last.bytes_out, last.bytes_in), (4, 4));
    }
}

#[path = "fixtures/echo-pack/src/gps.rs"]
#[allow(dead_code)]
mod gps;

mod gps_on_a_pty {
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use metor_fsw_3::coordinator::{
        CoordinatorConfig, EndReason, InputConfig, SystemConfig, SystemTable,
    };
    use metor_fsw_3::io::{Backing, Baud, IO_TYPE, RealKind};
    use metor_fsw_3::metor_proto_wkt::LogEvent;
    use metor_fsw_3::{Coordinator, Input, Timestamp, system};
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::gps::{GpsState, PMTK_RATE_10HZ, UntilFix, gps, until_fix};

    type Faults = Rc<RefCell<Vec<String>>>;

    /// Keeps the kind of every fault on its input.
    #[system]
    fn faults(#[state] seen: &mut Faults, log: &mut Input<LogEvent>) {
        for line in log.drain().flatten() {
            let kinds = line.fields.iter().filter(|(k, _)| k == "kind");
            seen.borrow_mut().extend(kinds.map(|(_, v)| v.to_string()));
        }
    }
    use super::pty;

    const GGA: &[u8] = b"$GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*47\r\n";

    fn build(path: &str, timeout_s: f64, seen: &Faults) -> Coordinator {
        let mut table = SystemTable::new();
        metor_fsw_3::builtins::register_builtins(&mut table).unwrap();
        let seen = seen.clone();
        table
            .register("faults", faults(), move || seen.clone())
            .unwrap();
        table.register("gps", gps(), GpsState::new).unwrap();
        table
            .register_sequence("until_fix", until_fix(), UntilFix::new)
            .unwrap();
        let config = CoordinatorConfig {
            resources: [(
                "gps1".to_string(),
                Backing::Real(RealKind::Tty {
                    path: path.into(),
                    baud: Baud::B9600,
                }),
            )]
            .into_iter()
            .collect(),
            systems: vec![
                SystemConfig {
                    grants: [("uart".into(), "gps1".into())].into(),
                    ..SystemConfig::new("gps", "gps")
                },
                SystemConfig {
                    inputs: vec![InputConfig::new("fix", "gps", "fix")],
                    params: serde_json::json!({ "timeout_s": timeout_s }),
                    ..SystemConfig::new("until", "until_fix")
                },
                SystemConfig::new("io_submit", IO_TYPE),
                SystemConfig {
                    params: serde_json::json!({ "status": true }),
                    ..SystemConfig::new("io_complete", IO_TYPE)
                },
                SystemConfig {
                    inputs: vec![InputConfig::new("log", "gps", "log")],
                    ..SystemConfig::new("faults", "faults")
                },
            ],
            ..CoordinatorConfig::default()
        };
        config.build(&table).unwrap()
    }

    /// Steps on cycle time from the wall clock until `until` ends, or for at
    /// most `limit`. Returns how it ended.
    fn run(c: &mut Coordinator, limit: Duration) -> Option<EndReason> {
        let start = Instant::now();
        while start.elapsed() < limit {
            c.step(Timestamp(start.elapsed().as_micros() as i64));
            if let Some((_, Some(end))) = c.systems().find(|(id, _)| *id == "until") {
                return Some(end.clone());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        None
    }

    /// Writes a GGA sentence to the master every 10 ms, and keeps what the
    /// driver writes, until `stop` is set.
    fn receiver(master: OwnedFd, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<Vec<u8>> {
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
        std::thread::spawn(move || {
            let file = std::fs::File::from(master);
            let mut got = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                let _ = (&file).write_all(GGA);
                drain(&file, &mut got);
                std::thread::sleep(Duration::from_millis(10));
            }
            std::thread::sleep(Duration::from_millis(20));
            drain(&file, &mut got);
            got
        })
    }

    /// Reads what is waiting on `file` into `got`.
    fn drain(mut file: &std::fs::File, got: &mut Vec<u8>) {
        let mut buf = [0u8; 256];
        while let Ok(n @ 1..) = file.read(&mut buf) {
            got.extend_from_slice(&buf[..n]);
        }
    }

    fn count(haystack: &[u8], needle: &[u8]) -> usize {
        haystack
            .windows(needle.len())
            .filter(|w| *w == needle)
            .count()
    }

    #[test]
    fn gps_gets_a_fix_through_the_io_steps() {
        let (master, _slave, path) = pty();
        let mut c = build(&path, 10.0, &Faults::default());
        let stop = Arc::new(AtomicBool::new(false));
        let thread = receiver(master, stop.clone());
        let end = run(&mut c, Duration::from_secs(10));
        stop.store(true, Ordering::Relaxed);
        let written = thread.join().unwrap();
        assert_eq!(end, Some(EndReason::Completed));
        assert_eq!(
            count(&written, PMTK_RATE_10HZ),
            1,
            "configured exactly once"
        );
        assert!(
            c.systems()
                .all(|(_, end)| end != Some(&EndReason::Panicked))
        );
    }

    #[test]
    fn silent_pty_times_out() {
        let (_master, _slave, path) = pty();
        let seen = Faults::default();
        let mut c = build(&path, 0.2, &seen);
        let end = run(&mut c, Duration::from_secs(5));
        assert!(matches!(end, Some(EndReason::Failed(_))), "{end:?}");
        assert!(
            c.systems().all(|(id, end)| end.is_none() || id == "until"),
            "gps never ends"
        );
        assert!(
            seen.borrow().is_empty(),
            "gps never faults: {:?}",
            seen.borrow()
        );
    }

    #[test]
    fn closed_master_never_panics() {
        let (master, slave, path) = pty();
        let seen = Faults::default();
        let mut c = build(&path, 0.2, &seen);
        drop((master, slave));
        let end = run(&mut c, Duration::from_secs(5));
        assert!(matches!(end, Some(EndReason::Failed(_))), "{end:?}");
        assert!(
            c.systems()
                .all(|(_, end)| end != Some(&EndReason::Panicked)),
            "nothing panicked"
        );
        let seen = seen.borrow();
        assert!(!seen.is_empty(), "the driver logs its failed IO");
        let io = ["gps_read", "gps_config", "gps_connect"];
        assert!(
            seen.iter().all(|kind| io.contains(&kind.as_str())),
            "{seen:?}"
        );
    }
}
