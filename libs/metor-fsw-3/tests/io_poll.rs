//! Runs the shared `Os` cases on `PollOs` with real descriptors.

mod os_cases;

use metor_fsw_3::io::{Limits, Os, OsError, PollOs, RawAddr, Tables};

fn poll(limits: &Limits, tables: Tables, dials: &[RawAddr]) -> Result<Box<dyn Os>, OsError> {
    Ok(Box::new(PollOs::new(limits, tables, dials)?))
}

#[test]
fn poll_os_cases() {
    os_cases::run_all(poll);
}
