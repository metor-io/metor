use std::rc::Rc;

use crate::coordinator::ParamError;
use crate::io::arena::Arena;
use crate::io::{IoInit, Span};

#[test]
fn bufs_read_empty_until_the_make_ends() {
    let arena = Rc::new(Arena::new());
    let mut init = IoInit::new(&arena);
    let a = init.alloc_from(b"hello");
    let b = init.alloc(3);
    assert!(a.is_empty(), "the arena may still move");
    init.finish("a").expect("fits");
    assert_eq!(&*a, b"hello");
    assert_eq!((b.len(), b.capacity()), (0, 3));
}

#[test]
fn spans_never_overlap() {
    let arena = Rc::new(Arena::new());
    let mut init = IoInit::new(&arena);
    let mut bufs: Vec<_> = (0..4).map(|n| init.alloc(n + 1)).collect();
    init.finish("a").expect("fits");
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
    let mut init = IoInit::new(&arena);
    let old = init.alloc_from(b"abcd");
    init.finish("a").expect("fits");
    let base = arena.region().unwrap().0;
    let mut init = IoInit::new(&arena);
    let new = init.alloc(2);
    assert!(init.finish("a").is_ok());
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
    let mut init = IoInit::new(&arena);
    drop(init.alloc(4));
    init.finish("a").expect("fits");
    let mut init = IoInit::new(&arena);
    let buf = init.alloc(9);
    assert_eq!(buf.capacity(), 0);
    let err = init.finish("a").unwrap_err();
    assert!(
        matches!(
            err,
            ParamError::IoMemory {
                requested_len: 9,
                available: Some(4),
                ..
            }
        ),
        "{err:?}"
    );
}

#[test]
fn no_arena_fails_any_allocation() {
    let mut init = IoInit::empty();
    drop(init.alloc(1));
    assert!(matches!(
        init.finish("a"),
        Err(ParamError::IoMemory {
            requested_len: 1,
            available: None,
            ..
        })
    ));
    assert!(IoInit::empty().finish("a").is_ok());
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
fn memory_is_set_once() {
    let arena = Rc::new(Arena::new());
    assert!(arena.region().is_none());
    IoInit::new(&arena).finish("a").expect("fits");
    assert_eq!(arena.region().map(|(_, len)| len), Some(0), "an empty first make sets the memory");
    let (base, _) = arena.region().expect("set");
    IoInit::new(&arena).finish("a").expect("fits");
    assert_eq!(arena.region(), Some((base, 0)), "a remake keeps the memory");
}
