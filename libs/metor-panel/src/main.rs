use std::sync::Arc;

use metor_db::{DB, Server};
use metor_panel::{ConnectContext, Connected, ConnectionTarget};
use stellarator::net::TcpListener;

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

    // The sandbox target reproduces the classic standalone setup: this
    // panel's DB serves the wire protocol on 2240 and flight software
    // dials in. Connecting boots the server; disconnecting stops it.
    let sandbox = ConnectionTarget::custom(
        "local-sandbox",
        "Local sandbox",
        "serve 127.0.0.1:2240",
        |ctx: ConnectContext| {
            let db = ctx.db.clone();
            let status = ctx.status.clone();
            ctx.spawn(move || async move {
                let listener = match TcpListener::bind("127.0.0.1:2240") {
                    Ok(listener) => listener,
                    Err(err) => {
                        status.set(metor_panel::ConnectionStatus::Failed(
                            format!("bind 127.0.0.1:2240: {err}").into(),
                        ));
                        return;
                    }
                };
                status.set(metor_panel::ConnectionStatus::Connected);
                let server = Server { listener, db };
                if let Err(err) = server.run().await {
                    status.set(metor_panel::ConnectionStatus::Failed(
                        format!("server: {err}").into(),
                    ));
                }
            });
            Connected {
                hydrator: None,
                local_authority: true,
                resolved: None,
            }
        },
    );

    metor_panel::PanelApp::new(db).connection(sandbox).run();
}
