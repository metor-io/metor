use std::rc::Rc;

use crate::coordinator::MakeError;
use crate::io::arena::Arena;
use crate::io::{IoInit, Span};

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
