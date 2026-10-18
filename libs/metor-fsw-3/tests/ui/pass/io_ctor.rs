use metor_fsw_3::io::{Cap, Uart, WriteOp};
use metor_fsw_3::{IoInit, ParamError, Slot, SystemTable, system};

struct Driver {
    tx: Slot<WriteOp>,
}

impl Driver {
    fn new(io: &mut IoInit) -> Result<Self, ParamError> {
        Ok(Self {
            tx: Slot::Idle(io.alloc_from(b"hello")?),
        })
    }
}

/// Writes its greeting once.
#[system]
fn driver(#[state] s: &mut Driver, uart: &Cap<Uart>) {
    let _ = s.tx.poll();
    if let Some(buf) = s.tx.take_idle() {
        s.tx = Slot::Busy(uart.write(buf));
    }
}

fn main() {
    let mut table = SystemTable::new();
    table.register("driver", driver(), Driver::new).unwrap();
}
