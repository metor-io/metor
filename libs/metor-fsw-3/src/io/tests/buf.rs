#![cfg(test)]

use std::rc::Rc;

use crate::io::*;

fn stamped(table: &BufTable, slot: u16) -> BufRef {
    let mut buf = BufRef::new(slot);
    buf.stamp(table.id().unwrap());
    buf
}

fn contains(arena: core::ptr::NonNull<[u8]>, raw: RawBuf) -> bool {
    let start = arena.cast::<u8>().as_ptr() as usize;
    let p = raw.ptr.as_ptr() as usize;
    p >= start && p + raw.cap as usize <= start + arena.len()
}

#[test]
fn alloc_from_is_filled() {
    let t = BufTable::new(4);
    let s = t.alloc_from(b"hello").unwrap();
    assert_eq!(unsafe { t.filled(s) }, b"hello");
}

#[test]
fn whole_mut_has_cap_bytes() {
    let t = BufTable::new(4);
    let s = t.alloc(32).unwrap();
    assert_eq!(unsafe { t.filled(s) }.len(), 0);
    let whole = unsafe { t.whole_mut(s) };
    assert_eq!(whole.len(), 32);
    whole[..3].copy_from_slice(b"abc");
    t.set_len(s, 3).unwrap();
    assert_eq!(unsafe { t.filled(s) }, b"abc");
}

#[test]
fn tables_resolve_stamped_refs() {
    let tables = Tables::new();
    let a = Rc::new(BufTable::new(2));
    let b = Rc::new(BufTable::new(2));
    let id_a = tables.add(&a);
    let id_b = tables.add(&b);
    assert_ne!(id_a, id_b);
    assert_eq!(b.id(), Some(id_b));
    let s = b.alloc_from(b"xyz").unwrap();
    b.freeze();
    let raw = tables.raw(stamped(&b, s)).unwrap();
    assert_eq!((raw.cap, raw.len), (3, 3));
    assert_eq!(raw.ptr, b.raw(s).unwrap().ptr);
    tables.set_len(stamped(&b, s), 1).unwrap();
    assert_eq!(unsafe { b.filled(s) }, b"x");
}

#[test]
fn released_slot_index_is_reused() {
    let t = BufTable::new(2);
    let first = t.alloc(4).unwrap();
    let _second = t.alloc(4).unwrap();
    t.release(first);
    assert_eq!(t.alloc(8).unwrap(), first);
}

#[test]
fn growth_keeps_earlier_bytes() {
    let t = BufTable::new(8);
    let a = t.alloc_from(b"first").unwrap();
    for _ in 0..6 {
        t.alloc(4096).unwrap();
    }
    assert_eq!(unsafe { t.filled(a) }, b"first");
}

#[test]
fn frozen_pointers_stay_inside_and_still() {
    let t = BufTable::new(4);
    let a = t.alloc_from(b"a").unwrap();
    t.freeze();
    t.freeze();
    let arena = t.arena().unwrap();
    let before = t.raw(a).unwrap();
    let z = t.alloc(0).unwrap();
    assert!(contains(arena, before));
    assert!(contains(arena, t.raw(z).unwrap()));
    assert_eq!(t.raw(a).unwrap().ptr, before.ptr);
    assert_eq!(t.arena().unwrap(), arena);
}

#[test]
fn table_added_after_clone_resolves() {
    let tables = Tables::new();
    let backend = tables.clone();
    let t = Rc::new(BufTable::new(1));
    tables.add(&t);
    let s = t.alloc(2).unwrap();
    t.freeze();
    assert!(backend.raw(stamped(&t, s)).is_some());
}

#[test]
fn past_slots_is_full() {
    let t = BufTable::new(2);
    t.alloc(1).unwrap();
    t.alloc(1).unwrap();
    assert_eq!(t.alloc(1), Err(BufError::Full));
}

#[test]
fn past_frozen_capacity_is_no_space() {
    let t = BufTable::new(2);
    t.alloc(16).unwrap();
    t.freeze();
    assert_eq!(t.alloc(5), Err(BufError::NoSpace { need: 5, free: 0 }));
}

#[test]
fn set_len_past_cap_is_too_large() {
    let t = BufTable::new(1);
    let s = t.alloc(4).unwrap();
    assert_eq!(t.set_len(s, 5), Err(BufError::TooLarge));
    assert_eq!(t.set_len(9, 0), Err(BufError::BadSlot));
}

#[test]
fn raw_is_none_unless_frozen_and_held() {
    let t = BufTable::new(2);
    let s = t.alloc(4).unwrap();
    assert!(t.raw(s).is_none());
    assert!(t.arena().is_none());
    t.freeze();
    assert!(t.raw(s).is_some());
    t.release(s);
    assert!(t.raw(s).is_none());
    assert!(t.raw(1).is_none());
    assert!(t.raw(400).is_none());
}

#[test]
fn unknown_table_is_none() {
    let tables = Tables::new();
    let t = Rc::new(BufTable::new(1));
    tables.add(&t);
    t.alloc(1).unwrap();
    t.freeze();
    let mut buf = BufRef::new(0);
    buf.stamp(TableId(7));
    assert!(tables.raw(buf).is_none());
    assert_eq!(tables.set_len(buf, 0), Err(BufError::BadSlot));
}

/// Also run under Miri, with and without `-Zmiri-tree-borrows`.
#[test]
fn raw_pointer_survives_allocs_and_reads() {
    let t = BufTable::new(8);
    let a = t.alloc(4).unwrap();
    let b = t.alloc_from(b"bb").unwrap();
    let held = t.alloc(16).unwrap();
    t.freeze();
    let raw = t.raw(a).unwrap();
    t.alloc(0).unwrap();
    assert_eq!(unsafe { t.filled(b) }, b"bb");
    unsafe { raw.ptr.as_ptr().write_bytes(7, raw.cap as usize) };
    t.set_len(a, 4).unwrap();
    assert_eq!(unsafe { t.filled(a) }, [7; 4]);
    let _ = held;
}

#[test]
fn drop_with_held_slots_frees_once() {
    let t = BufTable::new(3);
    t.alloc(8).unwrap();
    t.alloc(8).unwrap();
    t.freeze();
    drop(t);
    let unfrozen = BufTable::new(1);
    unfrozen.alloc(4).unwrap();
    drop(unfrozen);
}

#[test]
fn rewind_gives_the_same_slots_and_addresses() {
    let t = BufTable::new(4);
    let (a, b) = (t.alloc(8).unwrap(), t.alloc_from(b"xyz").unwrap());
    t.freeze();
    let (pa, pb) = (t.raw(a).unwrap().ptr, t.raw(b).unwrap().ptr);
    t.release(a);
    t.release(b);
    t.rewind().unwrap();
    assert_eq!((t.alloc(8).unwrap(), t.alloc(3).unwrap()), (a, b));
    assert_eq!((t.raw(a).unwrap().ptr, t.raw(b).unwrap().ptr), (pa, pb));
}

#[test]
fn smaller_round_fits_after_rewind() {
    let t = BufTable::new(4);
    let a = t.alloc(16).unwrap();
    t.freeze();
    t.release(a);
    t.rewind().unwrap();
    t.alloc(4).unwrap();
    t.alloc(12).unwrap();
}

#[test]
fn rewind_with_a_held_slot_is_busy() {
    let t = BufTable::new(2);
    t.alloc(4).unwrap();
    assert_eq!(t.rewind(), Err(BufError::Busy));
}

#[test]
fn past_capacity_after_rewind_is_no_space() {
    let t = BufTable::new(2);
    let a = t.alloc(16).unwrap();
    t.freeze();
    t.release(a);
    t.rewind().unwrap();
    assert_eq!(t.alloc(20), Err(BufError::NoSpace { need: 20, free: 16 }));
}
