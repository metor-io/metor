//! Reassembles `u32`-length-prefixed packet bodies across reads.

use crate::io::Buf;

/// The bytes of a body's length prefix.
const PREFIX: usize = 4;

/// A length prefix past what the buffer holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Oversized;

/// One connection's receive buffer. Its filled bytes are the ones received;
/// those before `start` have been read.
pub(crate) struct Deframer {
    buf: Buf,
    start: usize,
    cap: usize,
}

impl Deframer {
    pub(crate) fn new(mut buf: Buf) -> Self {
        buf.clear();
        let cap = buf.capacity();
        Self { buf, start: 0, cap }
    }

    /// Forgets every byte received, for a new connection. False, and
    /// nothing forgotten, while a tail is lent.
    pub(crate) fn reset(&mut self) -> bool {
        if self.lent() {
            return false;
        }
        self.buf.clear();
        self.start = 0;
        true
    }

    /// Whether `spare` lent a tail that has not come back.
    pub(crate) fn lent(&self) -> bool {
        self.buf.capacity() < self.cap
    }

    /// Lends the free bytes for a recv to fill, after moving the unread
    /// bytes to the front. `None` while a tail is lent or nothing is free.
    pub(crate) fn spare(&mut self) -> Option<Buf> {
        if self.lent() {
            return None;
        }
        self.compact();
        let at = self.buf.len();
        self.buf.split_off(at).filter(|tail| tail.capacity() > 0)
    }

    /// Takes back the tail `spare` lent, with the bytes a recv put in it.
    /// Hands back a buffer that is not that tail.
    pub(crate) fn filled(&mut self, tail: Buf) -> Result<(), Buf> {
        if self.buf.capacity() + tail.capacity() != self.cap {
            return Err(tail);
        }
        self.buf.join(tail)
    }

    /// The next complete body, if one is buffered.
    ///
    /// A prefix longer than the buffer can ever hold is `Oversized`, and
    /// stays at the front, so every later call is `Oversized` too.
    pub(crate) fn next(&mut self) -> Option<Result<&[u8], Oversized>> {
        let prefix = self.buf.get(self.start..)?.first_chunk::<PREFIX>()?;
        let body = u32::from_le_bytes(*prefix) as usize;
        let Some(end) = (self.start + PREFIX)
            .checked_add(body)
            .filter(|_| body <= self.cap.saturating_sub(PREFIX))
        else {
            return Some(Err(Oversized));
        };
        let range = self.start + PREFIX..end;
        if end > self.buf.len() {
            return None;
        }
        self.start = end;
        self.buf.get(range).map(Ok)
    }

    /// Moves the unread bytes to the front. Does nothing while a tail is
    /// lent, since the head must stay full to rejoin it.
    pub(crate) fn compact(&mut self) {
        if self.lent() || self.start == 0 {
            return;
        }
        if let Some(unread) = self.buf.len().checked_sub(self.start) {
            self.buf.copy_within(self.start.., 0);
            self.buf.set_len(unread);
        }
        self.start = 0;
    }
}

#[cfg(test)]
mod tests {
    use crate::io::Stream;
    use crate::io::test::{TestIo, cap};

    use super::*;

    fn deframer(cap_bytes: usize) -> (Deframer, TestIo) {
        let (_, io) = cap::<Stream>();
        let buf = io.make(|init| init.alloc(cap_bytes));
        (Deframer::new(buf), io)
    }

    fn frame(body: &[u8]) -> Vec<u8> {
        let mut bytes = (body.len() as u32).to_le_bytes().to_vec();
        bytes.extend_from_slice(body);
        bytes
    }

    /// One recv that delivers `bytes`, as much as fits.
    fn recv(d: &mut Deframer, bytes: &[u8]) -> usize {
        let mut tail = d.spare().expect("room to read");
        let n = tail.extend(bytes);
        d.filled(tail).expect("the lent tail");
        n
    }

    fn bodies(d: &mut Deframer) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while let Some(Ok(body)) = d.next() {
            out.push(body.to_vec());
        }
        out
    }

    #[test]
    fn test_one_packet_per_read() {
        let (mut d, _io) = deframer(64);
        recv(&mut d, &frame(b"one"));
        assert_eq!(bodies(&mut d), [b"one".to_vec()]);
        recv(&mut d, &frame(b"two"));
        assert_eq!(bodies(&mut d), [b"two".to_vec()]);
    }

    #[test]
    fn test_three_packets_in_one_read() {
        let (mut d, _io) = deframer(64);
        let bytes = [frame(b"a"), frame(b"bb"), frame(b"ccc")].concat();
        recv(&mut d, &bytes);
        assert_eq!(
            bodies(&mut d),
            [b"a".to_vec(), b"bb".to_vec(), b"ccc".to_vec()]
        );
        assert!(d.next().is_none());
    }

    #[test]
    fn test_packet_split_over_three_reads() {
        let (mut d, _io) = deframer(64);
        let bytes = frame(b"split body");
        for chunk in [&bytes[..2], &bytes[2..7], &bytes[7..]] {
            assert!(bodies(&mut d).is_empty());
            recv(&mut d, chunk);
        }
        assert_eq!(bodies(&mut d), [b"split body".to_vec()]);
    }

    #[test]
    fn test_zero_length_body() {
        let (mut d, _io) = deframer(16);
        recv(&mut d, &[frame(b""), frame(b"x")].concat());
        assert_eq!(bodies(&mut d), [Vec::new(), b"x".to_vec()]);
    }

    #[test]
    fn test_prefix_split_across_reads() {
        let (mut d, _io) = deframer(16);
        let bytes = frame(b"body");
        recv(&mut d, &bytes[..3]);
        assert!(d.next().is_none());
        recv(&mut d, &bytes[3..]);
        assert_eq!(bodies(&mut d), [b"body".to_vec()]);
    }

    #[test]
    fn test_compaction_makes_room_for_the_rest() {
        let (mut d, _io) = deframer(12);
        let (first, second) = (frame(b"1234"), frame(b"abcdefgh"));
        recv(&mut d, &[&first[..], &second[..4]].concat());
        assert_eq!(bodies(&mut d), [b"1234".to_vec()]);
        assert_eq!(recv(&mut d, &second[4..]), 8, "the read body moved out");
        assert_eq!(bodies(&mut d), [b"abcdefgh".to_vec()]);
    }

    #[test]
    fn test_compaction_with_nothing_left_over() {
        let (mut d, _io) = deframer(8);
        recv(&mut d, &frame(b"full"));
        assert_eq!(bodies(&mut d), [b"full".to_vec()]);
        d.compact();
        assert_eq!(d.spare().map(|tail| tail.capacity()), Some(8));
    }

    #[test]
    fn test_frame_filling_the_buffer_exactly() {
        let (mut d, _io) = deframer(8);
        recv(&mut d, &frame(b"abcd"));
        assert!(d.spare().is_none(), "full until read");
        assert_eq!(bodies(&mut d), [b"abcd".to_vec()]);
    }

    #[test]
    fn test_spare_lends_once() {
        let (mut d, _io) = deframer(16);
        let tail = d.spare().expect("room");
        assert!(d.lent() && d.spare().is_none());
        d.filled(tail).expect("the lent tail");
        assert!(!d.lent());
    }

    #[test]
    fn test_filled_with_nothing_lent_is_refused() {
        let (mut d, io) = deframer(16);
        let tail = d.spare().expect("room");
        d.filled(tail).expect("the lent tail");
        let next = io.make(|init| init.alloc(4));
        assert!(d.filled(next).is_err());
        assert_eq!(d.spare().map(|tail| tail.capacity()), Some(16));
    }

    #[test]
    fn test_a_failed_recv_returns_an_empty_tail() {
        let (mut d, _io) = deframer(16);
        recv(&mut d, &frame(b"ab")[..3]);
        let tail = d.spare().expect("room");
        d.filled(tail).expect("the lent tail");
        assert!(d.next().is_none());
        recv(&mut d, &frame(b"ab")[3..]);
        assert_eq!(bodies(&mut d), [b"ab".to_vec()]);
    }

    #[test]
    fn test_reset_waits_for_the_lent_tail() {
        let (mut d, _io) = deframer(16);
        recv(&mut d, &frame(b"old")[..5]);
        let tail = d.spare().expect("room");
        assert!(!d.reset());
        d.filled(tail).expect("the lent tail");
        assert!(d.reset());
        recv(&mut d, &frame(b"new"));
        assert_eq!(bodies(&mut d), [b"new".to_vec()]);
    }

    #[test]
    fn test_compact_while_lent_keeps_the_head_full() {
        let (mut d, _io) = deframer(16);
        recv(&mut d, &[frame(b"a"), frame(b"b")[..2].to_vec()].concat());
        let mut tail = d.spare().expect("room");
        assert_eq!(bodies(&mut d), [b"a".to_vec()]);
        d.compact();
        tail.extend(&frame(b"b")[2..]);
        d.filled(tail).expect("the lent tail");
        assert_eq!(bodies(&mut d), [b"b".to_vec()]);
    }

    #[test]
    fn test_largest_body_fills_the_buffer() {
        let (mut d, _io) = deframer(16);
        recv(&mut d, &frame(&[7; 12]));
        assert_eq!(bodies(&mut d), [vec![7; 12]]);
        recv(&mut d, &13u32.to_le_bytes());
        assert_eq!(d.next(), Some(Err(Oversized)), "one byte past");
    }

    #[test]
    fn test_remade_arena_yields_nothing() {
        let (mut d, io) = deframer(16);
        recv(&mut d, &frame(b"one"));
        io.make(|init| init.alloc(16));
        assert!(d.next().is_none());
        d.compact();
        assert!(d.next().is_none());
    }

    #[test]
    fn test_reset_forgets_a_partial_packet() {
        let (mut d, _io) = deframer(16);
        recv(&mut d, &frame(b"partial")[..6]);
        assert!(d.reset());
        recv(&mut d, &frame(b"new"));
        assert_eq!(bodies(&mut d), [b"new".to_vec()]);
    }

    #[test]
    fn test_oversized_prefix_stops_the_stream() {
        let (mut d, _io) = deframer(16);
        recv(
            &mut d,
            &[frame(b"ok"), 13u32.to_le_bytes().to_vec()].concat(),
        );
        assert_eq!(d.next(), Some(Ok(&b"ok"[..])));
        assert_eq!(d.next(), Some(Err(Oversized)));
        recv(&mut d, b"more");
        assert_eq!(d.next(), Some(Err(Oversized)), "nothing further");
    }

    #[test]
    fn test_largest_prefix_is_oversized() {
        let (mut d, _io) = deframer(16);
        recv(&mut d, &u32::MAX.to_le_bytes());
        assert_eq!(d.next(), Some(Err(Oversized)));
    }

    #[test]
    fn test_foreign_buffer_is_refused() {
        let (mut d, io) = deframer(16);
        let other = io.make(|init| init.alloc(4));
        let _tail = d.spare().expect("room");
        assert!(d.filled(other).is_err());
        assert!(d.lent());
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    const CAP: usize = 8;

    /// For any reads and any bytes, `next` yields only filled bytes, and
    /// `compact` keeps the unread ones.
    #[kani::proof]
    #[kani::unwind(10)]
    fn next_and_compact_stay_in_the_filled_bytes() {
        let mut d = Deframer::new(Buf::owned(Box::new(kani::any::<[u8; CAP]>())));
        for _ in 0..3 {
            if let Some(mut tail) = d.spare() {
                assert!(d.lent() && d.buf.len() == d.buf.capacity());
                let n: usize = kani::any();
                kani::assume(n <= tail.capacity());
                tail.set_len(n);
                assert!(d.filled(tail).is_ok());
            }
            assert!(!d.lent());
            for _ in 0..2 {
                let (base, len) = (d.buf.as_ptr() as usize, d.buf.len());
                if let Some(Ok(body)) = d.next() {
                    let at = body.as_ptr() as usize;
                    assert!(body.len() + PREFIX <= CAP);
                    assert!(at >= base && at + body.len() <= base + len);
                }
                assert!(d.start <= d.buf.len());
            }
            let unread: [u8; CAP] =
                core::array::from_fn(|i| d.buf.get(d.start + i).copied().unwrap_or(0));
            let count = d.buf.len() - d.start;
            d.compact();
            assert_eq!(d.start, 0);
            assert_eq!(d.buf.len(), count);
            assert!(d.buf.iter().zip(unread).all(|(a, b)| *a == b));
        }
    }
}
