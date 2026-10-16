use super::allocations;
use crate::io::test::{ScriptedUart, TestIo, cap};
use crate::io::*;

/// A system with one read in flight, on a `Cap` from `test::cap`.
struct Reader {
    uart: Cap<Uart>,
    io: TestIo,
    rx: Slot<ReadOp>,
}

impl Reader {
    fn new() -> Self {
        let (uart, io) = cap::<Uart>();
        let rx = Slot::Idle(io.init().alloc(8));
        Self { uart, io, rx }
    }

    /// Takes a finished read, then starts the next.
    fn cycle(&mut self) -> Option<Result<Vec<u8>, IoError>> {
        let got = self.rx.poll().map(|r| r.map(|_| self.rx.bytes().to_vec()));
        if let Some(buf) = self.rx.take_idle() {
            self.rx = Slot::Busy(self.uart.read(buf));
        }
        got
    }
}

#[test]
fn a_scripted_read_fills_the_buffer() {
    let mut r = Reader::new();
    let mut model = ScriptedUart::scripted([b"$GPGGA".to_vec()]);
    r.cycle();
    model.serve(r.io.turn());
    assert_eq!(r.cycle(), Some(Ok(b"$GPGGA".to_vec())));
}

#[test]
fn an_unanswered_read_waits_for_a_later_pass() {
    let mut r = Reader::new();
    let mut model = ScriptedUart::silent();
    r.cycle();
    for _ in 0..3 {
        model.serve(r.io.turn());
        assert_eq!(r.cycle(), None);
    }
    model.push(b"late");
    model.serve(r.io.turn());
    assert_eq!(r.cycle(), Some(Ok(b"late".to_vec())));
}

#[test]
fn a_cancel_completes_a_waiting_op() {
    let (uart, mut io) = cap::<Uart>();
    let buf = io.init().alloc(4);
    let mut read = uart.read(buf);
    ScriptedUart::silent().serve(io.turn());
    read.cancel();
    ScriptedUart::silent().serve(io.turn());
    assert_eq!(read.poll().map(|(r, _)| r), Some(Err(IoError::Canceled)));
}

#[test]
fn scripted_faults_reach_the_writer() {
    let (uart, mut io) = cap::<Uart>();
    let init = io.init();
    let (a, b) = (init.alloc_from(b"cmd"), init.alloc_from(b"cmd"));
    drop(init);
    let mut model = ScriptedUart::silent().fail_write(Errno::EIO).short_write(1);
    let mut first = uart.write(a);
    model.serve(io.turn());
    assert_eq!(
        first.poll().map(|(r, _)| r),
        Some(Err(IoError::Os(Errno::EIO)))
    );
    let mut second = uart.write(b);
    model.serve(io.turn());
    assert_eq!(second.poll().map(|(r, _)| r), Some(Ok(1)));
    assert_eq!(model.written(), [b"c".to_vec()]);
}

#[test]
fn serving_allocates_nothing() {
    let (uart, mut io) = cap::<Uart>();
    let mut model = ScriptedUart::silent();
    let mut n = 0;
    for _ in 0..8 {
        let mut flush = uart.flush(TtyQueue::Both);
        let port = io.turn();
        n += allocations(|| model.serve(port));
        assert_eq!(flush.poll(), Some(Ok(())));
    }
    assert_eq!(n, 0);
}

/// The 18-io.md driver: sets the rate once, then reads.
#[test]
fn driver_configures_once_then_reads() {
    struct Driver {
        tx: Slot<WriteOp>,
        rx: Slot<ReadOp>,
        configured: bool,
        got: Vec<u8>,
    }
    let mut model = ScriptedUart::scripted([b"$GP".to_vec(), b"GGA".to_vec()]);
    let (uart, mut io) = cap::<Uart>();
    let mut s = {
        let init = io.init();
        Driver {
            tx: Slot::Idle(init.alloc_from(b"$PMTK")),
            rx: Slot::Idle(init.alloc(8)),
            configured: false,
            got: Vec::new(),
        }
    };
    for _ in 0..4 {
        if let Some(r) = s.tx.poll() {
            s.configured = r.is_ok();
        }
        if let Some(Ok(_)) = s.rx.poll() {
            s.got.extend_from_slice(s.rx.bytes());
        }
        if !s.configured
            && let Some(cmd) = s.tx.take_idle()
        {
            s.tx = Slot::Busy(uart.write(cmd));
        }
        if let Some(buf) = s.rx.take_idle() {
            s.rx = Slot::Busy(uart.read(buf));
        }
        model.serve(io.turn());
    }
    assert_eq!(model.written(), [b"$PMTK".to_vec()]);
    assert_eq!(s.got, b"$GPGGA");
}
