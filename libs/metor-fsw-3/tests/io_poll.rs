//! Runs the shared `Os` cases on `PollOs`.

mod os_cases;

use metor_fsw_3::io::host::{Os, PollOs};

fn poll(ops: usize) -> std::io::Result<Box<dyn Os>> {
    Ok(Box::new(PollOs::new(ops)?))
}

#[test]
fn poll_os_cases() {
    os_cases::run_all(poll);
}
