//! Opening real resources, and IO through the host's io steps.
#![cfg(unix)]

use std::io::Write;
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use metor_fsw_3::io::{Access, FdClass, OpenError, Opened, RawAddr, RealKind, open};

fn opened(kind: RealKind) -> (OwnedFd, FdClass) {
    match open(&kind).expect("opens") {
        Opened::Fd { fd, class } => (fd, class),
        Opened::Dial { .. } => panic!("expected a descriptor"),
    }
}

fn pty() -> (OwnedFd, OwnedFd, String) {
    let (mut master, mut slave) = (0, 0);
    let mut name = [0 as libc::c_char; 128];
    let null = std::ptr::null_mut();
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
    let path = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    unsafe {
        (
            OwnedFd::from_raw_fd(master),
            OwnedFd::from_raw_fd(slave),
            path,
        )
    }
}

#[test]
fn tty_opens_raw_at_its_baud() {
    let (_master, _slave, path) = pty();
    let (fd, class) = opened(RealKind::Tty {
        path: path.into(),
        baud: 115200,
    });
    assert_eq!(class, FdClass::Tty);
    let mut t = unsafe { std::mem::zeroed::<libc::termios>() };
    assert_eq!(unsafe { libc::tcgetattr(fd.as_raw_fd(), &mut t) }, 0);
    assert_eq!(unsafe { libc::cfgetospeed(&t) }, libc::B115200);
    assert_eq!(t.c_lflag & libc::ICANON, 0, "raw mode");
}

#[test]
fn sockets_dirs_and_files_open_with_their_class() {
    let dir = tempfile::tempdir().unwrap();
    let cases = [
        (
            RealKind::Udp {
                bind: "127.0.0.1:0".into(),
                connect: None,
            },
            FdClass::Datagram,
        ),
        (
            RealKind::Dir {
                path: dir.path().into(),
            },
            FdClass::Dir,
        ),
        (
            RealKind::File {
                path: dir.path().join("log.bin"),
                access: Access::ReadWrite,
            },
            FdClass::File,
        ),
        (
            RealKind::TcpListen {
                addr: "127.0.0.1:0".into(),
            },
            FdClass::Listener,
        ),
    ];
    for (kind, class) in cases {
        assert_eq!(opened(kind).1, class);
    }
}

#[test]
fn tcp_connect_is_a_dial_target() {
    let Opened::Dial { addr } = open(&RealKind::TcpConnect {
        addr: "127.0.0.1:2240".into(),
    })
    .unwrap() else {
        panic!("expected a dial target");
    };
    let want: SocketAddr = "127.0.0.1:2240".parse().unwrap();
    assert_eq!(addr, RawAddr::from(want));
}

#[test]
fn udp_with_connect_is_connected() {
    let peer = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let to = peer.local_addr().unwrap().to_string();
    let (fd, _) = opened(RealKind::Udp {
        bind: "127.0.0.1:0".into(),
        connect: Some(to),
    });
    let socket = std::net::UdpSocket::from(fd);
    assert_eq!(socket.peer_addr().unwrap(), peer.local_addr().unwrap());
}

#[test]
fn read_only_file_refuses_a_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ro.bin");
    std::fs::write(&path, b"x").unwrap();
    let (fd, _) = opened(RealKind::File {
        path,
        access: Access::Read,
    });
    assert!(std::fs::File::from(fd).write_all(b"y").is_err());
}

#[test]
fn missing_tty_fails() {
    let err = open(&RealKind::Tty {
        path: "/dev/no-such-tty".into(),
        baud: 9600,
    })
    .unwrap_err();
    assert!(matches!(err, OpenError::Io(_)), "{err}");
}

#[test]
fn odd_baud_fails() {
    let (_master, _slave, path) = pty();
    let err = open(&RealKind::Tty {
        path: path.into(),
        baud: 12345,
    })
    .unwrap_err();
    assert!(matches!(err, OpenError::Baud(12345)));
}

#[test]
fn unresolvable_dial_fails_naming_the_address() {
    let addr = "no-such-host.invalid:2240".to_string();
    let err = open(&RealKind::TcpConnect { addr: addr.clone() }).unwrap_err();
    assert!(err.to_string().contains(&addr), "{err}");
}

#[cfg(not(target_os = "linux"))]
#[test]
fn can_is_unsupported_off_linux() {
    let err = open(&RealKind::Can {
        iface: "can0".into(),
        filters: vec![],
    })
    .unwrap_err();
    assert!(matches!(err, OpenError::Unsupported(_)));
}

/// Runs only where a `vcan0` interface exists.
#[cfg(target_os = "linux")]
#[test]
fn can_opens_on_vcan0() {
    if !std::path::Path::new("/sys/class/net/vcan0").exists() {
        println!("skipped: no vcan0");
        return;
    }
    assert_eq!(
        opened(RealKind::Can {
            iface: "vcan0".into(),
            filters: vec![]
        })
        .1,
        FdClass::Can
    );
}

mod through_the_io_steps {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Duration;

    use metor_fsw_3::coordinator::{
        CoordinatorConfig, InputConfig, OutputConfig, SystemConfig, SystemTable,
    };
    use metor_fsw_3::io::{
        Backing, Cap, Grant, IO_COMPLETE_TYPE, IO_CQ, IO_SQ, IO_STATUS_PORT, IO_SUBMIT_TYPE,
        RealKind, Recv, ResourceConfig, Send, Udp,
    };
    use metor_fsw_3::{Input, IoInit, ResourceStatus, Slot, Timestamp, system};

    type Got = Rc<RefCell<Vec<Vec<u8>>>>;

    /// Sends `ping` once, and receives whatever arrives.
    struct Pinger {
        tx: Slot<Send>,
        rx: Slot<Recv>,
        sent: bool,
        got: Got,
    }

    #[system]
    fn pinger(#[state] s: &mut Pinger, udp: &Cap<Udp>) {
        let _ = s.tx.poll();
        if let Some(Ok(_)) = s.rx.poll() {
            s.got.borrow_mut().push(s.rx.bytes().to_vec());
        }
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
        metor_fsw_3::link::register_builtins(&mut table).unwrap();
        let g = got.clone();
        let make = move |io: &IoInit<'_>| Pinger {
            tx: Slot::Idle(io.alloc_from(b"ping")),
            rx: Slot::Idle(io.alloc(64)),
            sent: false,
            got: g.clone(),
        };
        table.register("pinger", pinger(), make).unwrap();
        let st = status.clone();
        table
            .register("status_probe", status_probe(), move || st.clone())
            .unwrap();

        let addr = format!("127.0.0.1:{}", free_port());
        let config = CoordinatorConfig {
            resources: vec![ResourceConfig {
                id: "loop".into(),
                backing: Backing::Real(RealKind::Udp {
                    bind: addr.clone(),
                    connect: Some(addr),
                }),
            }],
            systems: vec![
                SystemConfig {
                    grants: vec![Grant {
                        param: "udp".into(),
                        resource: "loop".into(),
                    }],
                    inputs: vec![InputConfig::new(IO_CQ, "io_complete", "p")],
                    ..SystemConfig::new("p", "pinger")
                },
                SystemConfig {
                    inputs: vec![InputConfig::new("p", "p", IO_SQ)],
                    ..SystemConfig::new("io_submit", IO_SUBMIT_TYPE)
                },
                SystemConfig {
                    outputs: vec![OutputConfig::record("p", "io_cqe")],
                    ..SystemConfig::new("io_complete", IO_COMPLETE_TYPE)
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
        CoordinatorConfig, EndReason, InputConfig, OutputConfig, SystemConfig, SystemTable,
    };
    use metor_fsw_3::io::{
        Backing, Grant, IO_COMPLETE_TYPE, IO_CQ, IO_SQ, IO_SUBMIT_TYPE, RealKind, ResourceConfig,
    };
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
        metor_fsw_3::link::register_builtins(&mut table).unwrap();
        let seen = seen.clone();
        table
            .register("faults", faults(), move || seen.clone())
            .unwrap();
        table.register("gps", gps(), GpsState::new).unwrap();
        table
            .register_sequence("until_fix", until_fix(), UntilFix::new)
            .unwrap();
        let config = CoordinatorConfig {
            resources: vec![ResourceConfig {
                id: "gps1".into(),
                backing: Backing::Real(RealKind::Tty {
                    path: path.into(),
                    baud: 9600,
                }),
            }],
            systems: vec![
                SystemConfig {
                    grants: vec![Grant {
                        param: "uart".into(),
                        resource: "gps1".into(),
                    }],
                    inputs: vec![InputConfig::new(IO_CQ, "io_complete", "gps")],
                    ..SystemConfig::new("gps", "gps")
                },
                SystemConfig {
                    inputs: vec![InputConfig::new("fix", "gps", "fix")],
                    params: serde_json::json!({ "timeout_s": timeout_s }),
                    ..SystemConfig::new("until", "until_fix")
                },
                SystemConfig {
                    inputs: vec![InputConfig::new("gps", "gps", IO_SQ)],
                    ..SystemConfig::new("io_submit", IO_SUBMIT_TYPE)
                },
                SystemConfig {
                    outputs: vec![OutputConfig::record("gps", "io_cqe")],
                    ..SystemConfig::new("io_complete", IO_COMPLETE_TYPE)
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
            if let Some((_, end)) = c.ended().find(|(id, _)| *id == "until") {
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
        assert_eq!(c.latched().count(), 0);
    }

    #[test]
    fn silent_pty_times_out() {
        let (_master, _slave, path) = pty();
        let seen = Faults::default();
        let mut c = build(&path, 0.2, &seen);
        let end = run(&mut c, Duration::from_secs(5));
        assert!(matches!(end, Some(EndReason::Failed(_))), "{end:?}");
        assert!(c.ended().all(|(id, _)| id == "until"), "gps never ends");
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
        assert_eq!(c.latched().count(), 0, "nothing panicked");
        let seen = seen.borrow();
        assert!(!seen.is_empty(), "the driver logs its failed IO");
        assert!(
            seen.iter()
                .all(|kind| kind == "gps_read" || kind == "gps_config"),
            "{seen:?}"
        );
    }
}
