//! Runs the shared `Os` cases on `UringOs`.
#![cfg(target_os = "linux")]

mod os_cases;

use metor_fsw_3::io::host::{Os, UringOs};

fn uring(ops: usize) -> std::io::Result<Box<dyn Os>> {
    Ok(Box::new(UringOs::new(ops)?))
}

#[test]
fn uring_os_cases() {
    os_cases::run_all(uring);
}
