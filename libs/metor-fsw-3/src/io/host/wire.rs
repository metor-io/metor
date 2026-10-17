//! `Wire`: a sim resource, carrying bytes or a level between its two ends.
//!
//! A driver holds the host end and a model the device end, each through a
//! `Cap`. An op on an end completes at once, or parks until the other end
//! gives it something, as on a pty or a socketpair.

use core::ptr::NonNull;
use std::collections::VecDeque;

use super::{OpId, OsDone, Out};
use crate::io::{IoError, Op, ResourceEnd, SimKind, TtyQueue};

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
}

/// A read waiting for bytes.
struct Parked {
    id: OpId,
    buf: NonNull<[u8]>,
}

/// A queue of at most `FIFO` bytes.
struct Fifo(VecDeque<u8>);

impl Wire {
    /// A wire of `kind`, parking at most `ops` ops per end.
    pub(crate) fn new(kind: SimKind, ops: usize) -> Self {
        match kind {
            SimKind::Uart => Self::Uart(Uart {
                fifos: [Fifo::new(), Fifo::new()],
                parked: [VecDeque::with_capacity(ops), VecDeque::with_capacity(ops)],
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
        let mut dropped = 0;
        let result = match (self, op) {
            (Self::Uart(uart), Op::Read { .. }) => match uart.read(end, id, buf) {
                Some(n) => Ok(Out::len(n)),
                None => return 0,
            },
            (Self::Uart(uart), Op::Write { .. }) => {
                let (n, lost) = uart.write(end, buf, done);
                dropped = lost;
                Ok(Out::len(n))
            }
            (Self::Uart(_), Op::TtyConfigure(_)) => Ok(Out::len(0)),
            (Self::Uart(uart), Op::TtyFlush(queue)) => {
                if queue != TtyQueue::Output {
                    uart.fifos[end.index()].0.clear();
                }
                Ok(Out::len(0))
            }
            (Self::Gpio { level }, Op::GpioSet(set)) => {
                *level = set;
                Ok(Out::len(0))
            }
            (Self::Gpio { level }, Op::GpioGet) => Ok(Out::len(usize::from(*level))),
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
            if let Some(position) = parked.iter().position(|p| p.id == id) {
                parked.remove(position);
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
        let fifo = &mut self.fifos[end.index()];
        if fifo.0.is_empty() {
            self.parked[end.index()].push_back(Parked { id, buf });
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
        let to = end.other().index();
        // SAFETY: the writer lent `buf` to this op until its completion. The
        // borrow ends before any parked read's buffer is borrowed.
        let dropped = self.fifos[to].put(unsafe { buf.as_ref() });
        while !self.fifos[to].0.is_empty()
            && let Some(parked) = self.parked[to].pop_front()
        {
            // SAFETY: as in `read`.
            let n = self.fifos[to].take(unsafe { &mut *parked.buf.as_ptr() });
            let result = Ok(Out::len(n));
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
        Self(VecDeque::with_capacity(FIFO))
    }

    /// Appends what fits of `bytes`. Returns how many were dropped.
    fn put(&mut self, bytes: &[u8]) -> usize {
        let n = bytes.len().min(FIFO - self.0.len());
        self.0.extend(&bytes[..n]);
        bytes.len() - n
    }

    /// Moves the oldest bytes into `out`, as many as fit.
    fn take(&mut self, out: &mut [u8]) -> usize {
        let n = out.len().min(self.0.len());
        for (dst, src) in out.iter_mut().zip(self.0.drain(..n)) {
            *dst = src;
        }
        n
    }
}
