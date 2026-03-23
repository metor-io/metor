use std::net::SocketAddr;
use std::sync::Arc;

use gpui::{App, AppContext, Application, Bounds, WindowBounds, WindowOptions, px, size};
use metor_db::{DB, Server};
use metor_panel::elements::ComponentTable;
use stellarator::{net::TcpListener, struc_con::stellar};

fn main() {
    let tmp = std::env::temp_dir().join("metor_panel_table_example");
    let db = Arc::new(DB::create(tmp).unwrap());
    let server_db = db.clone();
    stellar(move || async move {
        let server = Server {
            listener: TcpListener::bind(SocketAddr::new([127, 0, 0, 1].into(), 2240)).unwrap(),
            db: server_db,
        };
        server.run().await
    });

    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(800.), px(600.)), cx);
        let db = db.clone();
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(|cx| ComponentTable::new(db, cx)),
        )
        .unwrap();
    });
}
