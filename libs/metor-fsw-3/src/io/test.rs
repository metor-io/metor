//! Doubles for testing IO systems without a target.
//!
//! [`cap`] gives a system a `Cap<K>` and lends the test the other end of its
//! queue, a [`TestPort`], to answer each op by hand. A driver and a model are
//! tested the same way: each is a system with a `Cap`. A `Connect` is
//! answered with [`Request::open`], which hands out a new handle.

use std::collections::VecDeque;
use std::rc::Rc;

use metor_fsw_3_ring::{Config, NoWake, RingBuffer};
use metor_proto::types::Timestamp;

use super::arena::Arena;
use super::cap::{Cap, Handle, Interface};
use super::{
    Cqe, DEFAULT_OPS, Errno, Io, IoError, IoInit, Op, RawHandle, Span, Sqe, Token, TtyConfig,
};
use crate::coordinator::MakeError;
use crate::port::{Input, Output, ring_capacity};
use crate::record::Record;

/// How far each [`TestIo::turn`] moves the clock, in microseconds.
pub const TURN_MICROS: i64 = 10_000;

fn ring<T: Record>(readers: usize, ops: u16) -> RingBuffer {
    RingBuffer::create_in_memory(Config {
        // PANIC Safety: a small record at a `u16` depth fits any host.
        capacity: ring_capacity(T::MAX_LEN, 4 * usize::from(ops)).expect("a small ring"),
        max_readers: readers,
    })
    .expect("a valid ring config")
}

fn writer<T: Record>(ring: &RingBuffer) -> Output<T> {
    // PANIC Safety: each fresh ring has a free writer.
    Output::new(ring.writer(NoWake).expect("a fresh writer"))
}

fn reader<T: Record>(ring: &RingBuffer) -> Input<T> {
    // PANIC Safety: each ring is made with a reader slot for each caller.
    Input::new(vec![ring.view(NoWake).expect("a free reader")])
}

/// The test's end of a [`cap`].
pub struct TestIo {
    io: Io,
    port: TestPort,
    now: Timestamp,
    sq: Input<Sqe>,
    submitted: Vec<Sqe>,
    _rings: [RingBuffer; 2],
}

/// A `Cap<K>` whose ops the test answers through the [`TestPort`] that
/// [`TestIo::turn`] lends.
pub fn cap<K: Interface>() -> (Cap<K>, TestIo) {
    cap_with_ops(DEFAULT_OPS)
}

/// A [`cap`] whose system may have `ops` ops in flight.
pub fn cap_with_ops<K: Interface>(ops: u16) -> (Cap<K>, TestIo) {
    let (sq_ring, cq_ring) = (ring::<Sqe>(2, ops), ring::<Cqe>(1, ops));
    let arena = Rc::new(Arena::new());
    let io = Io::new(writer(&sq_ring), reader(&cq_ring), arena.clone(), ops);
    let port = TestPort {
        requests: reader(&sq_ring),
        replies: writer(&cq_ring),
        arena,
        held: Vec::with_capacity(Sqe::depth(ops)),
        timers: Vec::with_capacity(usize::from(ops)),
        now: Timestamp(0),
        cursor: 0,
        opened: 0,
    };
    let cap = Cap::new(Handle::new(io.next_grant(), io.clone()));
    let test = TestIo {
        sq: reader(&sq_ring),
        io,
        port,
        now: Timestamp(0),
        submitted: Vec::new(),
        _rings: [sq_ring, cq_ring],
    };
    (cap, test)
}

impl TestIo {
    /// Runs `ctor` with the system's buffers, as a make does.
    pub fn make<S>(&self, ctor: impl FnOnce(&mut IoInit<'_>) -> Result<S, MakeError>) -> S {
        let mut init = IoInit::new(self.io.arena(), "test");
        // PANIC Safety: a test's buffers fit, or the test is wrong.
        let state = ctor(&mut init).expect("the test's buffers fit");
        init.finish();
        state
    }

    /// The ops the system has not taken back yet.
    pub fn in_flight(&self) -> usize {
        self.io.in_flight()
    }

    /// Advances time, lands the timeouts it passes, and lends the queue's
    /// other end, for the test to answer once.
    pub fn turn(&mut self) -> &mut TestPort {
        self.now = Timestamp(self.now.0 + TURN_MICROS);
        self.submitted = self.sq.drain().flatten().collect();
        self.port.tick(self.now);
        &mut self.port
    }

    /// The ops the last turn took.
    pub fn submitted(&self) -> &[Sqe] {
        &self.submitted
    }

    pub fn now(&self) -> Timestamp {
        self.now
    }
}

/// An op the test has not answered yet.
struct Held {
    token: Token,
    handle: RawHandle,
    op: Op,
    span: Span,
}

/// The other end of a system's queue: the test answers its ops one by one.
/// Timeouts land on their own as turns pass, as the dispatch lands them.
pub struct TestPort {
    requests: Input<Sqe>,
    replies: Output<Cqe>,
    arena: Rc<Arena>,
    held: Vec<Held>,
    /// Each timeout in flight and when it lands.
    timers: Vec<(Token, Timestamp)>,
    now: Timestamp,
    cursor: usize,
    /// The handles `Request::open` has handed out.
    opened: u16,
}

impl TestPort {
    /// Starts a pass over the ops waiting for an answer, oldest first. A
    /// cancel completes its target `Canceled` and is never lent.
    pub fn requests(&mut self) -> Requests<'_> {
        self.take();
        self.cursor = 0;
        Requests { port: self }
    }

    /// Takes what the system submitted since the last turn, timing its
    /// timeouts from that turn, then lands those due at `now`. One a full
    /// CQ refuses lands on a later turn.
    fn tick(&mut self, now: Timestamp) {
        self.take();
        self.now = now;
        let replies = &mut self.replies;
        self.timers
            .retain(|&(token, at)| at > now || replies.write(&Cqe::ok(token, 0)).is_err());
    }

    /// Moves the SQ's new ops into `held` and `timers`, and answers its
    /// cancels.
    fn take(&mut self) {
        let Self {
            requests,
            replies,
            held,
            timers,
            now,
            ..
        } = self;
        for sqe in requests.drain().flatten() {
            let reply = match sqe.op() {
                Ok(Op::Cancel) => cancel(held, timers, sqe.token),
                Ok(Op::Timeout(after)) => {
                    let micros = i64::try_from(after.as_micros()).unwrap_or(i64::MAX);
                    timers.push((sqe.token, Timestamp(now.0.saturating_add(micros))));
                    None
                }
                Ok(op) => {
                    held.push(Held {
                        token: sqe.token,
                        handle: sqe.handle,
                        op,
                        span: sqe.buf,
                    });
                    None
                }
                Err(e) => Some(Cqe::err(sqe.token, e)),
            };
            if let Some(cqe) = reply {
                let _ = replies.write(&cqe);
            }
        }
    }
}

/// Takes the op `token` names out of `held` or `timers`, and its
/// `Canceled` reply.
fn cancel(held: &mut Vec<Held>, timers: &mut Vec<(Token, Timestamp)>, token: Token) -> Option<Cqe> {
    if let Some(at) = held.iter().position(|h| h.token == token) {
        held.remove(at);
    } else {
        let at = timers.iter().position(|t| t.0 == token)?;
        timers.remove(at);
    }
    Some(Cqe::err(token, IoError::Canceled))
}

/// One pass over a [`TestPort`]'s ops.
pub struct Requests<'a> {
    port: &'a mut TestPort,
}

impl Requests<'_> {
    /// The next op of this pass.
    #[allow(
        clippy::should_implement_trait,
        reason = "a lending iterator: each request borrows the pass"
    )]
    pub fn next(&mut self) -> Option<Request<'_>> {
        let at = self.port.cursor;
        (at < self.port.held.len()).then(|| Request {
            port: &mut *self.port,
            at,
            answered: false,
        })
    }
}

/// An op to answer. Dropped unanswered, it waits for the next pass.
pub struct Request<'a> {
    port: &'a mut TestPort,
    at: usize,
    answered: bool,
}

impl Request<'_> {
    pub fn op(&self) -> Op {
        self.port.held[self.at].op
    }

    /// The handle the op was submitted on.
    pub fn handle(&self) -> RawHandle {
        self.port.held[self.at].handle
    }

    /// Whether the op was submitted on the capability, not on a handle it
    /// opened.
    pub fn on_cap(&self) -> bool {
        self.handle().generation() == 0
    }

    /// Answers an op that opens a handle with a new one, after the
    /// capability's grants, and returns it.
    pub fn open(mut self) -> RawHandle {
        self.port.opened += 1;
        let handle = RawHandle::new(0x100 + self.port.opened, 1);
        let token = self.port.held.remove(self.at).token;
        let _ = self.port.replies.write(&Cqe::ok(token, handle.get()));
        self.answered = true;
        handle
    }

    /// The op's buffer: the whole capacity for an op that fills it, the
    /// filled bytes otherwise.
    pub fn buf(&mut self) -> &mut [u8] {
        let span = self.port.held[self.at].span;
        match self.port.arena.get_slice(span) {
            // SAFETY: the system lent this span to the op until its reply,
            // and `&mut self` keeps two slices of it from coexisting.
            Some(buf) => unsafe { &mut *buf.as_ptr() },
            None => &mut [],
        }
    }

    /// Answers with a length or a GPIO level, or an error.
    pub fn done(mut self, result: Result<usize, IoError>) {
        let token = self.port.held.remove(self.at).token;
        let cqe = match result {
            Ok(n) => Cqe::ok(token, n as u32),
            Err(e) => Cqe::err(token, e),
        };
        let _ = self.port.replies.write(&cqe);
        self.answered = true;
    }
}

impl Drop for Request<'_> {
    fn drop(&mut self) {
        if !self.answered {
            self.port.cursor += 1;
        }
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
    connects: usize,
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

    /// How many connects it has answered.
    pub fn connects(&self) -> usize {
        self.connects
    }
}

impl ScriptedUart {
    /// Answers every op on `port` once: a connect opens a handle, a read
    /// takes the next chunk, a write is recorded, and a configure is kept. A
    /// read with nothing to send waits. A data op on the capability is
    /// `Unsupported`, as the dispatch has it.
    pub fn serve(&mut self, port: &mut TestPort) {
        let mut reqs = port.requests();
        while let Some(mut req) = reqs.next() {
            if req.op() == Op::Connect {
                self.connects += 1;
                req.open();
                continue;
            }
            if req.on_cap() {
                req.done(Err(IoError::Unsupported));
                continue;
            }
            let result = match req.op() {
                Op::Read { .. } => match self.rx(req.buf()) {
                    Some(n) => Ok(n),
                    None => continue,
                },
                Op::Write { .. } => self.tx(req.buf()),
                Op::TtyConfigure(cfg) => {
                    self.config = Some(cfg);
                    Ok(0)
                }
                Op::TtyFlush(_) | Op::Close => Ok(0),
                _ => Err(IoError::Unsupported),
            };
            req.done(result);
        }
    }

    /// Reads the next chunk. A chunk longer than `buf` gives a short read and
    /// keeps the rest.
    fn rx(&mut self, buf: &mut [u8]) -> Option<usize> {
        let chunk = self.script.front_mut()?;
        let n = chunk.len().min(buf.len());
        buf[..n].copy_from_slice(&chunk[..n]);
        chunk.drain(..n);
        if chunk.is_empty() {
            self.script.pop_front();
        }
        Some(n)
    }

    fn tx(&mut self, bytes: &[u8]) -> Result<usize, IoError> {
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
}
