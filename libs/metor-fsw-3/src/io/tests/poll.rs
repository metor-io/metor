use crate::io::poll::{HOST_ERRNOS, errno};
use crate::io::*;

#[test]
fn every_errno_pair_round_trips() {
    for (linux, host) in HOST_ERRNOS {
        assert_eq!(errno(host), linux);
        assert!(linux.name().is_some());
    }
}

#[test]
fn unknown_host_errno_is_eio() {
    assert_eq!(errno(0), Errno::EIO);
    assert_eq!(errno(99_999), Errno::EIO);
}
