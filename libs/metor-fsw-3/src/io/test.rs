//! Doubles for testing IO systems and models without a target.
//!
//! [`cap`] gives a system a `Cap<K>` whose ops a model answers through a
//! real `DeviceSide`, as a sim resource's ops are. [`device`] gives a model
//! system a `DeviceSide<K>` whose requests the test writes.

use std::collections::VecDeque;
use std::rc::Rc;

use metor_fsw_3_ring::{Config, NoWake, RingBuffer};
use metor_proto::types::Timestamp;

use super::arena::Arena;
use super::cap::{Cap, Handle, Interface};
use super::device::{Arenas, Device, DeviceSide, Model, UartModel};
use super::{Cqe, Errno, Io, IoError, IoInit, MAX_OPS, Op, RawHandle, Span, Sqe, Token, TtyConfig};
use crate::port::{Input, Output, ring_capacity};
use crate::record::Record;

/// How far each [`TestIo::turn`] moves the clock, in microseconds.
pub const TURN_MICROS: i64 = 10_000;

fn ring<T: Record>(readers: usize) -> RingBuffer {
    RingBuffer::create_in_memory(Config {
        // PANIC Safety: a small record at a small depth fits any host.
        capacity: ring_capacity(T::MAX_LEN, 4 * MAX_OPS).expect("a small ring"),
        max_readers: readers,
    })
}

fn writer<T: Record>(ring: &RingBuffer) -> Output<T> {
    // PANIC Safety: each fresh ring has a free writer.
    Output::new(ring.writer(NoWake).expect("a fresh writer"))
}

fn reader<T: Record>(ring: &RingBuffer) -> Input<T> {
    // PANIC Safety: each ring is made with a reader slot for each caller.
    Input::new(vec![ring.view(NoWake).expect("a free reader")])
}

/// The model side of a [`cap`].
pub struct TestIo<M, K: Interface = super::Uart> {
    io: Io,
    device: DeviceSide<K>,
    model: M,
    now: Timestamp,
    sq: Input<Sqe>,
    submitted: Vec<Sqe>,
    _rings: [RingBuffer; 2],
}

/// A `Cap<K>` whose ops `model` answers when the test calls
/// [`TestIo::turn`].
pub fn cap<K: Interface, M: Model<K>>(model: M) -> (Cap<K>, TestIo<M, K>) {
    let (sq_ring, cq_ring) = (ring::<Sqe>(2), ring::<Cqe>(1));
    let arena = Rc::new(Arena::new());
    let arenas = Rc::new(Arenas::default());
    arenas.push(arena.clone());
    let io = Io::new(writer(&sq_ring), reader(&cq_ring), arena);
    let device = Device::new(reader(&sq_ring), writer(&cq_ring), arenas, MAX_OPS);
    let cap = Cap::new(Handle::new(io.next_grant(), io.clone()));
    let test = TestIo {
        sq: reader(&sq_ring),
        io,
        device: device.typed(),
        model,
        now: Timestamp(0),
        submitted: Vec::new(),
        _rings: [sq_ring, cq_ring],
    };
    (cap, test)
}

impl<M: Model<K>, K: Interface> TestIo<M, K> {
    /// Allocates the system's buffers, as a constructor does.
    pub fn init(&self) -> IoInit<'_> {
        IoInit::new(self.io.arena())
    }

    /// Advances time and serves the model once.
    pub fn turn(&mut self) {
        self.now = Timestamp(self.now.0 + TURN_MICROS);
        self.submitted = self.sq.drain().flatten().collect();
        self.device.serve(self.now, &mut self.model);
    }

    pub fn model(&self) -> &M {
        &self.model
    }

    pub fn model_mut(&mut self) -> &mut M {
        &mut self.model
    }

    /// The ops the last turn took.
    pub fn submitted(&self) -> &[Sqe] {
        &self.submitted
    }

    pub fn now(&self) -> Timestamp {
        self.now
    }
}

/// The bytes a [`TestDevice`] lends each request.
pub const DRIVER_BUF_LEN: usize = 256;

/// The driver side of a [`device`]: it writes requests and reads replies.
pub struct TestDevice {
    arena: Rc<Arena>,
    requests: Output<Sqe>,
    replies: Input<Cqe>,
    /// The filled length of each slot's buffer.
    lens: [u32; MAX_OPS],
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
    let (req_ring, rep_ring) = (ring::<Sqe>(1), ring::<Cqe>(1));
    let arena = Rc::new(Arena::owning(vec![0; MAX_OPS * DRIVER_BUF_LEN].into()));
    let arenas = Rc::new(Arenas::default());
    arenas.push(arena.clone());
    let device = Device::new(
        reader(&req_ring),
        writer(&rep_ring),
        arenas,
        MAX_OPS * hosts,
    );
    let test = TestDevice {
        arena,
        requests: writer(&req_ring),
        replies: reader(&rep_ring),
        lens: [0; MAX_OPS],
        next: 0,
        _rings: [req_ring, rep_ring],
    };
    (device.typed(), test)
}

impl TestDevice {
    /// Sends `op`, lending it the buffer of its slot filled with `bytes`.
    /// Slots are reused in turn, every `MAX_OPS` requests.
    pub fn request(&mut self, op: Op, bytes: &[u8]) -> Token {
        let slot = (self.next as usize % MAX_OPS) as u16;
        let token = Token::new(slot, (self.next / MAX_OPS as u32) as u16);
        self.next += 1;
        let n = bytes.len().min(DRIVER_BUF_LEN);
        let whole = self.whole(slot);
        whole[..n].copy_from_slice(&bytes[..n]);
        let len = match op.fills() {
            true => DRIVER_BUF_LEN,
            false => n,
        };
        self.lens[usize::from(slot)] = n as u32;
        let span = Span::new(u32::from(slot) * DRIVER_BUF_LEN as u32, len as u32);
        self.send(Sqe::new(token, RawHandle::new(0, 0), &op, span));
        token
    }

    /// Writes any request, such as one with a bad span.
    pub fn send(&self, sqe: Sqe) {
        let _ = self.requests.write(&sqe);
    }

    /// Every reply since the last call. A success sets its buffer's length.
    pub fn replies(&mut self) -> Vec<Cqe> {
        let replies: Vec<Cqe> = self.replies.drain().flatten().collect();
        for cqe in &replies {
            if let Ok(n) = cqe.result() {
                self.lens[usize::from(cqe.token.slot()) % MAX_OPS] = n;
            }
        }
        replies
    }

    /// A copy of the filled bytes `token`'s buffer holds.
    pub fn driver_bytes(&mut self, token: Token) -> Vec<u8> {
        let slot = token.slot() % MAX_OPS as u16;
        let n = self.lens[usize::from(slot)] as usize;
        self.whole(slot)[..n.min(DRIVER_BUF_LEN)].to_vec()
    }

    fn whole(&mut self, slot: u16) -> &mut [u8] {
        let span = Span::new(
            u32::from(slot) * DRIVER_BUF_LEN as u32,
            DRIVER_BUF_LEN as u32,
        );
        // PANIC Safety: the arena holds `MAX_OPS` buffers and `slot` is below that.
        let bytes = self.arena.get_slice(span).expect("a slot's buffer");
        // SAFETY: the test holds `&mut self`, and a model only touches a
        // buffer while `serve` runs.
        unsafe { &mut *bytes.as_ptr() }
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
