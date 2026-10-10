//! Runs the shared `Os` cases on `PollOs`.

mod os2_cases;

use metor_fsw_3::io2_host::{Os, PollOs};

fn poll(ops: usize) -> std::io::Result<Box<dyn Os>> {
    Ok(Box::new(PollOs::new(ops)?))
}

#[test]
fn poll_os_cases() {
    os2_cases::run_all(poll);
}
