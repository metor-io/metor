use crate::io::*;

#[cfg(test)]
mod unit {
    use core::net::SocketAddr;

    use zerocopy::{IntoBytes, TryFromBytes};

    use super::*;

    fn addr(s: &str) -> RawAddr {
        RawAddr::from(s.parse::<SocketAddr>().unwrap())
    }

    fn every_op() -> [Op; 14] {
        let buf = BufRef::new(3);
        [
            Op::read(buf, Some(7)),
            Op::write(buf, None),
            Op::recv(buf),
            Op::send(buf),
            Op::send_to(buf, addr("10.0.0.1:9")),
            Op::open(buf, OpenFlags::READ | OpenFlags::CREATE),
            Op::ACCEPT,
            Op::RESOLVE,
            Op::connect(addr("[::1]:80")),
            Op::CLOSE,
            Op::FSYNC,
            Op::tty(TtyOp::Configure(TtyConfig::new(9600))),
            Op::gpio(GpioOp::set(1)),
            Op::cancel(Token::new(4)),
        ]
    }

    #[test]
    fn sqe_round_trips_every_op() {
        for (op, kind) in every_op().into_iter().zip(OpKind::ALL) {
            let sqe = Sqe::new(Token::new(9), RawHandle::new(2, 5), op);
            let back = Sqe::try_read_from_bytes(sqe.as_bytes()).unwrap();
            assert_eq!(back, sqe);
            assert_eq!(back.op.kind(), kind);
            let has_buf = kind as u8 <= OpKind::Open as u8;
            assert_eq!(back.op.buf().is_some(), has_buf, "{kind:?}");
        }
    }

    #[test]
    fn cqe_round_trips_every_out() {
        let outs = [
            Out::UNIT,
            Out::len(12),
            Out::handle(RawHandle::new(1, 2)),
            Out::addr(addr("127.0.0.1:5")),
        ];
        for out in outs {
            let cqe = Cqe::ok(Token::new(1), out);
            assert_eq!(Cqe::try_read_from_bytes(cqe.as_bytes()).unwrap(), cqe);
        }
        let cqe = Cqe::err(Token::new(2), IoError::Stale);
        let back = Cqe::try_read_from_bytes(cqe.as_bytes()).unwrap();
        assert_eq!(back.status.result(), Err(IoError::Stale));
    }

    #[test]
    fn addresses_round_trip() {
        for s in ["192.168.1.7:4000", "[fe80::1%3]:22", "[2001:db8::2]:0"] {
            let a: SocketAddr = s.parse().unwrap();
            assert_eq!(SocketAddr::try_from(RawAddr::from(a)), Ok(a));
        }
    }

    #[test]
    fn scope_id_survives() {
        let a: SocketAddr = "[fe80::1%7]:1".parse().unwrap();
        let back = SocketAddr::try_from(RawAddr::from(a)).unwrap();
        let SocketAddr::V6(v6) = back else { panic!() };
        assert_eq!(v6.scope_id(), 7);
    }

    #[test]
    fn current_position_is_max_offset() {
        let op = Op::read(BufRef::new(0), None);
        assert_eq!(op.offset(), None);
        assert_eq!(&op.as_bytes()[9..17], &u64::MAX.to_le_bytes());
        assert_eq!(Op::write(BufRef::new(0), Some(3)).offset(), Some(3));
    }

    #[test]
    fn ok_status_is_ok() {
        assert_eq!(IoStatus::OK.result(), Ok(()));
    }

    #[test]
    fn op_kind_names_parse() {
        for k in OpKind::ALL {
            assert_eq!(OpKind::parse(k.name()), Some(k));
        }
        assert_eq!(OpKind::SendTo.name(), "send_to");
        assert_eq!(OpKind::parse("nope"), None);
    }

    #[test]
    fn errno_names_parse() {
        for (e, n) in Errno::NAMED {
            assert_eq!(Errno::parse(n), Some(e));
            assert_eq!(e.name(), Some(n));
        }
        assert_eq!(Errno::parse("EWHAT"), None);
    }

    #[test]
    fn errors_round_trip_through_status() {
        let errs = [
            IoError::Os(Errno::EIO),
            IoError::QueueFull,
            IoError::Stale,
            IoError::Canceled,
            IoError::Refused,
            IoError::Device,
            IoError::Unsupported,
            IoError::BadBuf,
            IoError::BadAddr,
        ];
        for e in errs {
            assert_eq!(IoStatus::err(e).result(), Err(e));
        }
    }

    #[test]
    fn tag_past_last_variant_fails() {
        let mut bytes = Sqe::new(Token::new(0), RawHandle::new(0, 0), Op::CLOSE)
            .as_bytes()
            .to_vec();
        bytes[12] = 14;
        assert!(Sqe::try_read_from_bytes(&bytes).is_err());
        bytes[12] = 13;
        assert!(Sqe::try_read_from_bytes(&bytes).is_ok());
    }

    #[test]
    fn short_slice_fails() {
        let sqe = Sqe::new(Token::new(0), RawHandle::new(0, 0), Op::CLOSE);
        assert!(Sqe::try_read_from_bytes(&sqe.as_bytes()[..47]).is_err());
        let cqe = Cqe::ok(Token::new(0), Out::UNIT);
        assert!(Cqe::try_read_from_bytes(&cqe.as_bytes()[..39]).is_err());
    }

    #[test]
    fn unknown_status_codes_decode() {
        let errno = IoStatus::try_read_from_bytes(&4095u32.to_le_bytes()).unwrap();
        assert_eq!(errno.result(), Err(IoError::Os(Errno(4095))));
        let framework = IoStatus::try_read_from_bytes(&0x1_0042u32.to_le_bytes()).unwrap();
        assert_eq!(framework.result(), Err(IoError::Refused));
    }

    #[test]
    fn family_zero_is_bad_addr() {
        let mut a = addr("1.2.3.4:5");
        a.as_mut_bytes()[0] = 0;
        assert_eq!(SocketAddr::try_from(a), Err(IoError::BadAddr));
    }
}

#[cfg(kani)]
mod proofs {
    use zerocopy::{IntoBytes, TryFromBytes};

    use super::*;

    #[kani::proof]
    fn sqe_decode_reencodes() {
        let bytes: [u8; 48] = kani::any();
        if let Ok(sqe) = Sqe::try_read_from_bytes(&bytes) {
            assert!(sqe.as_bytes() == bytes);
        }
    }

    #[kani::proof]
    fn cqe_decode_reencodes() {
        let bytes: [u8; 40] = kani::any();
        if let Ok(cqe) = Cqe::try_read_from_bytes(&bytes) {
            assert!(cqe.as_bytes() == bytes);
        }
    }
}
