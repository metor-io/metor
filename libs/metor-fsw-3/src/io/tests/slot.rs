#![cfg(test)]

use super::Rig;
use crate::coordinator::ParamError;
use crate::io::*;

#[test]
fn alloc_from_holds_its_bytes() {
    let rig = Rig::new();
    let buf = rig.buf(b"pmtk");
    assert_eq!(&buf[..], b"pmtk");
    assert_eq!(buf.capacity(), 4);
}

#[test]
fn fill_clamps_to_capacity() {
    let rig = Rig::new();
    let mut buf = rig.empty_buf(3);
    assert!(buf.is_empty());
    assert_eq!(buf.fill(b"abcdef"), 3);
    assert_eq!(&buf[..], b"abc");
    buf.set_len(99);
    assert_eq!(buf.len(), 3);
    buf.clear();
    assert!(buf.is_empty());
}

#[test]
fn dropped_buf_frees_its_slot() {
    let rig = Rig::new();
    let first = rig.empty_buf(2);
    let slot = first.buf_ref().slot();
    drop(first);
    assert_eq!(rig.empty_buf(2).buf_ref().slot(), slot);
}

#[test]
fn bytes_hide_while_the_table_is_made() {
    let rig = Rig::new();
    let init = IoInit::new(&rig.io);
    let buf = init.alloc_from(b"cmd");
    assert!(buf.is_empty());
    assert_eq!(init.finish(), Ok(()));
    assert_eq!(&buf[..], b"cmd");
}

#[test]
fn refused_alloc_fails_the_make() {
    let rig = Rig::new();
    let init = IoInit::new(&rig.io);
    let bufs: Vec<_> = (0..crate::io::queue::BUF_SLOTS)
        .map(|_| init.alloc(1))
        .collect();
    let extra = init.alloc(5);
    assert_eq!(extra.capacity(), 0);
    let Err(ParamError::IoMemory { need, reason }) = init.finish() else {
        panic!("expected IoMemory");
    };
    assert_eq!(need, 5);
    assert!(reason.contains("slot"), "{reason}");
    drop(bufs);
}

#[test]
fn alloc_without_a_queue_fails_the_make() {
    let init = IoInit::empty();
    let buf = init.alloc_from(b"abc");
    assert_eq!(buf.capacity(), 0);
    let Err(ParamError::IoMemory { need, reason }) = init.finish() else {
        panic!("expected IoMemory");
    };
    assert_eq!(need, 3);
    assert!(reason.contains("no `Cap`"), "{reason}");
}

#[test]
fn unused_init_finishes_clean() {
    assert_eq!(IoInit::empty().finish(), Ok(()));
    let rig = Rig::new();
    assert_eq!(IoInit::new(&rig.io).finish(), Ok(()));
}

#[test]
fn finished_init_freezes_the_table() {
    let rig = Rig::new();
    let first = IoInit::new(&rig.io);
    let buf = first.alloc_from(b"one");
    drop(first);
    assert!(rig.io.table().bufs.region().is_some());
    let again = IoInit::new(&rig.io);
    assert_eq!(again.alloc(1).capacity(), 0);
    assert!(matches!(
        again.finish(),
        Err(ParamError::IoMemory { need: 1, .. })
    ));
    assert_eq!(&buf[..], b"one");
}
