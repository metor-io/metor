//! A non-port parameter in an execute fn is rejected with a note pointing at
//! the accepted parameter kinds.

fn bad(_state: &mut u64, _config: String) {}

fn main() {
    let _ = metor_fsw_2::Pack::new().system("bad", metor_fsw_2::system(bad));
}
