//! `DeviceSide::serve`, which answers driver requests with a model.

use metor_proto::types::Timestamp;

use super::super::cap::{GpioLine, Interface, Uart};
use super::super::device::DeviceSide;
use super::super::{Cqe, Errno, IoError, Op, OpKind, Out, Sqe, Token};
use super::held::Held;
use super::model::{GpioModel, Reply, UartModel, fills, serve_gpio, serve_uart};

/// A model of a device on interface `K`.
pub trait Model<K: Interface> {
    fn tick(&mut self, now: Timestamp);
    /// How many serves an op of `kind` waits before the model sees it.
    fn latency(&self, kind: OpKind) -> u32;
    /// Answers one op. `bytes` is its buffer: the whole capacity for a read,
    /// the filled bytes otherwise.
    fn answer(&mut self, now: Timestamp, sqe: &Sqe, bytes: &mut [u8]) -> Reply;
}

impl<M: UartModel> Model<Uart> for M {
    fn tick(&mut self, now: Timestamp) {
        UartModel::tick(self, now);
    }

    fn latency(&self, kind: OpKind) -> u32 {
        UartModel::latency(self, kind)
    }

    fn answer(&mut self, now: Timestamp, sqe: &Sqe, bytes: &mut [u8]) -> Reply {
        serve_uart(self, now, sqe, bytes)
    }
}

impl<M: GpioModel> Model<GpioLine> for M {
    fn tick(&mut self, now: Timestamp) {
        GpioModel::tick(self, now);
    }

    fn latency(&self, kind: OpKind) -> u32 {
        GpioModel::latency(self, kind)
    }

    fn answer(&mut self, now: Timestamp, sqe: &Sqe, _bytes: &mut [u8]) -> Reply {
        serve_gpio(self, now, sqe)
    }
}

impl<K: Interface> DeviceSide<K> {
    /// Ticks `model` to `now`, takes new requests, then answers each held
    /// request that is due and that the model can answer. Replies the full
    /// reply port refuses wait in order for a later serve.
    pub fn serve(&mut self, now: Timestamp, model: &mut impl Model<K>) {
        self.serves += 1;
        model.tick(now);
        self.flush();
        self.take_requests(model);
        self.answer_due(now, model);
        self.flush();
    }

    /// Holds each new request. A cancel is answered at once.
    fn take_requests(&mut self, model: &impl Model<K>) {
        let Self {
            requests,
            outbox,
            held,
            serves,
            ..
        } = self;
        for sqe in requests.drain().flatten() {
            if let Op::Cancel { target, .. } = sqe.op {
                cancel(held, outbox, sqe.token, target);
                continue;
            }
            let ready_at = *serves + u64::from(model.latency(sqe.op.kind()));
            if let Err(e) = held.push(sqe, ready_at) {
                post(outbox, Cqe::err(sqe.token, e));
            }
        }
    }

    fn answer_due(&mut self, now: Timestamp, model: &mut impl Model<K>) {
        let mut at = 0;
        while let Some(entry) = self.held.get(at) {
            let due = entry.ready_at <= self.serves;
            match due.then(|| self.answer(now, model, &entry.sqe)).flatten() {
                Some(cqe) => {
                    self.held.remove_at(at);
                    post(&mut self.outbox, cqe);
                }
                None => at += 1,
            }
        }
    }

    /// Writes waiting replies, oldest first, until the port is full.
    fn flush(&mut self) {
        let replies = &self.replies;
        let sent = self.outbox.iter().take_while(|c| replies.write(c).is_ok());
        let sent = sent.count();
        self.outbox.drain(..sent);
    }

    /// The model's answer to `sqe`, or `None` while it has none.
    fn answer(&mut self, now: Timestamp, model: &mut impl Model<K>, sqe: &Sqe) -> Option<Cqe> {
        let bytes = match sqe.op.buf() {
            Some(buf) if fills(&sqe.op) => self.space(buf),
            Some(buf) => self.bytes(buf),
            None => Ok(&mut [][..]),
        };
        let reply = match bytes {
            Ok(bytes) => model.answer(now, sqe, bytes),
            Err(e) => return Some(Cqe::err(sqe.token, e)),
        };
        match reply {
            Reply::Ready(status, out) => Some(Cqe::new(sqe.token, status, out)),
            Reply::Later => None,
        }
    }
}

/// Completes the held op `target` `Canceled`. A target not held fails the
/// cancel with `ENOENT`.
fn cancel(held: &mut Held, outbox: &mut Vec<Cqe>, token: Token, target: Token) {
    let Some(op) = held.remove(target) else {
        post(outbox, Cqe::err(token, IoError::Os(Errno::ENOENT)));
        return;
    };
    post(outbox, Cqe::err(op.token, IoError::Canceled));
    post(outbox, Cqe::ok(token, Out::UNIT));
}

/// Queues a reply. The outbox holds every reply its ops can give, so it is
/// never full.
fn post(outbox: &mut Vec<Cqe>, cqe: Cqe) {
    if outbox.len() < outbox.capacity() {
        outbox.push(cqe);
    }
}
