//! Cases every `Os` backend must pass, with real descriptors.
#![allow(dead_code)]

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use metor_fsw_3::io::host::*;

pub type Backend = fn(usize) -> std::io::Result<Box<dyn Os>>;

/// A backend and the buffers its ops use.
pub struct Rig {
    pub os: Box<dyn Os>,
    bufs: Vec<Box<[u8]>>,
    done: Vec<OsDone>,
    next: u16,
}

impl Rig {
    pub fn new(backend: Backend) -> Self {
        Self {
            os: backend(32).unwrap(),
            bufs: Vec::new(),
            done: Vec::new(),
            next: 0,
        }
    }

    /// A buffer holding `bytes`, or `len` zeroes for an empty `bytes`.
    pub fn buf(&mut self, bytes: &[u8], len: usize) -> usize {
        let mut buf = vec![0; len.max(bytes.len())].into_boxed_slice();
        buf[..bytes.len()].copy_from_slice(bytes);
        self.bufs.push(buf);
        self.bufs.len() - 1
    }

    pub fn bytes(&self, buf: usize, n: usize) -> &[u8] {
        &self.bufs[buf][..n]
    }

    pub fn start(&mut self, fd: RawFd, class: Class, op: Op, buf: Option<usize>) -> OpId {
        let id = OpId::new(self.next, 1);
        self.next += 1;
        let buf = buf.map(|b| NonNull::from(&mut self.bufs[b][..]));
        self.os.push(OsOp {
            id,
            fd,
            class,
            op,
            buf,
        });
        self.os.submit();
        id
    }

    /// One submit and reap round.
    pub fn poll(&mut self, id: OpId) -> Option<Result<Out, IoError>> {
        self.os.submit();
        self.os.reap(&mut self.done);
        let at = self.done.iter().position(|d| d.id == id)?;
        Some(self.done.remove(at).result)
    }

    pub fn wait(&mut self, id: OpId) -> Result<Out, IoError> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(result) = self.poll(id) {
                return result;
            }
            assert!(Instant::now() < deadline, "op {id:?} never completed");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    pub fn run(
        &mut self,
        fd: RawFd,
        class: Class,
        op: Op,
        buf: Option<usize>,
    ) -> Result<Out, IoError> {
        let id = self.start(fd, class, op, buf);
        self.wait(id)
    }
}

pub fn len(r: Result<Out, IoError>) -> usize {
    match r {
        Ok(Out::Len(n)) => n as usize,
        other => panic!("{other:?}"),
    }
}

fn opened(r: Result<Out, IoError>) -> (OwnedFd, Class) {
    match r {
        Ok(Out::Opened(fd, class)) => (fd, class),
        other => panic!("{other:?}"),
    }
}

pub fn pty() -> (OwnedFd, OwnedFd) {
    let (mut master, mut slave) = (0, 0);
    let null = std::ptr::null_mut();
    // SAFETY: plain `openpty` with no name, termios, or winsize.
    let r = unsafe { libc::openpty(&mut master, &mut slave, null, null as _, null as _) };
    assert_eq!(r, 0);
    // SAFETY: `openpty` returned two new descriptors.
    unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
}

fn pair() -> (UnixStream, UnixStream) {
    let (a, z) = UnixStream::pair().unwrap();
    a.set_nonblocking(true).unwrap();
    (a, z)
}

pub fn datagram_send_and_recv(b: Backend) {
    let mut rig = Rig::new(b);
    let (a, z) = (
        UdpSocket::bind("127.0.0.1:0").unwrap(),
        UdpSocket::bind("127.0.0.1:0").unwrap(),
    );
    a.set_nonblocking(true).unwrap();
    z.set_nonblocking(true).unwrap();
    let (ping, rx) = (rig.buf(b"ping", 0), rig.buf(b"", 16));
    let to = z.local_addr().unwrap();
    assert_eq!(
        len(rig.run(a.as_raw_fd(), Class::Datagram, Op::SendTo(to), Some(ping))),
        4
    );
    assert_eq!(
        len(rig.run(z.as_raw_fd(), Class::Datagram, Op::Recv, Some(rx))),
        4
    );
    assert_eq!(rig.bytes(rx, 4), b"ping");
}

pub fn file_write_read_at_offsets(b: Backend) {
    let mut rig = Rig::new(b);
    let file = tempfile::tempfile().unwrap();
    let fd = file.as_raw_fd();
    let (hello, world, rx) = (rig.buf(b"hello", 0), rig.buf(b"world", 0), rig.buf(b"", 4));
    rig.run(fd, Class::File, Op::Write { offset: Some(0) }, Some(hello))
        .unwrap();
    rig.run(fd, Class::File, Op::Write { offset: Some(5) }, Some(world))
        .unwrap();
    assert_eq!(
        len(rig.run(fd, Class::File, Op::Read { offset: Some(3) }, Some(rx))),
        4
    );
    assert_eq!(rig.bytes(rx, 4), b"lowo");
    rig.run(fd, Class::File, Op::Fsync, None).unwrap();
}

pub fn tty_configure_and_gpio_on_a_tty(b: Backend) {
    let mut rig = Rig::new(b);
    let (_master, slave) = pty();
    let fd = slave.as_raw_fd();
    let cfg = TtyConfig::new(Baud::B115200);
    rig.run(fd, Class::Tty, Op::TtyConfigure(cfg), None)
        .unwrap();
    rig.run(fd, Class::Tty, Op::TtyFlush(TtyQueue::Both), None)
        .unwrap();
    let file = tempfile::tempfile().unwrap();
    let refused = rig.run(file.as_raw_fd(), Class::Tty, Op::TtyConfigure(cfg), None);
    assert_eq!(refused.unwrap_err(), IoError::Os(Errno::ENOTTY));
    assert!(rig.run(fd, Class::Gpio, Op::GpioGet, None).is_err());
}

pub fn recv_parks_until_data(b: Backend) {
    let mut rig = Rig::new(b);
    let (a, mut z) = pair();
    let rx = rig.buf(b"", 8);
    let id = rig.start(a.as_raw_fd(), Class::Stream, Op::Recv, Some(rx));
    for _ in 0..3 {
        assert!(rig.poll(id).is_none());
    }
    z.write_all(b"late").unwrap();
    assert_eq!(len(rig.wait(id)), 4);
    assert_eq!(rig.bytes(rx, 4), b"late");
}

/// Fills `a`'s send buffer, so a send on it waits.
fn fill(a: &UnixStream) {
    let chunk = [0u8; 4096];
    loop {
        match (&*a).write(&chunk) {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => return,
            Err(e) => panic!("{e}"),
        }
    }
}

pub fn read_and_write_wait_on_one_descriptor(b: Backend) {
    let mut rig = Rig::new(b);
    let (a, mut z) = pair();
    fill(&a);
    let (tx, rx) = (rig.buf(b"x", 0), rig.buf(b"", 8));
    let send = rig.start(a.as_raw_fd(), Class::Stream, Op::Send, Some(tx));
    let recv = rig.start(a.as_raw_fd(), Class::Stream, Op::Recv, Some(rx));
    assert!(rig.poll(send).is_none() && rig.poll(recv).is_none());
    z.write_all(b"hi").unwrap();
    assert_eq!(len(rig.wait(recv)), 2);
    z.set_nonblocking(true).unwrap();
    let mut sink = vec![0; 1 << 16];
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let _ = z.read(&mut sink);
        if let Some(r) = rig.poll(send) {
            assert_eq!(len(r), 1);
            break;
        }
        assert!(Instant::now() < deadline, "the send never ran");
    }
}

pub fn cancel_waiting_recv(b: Backend) {
    let mut rig = Rig::new(b);
    let (a, _z) = pair();
    let rx = rig.buf(b"", 8);
    let id = rig.start(a.as_raw_fd(), Class::Stream, Op::Recv, Some(rx));
    assert!(rig.poll(id).is_none());
    rig.os.cancel(id);
    assert_eq!(rig.wait(id).unwrap_err(), IoError::Canceled);
}

pub fn cancel_waiting_accept(b: Backend) {
    let mut rig = Rig::new(b);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let id = rig.start(listener.as_raw_fd(), Class::Listener, Op::Accept, None);
    assert!(rig.poll(id).is_none());
    rig.os.cancel(id);
    assert_eq!(rig.wait(id).unwrap_err(), IoError::Canceled);
}

pub fn close_cancels_waiting_ops(b: Backend) {
    let mut rig = Rig::new(b);
    let (a, _z) = pair();
    let rx = rig.buf(b"", 8);
    let id = rig.start(a.as_raw_fd(), Class::Stream, Op::Recv, Some(rx));
    assert!(rig.poll(id).is_none());
    rig.os.close(a.into());
    assert_eq!(rig.wait(id).unwrap_err(), IoError::Canceled);
}

pub fn accept_gives_a_stream(b: Backend) {
    let mut rig = Rig::new(b);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let id = rig.start(listener.as_raw_fd(), Class::Listener, Op::Accept, None);
    let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (stream, class) = opened(rig.wait(id));
    assert_eq!(class, Class::Stream);
    client.write_all(b"hey").unwrap();
    let rx = rig.buf(b"", 8);
    assert_eq!(
        len(rig.run(stream.as_raw_fd(), class, Op::Recv, Some(rx))),
        3
    );
    rig.os.close(stream);
}

pub fn open_under_a_dir(b: Backend) {
    let mut rig = Rig::new(b);
    let dir = tempfile::tempdir().unwrap();
    let dir_fd = std::fs::File::open(dir.path()).unwrap();
    let path = rig.buf(b"made.txt", 0);
    let flags = OpenFlags::WRITE | OpenFlags::CREATE;
    let (file, class) =
        opened(rig.run(dir_fd.as_raw_fd(), Class::Dir, Op::Open(flags), Some(path)));
    assert_eq!(class, Class::File);
    let body = rig.buf(b"abc", 0);
    rig.run(
        file.as_raw_fd(),
        Class::File,
        Op::Write { offset: None },
        Some(body),
    )
    .unwrap();
    assert_eq!(std::fs::read(dir.path().join("made.txt")).unwrap(), b"abc");
    let missing = rig.buf(b"nope.txt", 0);
    let read = rig.run(
        dir_fd.as_raw_fd(),
        Class::Dir,
        Op::Open(OpenFlags::READ),
        Some(missing),
    );
    assert_eq!(read.unwrap_err(), IoError::Os(Errno::ENOENT));
}

pub fn closed_peer_reads_zero(b: Backend) {
    let mut rig = Rig::new(b);
    let (a, z) = pair();
    drop(z);
    let rx = rig.buf(b"", 8);
    assert_eq!(
        len(rig.run(a.as_raw_fd(), Class::Stream, Op::Recv, Some(rx))),
        0
    );
}

pub fn run_all(b: Backend) {
    datagram_send_and_recv(b);
    file_write_read_at_offsets(b);
    tty_configure_and_gpio_on_a_tty(b);
    recv_parks_until_data(b);
    read_and_write_wait_on_one_descriptor(b);
    cancel_waiting_recv(b);
    cancel_waiting_accept(b);
    close_cancels_waiting_ops(b);
    accept_gives_a_stream(b);
    open_under_a_dir(b);
    closed_peer_reads_zero(b);
}
