#![cfg(test)]

use crate::io::test::{ScriptedUart, cap, device};
use crate::io::*;

#[test]
fn scripted_read_lands_the_turn_after() {
    let (uart, mut io) = cap::<Uart, _>(ScriptedUart::scripted([b"$GP".to_vec()]));
    let mut read = uart.read(io.init().alloc(16));
    assert!(read.poll().is_none());
    io.turn();
    let (result, buf) = read.poll().unwrap();
    assert_eq!(result, Ok(3));
    assert_eq!(&buf[..], b"$GP");
}

#[test]
fn write_reaches_the_model() {
    let (uart, mut io) = cap::<Uart, _>(ScriptedUart::silent());
    let mut write = uart.write(io.init().alloc_from(b"$PMTK"));
    io.turn();
    assert_eq!(write.poll().map(|(r, _)| r), Some(Ok(5)));
    assert_eq!(io.model().written(), [b"$PMTK".to_vec()]);
    assert_eq!(io.submitted().len(), 1);
}

#[test]
fn silent_read_waits_for_bytes() {
    let (uart, mut io) = cap::<Uart, _>(ScriptedUart::silent());
    let mut read = uart.read(io.init().alloc(8));
    for _ in 0..10 {
        io.turn();
        assert!(read.poll().is_none());
    }
    io.model_mut().push(b"late");
    io.turn();
    assert_eq!(read.poll().map(|(r, _)| r), Some(Ok(4)));
}

#[test]
fn long_chunk_gives_a_short_read() {
    let (uart, mut io) = cap::<Uart, _>(ScriptedUart::scripted([b"abcdef".to_vec()]));
    let mut first = uart.read(io.init().alloc(4));
    io.turn();
    let (_, buf) = first.poll().unwrap();
    assert_eq!(&buf[..], b"abcd");
    let mut second = uart.read(buf);
    io.turn();
    assert_eq!(&second.poll().unwrap().1[..], b"ef");
}

#[test]
fn configure_reaches_the_model() {
    let (uart, mut io) = cap::<Uart, _>(ScriptedUart::silent());
    let mut config = uart.configure(TtyConfig::new(9600));
    io.turn();
    assert_eq!(config.poll(), Some(Ok(())));
    assert_eq!(io.model().config().map(|c| c.baud()), Some(9600));
}

#[test]
fn cancel_of_a_held_read() {
    let (uart, mut io) = cap::<Uart, _>(ScriptedUart::silent());
    let mut read = uart.read(io.init().alloc(8));
    io.turn();
    read.cancel();
    io.turn();
    assert_eq!(read.poll().map(|(r, _)| r), Some(Err(IoError::Canceled)));
}

#[test]
fn failed_write_hands_the_buffer_back() {
    let model = ScriptedUart::silent().fail_write(Errno::EIO);
    let (uart, mut io) = cap::<Uart, _>(model);
    let mut write = uart.write(io.init().alloc_from(b"cmd"));
    io.turn();
    let (result, buf) = write.poll().unwrap();
    assert_eq!(result, Err(IoError::Os(Errno::EIO)));
    assert_eq!(&buf[..], b"cmd");
    let mut again = uart.write(buf);
    io.turn();
    assert_eq!(again.poll().map(|(r, _)| r), Some(Ok(3)));
    assert_eq!(io.model().written(), [b"cmd".to_vec()]);
}

#[test]
fn device_reads_a_write_payload() {
    let (mut dev, mut driver) = device::<Uart>();
    let token = driver
        .request(Op::write(BufRef::new(0), None), b"$PMTK")
        .unwrap();
    let reqs: Vec<Sqe> = dev.requests().collect();
    assert_eq!(reqs.len(), 1);
    let buf = reqs[0].op.buf().unwrap();
    assert_eq!(dev.bytes(buf).unwrap(), b"$PMTK");
    dev.reply(Cqe::ok(token, Out::len(5))).unwrap();
    assert_eq!(driver.replies(), [Cqe::ok(token, Out::len(5))]);
}

#[test]
fn device_fills_a_read_buffer() {
    let (mut dev, mut driver) = device::<Uart>();
    let token = driver.request(Op::read(BufRef::new(0), None), b"").unwrap();
    let req = dev.requests().next().unwrap();
    let space = dev.space(req.op.buf().unwrap()).unwrap();
    space[..3].copy_from_slice(b"fix");
    dev.reply(Cqe::ok(token, Out::len(3))).unwrap();
    driver.replies();
    assert_eq!(driver.driver_bytes(token), b"fix");
}

#[test]
fn two_requests_resolve_to_their_own_bytes() {
    let (mut dev, mut driver) = device::<Uart>();
    driver
        .request(Op::write(BufRef::new(0), None), b"one")
        .unwrap();
    driver
        .request(Op::write(BufRef::new(0), None), b"two")
        .unwrap();
    let reqs: Vec<Sqe> = dev.requests().collect();
    assert_eq!(dev.bytes(reqs[0].op.buf().unwrap()).unwrap(), b"one");
    assert_eq!(dev.bytes(reqs[1].op.buf().unwrap()).unwrap(), b"two");
}

#[test]
fn bad_driver_buffers_are_bad_buf() {
    let (mut dev, mut driver) = device::<Uart>();
    let mut stray = BufRef::new(0);
    stray.stamp(TableId(9));
    let unknown = Token::new(900);
    driver.send(Sqe::new(
        unknown,
        RawHandle::new(0, 0),
        Op::write(stray, None),
    ));
    let released = driver
        .request(Op::write(BufRef::new(0), None), b"x")
        .unwrap();
    driver.release(released);
    let reqs: Vec<Sqe> = dev.requests().collect();
    for req in &reqs {
        assert_eq!(dev.bytes(req.op.buf().unwrap()), Err(IoError::BadBuf));
        dev.reply(Cqe::err(req.token, IoError::BadBuf)).unwrap();
    }
    let replies = driver.replies();
    assert!(
        replies
            .iter()
            .all(|c| c.status.result() == Err(IoError::BadBuf))
    );
    assert_eq!(replies.len(), 2);
}
