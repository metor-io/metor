use std::rc::Rc;

use crate::coordinator::MakeError;
use crate::io::arena::Arena;
use crate::io::{Buf, IoInit, Op, Span};

#[test]
fn bufs_read_empty_until_the_make_ends() -> Result<(), MakeError> {
    let arena = Rc::new(Arena::new());
    let mut init = IoInit::new(&arena, "a");
    let a = init.alloc_from(b"hello")?;
    let b = init.alloc(3)?;
    assert!(a.is_empty(), "the arena may still move");
    init.finish();
    assert_eq!(&*a, b"hello");
    assert_eq!((b.len(), b.capacity()), (0, 3));
    Ok(())
}

#[test]
fn spans_never_overlap() -> Result<(), MakeError> {
    let arena = Rc::new(Arena::new());
    let mut init = IoInit::new(&arena, "a");
    let mut bufs = (0..4)
        .map(|n| init.alloc(n + 1))
        .collect::<Result<Vec<_>, _>>()?;
    init.finish();
    for (n, buf) in bufs.iter_mut().enumerate() {
        buf.fill(&[n as u8; 8]);
    }
    for (n, buf) in bufs.iter().enumerate() {
        assert_eq!(&**buf, &vec![n as u8; n + 1][..]);
    }
    Ok(())
}

#[test]
fn remake_within_capacity_reuses_the_arena() -> Result<(), MakeError> {
    let arena = Rc::new(Arena::new());
    let mut init = IoInit::new(&arena, "a");
    let old = init.alloc_from(b"abcd")?;
    init.finish();
    let base = arena.region().unwrap().0;
    let mut init = IoInit::new(&arena, "a");
    let mut new = init.alloc(2)?;
    init.finish();
    assert_eq!(arena.region().unwrap().0, base);
    assert!(old.is_empty(), "a buffer from the old make reads empty");
    assert_eq!(new.capacity(), 2);
    new.fill(b"zz");
    assert_eq!(&*new, b"zz", "allocations are zeroed and writable");
    Ok(())
}

#[test]
fn remake_past_capacity_fails_the_alloc() -> Result<(), MakeError> {
    let arena = Rc::new(Arena::new());
    let mut init = IoInit::new(&arena, "a");
    drop(init.alloc(4)?);
    init.finish();
    let mut init = IoInit::new(&arena, "a");
    let err = init.alloc(9).unwrap_err();
    assert_eq!(
        err,
        MakeError::IoMemory {
            system: "a".into(),
            requested_len: 9,
            available: Some(4),
        }
    );
    assert_eq!(
        init.alloc(4)?.capacity(),
        4,
        "a refused alloc takes nothing"
    );
    Ok(())
}

#[test]
fn no_arena_fails_any_allocation() {
    let mut init = IoInit::empty("a");
    assert_eq!(
        init.alloc_from(b"x").unwrap_err(),
        MakeError::IoMemory {
            system: "a".into(),
            requested_len: 1,
            available: None,
        }
    );
    assert!(
        init.alloc(0).is_err(),
        "even an empty buffer needs an arena"
    );
    init.finish();
}

#[test]
fn span_checks_bounds() {
    let arena = super::arena(16);
    assert!(arena.get_slice(Span::new(0, 16)).is_some());
    assert!(arena.get_slice(Span::new(8, 9)).is_none());
    assert!(arena.get_slice(Span::new(u32::MAX, 2)).is_none());
    assert!(Arena::new().get_slice(Span::new(0, 0)).is_none(), "not set");
}

#[test]
fn owner_keeps_the_memory_alive() {
    let arena = Arena::new();
    arena.set_owned(vec![7; 4].into());
    let (base, len) = arena.region().unwrap();
    let owner = arena.owner().unwrap();
    // SAFETY: the owner is live, and the lease meets its contract.
    let lease = unsafe { metor_fsw_3_ring::Lease::acquire(owner) };
    drop(arena);
    // SAFETY: the lease keeps the bytes alive.
    let other = Arena::new();
    // SAFETY: the lease keeps the bytes alive.
    unsafe { other.set(base, len, crate::io::arena::Keep::Lease(lease)) };
    // SAFETY: the region is live and unshared.
    let bytes = unsafe { other.get_slice(Span::new(0, 4)).unwrap().as_ref() };
    assert_eq!(bytes, [7; 4]);
}

#[test]
fn memory_is_set_once() {
    let arena = Rc::new(Arena::new());
    assert!(arena.region().is_none());
    IoInit::new(&arena, "a").finish();
    assert_eq!(
        arena.region().map(|(_, len)| len),
        Some(0),
        "an empty first make sets the memory"
    );
    let (base, _) = arena.region().expect("set");
    IoInit::new(&arena, "a").finish();
    assert_eq!(arena.region(), Some((base, 0)), "a remake keeps the memory");
}

/// One finished make of `len` bytes, as one buffer.
fn buf(len: usize) -> (Rc<Arena>, Buf) {
    let arena = Rc::new(Arena::new());
    let mut init = IoInit::new(&arena, "a");
    let buf = init.alloc(len).expect("the first make grows");
    init.finish();
    (arena, buf)
}

#[test]
fn extend_appends_to_capacity() {
    let (_arena, mut buf) = buf(6);
    assert_eq!(buf.extend(b"abcd"), 4);
    assert_eq!(buf.extend(b"efgh"), 2, "only the spare bytes are taken");
    assert_eq!((&*buf, buf.spare()), (&b"abcdef"[..], 0));
    assert_eq!(buf.extend(b"x"), 0, "a full buffer takes nothing");
}

#[test]
fn advance_names_the_unsent_tail() {
    let (_arena, mut buf) = buf(8);
    buf.fill(b"abcdef");
    buf.advance(4);
    assert_eq!(&*buf, b"ef");
    assert_eq!(buf.span_for(&Op::Send), Span::new(4, 2));
    assert_eq!(
        buf.span_for(&Op::Recv),
        Span::new(0, 8),
        "a fill names it all"
    );
    buf.advance(9);
    assert!(buf.is_empty(), "advancing past the filled bytes clamps");
    assert_eq!(buf.extend(b"g"), 1, "the bytes after the old end are spare");
    assert_eq!(buf.span_for(&Op::Send), Span::new(6, 1));
}

#[test]
fn clear_and_fill_start_over() {
    let (_arena, mut buf) = buf(4);
    buf.fill(b"abcd");
    buf.advance(3);
    buf.clear();
    assert_eq!((buf.len(), buf.spare()), (0, 4));
    buf.fill(b"wxyz");
    assert_eq!(&*buf, b"wxyz");
}

#[test]
fn split_then_join_gives_back_the_span() {
    let (_arena, mut head) = buf(8);
    head.fill(b"abc");
    let mut tail = head.split_off(3).expect("past the filled bytes");
    assert_eq!((head.capacity(), tail.capacity()), (3, 5));
    assert_eq!(tail.span_for(&Op::Recv), Span::new(3, 5));
    tail.fill(b"de");
    head.join(tail).expect("adjacent and meeting at the seam");
    assert_eq!((&*head, head.capacity()), (&b"abcde"[..], 8));
}

#[test]
fn split_at_the_edges() {
    let (_arena, mut buf) = buf(4);
    let all = buf.split_off(0).expect("an empty head");
    assert_eq!((buf.capacity(), all.capacity()), (0, 4));
    buf.join(all).expect("rejoins");
    let none = buf.split_off(4).expect("an empty tail");
    assert_eq!((buf.capacity(), none.capacity()), (4, 0));
}

#[test]
fn split_refuses_the_filled_bytes_and_past_capacity() {
    let (_arena, mut buf) = buf(8);
    buf.fill(b"abcd");
    buf.advance(1);
    assert!(buf.split_off(3).is_none(), "inside the filled bytes");
    assert!(buf.split_off(9).is_none(), "past the capacity");
    assert_eq!(&*buf, b"bcd", "a refused split changes nothing");
}

#[test]
fn join_refuses_what_does_not_fit() {
    let (_arena, mut head) = buf(8);
    let mut tail = head.split_off(4).expect("a tail");
    tail.fill(b"x");
    let tail = head
        .join(tail)
        .expect_err("the head's bytes stop short of the seam");
    assert_eq!(&*tail, b"x", "handed back unchanged");
    let (_other, stranger) = buf(4);
    assert!(head.join(stranger).is_err(), "another arena");
    let mut next = tail;
    next.clear();
    let gap = next.split_off(1).expect("a span one byte on");
    assert!(head.join(gap).is_err(), "not adjacent");
    head.join(next).expect("adjacent and empty");
}

#[test]
fn join_refuses_an_earlier_make() -> Result<(), MakeError> {
    let (arena, mut old) = buf(4);
    let tail = old.split_off(2).expect("a tail");
    let mut init = IoInit::new(&arena, "a");
    let mut head = init.alloc(2)?;
    init.finish();
    assert!(head.join(tail).is_err(), "adjacent, but from the last make");
    Ok(())
}
