use std::net::SocketAddr;
use std::sync::Arc;

use metor_db::{DB, Server};
use stellarator::{net::TcpListener, struc_con::stellar};

fn main() {
    let tmp = std::env::temp_dir().join("metor_panel");
    let db = Arc::new(DB::create(tmp).unwrap());
    let server_db = db.clone();
    stellar(move || async move {
        let server = Server {
            listener: TcpListener::bind(SocketAddr::new([127, 0, 0, 1].into(), 2240)).unwrap(),
            db: server_db,
        };
        server.run().await
    });

    metor_panel::app::run(db)
}
