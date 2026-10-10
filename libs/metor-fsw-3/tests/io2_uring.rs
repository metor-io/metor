//! Runs the shared `Os` cases on `UringOs`.
#![cfg(target_os = "linux")]

mod os2_cases;

use metor_fsw_3::io2_host::{Os, UringOs};

fn uring(ops: usize) -> std::io::Result<Box<dyn Os>> {
    Ok(Box::new(UringOs::new(ops)?))
}

#[test]
fn uring_os_cases() {
    os2_cases::run_all(uring);
}
