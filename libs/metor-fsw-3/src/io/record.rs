//! `Record` impls that carry `Sqe` and `Cqe` on rings as their own bytes.

use zerocopy::{IntoBytes, TryFromBytes};

use crate::record::{DecodeError, EncodeError, MsgCodec, Record, RecordSchema};

use super::{Cqe, MAX_OPS, Sqe};

impl Record for Sqe {
    const NAME: &'static str = "io_sqe";
    const MAX_LEN: usize = size_of::<Sqe>();
    const ALIGN: usize = align_of::<Sqe>();
    const DEPTH: usize = MAX_OPS;
    type Read<'a> = Sqe;

    fn encode<'a>(&'a self, _buf: &'a mut [u8]) -> Result<&'a [u8], EncodeError> {
        Ok(self.as_bytes())
    }

    fn decode(bytes: &[u8]) -> Result<Sqe, DecodeError> {
        decode(bytes)
    }

    fn schema() -> RecordSchema {
        RecordSchema::msg(Self::NAME, MsgCodec::Other("io.sqe".into()))
    }
}

impl Record for Cqe {
    const NAME: &'static str = "io_cqe";
    const MAX_LEN: usize = size_of::<Cqe>();
    const ALIGN: usize = align_of::<Cqe>();
    const DEPTH: usize = MAX_OPS;
    type Read<'a> = Cqe;

    fn encode<'a>(&'a self, _buf: &'a mut [u8]) -> Result<&'a [u8], EncodeError> {
        Ok(self.as_bytes())
    }

    fn decode(bytes: &[u8]) -> Result<Cqe, DecodeError> {
        decode(bytes)
    }

    fn schema() -> RecordSchema {
        RecordSchema::msg(Self::NAME, MsgCodec::Other("io.cqe".into()))
    }
}

/// Reads a `T` from the front of `bytes`, ignoring any bytes after it.
fn decode<T: TryFromBytes + zerocopy::KnownLayout + zerocopy::Immutable>(
    bytes: &[u8],
) -> Result<T, DecodeError> {
    match T::try_read_from_prefix(bytes) {
        Ok((value, _)) => Ok(value),
        Err(_) if bytes.len() < size_of::<T>() => Err(DecodeError::Truncated),
        Err(_) => Err(DecodeError::Codec),
    }
}

const _: () = {
    assert!(<Sqe as Record>::MAX_LEN == 48);
    assert!(<Cqe as Record>::MAX_LEN == 40);
};

#[cfg(all(test, feature = "host"))]
mod tests {
    use zerocopy::IntoBytes;

    use super::*;
    use crate::coordinator::SystemTable;
    use crate::io::{BufRef, IoError, Op, Out, RawHandle, Token};
    use crate::tests::utils::{input, output, ring};
    use crate::{Input, Output, system};

    fn sqe() -> Sqe {
        Sqe::new(
            Token::new(3),
            RawHandle::new(1, 2),
            Op::read(BufRef::new(4), None),
        )
    }

    #[test]
    fn test_records_round_trip() {
        let mut buf = [0; 64];
        let sqe = sqe();
        let bytes = sqe.encode(&mut buf).unwrap().to_vec();
        assert_eq!(Sqe::decode(&bytes), Ok(sqe));
        let cqe = Cqe::ok(Token::new(3), Out::len(9));
        let bytes = cqe.encode(&mut buf).unwrap().to_vec();
        assert_eq!(Cqe::decode(&bytes), Ok(cqe));
    }

    #[test]
    fn test_records_cross_a_ring() {
        let ring = ring::<Cqe>(4, 1);
        let out: Output<Cqe> = output(&ring);
        let mut inp: Input<Cqe> = input(&ring);
        let cqe = Cqe::err(Token::new(7), IoError::Stale);
        out.write(&cqe).unwrap();
        let got: Vec<_> = inp.drain().map(Result::unwrap).collect();
        assert_eq!(got, [cqe]);
    }

    #[test]
    fn test_trailing_bytes_are_ignored() {
        let mut bytes = sqe().as_bytes().to_vec();
        bytes.extend([0xAA; 8]);
        assert_eq!(Sqe::decode(&bytes), Ok(sqe()));
    }

    #[test]
    fn test_short_record_is_truncated() {
        assert_eq!(
            Sqe::decode(&sqe().as_bytes()[..47]),
            Err(DecodeError::Truncated)
        );
        assert_eq!(Cqe::decode(&[0; 39]), Err(DecodeError::Truncated));
    }

    #[test]
    fn test_bad_op_tag_is_codec() {
        let mut bytes = sqe().as_bytes().to_vec();
        bytes[12] = 0xFF;
        assert_eq!(Sqe::decode(&bytes), Err(DecodeError::Codec));
    }

    #[system]
    fn both(sq: &mut Output<Sqe>, cq: &mut Input<Cqe>) {
        let _ = (sq, cq);
    }

    #[test]
    fn test_records_register_without_conflict() {
        let mut table = SystemTable::new();
        table.register("both", both(), Default::default).unwrap();
        assert!(table.records().port("out", Sqe::NAME).is_ok());
        assert!(table.records().port("out", Cqe::NAME).is_ok());
    }
}
