//! Test harnesses for IO systems and models, with no reactor or OS.
//!
//! [`cap`] gives a driver a `Cap<K>` whose ops a model answers each time the
//! test calls [`TestIo::turn`]. [`device`] gives a model a `DeviceSide<K>`
//! whose requests the test writes as the driver.

use core::marker::PhantomData;
use std::collections::VecDeque;
use std::rc::Rc;

use metor_fsw_3_ring::{Config, NoWake, RingBuffer};
use metor_proto::types::Timestamp;

use crate::port::{Input, Output, ring_capacity};
use crate::record::Record;

use super::cap::{Cap, Handle, Interface, Uart};
use super::device::DeviceSide;
use super::queue::{Io, Table};
use super::sim::{Model, Reply, UartModel, fills};
use super::{
    BufError, BufTable, Cqe, Errno, IoError, IoInit, IoStatus, MAX_OPS, Op, Out, Sqe, Tables,
    Token, TtyConfig,
};

/// How far each [`TestIo::turn`] moves the clock, in microseconds.
pub const TURN_MICROS: u64 = 10_000;

/// Rings a test plays one end of.
fn ring<T: Record>(depth: usize) -> RingBuffer {
    RingBuffer::create_in_memory(Config {
        // PANIC Safety: the record sizes and depths here are small constants.
        capacity: ring_capacity(T::MAX_LEN, depth).expect("a small ring"),
        max_readers: 1,
    })
}

/// The reactor side of a [`cap`]: it answers the cap's ops with model `M`.
pub struct TestIo<M, K: Interface = Uart> {
    io: Io,
    sq: Input<Sqe>,
    cq: Output<Cqe>,
    model: M,
    now: Timestamp,
    held: Vec<Sqe>,
    submitted: Vec<Sqe>,
    _rings: [RingBuffer; 2],
    _k: PhantomData<K>,
}

/// A `Cap<K>` whose ops `model` answers when the test calls
/// [`TestIo::turn`].
pub fn cap<K: Interface, M: Model<K>>(model: M) -> (Cap<K>, TestIo<M, K>) {
    let (sq_ring, cq_ring) = (ring::<Sqe>(MAX_OPS * 4), ring::<Cqe>(MAX_OPS * 4));
    // PANIC Safety: each fresh ring has one writer and one reader slot.
    let writer = |r: &RingBuffer| r.writer(NoWake).expect("a fresh writer");
    let view = |r: &RingBuffer| r.view(NoWake).expect("a fresh reader");
    let io = Io::new(
        Output::new(writer(&sq_ring)),
        Input::new(vec![view(&cq_ring)]),
        Table::new(),
    );
    let cap = Cap::new(Handle::new(io.next_grant(), io.clone()));
    let test = TestIo {
        sq: Input::new(vec![view(&sq_ring)]),
        cq: Output::new(writer(&cq_ring)),
        io,
        model,
        now: Timestamp(0),
        held: Vec::new(),
        submitted: Vec::new(),
        _rings: [sq_ring, cq_ring],
        _k: PhantomData,
    };
    (cap, test)
}

impl<M: Model<K>, K: Interface> TestIo<M, K> {
    /// Allocates the system's buffers, as a constructor does.
    pub fn init(&self) -> IoInit<'_> {
        IoInit::new(&self.io)
    }

    /// Advances time, ticks the model, and answers every op submitted since
    /// the last turn, along with every op the model held back.
    pub fn turn(&mut self) {
        self.now = Timestamp(self.now.0 + TURN_MICROS as i64);
        self.model.tick(self.now);
        self.submitted = self.sq.drain().flatten().collect();
        let ops: Vec<Sqe> = self
            .held
            .drain(..)
            .chain(self.submitted.iter().copied())
            .collect();
        for sqe in ops {
            self.answer(sqe);
        }
    }

    fn answer(&mut self, sqe: Sqe) {
        if let Op::Cancel { target, .. } = sqe.op {
            let status = self.cancel(target);
            self.post(Cqe::new(sqe.token, status, Out::UNIT));
            return;
        }
        let bufs = &self.io.table().bufs;
        let bytes: &mut [u8] = match sqe.op.buf() {
            // SAFETY: the op holds its buffer, so the `Buf` sits in the
            // queue and nothing else reaches the slot.
            Some(buf) if fills(&sqe.op) => unsafe { bufs.whole_mut(buf.slot()) },
            // SAFETY: as above.
            Some(buf) => unsafe { bufs.filled_mut(buf.slot()) },
            None => &mut [],
        };
        match self.model.answer(self.now, &sqe, bytes) {
            Reply::Ready(status, out) => self.post(Cqe::new(sqe.token, status, out)),
            Reply::Later => self.held.push(sqe),
        }
    }

    /// Answers a held op `Canceled`.
    fn cancel(&mut self, target: Token) -> IoStatus {
        let Some(at) = self.held.iter().position(|s| s.token == target) else {
            return IoStatus::err(IoError::Os(Errno::ENOENT));
        };
        let held = self.held.remove(at);
        self.post(Cqe::err(held.token, IoError::Canceled));
        IoStatus::OK
    }

    fn post(&self, cqe: Cqe) {
        let _ = self.cq.write(&cqe);
    }

    pub fn model(&self) -> &M {
        &self.model
    }

    pub fn model_mut(&mut self) -> &mut M {
        &mut self.model
    }

    /// The ops submitted before the last turn.
    pub fn submitted(&self) -> &[Sqe] {
        &self.submitted
    }

    pub fn now(&self) -> Timestamp {
        self.now
    }
}

/// The buffers a [`TestDevice`] holds, and their size.
pub const DRIVER_BUFS: usize = 64;
pub const DRIVER_BUF_LEN: usize = 256;

/// A request the test sent, and the driver slot it lent.
struct Sent {
    token: Token,
    slot: Option<u16>,
    fills: bool,
}

/// The driver side of a [`device`]: it writes requests and reads replies.
pub struct TestDevice {
    table: Rc<BufTable>,
    requests: Output<Sqe>,
    replies: Input<Cqe>,
    free: Vec<u16>,
    sent: Vec<Sent>,
    next: u32,
    _rings: [RingBuffer; 2],
}

/// A `DeviceSide<K>` whose requests the test writes through the returned
/// [`TestDevice`].
pub fn device<K: Interface>() -> (DeviceSide<K>, TestDevice) {
    device_shared::<K>(1)
}

/// A [`device`] on a resource with `hosts` host grants, so it holds
/// `hosts * MAX_OPS` ops.
pub fn device_shared<K: Interface>(hosts: usize) -> (DeviceSide<K>, TestDevice) {
    let (req_ring, rep_ring) = (ring::<Sqe>(MAX_OPS * 4), ring::<Cqe>(MAX_OPS * 4));
    // PANIC Safety: each fresh ring has one writer and one reader slot.
    let writer = |r: &RingBuffer| r.writer(NoWake).expect("a fresh writer");
    let view = |r: &RingBuffer| r.view(NoWake).expect("a fresh reader");
    let (table, free) = driver_table();
    let tables = Tables::new();
    tables.add(&table);
    let side = DeviceSide::new(
        Input::new(vec![view(&req_ring)]),
        Output::new(writer(&rep_ring)),
        tables,
        MAX_OPS * hosts,
    );
    let test = TestDevice {
        table,
        requests: Output::new(writer(&req_ring)),
        replies: Input::new(vec![view(&rep_ring)]),
        free,
        sent: Vec::new(),
        next: 0,
        _rings: [req_ring, rep_ring],
    };
    (side.typed(), test)
}

/// A frozen driver table of `DRIVER_BUFS` free buffers.
fn driver_table() -> (Rc<BufTable>, Vec<u16>) {
    let table = Rc::new(BufTable::new(DRIVER_BUFS));
    let slots = (0..DRIVER_BUFS).filter_map(|_| table.alloc(DRIVER_BUF_LEN).ok());
    let mut free: Vec<u16> = slots.collect();
    free.reverse();
    table.freeze();
    (table, free)
}

impl TestDevice {
    /// Sends `op`, lending it a driver buffer filled with `bytes` if it
    /// takes one.
    pub fn request(&mut self, mut op: Op, bytes: &[u8]) -> Result<Token, BufError> {
        let fills = fills(&op);
        let slot = match op.buf_mut() {
            Some(buf) => Some(self.lend(buf, bytes)?),
            None => None,
        };
        let token = Token::new(self.next);
        self.next += 1;
        self.send(Sqe::new(token, super::RawHandle::new(0, 0), op));
        self.sent.push(Sent { token, slot, fills });
        Ok(token)
    }

    fn lend(&mut self, buf: &mut super::BufRef, bytes: &[u8]) -> Result<u16, BufError> {
        if bytes.len() > DRIVER_BUF_LEN {
            return Err(BufError::TooLarge);
        }
        let slot = self.free.pop().ok_or(BufError::Full)?;
        // SAFETY: the slot is free, so no request holds it.
        let whole = unsafe { self.table.whole_mut(slot) };
        whole[..bytes.len()].copy_from_slice(bytes);
        self.table.set_len(slot, bytes.len() as u32)?;
        *buf = super::BufRef::new(slot);
        // PANIC Safety: `device` added the table to its `Tables`.
        buf.stamp(self.table.id().expect("an added table"));
        Ok(slot)
    }

    /// Writes any request, such as one with a bad `BufRef`.
    pub fn send(&self, sqe: Sqe) {
        let _ = self.requests.write(&sqe);
    }

    /// Frees the driver buffer `token` lent, so the model can no longer
    /// reach it.
    pub fn release(&mut self, token: Token) {
        if let Some(sent) = self.sent.iter().find(|s| s.token == token)
            && let Some(slot) = sent.slot
        {
            self.table.release(slot);
        }
    }

    /// Every reply since the last call. A read's reply sets its buffer's
    /// length.
    pub fn replies(&mut self) -> Vec<Cqe> {
        let replies: Vec<Cqe> = self.replies.drain().flatten().collect();
        for cqe in &replies {
            let sent = self.sent.iter().find(|s| s.token == cqe.token);
            if let (
                Some(Sent {
                    slot: Some(slot),
                    fills: true,
                    ..
                }),
                Some(n),
            ) = (sent, cqe.out.as_len())
            {
                let _ = self.table.set_len(*slot, n);
            }
        }
        replies
    }

    /// A copy of the bytes in the driver buffer `token` lent.
    pub fn driver_bytes(&self, token: Token) -> Vec<u8> {
        let slot = self
            .sent
            .iter()
            .find(|s| s.token == token)
            .and_then(|s| s.slot);
        // SAFETY: the copy ends the borrow before any model runs again.
        slot.map_or_else(Vec::new, |slot| unsafe { self.table.filled(slot) }.to_vec())
    }
}

/// A serial device that replies from a script and records every write.
#[derive(Default)]
pub struct ScriptedUart {
    script: VecDeque<Vec<u8>>,
    written: Vec<Vec<u8>>,
    fail_write: Option<Errno>,
    short_write: Option<usize>,
    config: Option<TtyConfig>,
}

impl ScriptedUart {
    /// A device with nothing to read.
    pub fn silent() -> Self {
        Self::default()
    }

    /// A device that reads `chunks`, one per read.
    pub fn scripted(chunks: impl IntoIterator<Item = Vec<u8>>) -> Self {
        Self {
            script: chunks.into_iter().collect(),
            ..Self::default()
        }
    }

    /// Fails the next write with `errno`.
    pub fn fail_write(mut self, errno: Errno) -> Self {
        self.fail_write = Some(errno);
        self
    }

    /// Accepts only the first `n` bytes of the next write.
    pub fn short_write(mut self, n: usize) -> Self {
        self.short_write = Some(n);
        self
    }

    /// Queues one more chunk to read.
    pub fn push(&mut self, chunk: &[u8]) {
        self.script.push_back(chunk.to_vec());
    }

    pub fn written(&self) -> &[Vec<u8>] {
        &self.written
    }

    pub fn config(&self) -> Option<TtyConfig> {
        self.config
    }
}

impl UartModel for ScriptedUart {
    /// Reads the next chunk. A chunk longer than `buf` gives a short read and
    /// keeps the rest.
    fn rx(&mut self, _now: Timestamp, buf: &mut [u8]) -> Option<usize> {
        let chunk = self.script.front_mut()?;
        let n = chunk.len().min(buf.len());
        buf[..n].copy_from_slice(&chunk[..n]);
        chunk.drain(..n);
        if chunk.is_empty() {
            self.script.pop_front();
        }
        Some(n)
    }

    fn tx(&mut self, _now: Timestamp, bytes: &[u8]) -> Result<usize, IoError> {
        if let Some(errno) = self.fail_write.take() {
            return Err(IoError::Os(errno));
        }
        let n = self
            .short_write
            .take()
            .map_or(bytes.len(), |n| n.min(bytes.len()));
        self.written.push(bytes[..n].to_vec());
        Ok(n)
    }

    fn configure(&mut self, _now: Timestamp, cfg: &TtyConfig) -> Result<(), IoError> {
        self.config = Some(*cfg);
        Ok(())
    }

    fn tick(&mut self, _now: Timestamp) {}
}
