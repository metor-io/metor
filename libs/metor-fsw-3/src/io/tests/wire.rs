use std::net::SocketAddr;

use zerocopy::{FromBytes, IntoBytes};

use crate::io::*;
use crate::record::{DecodeError, Payload};

fn ops() -> Vec<Op> {
    let addr: SocketAddr = "[::1]:9000".parse().unwrap();
    vec![
        Op::Read { offset: None },
        Op::Read { offset: Some(7) },
        Op::Write { offset: Some(0) },
        Op::Recv,
        Op::Send,
        Op::SendTo("10.0.0.2:53".parse().unwrap()),
        Op::SendTo(addr),
        Op::Open(OpenFlags::READ | OpenFlags::CREATE.with_mode(0o600)),
        Op::Close,
        Op::Fsync,
        Op::Accept,
        Op::TtyConfigure(TtyConfig::new(Baud::B115200)),
        Op::TtyFlush(TtyQueue::Both),
        Op::GpioGet,
        Op::GpioSet(true),
        Op::Cancel,
    ]
}

#[test]
fn every_op_round_trips_through_an_sqe() {
    for op in ops() {
        let sqe = Sqe::new(Token::new(3, 9), RawHandle::new(1, 2), &op, Span::new(4, 8));
        let back = Sqe::decode(sqe.as_bytes()).unwrap();
        assert_eq!(back.op(), Ok(op));
        assert_eq!(
            (back.token, back.handle, back.buf),
            (sqe.token, sqe.handle, sqe.buf)
        );
    }
}

#[test]
fn unknown_opcode_is_unsupported() {
    let mut sqe = Sqe::new(
        Token::new(0, 0),
        RawHandle::new(0, 0),
        &Op::Recv,
        Span::default(),
    );
    sqe.opcode = 0xEE;
    assert_eq!(sqe.op(), Err(IoError::Unsupported));
    sqe = Sqe::new(
        sqe.token,
        sqe.handle,
        &Op::TtyFlush(TtyQueue::Input),
        Span::default(),
    );
    sqe.aux = 9;
    assert_eq!(sqe.op(), Err(IoError::Unsupported));
}

#[test]
fn bad_send_address_is_bad_addr() {
    let mut sqe = Sqe::new(
        Token::new(0, 0),
        RawHandle::new(0, 0),
        &Op::SendTo("1.2.3.4:5".parse().unwrap()),
        Span::default(),
    );
    sqe.arg[0] = 77;
    assert_eq!(sqe.op(), Err(IoError::BadAddr));
}

#[test]
fn short_records_are_truncated_and_trailing_bytes_ignored() {
    let cqe = Cqe::ok(Token::new(1, 1), 5);
    assert_eq!(
        Cqe::decode(&cqe.as_bytes()[..11]),
        Err(DecodeError::Truncated)
    );
    let mut long = cqe.as_bytes().to_vec();
    long.extend([0xAA; 4]);
    assert_eq!(Cqe::decode(&long), Ok(cqe));
}

#[test]
fn status_codes_round_trip() {
    let errors = [
        IoError::Os(Errno::EIO),
        IoError::Os(Errno::new(4000).unwrap()),
        IoError::QueueFull,
        IoError::Stale,
        IoError::Canceled,
        IoError::Refused,
        IoError::Unsupported,
        IoError::BadBuf,
        IoError::BadAddr,
    ];
    for e in errors {
        assert_eq!(IoStatus::err(e).result(), Err(e));
        assert_eq!(Cqe::err(Token::new(0, 0), e).result(), Err(e));
    }
    assert_eq!(Errno::new(0), None);
    assert_eq!(IoStatus::OK.result(), Ok(()));
    let unknown = IoStatus::read_from_bytes(&0x1_00FF_u32.to_le_bytes()).unwrap();
    assert_eq!(unknown.result(), Err(IoError::Refused));
}

#[test]
fn errno_names_parse_back() {
    assert_eq!(Errno::parse("ECONNREFUSED"), Some(Errno::ECONNREFUSED));
    assert_eq!(format!("{:?}", Errno::new(4000).unwrap()), "Errno(4000)");
    assert_eq!(Errno::parse("ENOPE"), None);
}

#[test]
fn tokens_and_handles_pack_slot_and_generation() {
    let t = Token::new(u16::MAX, 7);
    assert_eq!((t.index(), t.generation()), (u16::MAX, 7));
    let h = RawHandle::new(3, u16::MAX);
    assert_eq!((h.index(), h.generation()), (3, u16::MAX));
    assert_eq!(RawHandle::from_raw(h.get()), h);
}

#[test]
fn baud_parses_only_supported_rates() {
    assert_eq!(Baud::try_from(115_200), Ok(Baud::B115200));
    assert_eq!(u32::from(Baud::B1000000), 1_000_000);
    assert_eq!(Baud::try_from(12_345), Err(UnsupportedBaud(12_345)));
    let parsed: Baud = serde_json::from_value(serde_json::json!(9600)).unwrap();
    assert_eq!(parsed, Baud::B9600);
    assert!(serde_json::from_value::<Baud>(serde_json::json!(0)).is_err());
}

#[test]
fn bad_tty_settings_do_not_decode() {
    let op = Op::TtyConfigure(TtyConfig::new(Baud::B9600));
    let mut sqe = Sqe::new(Token::new(0, 0), RawHandle::new(0, 0), &op, Span::default());
    assert_eq!(sqe.op(), Ok(op));
    sqe.arg[1] = 9;
    assert_eq!(sqe.op(), Err(IoError::Os(Errno::EINVAL)));
    let flush = Op::TtyFlush(TtyQueue::Both);
    let mut sqe = Sqe::new(
        Token::new(0, 0),
        RawHandle::new(0, 0),
        &flush,
        Span::default(),
    );
    sqe.aux = 3;
    assert_eq!(sqe.op(), Err(IoError::Unsupported));
}

#[test]
fn open_flags_keep_one_mode() {
    let plain = OpenFlags::READ | OpenFlags::CREATE;
    assert_eq!(plain.mode(), OpenFlags::DEFAULT_MODE);
    let right = OpenFlags::READ | OpenFlags::CREATE.with_mode(0o600);
    assert_eq!(right.mode(), 0o600, "an unset left mode takes the right");
    let both = OpenFlags::WRITE.with_mode(0o640) | OpenFlags::CREATE.with_mode(0o600);
    assert_eq!(both.mode(), 0o640, "the left mode wins");
    assert!(both.contains(OpenFlags::WRITE | OpenFlags::CREATE));
}

#[test]
fn current_position_is_no_offset() {
    let token = Token::new(0, 1);
    for offset in [None, Some(0), Some(CURRENT_POSITION - 1)] {
        let sqe = Sqe::new(
            token,
            RawHandle::new(0, 0),
            &Op::Read { offset },
            Span::default(),
        );
        assert_eq!(sqe.op(), Ok(Op::Read { offset }));
    }
    let sqe = Sqe::new(
        token,
        RawHandle::new(0, 0),
        &Op::Read { offset: None },
        Span::default(),
    );
    assert_eq!(sqe.offset.get(), CURRENT_POSITION);
}
