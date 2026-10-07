#![cfg(test)]

use super::Rig;
use crate::io::queue::Io;
use crate::io::*;

fn handle<K>(io: &Io, index: u32) -> Handle<K> {
    Handle::new(RawHandle::new(index, 1), io.clone())
}

#[test]
fn uart_methods_write_their_ops() {
    let mut rig = Rig::new();
    let uart: Handle<Uart> = handle(&rig.io, 2);
    let buf = rig.empty_buf(4);
    let slot = buf.buf_ref();
    let _read = uart.read(buf);
    let _write = uart.write(rig.buf(b"w"));
    let _config = uart.configure(TtyConfig::new(9600));
    let _flush = uart.flush(TtyQueue::Both);
    let _close = uart.close();
    let sent = rig.sent();
    assert!(sent.iter().all(|s| s.handle == RawHandle::new(2, 1)));
    let ops: Vec<_> = sent.iter().map(|s| s.op).collect();
    assert_eq!(ops[0], Op::read(slot, None));
    assert_eq!(ops[1].kind(), OpKind::Write);
    assert_eq!(ops[2], Op::tty(TtyOp::Configure(TtyConfig::new(9600))));
    assert_eq!(ops[3], Op::tty(TtyOp::flush(2)));
    assert_eq!(ops[4], Op::CLOSE);
}

#[test]
fn other_interfaces_write_their_ops() {
    let mut rig = Rig::new();
    let udp: Handle<Udp> = handle(&rig.io, 0);
    let file: Handle<File> = handle(&rig.io, 1);
    let listen: Handle<Listen> = handle(&rig.io, 2);
    let gpio: Handle<GpioLine> = handle(&rig.io, 3);
    let _a = udp.recv(rig.empty_buf(4));
    let _b = udp.send(rig.buf(b"s"));
    let _c = file.read(rig.empty_buf(4), 8);
    let _d = file.write(rig.buf(b"f"), 16);
    let _e = file.fsync();
    let _f = listen.accept();
    let _g = gpio.get();
    let _h = gpio.set(7);
    let ops: Vec<_> = rig.sent().iter().map(|s| s.op).collect();
    let kinds: Vec<_> = ops.iter().map(Op::kind).collect();
    use OpKind as K;
    assert_eq!(
        kinds,
        [
            K::Recv,
            K::Send,
            K::Read,
            K::Write,
            K::Fsync,
            K::Accept,
            K::Gpio,
            K::Gpio
        ]
    );
    assert_eq!(ops[2].offset(), Some(8));
    assert_eq!(ops[3].offset(), Some(16));
    assert_eq!(ops[6], Op::gpio(GpioOp::GET));
    assert_eq!(ops[7], Op::gpio(GpioOp::set(1)));
}

#[test]
fn open_gives_a_file_on_the_same_queue() {
    let mut rig = Rig::new();
    let dir: Handle<Dir> = handle(&rig.io, 0);
    let mut open = dir.open(rig.buf(b"log.bin"), OpenFlags::WRITE | OpenFlags::CREATE);
    let sent = rig.sent();
    assert_eq!(sent[0].op.kind(), OpKind::Open);
    let opened = RawHandle::new(9, 4);
    rig.reply(Cqe::ok(sent[0].token, Out::handle(opened)));
    let file = open.poll().unwrap().unwrap();
    assert_eq!(file.raw(), opened);
    let _sync = file.fsync();
    assert_eq!(rig.sent()[0].handle, opened);
}

#[test]
fn stale_close_is_an_error() {
    let mut rig = Rig::new();
    let uart: Handle<Uart> = handle(&rig.io, 0);
    let mut close = uart.close();
    let token = rig.sent()[0].token;
    rig.reply(Cqe::err(token, IoError::Stale));
    assert_eq!(close.poll(), Some(Err(IoError::Stale)));
}

#[test]
fn gpio_get_reads_the_line() {
    let mut rig = Rig::new();
    let line: Handle<GpioLine> = handle(&rig.io, 0);
    let mut get = line.get();
    let token = rig.sent()[0].token;
    rig.reply(Cqe::ok(token, Out::len(1)));
    assert_eq!(get.poll(), Some(Ok(1)));
}
