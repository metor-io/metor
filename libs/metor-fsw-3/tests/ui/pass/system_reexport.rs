use metor_fsw_3::SystemTable;

mod nav {
    use metor_fsw_3::{Timestamp, system};

    #[derive(Default)]
    pub struct NavState(pub u64);

    #[system]
    pub fn nav(#[state] s: &mut NavState, now: Timestamp) {
        s.0 = now.0 as u64;
    }
}

pub use nav::nav;

fn main() {
    let mut table = SystemTable::new();
    table
        .register("nav", nav(), nav::NavState::default)
        .expect("valid records");
}
