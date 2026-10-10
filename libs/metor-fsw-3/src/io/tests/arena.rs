use std::rc::Rc;

use crate::coordinator::ParamError;
use crate::io::arena::Arena;
use crate::io::{IoInit, Span};

#[test]
fn bufs_read_empty_until_the_make_ends() {
    let arena = Rc::new(Arena::new());
    let init = IoInit::new(&arena);
    let a = init.alloc_from(b"hello");
    let b = init.alloc(3);
    assert!(a.is_empty(), "the arena may still move");
    drop(init);
    assert_eq!(&*a, b"hello");
    assert_eq!((b.len(), b.capacity()), (0, 3));
}

#[test]
fn spans_never_overlap() {
    let arena = Rc::new(Arena::new());
    let init = IoInit::new(&arena);
    let mut bufs: Vec<_> = (0..4).map(|n| init.alloc(n + 1)).collect();
    drop(init);
    for (n, buf) in bufs.iter_mut().enumerate() {
        buf.fill(&[n as u8; 8]);
    }
    for (n, buf) in bufs.iter().enumerate() {
        assert_eq!(&**buf, &vec![n as u8; n + 1][..]);
    }
}

#[test]
fn remake_within_capacity_reuses_the_arena() {
    let arena = Rc::new(Arena::new());
    let old = {
        let init = IoInit::new(&arena);
        init.alloc_from(b"abcd")
    };
    let base = arena.region().unwrap().0;
    let init = IoInit::new(&arena);
    let new = init.alloc(2);
    assert!(init.finish().is_ok());
    assert_eq!(arena.region().unwrap().0, base);
    assert!(old.is_empty(), "a buffer from the old make reads empty");
    assert_eq!(new.capacity(), 2);
    let mut new = new;
    new.fill(b"zz");
    assert_eq!(&*new, b"zz", "allocations are zeroed and writable");
}

#[test]
fn remake_past_capacity_fails_the_make() {
    let arena = Rc::new(Arena::new());
    drop(IoInit::new(&arena).alloc(4));
    let init = IoInit::new(&arena);
    let buf = init.alloc(9);
    assert_eq!(buf.capacity(), 0);
    let err = init.finish().unwrap_err();
    assert!(
        matches!(err, ParamError::IoMemory { need: 9, .. }),
        "{err:?}"
    );
}

#[test]
fn no_arena_fails_any_allocation() {
    let init = IoInit::empty();
    drop(init.alloc(1));
    assert!(matches!(
        init.finish(),
        Err(ParamError::IoMemory { need: 1, .. })
    ));
    assert!(IoInit::empty().finish().is_ok());
}

#[test]
fn span_checks_bounds() {
    let arena = Arena::owning(vec![0; 16].into());
    assert!(arena.span(Span::new(0, 16)).is_some());
    assert!(arena.span(Span::new(8, 9)).is_none());
    assert!(arena.span(Span::new(u32::MAX, 2)).is_none());
    assert!(Arena::new().span(Span::new(0, 0)).is_none(), "not frozen");
}

#[test]
fn owner_keeps_the_memory_alive() {
    let arena = Arena::owning(vec![7; 4].into());
    let (base, len) = arena.region().unwrap();
    let owner = arena.owner().unwrap();
    // SAFETY: the owner is live, and the lease meets its contract.
    let lease = unsafe { metor_fsw_3_ring::Lease::acquire(owner) };
    drop(arena);
    // SAFETY: the lease keeps the bytes alive.
    let other = unsafe { Arena::over(base, len, crate::io::arena::Keep::Lease(lease)) };
    // SAFETY: the region is live and unshared.
    let bytes = unsafe { other.span(Span::new(0, 4)).unwrap().as_ref() };
    assert_eq!(bytes, [7; 4]);
}
