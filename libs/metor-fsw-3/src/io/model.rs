//! Simulated peripherals: traits a model implements, and the functions that
//! answer one op with them.

use metor_proto::types::Timestamp;

use super::{IoError, IoStatus, Op, Out, Sqe, TtyConfig, TtyOp};

/// A simulated serial device.
pub trait UartModel {
    /// Fills `buf` with received bytes. `None` when there are none yet; the
    /// read waits for a later tick.
    fn rx(&mut self, now: Timestamp, buf: &mut [u8]) -> Option<usize>;
    fn tx(&mut self, now: Timestamp, bytes: &[u8]) -> Result<usize, IoError>;
    fn configure(&mut self, now: Timestamp, cfg: &TtyConfig) -> Result<(), IoError>;
    fn tick(&mut self, now: Timestamp);
}

/// A model's answer to one op.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
    Ready(IoStatus, Out),
    /// Not ready yet; ask again on a later serve.
    Later,
}

impl Reply {
    fn of(result: Result<Out, IoError>) -> Self {
        match result {
            Ok(out) => Self::Ready(IoStatus::OK, out),
            Err(e) => Self::Ready(IoStatus::err(e), Out::UNIT),
        }
    }
}

/// Whether an op's model sees its buffer's whole capacity, to fill, rather
/// than its filled bytes.
pub fn fills(op: &Op) -> bool {
    matches!(op, Op::Read { .. } | Op::Recv { .. })
}

/// Answers one op on a serial port. `bytes` is the op's buffer: its whole
/// capacity for a read, its filled bytes for a write.
pub fn serve_uart(
    model: &mut impl UartModel,
    now: Timestamp,
    sqe: &Sqe,
    bytes: &mut [u8],
) -> Reply {
    match &sqe.op {
        Op::Read { .. } => match model.rx(now, bytes) {
            Some(n) => Reply::of(Ok(Out::len(n as u32))),
            None => Reply::Later,
        },
        Op::Write { .. } => Reply::of(model.tx(now, bytes).map(|n| Out::len(n as u32))),
        Op::Tty {
            op: TtyOp::Configure(cfg),
            ..
        } => Reply::of(model.configure(now, cfg).map(|()| Out::UNIT)),
        Op::Tty {
            op: TtyOp::Flush { .. },
            ..
        }
        | Op::Close { .. } => Reply::of(Ok(Out::UNIT)),
        _ => Reply::of(Err(IoError::Unsupported)),
    }
}
