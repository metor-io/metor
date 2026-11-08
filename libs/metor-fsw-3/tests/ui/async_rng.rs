use metor_fsw_3::{Rng, Stop, system};

#[system]
async fn drawer(rng: &mut Rng, stop: Stop) {
    let _ = rng;
    stop.wait().await;
}

fn main() {}
