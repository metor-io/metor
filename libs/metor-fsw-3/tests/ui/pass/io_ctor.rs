use metor_fsw_3::io::{Cap, Reconnect, Uart, WriteOp};
use metor_fsw_3::{IoInit, MakeError, Slot, SystemTable, system};

struct Driver {
    link: Reconnect<Uart>,
    tx: Slot<WriteOp>,
}

impl Driver {
    fn new(io: &mut IoInit) -> Result<Self, MakeError> {
        Ok(Self {
            link: Reconnect::new(),
            tx: Slot::Idle(io.alloc_from(b"hello")?),
        })
    }
}

/// Connects, then writes its greeting once.
#[system]
fn driver(#[state] s: &mut Driver, uart: &Cap<Uart>) {
    s.link.poll(uart);
    let _ = s.tx.poll();
    if let Some(port) = s.link.handle()
        && let Some(buf) = s.tx.take_idle()
    {
        s.tx = Slot::Busy(port.write(buf));
    }
}

fn main() {
    let mut table = SystemTable::new();
    table.register("driver", driver(), Driver::new).unwrap();
}
