use std::sync::Arc;
use std::{net::SocketAddr, path::PathBuf};

use gpui::{App, AppContext, Application, Bounds, WindowBounds, WindowOptions, px, size};
use metor_db::{DB, Server};
use metor_panel::elements;
use metor_proto::types::ComponentId;
use stellarator::{net::TcpListener, struc_con::stellar};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {}  <component_name>", args[0]);
        std::process::exit(1);
    }
    let component_id = ComponentId::new(args[1].as_str());
    let tmp = std::env::temp_dir().join("metor_panel_example");
    let db = Arc::new(DB::create(tmp).unwrap());
    let server_db = db.clone();
    stellar(move || async move {
        let server = Server {
            listener: TcpListener::bind(SocketAddr::new([127, 0, 0, 1].into(), 2240)).unwrap(),
            db: server_db,
        };
        server.run().await
    });

    Application::new()
        .with_assets(metor_panel::icons::IconAssets)
        .run(move |cx: &mut App| {
        metor_panel::theme::register_fonts(cx);
        cx.set_global(metor_panel::theme::ActiveTheme(
            std::sync::Arc::new(metor_panel::theme::DARK.clone()),
        ));
        let bounds = Bounds::centered(None, size(px(400.), px(200.)), cx);
        let db = db.clone();
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(|cx| elements::ComponentText::new(db, component_id, cx)),
        )
        .unwrap();
    });
}
