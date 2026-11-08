use metor_fsw_3::io::Uart;
use metor_fsw_3::{Cap, Stop, system};

#[system]
async fn serial(uart: &Cap<Uart>, stop: Stop) {
    let _ = uart;
    stop.wait().await;
}

fn main() {}
