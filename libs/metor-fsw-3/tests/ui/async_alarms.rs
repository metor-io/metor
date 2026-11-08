use metor_fsw_3::{Alarms, Stop, system};

#[system]
async fn watcher(alarms: &mut Alarms, stop: Stop) {
    let _ = alarms;
    stop.wait().await;
}

fn main() {}
