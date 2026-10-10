use metor_proto::types::Timestamp;

use super::allocations;
use crate::io::test::{ScriptedUart, TestDevice, cap, device, device_shared};
use crate::io::*;

const NOW: Timestamp = Timestamp(0);

#[derive(Default)]
struct Line {
    level: bool,
    ticks: u32,
}

impl GpioModel for Line {
    fn get(&mut self, _now: Timestamp) -> Result<bool, IoError> {
        Ok(self.level)
    }

    fn set(&mut self, _now: Timestamp, level: bool) -> Result<(), IoError> {
        self.level = level;
        Ok(())
    }

    fn tick(&mut self, _now: Timestamp) {
        self.ticks += 1;
    }
}

/// A `ScriptedUart` that waits `latency` serves before it sees an op.
struct Slow {
    uart: ScriptedUart,
    latency: u32,
}

impl UartModel for Slow {
    fn rx(&mut self, now: Timestamp, buf: &mut [u8]) -> Option<usize> {
        self.uart.rx(now, buf)
    }

    fn tx(&mut self, now: Timestamp, bytes: &[u8]) -> Result<usize, IoError> {
        self.uart.tx(now, bytes)
    }

    fn configure(&mut self, now: Timestamp, cfg: &TtyConfig) -> Result<(), IoError> {
        self.uart.configure(now, cfg)
    }

    fn tick(&mut self, _now: Timestamp) {}

    fn latency(&self, _kind: OpKind) -> u32 {
        self.latency
    }
}

fn read(driver: &mut TestDevice) -> Token {
    driver.request(Op::Read { offset: None }, b"")
}

fn results(driver: &mut TestDevice) -> Vec<(Token, Result<u32, IoError>)> {
    let replies = driver.replies().into_iter();
    replies.map(|c| (c.token, c.result())).collect()
}

#[test]
fn read_fills_the_driver_buffer() {
    let (mut dev, mut driver) = device::<Uart>();
    let mut uart = ScriptedUart::scripted([b"$GPGGA".to_vec()]);
    let token = read(&mut driver);
    dev.serve(NOW, &mut uart);
    assert_eq!(results(&mut driver), [(token, Ok(6))]);
    assert_eq!(driver.driver_bytes(token), b"$GPGGA");
}

#[test]
fn write_reaches_tx_once() {
    let (mut dev, mut driver) = device::<Uart>();
    let mut uart = ScriptedUart::silent();
    let token = driver.request(Op::Write { offset: None }, b"$PMTK");
    dev.serve(NOW, &mut uart);
    dev.serve(NOW, &mut uart);
    assert_eq!(results(&mut driver), [(token, Ok(5))]);
    assert_eq!(uart.written(), [b"$PMTK".to_vec()]);
}

#[test]
fn gpio_get_answers_the_level() {
    let (mut dev, mut driver) = device::<GpioLine>();
    let mut line = Line {
        level: true,
        ..Line::default()
    };
    let get = driver.request(Op::GpioGet, b"");
    let set = driver.request(Op::GpioSet(false), b"");
    dev.serve(NOW, &mut line);
    assert_eq!(results(&mut driver), [(get, Ok(1)), (set, Ok(0))]);
    assert!(!line.level);
    assert_eq!(line.ticks, 1);
}

#[test]
fn unsupported_op_is_refused() {
    let (mut dev, mut driver) = device::<Uart>();
    let token = driver.request(Op::Accept, b"");
    dev.serve(NOW, &mut ScriptedUart::silent());
    assert_eq!(results(&mut driver), [(token, Err(IoError::Unsupported))]);
}

#[test]
fn latency_waits_that_many_serves() {
    let (mut dev, mut driver) = device::<Uart>();
    let mut uart = Slow {
        uart: ScriptedUart::scripted([b"x".to_vec()]),
        latency: 3,
    };
    let token = read(&mut driver);
    for _ in 0..3 {
        dev.serve(NOW, &mut uart);
        assert!(driver.replies().is_empty());
    }
    dev.serve(NOW, &mut uart);
    assert_eq!(results(&mut driver), [(token, Ok(1))]);
}

#[test]
fn empty_read_holds_until_bytes_arrive() {
    let (mut dev, mut driver) = device::<Uart>();
    let mut uart = ScriptedUart::silent();
    let token = read(&mut driver);
    for _ in 0..5 {
        dev.serve(NOW, &mut uart);
        assert!(driver.replies().is_empty());
    }
    uart.push(b"late");
    dev.serve(NOW, &mut uart);
    assert_eq!(results(&mut driver), [(token, Ok(4))]);
}

#[test]
fn cancel_completes_a_held_op_and_nothing_else() {
    let (mut dev, mut driver) = device::<Uart>();
    let mut uart = ScriptedUart::silent();
    let token = read(&mut driver);
    dev.serve(NOW, &mut uart);
    let cancel = |t| Sqe::new(t, RawHandle::new(0, 0), &Op::Cancel, Span::default());
    driver.send(cancel(token));
    driver.send(cancel(Token::new(9, 9)));
    dev.serve(NOW, &mut uart);
    assert_eq!(results(&mut driver), [(token, Err(IoError::Canceled))]);
}

#[test]
fn held_ops_past_capacity_are_queue_full() {
    let (mut dev, mut driver) = device_shared::<Uart>(1);
    let tokens: Vec<Token> = (0..=MAX_OPS).map(|_| read(&mut driver)).collect();
    dev.serve(NOW, &mut ScriptedUart::silent());
    assert_eq!(
        results(&mut driver),
        [(tokens[MAX_OPS], Err(IoError::QueueFull))]
    );
}

#[test]
fn span_outside_the_driver_arena_is_bad_buf() {
    let (mut dev, mut driver) = device::<Uart>();
    let token = Token::new(0, 0);
    let op = Op::Read { offset: None };
    driver.send(Sqe::new(
        token,
        RawHandle::new(0, 0),
        &op,
        Span::new(u32::MAX - 4, 8),
    ));
    let other = Token::new(MAX_OPS as u16, 0);
    driver.send(Sqe::new(other, RawHandle::new(0, 0), &op, Span::new(0, 8)));
    dev.serve(NOW, &mut ScriptedUart::scripted([b"x".to_vec()]));
    let got = results(&mut driver);
    assert_eq!(
        got,
        [(token, Err(IoError::BadBuf)), (other, Err(IoError::BadBuf))]
    );
}

#[test]
fn full_reply_port_keeps_replies_in_order() {
    let (mut dev, mut driver) = device::<GpioLine>();
    let mut line = Line::default();
    let mut sent = Vec::new();
    for _ in 0..3 {
        for _ in 0..MAX_OPS {
            sent.push(driver.request(Op::GpioGet, b""));
        }
        dev.serve(NOW, &mut line);
    }
    let mut got = Vec::new();
    for _ in 0..4 {
        got.extend(driver.replies().into_iter().map(|c| c.token));
        dev.serve(NOW, &mut line);
    }
    assert_eq!(got, sent);
}

#[test]
fn serving_allocates_nothing() {
    let (mut dev, mut driver) = device::<Uart>();
    let mut uart = ScriptedUart::silent();
    let mut n = 0;
    for _ in 0..8 {
        driver.request(Op::TtyFlush(TtyQueue::Both), b"");
        n += allocations(|| dev.serve(NOW, &mut uart));
        assert_eq!(driver.replies().len(), 1);
    }
    assert_eq!(n, 0);
}

/// The 18-io.md driver: sets the rate once, then reads.
#[test]
fn driver_configures_once_then_reads_through_cap() {
    struct Driver {
        tx: Slot<Write>,
        rx: Slot<Read>,
        configured: bool,
        got: Vec<u8>,
    }
    let (uart, mut io) = cap::<Uart, _>(ScriptedUart::scripted([b"$GP".to_vec(), b"GGA".to_vec()]));
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
        io.turn();
    }
    assert_eq!(io.model().written(), [b"$PMTK".to_vec()]);
    assert_eq!(s.got, b"$GPGGA");
}
