//! A serial device that sends random bytes, and a driver that reads them.

use metor_fsw_3::io::{Cap, DeviceSide, IoError, Read, Rng, TtyConfig, Uart, UartModel};
use metor_fsw_3::{IoInit, Output, Slot, Timestamp, system};

use super::Ping;

/// Answers a read half the time, with one to eight bytes drawn from `0`.
struct Chat<'a>(&'a mut Rng);

impl UartModel for Chat<'_> {
    fn rx(&mut self, _now: Timestamp, buf: &mut [u8]) -> Option<usize> {
        if !self.0.chance(u32::MAX / 2) {
            return None;
        }
        let n = (self.0.below(8) as usize + 1).min(buf.len());
        let bytes = self.0.next_u64().to_le_bytes();
        buf[..n].copy_from_slice(&bytes[..n]);
        Some(n)
    }

    fn tx(&mut self, _now: Timestamp, bytes: &[u8]) -> Result<usize, IoError> {
        Ok(bytes.len())
    }

    fn configure(&mut self, _now: Timestamp, _cfg: &TtyConfig) -> Result<(), IoError> {
        Ok(())
    }

    fn tick(&mut self, _now: Timestamp) {}
}

/// A serial device that sends random bytes, drawn from the target seed.
#[system]
pub fn chatter(now: Timestamp, rng: &mut Rng, uart: &mut DeviceSide<Uart>) {
    uart.serve(now, &mut Chat(rng));
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
    rx: Slot<Read>,
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
