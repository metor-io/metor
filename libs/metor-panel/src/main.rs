use std::net::SocketAddr;
use std::sync::Arc;

use metor_db::DB;

fn main() {
    let tmp = std::env::temp_dir().join("metor_panel");
    let db = Arc::new(DB::create(tmp).unwrap());
    metor_panel::PanelApp::new(db)
        .serve(SocketAddr::new([127, 0, 0, 1].into(), 2240))
        .run();
}
