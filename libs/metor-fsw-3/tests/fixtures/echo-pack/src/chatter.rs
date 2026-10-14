//! A serial device that sends random bytes, and a driver that reads them.

use metor_fsw_3::Rng;
use metor_fsw_3::io::{Cap, ReadOp, Uart, WriteOp};
use metor_fsw_3::{IoInit, Output, Slot, system};

use super::Ping;

/// A serial device's buffer for the bytes it sends.
pub struct Chatter {
    tx: Slot<WriteOp>,
}

impl Chatter {
    pub fn new(io: &IoInit) -> Self {
        Self {
            tx: Slot::Idle(io.alloc(8)),
        }
    }
}

/// A serial device that sends random bytes, drawn from the target seed.
#[system]
pub fn chatter(#[state] s: &mut Chatter, rng: &mut Rng, uart: &Cap<Uart>) {
    // Half the cycles it can, it sends one to eight bytes.
    let _ = s.tx.poll();
    let Some(mut buf) = s.tx.take_idle() else {
        return;
    };
    if rng.chance(u32::MAX / 2) {
        let n = rng.below(8) as usize + 1;
        buf.fill(&rng.next_u64().to_le_bytes()[..n]);
        s.tx = Slot::Busy(uart.write(buf));
    } else {
        s.tx = Slot::Idle(buf);
    }
}

/// How many `Grower`s this process has made.
static GROWN: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Asks for 16 more bytes on each make, so a remake outgrows the region the
/// first make laid out.
pub struct Grower {
    _buf: metor_fsw_3::Buf,
}

impl Grower {
    pub fn new(io: &IoInit) -> Self {
        let made = GROWN.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        Self {
            _buf: io.alloc(16 * (made + 1)),
        }
    }
}

/// Holds a buffer and a serial port, and does nothing with them.
#[system]
pub fn grower(#[state] s: &mut Grower, uart: &Cap<Uart>) {
    let _ = (s, uart);
}

pub struct Reader {
    rx: Slot<ReadOp>,
}

impl Reader {
    pub fn new(io: &IoInit) -> Self {
        Self {
            rx: Slot::Idle(io.alloc(16)),
        }
    }
}

/// Reads continuously, and pings the sum of each read's bytes.
#[system]
pub fn reader(#[state] s: &mut Reader, uart: &Cap<Uart>, sums: &mut Output<Ping>) {
    if let Some(Ok(_)) = s.rx.poll() {
        let n = s.rx.bytes().iter().map(|b| u32::from(*b)).sum();
        let _ = sums.write(&Ping { n });
    }
    if let Some(buf) = s.rx.take_idle() {
        s.rx = Slot::Busy(uart.read(buf));
    }
}
