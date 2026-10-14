//! Opening real resources, and IO through the host's io steps.
#![cfg(unix)]

use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use metor_fsw_3::io::host::{Access, Class, OpenError, RealKind, open};

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
fn tty_opens_raw_at_its_baud() {
    let (_master, _slave, path) = pty();
    let (fd, class) = open(&RealKind::Tty {
        path: path.into(),
        baud: 115_200,
    })
    .unwrap();
    assert_eq!(class, Class::Tty);
    // SAFETY: an all-zero termios is valid, and `tcgetattr` fills it.
    let mut t = unsafe { std::mem::zeroed::<libc::termios>() };
    assert_eq!(unsafe { libc::tcgetattr(fd.as_raw_fd(), &mut t) }, 0);
    assert_eq!(unsafe { libc::cfgetospeed(&t) }, libc::B115200);
    assert_eq!(t.c_lflag & libc::ICANON, 0, "raw mode");
}

#[test]
fn each_kind_opens_with_its_class() {
    let dir = tempfile::tempdir().unwrap();
    let cases = [
        (
            RealKind::Udp {
                bind: "127.0.0.1:0".into(),
                connect: None,
            },
            Class::Datagram,
        ),
        (
            RealKind::Dir {
                path: dir.path().into(),
            },
            Class::Dir,
        ),
        (
            RealKind::File {
                path: dir.path().join("log.bin"),
                access: Access::ReadWrite,
            },
            Class::File,
        ),
        (
            RealKind::TcpListen {
                addr: "127.0.0.1:0".into(),
            },
            Class::Listener,
        ),
    ];
    for (kind, class) in cases {
        assert_eq!(open(&kind).unwrap().1, class, "{kind:?}");
    }
}

#[test]
fn udp_with_connect_is_connected() {
    let peer = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let to = peer.local_addr().unwrap().to_string();
    let (fd, _) = open(&RealKind::Udp {
        bind: "127.0.0.1:0".into(),
        connect: Some(to),
    })
    .unwrap();
    let socket = std::net::UdpSocket::from(fd);
    assert_eq!(socket.peer_addr().unwrap(), peer.local_addr().unwrap());
}

#[test]
fn read_only_file_refuses_a_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ro.bin");
    std::fs::write(&path, b"x").unwrap();
    let (fd, _) = open(&RealKind::File {
        path,
        access: Access::Read,
    })
    .unwrap();
    assert!(std::fs::File::from(fd).write_all(b"y").is_err());
}

#[test]
fn missing_tty_and_odd_baud_fail() {
    let missing = open(&RealKind::Tty {
        path: "/dev/no-such-tty".into(),
        baud: 9600,
    });
    assert!(matches!(missing, Err(OpenError::Io(_))));
    let (_master, _slave, path) = pty();
    let odd = open(&RealKind::Tty {
        path: path.into(),
        baud: 12_345,
    });
    assert!(matches!(odd, Err(OpenError::Baud(12_345))));
}

#[test]
fn tcp_connect_is_unsupported_until_links() {
    let err = open(&RealKind::TcpConnect {
        addr: "127.0.0.1:2240".into(),
    })
    .unwrap_err();
    assert!(matches!(err, OpenError::Unsupported("tcp_connect")));
}

#[cfg(not(target_os = "linux"))]
#[test]
fn can_and_gpio_are_unsupported_off_linux() {
    let can = open(&RealKind::Can {
        iface: "can0".into(),
        filters: vec![],
    });
    assert!(matches!(can, Err(OpenError::Unsupported(_))));
}

/// Runs only where a `vcan0` interface exists.
#[cfg(target_os = "linux")]
#[test]
fn can_opens_on_vcan0() {
    if !std::path::Path::new("/sys/class/net/vcan0").exists() {
        return;
    }
    let (_, class) = open(&RealKind::Can {
        iface: "vcan0".into(),
        filters: vec![],
    })
    .unwrap();
    assert_eq!(class, Class::Datagram);
}

mod through_the_io_steps {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Duration;

    use metor_fsw_3::coordinator::{
        CoordinatorConfig, InputConfig, OutputConfig, SystemConfig, SystemTable,
    };
    use metor_fsw_3::io::{
        Backing, Cap, Grant, IO_CQ, IO_SQ, IO_STATUS_PORT, IO_TYPE, RealKind, RecvOp,
        ResourceConfig, SendOp, Udp,
    };
    use metor_fsw_3::{Input, IoInit, ResourceStatus, Slot, Timestamp, system};

    type Got = Rc<RefCell<Vec<Vec<u8>>>>;

    /// Sends `ping` once, and receives whatever arrives.
    struct Pinger {
        tx: Slot<SendOp>,
        rx: Slot<RecvOp>,
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
        metor_fsw_3::builtins::register_builtins(&mut table).unwrap();
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
                        end: Default::default(),
                    }],
                    inputs: vec![InputConfig::new(IO_CQ, "io_complete", "p")],
                    ..SystemConfig::new("p", "pinger")
                },
                SystemConfig {
                    inputs: vec![InputConfig::new("p", "p", IO_SQ)],
                    ..SystemConfig::new("io_submit", IO_TYPE)
                },
                SystemConfig {
                    outputs: vec![OutputConfig::with_record("p", "io_cqe")],
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
