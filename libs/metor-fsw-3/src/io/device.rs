//! `DeviceSide<K>`, the end of a simulated resource that a model system
//! serves, and the traits a model implements.
//!
//! Each request carries its op table index as its token and a span of its
//! driver's arena. Its driver is member `index / MAX_OPS`, so the model reads
//! and fills the driver's bytes in place.

use core::cell::RefCell;
use core::marker::PhantomData;
use std::rc::Rc;

use metor_proto::types::Timestamp;

use super::arena::Arena;
use super::cap::{GpioLine, Interface, Uart};
use super::{Cqe, IoError, MAX_OPS, Op, OpKind, Sqe, TtyConfig};
use crate::port::{Input, Output};

/// A simulated serial device.
pub trait UartModel {
    /// Fills `buf` with received bytes. `None` when there are none yet; the
    /// read waits for a later serve.
    fn rx(&mut self, now: Timestamp, buf: &mut [u8]) -> Option<usize>;
    fn tx(&mut self, now: Timestamp, bytes: &[u8]) -> Result<usize, IoError>;
    fn configure(&mut self, now: Timestamp, cfg: &TtyConfig) -> Result<(), IoError>;
    fn tick(&mut self, now: Timestamp);
    /// How many serves an op of `kind` waits before the model sees it.
    fn latency(&self, kind: OpKind) -> u32 {
        let _ = kind;
        0
    }
}

/// A simulated GPIO line.
pub trait GpioModel {
    fn get(&mut self, now: Timestamp) -> Result<bool, IoError>;
    fn set(&mut self, now: Timestamp, level: bool) -> Result<(), IoError>;
    fn tick(&mut self, now: Timestamp);
    /// How many serves an op of `kind` waits before the model sees it.
    fn latency(&self, kind: OpKind) -> u32 {
        let _ = kind;
        0
    }
}

/// A model's answer to one op.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
    /// A length, a level, or an error.
    Ready(Result<u32, IoError>),
    /// Not ready yet; ask again on a later serve.
    Later,
}

/// A model of a device on interface `K`.
pub trait Model<K: Interface> {
    fn tick(&mut self, now: Timestamp);
    /// How many serves an op of `kind` waits before the model sees it.
    fn latency(&self, kind: OpKind) -> u32;
    /// Answers one op. `bytes` is its buffer: the whole capacity for an op
    /// that fills it, the filled bytes otherwise.
    fn answer(&mut self, now: Timestamp, op: &Op, bytes: &mut [u8]) -> Reply;
}

impl<M: UartModel> Model<Uart> for M {
    fn tick(&mut self, now: Timestamp) {
        UartModel::tick(self, now);
    }

    fn latency(&self, kind: OpKind) -> u32 {
        UartModel::latency(self, kind)
    }

    fn answer(&mut self, now: Timestamp, op: &Op, bytes: &mut [u8]) -> Reply {
        let len = |n: usize| n as u32;
        Reply::Ready(match op {
            Op::Read { .. } => match self.rx(now, bytes) {
                Some(n) => Ok(len(n)),
                None => return Reply::Later,
            },
            Op::Write { .. } => self.tx(now, bytes).map(len),
            Op::TtyConfigure(cfg) => self.configure(now, cfg).map(|()| 0),
            Op::TtyFlush(_) => Ok(0),
            _ => Err(IoError::Unsupported),
        })
    }
}

impl<M: GpioModel> Model<GpioLine> for M {
    fn tick(&mut self, now: Timestamp) {
        GpioModel::tick(self, now);
    }

    fn latency(&self, kind: OpKind) -> u32 {
        GpioModel::latency(self, kind)
    }

    fn answer(&mut self, now: Timestamp, op: &Op, _bytes: &mut [u8]) -> Reply {
        Reply::Ready(match op {
            Op::GpioGet => self.get(now).map(u32::from),
            Op::GpioSet(level) => self.set(now, *level).map(|()| 0),
            _ => Err(IoError::Unsupported),
        })
    }
}

/// Every driver's arena, by member index. Filled as drivers bind.
#[derive(Default)]
pub(crate) struct Arenas(RefCell<Vec<Rc<Arena>>>);

impl Arenas {
    pub(crate) fn push(&self, arena: Rc<Arena>) {
        self.0.borrow_mut().push(arena);
    }

    /// The bytes request `sqe` lends, in its driver's arena.
    fn bytes(&self, sqe: &Sqe) -> Option<core::ptr::NonNull<[u8]>> {
        let member = usize::from(sqe.token.slot()) / MAX_OPS;
        self.0.try_borrow().ok()?.get(member)?.span(sqe.buf)
    }
}

#[derive(Clone, Copy)]
struct Held {
    sqe: Sqe,
    op: Op,
    /// The serve at which the model first sees the op.
    ready_at: u64,
}

/// One device end, before its parameter types its interface.
pub(crate) struct Device {
    requests: Input<Sqe>,
    replies: Output<Cqe>,
    arenas: Rc<Arenas>,
    held: Vec<Held>,
    /// Replies the reply port has not taken yet, oldest first.
    outbox: Vec<Cqe>,
    serves: u64,
}

impl Device {
    /// A device end that holds at most `held` ops.
    pub(crate) fn new(
        requests: Input<Sqe>,
        replies: Output<Cqe>,
        arenas: Rc<Arenas>,
        held: usize,
    ) -> Self {
        Self {
            requests,
            replies,
            arenas,
            held: Vec::with_capacity(held),
            // Each held op gives one reply, and each refused one another.
            outbox: Vec::with_capacity(2 * held),
            serves: 0,
        }
    }

    pub(crate) fn typed<K: Interface>(self) -> DeviceSide<K> {
        DeviceSide {
            device: self,
            _k: PhantomData,
        }
    }
}

/// The device end of a simulated resource: requests from drivers come in,
/// and the model's replies go back.
pub struct DeviceSide<K: Interface> {
    device: Device,
    _k: PhantomData<K>,
}

impl<K: Interface> DeviceSide<K> {
    /// Ticks `model` to `now`, takes new requests, then answers each held
    /// request that is due and that the model can answer. Replies the full
    /// reply port refuses wait in order for a later serve.
    pub fn serve(&mut self, now: Timestamp, model: &mut impl Model<K>) {
        let d = &mut self.device;
        d.serves += 1;
        model.tick(now);
        d.flush();
        d.take(|kind| model.latency(kind));
        d.answer_due(now, model);
        d.flush();
    }
}

impl Device {
    /// Holds each new request. A cancel takes its target out unanswered,
    /// and completes it `Canceled`.
    fn take(&mut self, latency: impl Fn(OpKind) -> u32) {
        let Self {
            requests,
            held,
            outbox,
            serves,
            ..
        } = self;
        for sqe in requests.drain().flatten() {
            let op = match sqe.op() {
                Ok(Op::Cancel) => {
                    cancel(held, outbox, sqe.token);
                    continue;
                }
                Ok(op) => op,
                Err(e) => {
                    post(outbox, Cqe::err(sqe.token, e));
                    continue;
                }
            };
            if held.len() == held.capacity() {
                post(outbox, Cqe::err(sqe.token, IoError::QueueFull));
                continue;
            }
            let ready_at = *serves + u64::from(latency(op.kind()));
            held.push(Held { sqe, op, ready_at });
        }
    }

    fn answer_due<K: Interface>(&mut self, now: Timestamp, model: &mut impl Model<K>) {
        let mut at = 0;
        while let Some(&held) = self.held.get(at) {
            let reply = match held.ready_at <= self.serves {
                true => self.answer(now, model, &held),
                false => Reply::Later,
            };
            match reply {
                Reply::Ready(result) => {
                    self.held.remove(at);
                    let token = held.sqe.token;
                    post(
                        &mut self.outbox,
                        result.map_or_else(|e| Cqe::err(token, e), |n| Cqe::ok(token, n)),
                    );
                }
                Reply::Later => at += 1,
            }
        }
    }

    fn answer<K: Interface>(
        &self,
        now: Timestamp,
        model: &mut impl Model<K>,
        held: &Held,
    ) -> Reply {
        if !held.op.fills() && !held.op.drains() {
            return model.answer(now, &held.op, &mut []);
        }
        match self.arenas.bytes(&held.sqe) {
            // SAFETY: the driver lent this span to the request until its
            // reply is written, and models run on the coordinator thread.
            Some(bytes) => model.answer(now, &held.op, unsafe { &mut *bytes.as_ptr() }),
            None => Reply::Ready(Err(IoError::BadBuf)),
        }
    }

    /// Writes waiting replies, oldest first, until the port is full.
    fn flush(&mut self) {
        let replies = &self.replies;
        let sent = self
            .outbox
            .iter()
            .take_while(|c| replies.write(c).is_ok())
            .count();
        self.outbox.drain(..sent);
    }
}

/// Completes the held op `token` `Canceled`, if it is held.
fn cancel(held: &mut Vec<Held>, outbox: &mut Vec<Cqe>, token: super::Token) {
    if let Some(at) = held.iter().position(|h| h.sqe.token == token) {
        let held = held.remove(at);
        post(outbox, Cqe::err(held.sqe.token, IoError::Canceled));
    }
}

/// Queues a reply. The outbox holds every reply its held ops can give.
fn post(outbox: &mut Vec<Cqe>, cqe: Cqe) {
    if outbox.len() < outbox.capacity() {
        outbox.push(cqe);
    }
}
