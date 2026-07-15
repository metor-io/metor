use std::net::SocketAddr;
use std::sync::Arc;

use metor_db::DB;

fn main() {
    // GPU init failures render as blank views, not crashes; the tracing
    // output is the only way to diagnose them, so surface warnings by
    // default and let RUST_LOG widen the filter.
    let filter = if std::env::var("RUST_LOG").is_ok() {
        tracing_subscriber::EnvFilter::builder().from_env_lossy()
    } else {
        tracing_subscriber::EnvFilter::builder().parse_lossy("metor_panel=warn")
    };
    let _ = tracing_subscriber::fmt::fmt()
        .with_target(false)
        .with_env_filter(filter)
        .try_init();

    let tmp = std::env::temp_dir().join("metor_panel");
    let db = Arc::new(DB::create(tmp).unwrap());
    metor_panel::PanelApp::new(db)
        .serve(SocketAddr::new([127, 0, 0, 1].into(), 2240))
        .run();
}
