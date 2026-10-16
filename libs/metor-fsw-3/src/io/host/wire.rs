//! `Wire`: a sim resource, carrying bytes or a level between its two ends.
//!
//! A driver holds the host end and a model the device end, each through a
//! `Cap`. An op on an end completes at once, or parks until the other end
//! gives it something, as on a pty or a socketpair.

use core::ptr::NonNull;
use std::collections::VecDeque;

use super::{OpId, OsDone, Out};
use crate::io::{IoError, Op, ResourceEnd, SimKind, TtyConfig, TtyQueue};

/// The bytes a UART buffers toward each end.
pub(crate) const FIFO: usize = 4096;

pub(crate) enum Wire {
    Uart(Uart),
    Gpio { level: bool },
}

/// A serial line: a FIFO toward each end, and the reads parked on each.
pub(crate) struct Uart {
    /// Indexed by the end that reads it.
    fifos: [Fifo; 2],
    parked: [VecDeque<Parked>; 2],
    config: Option<TtyConfig>,
}

/// A read waiting for bytes.
struct Parked {
    id: OpId,
    buf: NonNull<[u8]>,
}

/// A ring of bytes with a fixed capacity.
struct Fifo {
    bytes: Box<[u8]>,
    head: usize,
    len: usize,
}

impl Wire {
    /// A wire of `kind`, parking at most `ops` ops per end.
    pub(crate) fn new(kind: SimKind, ops: usize) -> Self {
        match kind {
            SimKind::Uart => Self::Uart(Uart {
                fifos: [Fifo::new(), Fifo::new()],
                parked: [VecDeque::with_capacity(ops), VecDeque::with_capacity(ops)],
                config: None,
            }),
            SimKind::GpioLine => Self::Gpio { level: false },
        }
    }

    /// Runs `op` from `end`, lending `buf`. Each op it finishes, this one or
    /// a read it woke, goes to `done`. Returns the bytes a full FIFO dropped.
    pub(crate) fn run(
        &mut self,
        end: ResourceEnd,
        id: OpId,
        op: Op,
        buf: Option<NonNull<[u8]>>,
        done: &mut Vec<OsDone>,
    ) -> usize {
        let len = |n: usize| Ok(Out::Len(n as u32));
        let mut dropped = 0;
        let result = match (self, op) {
            (Self::Uart(uart), Op::Read { .. }) => match uart.read(end, id, buf) {
                Some(n) => len(n),
                None => return 0,
            },
            (Self::Uart(uart), Op::Write { .. }) => {
                let (n, lost) = uart.write(end, buf, done);
                dropped = lost;
                len(n)
            }
            (Self::Uart(uart), Op::TtyConfigure(cfg)) => {
                uart.config = Some(cfg);
                len(0)
            }
            (Self::Uart(uart), Op::TtyFlush(queue)) => {
                if queue != TtyQueue::Output {
                    uart.fifos[index(end)].clear();
                }
                len(0)
            }
            (Self::Gpio { level }, Op::GpioSet(set)) => {
                *level = set;
                len(0)
            }
            (Self::Gpio { level }, Op::GpioGet) => len(usize::from(*level)),
            _ => Err(IoError::Unsupported),
        };
        done.push(OsDone { id, result });
        dropped
    }

    /// Completes the parked op `id` `Canceled`, if one is parked.
    pub(crate) fn cancel(&mut self, id: OpId, done: &mut Vec<OsDone>) {
        let Self::Uart(uart) = self else {
            return;
        };
        for parked in &mut uart.parked {
            if let Some(at) = parked.iter().position(|p| p.id == id) {
                parked.remove(at);
                let result = Err(IoError::Canceled);
                done.push(OsDone { id, result });
            }
        }
    }
}

impl Uart {
    /// Takes what `end`'s FIFO holds into `buf`, or parks the read while it
    /// is empty. A read of no bytes finishes at once, as `read(2)` does.
    fn read(&mut self, end: ResourceEnd, id: OpId, buf: Option<NonNull<[u8]>>) -> Option<usize> {
        let Some(buf) = buf.filter(|b| !b.is_empty()) else {
            return Some(0);
        };
        let fifo = &mut self.fifos[index(end)];
        if fifo.len == 0 {
            self.parked[index(end)].push_back(Parked { id, buf });
            return None;
        }
        // SAFETY: the reader lent `buf` to this op until its completion,
        // and the io steps run on one thread.
        Some(fifo.take(unsafe { &mut *buf.as_ptr() }))
    }

    /// Puts `buf`'s bytes in the other end's FIFO, then wakes the reads
    /// parked there. Returns the bytes written, which the line takes whole,
    /// and those dropped past a full FIFO.
    fn write(
        &mut self,
        end: ResourceEnd,
        buf: Option<NonNull<[u8]>>,
        done: &mut Vec<OsDone>,
    ) -> (usize, usize) {
        let Some(buf) = buf else {
            return (0, 0);
        };
        let to = index(end.other());
        // SAFETY: the writer lent `buf` to this op until its completion. The
        // borrow ends before any parked read's buffer is borrowed.
        let dropped = self.fifos[to].put(unsafe { buf.as_ref() });
        while self.fifos[to].len > 0
            && let Some(parked) = self.parked[to].pop_front()
        {
            // SAFETY: as in `read`.
            let n = self.fifos[to].take(unsafe { &mut *parked.buf.as_ptr() });
            let result = Ok(Out::Len(n as u32));
            done.push(OsDone {
                id: parked.id,
                result,
            });
        }
        (buf.len(), dropped)
    }
}

impl Fifo {
    fn new() -> Self {
        Self {
            bytes: vec![0; FIFO].into(),
            head: 0,
            len: 0,
        }
    }

    /// Appends what fits of `bytes`. Returns how many were dropped.
    fn put(&mut self, bytes: &[u8]) -> usize {
        let n = bytes.len().min(FIFO - self.len);
        let tail = (self.head + self.len) % FIFO;
        let first = n.min(FIFO - tail);
        self.bytes[tail..tail + first].copy_from_slice(&bytes[..first]);
        self.bytes[..n - first].copy_from_slice(&bytes[first..n]);
        self.len += n;
        bytes.len() - n
    }

    /// Moves the oldest bytes into `out`, as many as fit.
    fn take(&mut self, out: &mut [u8]) -> usize {
        let n = out.len().min(self.len);
        let first = n.min(FIFO - self.head);
        out[..first].copy_from_slice(&self.bytes[self.head..self.head + first]);
        out[first..n].copy_from_slice(&self.bytes[..n - first]);
        self.head = (self.head + n) % FIFO;
        self.len -= n;
        n
    }

    fn clear(&mut self) {
        self.len = 0;
    }
}

fn index(end: ResourceEnd) -> usize {
    match end {
        ResourceEnd::Host => 0,
        ResourceEnd::Device => 1,
    }
}
