//! Test cases every `Os` backend must pass, run through a `Reactor`.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::time::Duration;

use metor_fsw_3::io::*;

pub type Backend = fn(&Limits, Tables, &[RawAddr]) -> Result<Box<dyn Os>, OsError>;

const LIMITS: Limits = Limits {
    ops: 32,
    sources: 1,
};
pub const SRC: SourceId = SourceId(0);

/// A reactor with one source whose table holds `bufs`. An empty entry
/// allocates a 256-byte buffer.
pub struct Rig {
    pub reactor: Reactor,
    pub table: Rc<BufTable>,
    pub handles: Vec<RawHandle>,
    done: Vec<(SourceId, Cqe)>,
    next: u32,
}

impl Rig {
    pub fn new(
        backend: Backend,
        dials: &[SocketAddr],
        fds: Vec<(OwnedFd, FdClass)>,
        bufs: &[&[u8]],
    ) -> Self {
        Self::with_limits(backend, LIMITS, dials, fds, bufs)
    }

    pub fn with_limits(
        backend: Backend,
        limits: Limits,
        dials: &[SocketAddr],
        fds: Vec<(OwnedFd, FdClass)>,
        bufs: &[&[u8]],
    ) -> Self {
        let tables = Tables::new();
        let table = Rc::new(BufTable::new(16));
        tables.add(&table);
        for b in bufs {
            match b {
                [] => table.alloc(256).unwrap(),
                b => table.alloc_from(b).unwrap(),
            };
        }
        table.freeze();
        let dials: Vec<RawAddr> = dials.iter().map(|a| RawAddr::from(*a)).collect();
        let mut os = backend(&limits, tables, &dials).unwrap();
        let handles = fds.into_iter().map(|(fd, c)| os.register(fd, c)).collect();
        let mut reactor = Reactor::new(os, limits);
        reactor.source(table.clone());
        Self {
            reactor,
            table,
            handles,
            done: Vec::new(),
            next: 0,
        }
    }

    /// Submits an op and returns its token.
    pub fn start(&mut self, handle: RawHandle, op: Op) -> Token {
        self.next += 1;
        let token = Token::new(self.next);
        self.reactor.push(SRC, Sqe::new(token, handle, op));
        self.reactor.submit();
        token
    }

    /// Reaps until `token` completes.
    pub fn wait(&mut self, token: Token) -> Cqe {
        for _ in 0..2000 {
            if let Some(c) = self.poll(token) {
                return c;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("op {token:?} never completed");
    }

    /// Reaps once and returns `token`'s completion if it finished.
    pub fn poll(&mut self, token: Token) -> Option<Cqe> {
        let mut fresh = Vec::new();
        self.reactor.reap(&mut fresh);
        self.done.extend(fresh);
        let at = self.done.iter().position(|(_, c)| c.token == token)?;
        Some(self.done.remove(at).1)
    }

    pub fn run(&mut self, handle: RawHandle, op: Op) -> Cqe {
        let token = self.start(handle, op);
        self.wait(token)
    }

    pub fn bytes(&self, slot: u16) -> Vec<u8> {
        unsafe { self.table.filled(slot) }.to_vec()
    }
}

pub fn ok(c: Cqe) -> Out {
    c.status.result().unwrap();
    c.out
}

pub fn err(c: Cqe) -> IoError {
    c.status.result().unwrap_err()
}

pub fn owned<T: Into<OwnedFd>>(t: T) -> OwnedFd {
    t.into()
}

fn pty() -> (OwnedFd, OwnedFd) {
    let (mut master, mut slave) = (0, 0);
    let null = std::ptr::null_mut();
    let r = unsafe { libc::openpty(&mut master, &mut slave, null, null as _, null as _) };
    assert_eq!(r, 0);
    unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
}

fn local_udp() -> UdpSocket {
    UdpSocket::bind("127.0.0.1:0").unwrap()
}

pub fn datagram_send_and_recv(b: Backend) {
    let (a, z) = (local_udp(), local_udp());
    let to = RawAddr::from(z.local_addr().unwrap());
    let fds = vec![(owned(a), FdClass::Datagram), (owned(z), FdClass::Datagram)];
    let mut rig = Rig::new(b, &[], fds, &[b"ping", b""]);
    let (ha, hz) = (rig.handles[0], rig.handles[1]);
    let rx = rig.start(hz, Op::recv(BufRef::new(1)));
    assert_eq!(
        ok(rig.run(ha, Op::send_to(BufRef::new(0), to))).as_len(),
        Some(4)
    );
    assert_eq!(ok(rig.wait(rx)).as_len(), Some(4));
    assert_eq!(rig.bytes(1), b"ping");
}

pub fn file_write_read_at_offsets(b: Backend) {
    let file = tempfile::tempfile().unwrap();
    let mut rig = Rig::new(b, &[], vec![(owned(file), FdClass::File)], &[b"abc", b""]);
    let h = rig.handles[0];
    assert_eq!(
        ok(rig.run(h, Op::write(BufRef::new(0), Some(4)))).as_len(),
        Some(3)
    );
    assert_eq!(
        ok(rig.run(h, Op::read(BufRef::new(1), Some(4)))).as_len(),
        Some(3)
    );
    assert_eq!(rig.bytes(1), b"abc");
    assert_eq!(
        ok(rig.run(h, Op::read(BufRef::new(1), Some(0)))).as_len(),
        Some(7)
    );
    assert_eq!(rig.bytes(1), b"\0\0\0\0abc");
    ok(rig.run(h, Op::FSYNC));
}

pub fn tty_configure(b: Backend) {
    let (_master, slave) = pty();
    let probe = slave.try_clone().unwrap();
    let mut rig = Rig::new(b, &[], vec![(slave, FdClass::Tty)], &[]);
    let cfg = TtyConfig::new(9600);
    ok(rig.run(rig.handles[0], Op::tty(TtyOp::Configure(cfg))));
    let mut t = unsafe { std::mem::zeroed::<libc::termios>() };
    assert_eq!(unsafe { libc::tcgetattr(probe.as_raw_fd(), &mut t) }, 0);
    assert_eq!(unsafe { libc::cfgetospeed(&t) }, libc::B9600);
    ok(rig.run(rig.handles[0], Op::tty(TtyOp::flush(2))));
}

pub fn resolve_on_dial(b: Backend) {
    let addr: SocketAddr = "10.1.2.3:4500".parse().unwrap();
    let mut rig = Rig::new(b, &[addr], vec![], &[]);
    let out = ok(rig.run(RawHandle::new(0, 0), Op::RESOLVE));
    assert_eq!(out.as_addr(), Some(RawAddr::from(addr)));
}

pub fn connect_and_send(b: Backend) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let mut rig = Rig::new(b, &[addr], vec![], &[b"hello"]);
    let dial = RawHandle::new(0, 0);
    let stream = ok(rig.run(dial, Op::connect(addr.into())))
        .as_handle()
        .unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    assert_eq!(
        ok(rig.run(stream, Op::send(BufRef::new(0)))).as_len(),
        Some(5)
    );
    let mut got = [0; 5];
    peer.read_exact(&mut got).unwrap();
    assert_eq!(&got, b"hello");
    ok(rig.run(stream, Op::CLOSE));
}

pub fn recv_parks_until_data(b: Backend) {
    let (ours, mut theirs) = UnixStream::pair().unwrap();
    let mut rig = Rig::new(b, &[], vec![(owned(ours), FdClass::Stream)], &[b""]);
    let rx = rig.start(rig.handles[0], Op::recv(BufRef::new(0)));
    for _ in 0..5 {
        assert!(rig.poll(rx).is_none());
    }
    theirs.write_all(b"late").unwrap();
    assert_eq!(ok(rig.wait(rx)).as_len(), Some(4));
    assert_eq!(rig.bytes(0), b"late");
}

pub fn two_parked_ops_keep_both_interests(b: Backend) {
    let (ours, mut theirs) = UnixStream::pair().unwrap();
    ours.set_nonblocking(true).unwrap();
    let fill = vec![0u8; 4096];
    while (&ours).write(&fill).is_ok() {}
    let big = vec![1u8; 200];
    let mut rig = Rig::new(b, &[], vec![(owned(ours), FdClass::Stream)], &[b"", &big]);
    let h = rig.handles[0];
    let rx = rig.start(h, Op::recv(BufRef::new(0)));
    let tx = rig.start(h, Op::send(BufRef::new(1)));
    assert!(rig.poll(rx).is_none() && rig.poll(tx).is_none());
    theirs.write_all(b"x").unwrap();
    assert_eq!(ok(rig.wait(rx)).as_len(), Some(1));
    theirs.set_nonblocking(true).unwrap();
    let mut sink = vec![0u8; 1 << 16];
    let mut sent = None;
    for _ in 0..2000 {
        let _ = theirs.read(&mut sink);
        sent = rig.poll(tx);
        if sent.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(ok(sent.expect("send never completed")).as_len().unwrap() > 0);
}

pub fn two_connects_give_two_streams(b: Backend) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let mut rig = Rig::new(b, &[addr], vec![], &[]);
    let dial = RawHandle::new(0, 0);
    let s1 = ok(rig.run(dial, Op::connect(addr.into())))
        .as_handle()
        .unwrap();
    let s2 = ok(rig.run(dial, Op::connect(addr.into())))
        .as_handle()
        .unwrap();
    assert_ne!(s1, s2);
    listener.accept().unwrap();
    listener.accept().unwrap();
}

pub fn cancel_parked_read(b: Backend) {
    let (ours, _theirs) = UnixStream::pair().unwrap();
    let mut rig = Rig::new(b, &[], vec![(owned(ours), FdClass::Stream)], &[b""]);
    let h = rig.handles[0];
    let rx = rig.start(h, Op::recv(BufRef::new(0)));
    assert!(rig.poll(rx).is_none());
    let cancel = rig.start(h, Op::cancel(rx));
    assert_eq!(err(rig.wait(rx)), IoError::Canceled);
    assert_eq!(ok(rig.wait(cancel)), Out::UNIT);
    assert_eq!(rig.reactor.in_flight(SRC), 0);
}

pub fn close_runs_after_queued_write(b: Backend) {
    let file = tempfile::tempfile().unwrap();
    let mut rig = Rig::new(b, &[], vec![(owned(file), FdClass::File)], &[b"abc"]);
    let h = rig.handles[0];
    let (w, c) = (Token::new(100), Token::new(101));
    rig.reactor
        .push(SRC, Sqe::new(w, h, Op::write(BufRef::new(0), Some(0))));
    rig.reactor.push(SRC, Sqe::new(c, h, Op::CLOSE));
    rig.reactor.submit();
    assert_eq!(ok(rig.wait(w)).as_len(), Some(3));
    assert_eq!(ok(rig.wait(c)), Out::UNIT);
}

pub fn unissued_handle_is_stale(b: Backend) {
    let mut rig = Rig::new(b, &[], vec![], &[b""]);
    let c = rig.run(RawHandle::new(5, 0), Op::read(BufRef::new(0), None));
    assert_eq!(err(c), IoError::Stale);
}

pub fn closed_handle_is_stale(b: Backend) {
    let (ours, _theirs) = UnixStream::pair().unwrap();
    let mut rig = Rig::new(b, &[], vec![(owned(ours), FdClass::Stream)], &[b""]);
    let h = rig.handles[0];
    ok(rig.run(h, Op::CLOSE));
    assert_eq!(err(rig.run(h, Op::recv(BufRef::new(0)))), IoError::Stale);
}

pub fn connect_refused(b: Backend) {
    let addr = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let mut rig = Rig::new(b, &[addr], vec![], &[]);
    let c = rig.run(RawHandle::new(0, 0), Op::connect(addr.into()));
    assert_eq!(err(c), IoError::Os(Errno::ECONNREFUSED));
}

pub fn read_on_dial_unsupported(b: Backend) {
    let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
    let mut rig = Rig::new(b, &[addr], vec![], &[b""]);
    let c = rig.run(RawHandle::new(0, 0), Op::read(BufRef::new(0), None));
    assert_eq!(err(c), IoError::Unsupported);
}

pub fn resolve_on_stream_unsupported(b: Backend) {
    let (ours, _theirs) = UnixStream::pair().unwrap();
    let mut rig = Rig::new(b, &[], vec![(owned(ours), FdClass::Stream)], &[]);
    assert_eq!(
        err(rig.run(rig.handles[0], Op::RESOLVE)),
        IoError::Unsupported
    );
}

pub fn gpio_on_pty_fails(b: Backend) {
    let (_master, slave) = pty();
    let mut rig = Rig::new(b, &[], vec![(slave, FdClass::Gpio)], &[]);
    let e = err(rig.run(rig.handles[0], Op::gpio(GpioOp::GET)));
    if cfg!(target_os = "linux") {
        assert_eq!(e, IoError::Os(Errno::ENOTTY));
    } else {
        assert_eq!(e, IoError::Unsupported);
    }
}

pub type Case = (&'static str, fn(Backend));

/// Every case with its name.
pub const ALL: [Case; 16] = [
    (
        "close_runs_after_queued_write",
        close_runs_after_queued_write,
    ),
    ("datagram_send_and_recv", datagram_send_and_recv),
    ("file_write_read_at_offsets", file_write_read_at_offsets),
    ("tty_configure", tty_configure),
    ("resolve_on_dial", resolve_on_dial),
    ("connect_and_send", connect_and_send),
    ("recv_parks_until_data", recv_parks_until_data),
    (
        "two_parked_ops_keep_both_interests",
        two_parked_ops_keep_both_interests,
    ),
    (
        "two_connects_give_two_streams",
        two_connects_give_two_streams,
    ),
    ("cancel_parked_read", cancel_parked_read),
    ("unissued_handle_is_stale", unissued_handle_is_stale),
    ("closed_handle_is_stale", closed_handle_is_stale),
    ("connect_refused", connect_refused),
    ("read_on_dial_unsupported", read_on_dial_unsupported),
    (
        "resolve_on_stream_unsupported",
        resolve_on_stream_unsupported,
    ),
    ("gpio_on_pty_fails", gpio_on_pty_fails),
];

/// Runs every case on `backend`, printing each name first.
pub fn run_all(backend: Backend) {
    for (name, case) in ALL {
        println!("case {name}");
        case(backend);
    }
}
