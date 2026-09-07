mod startup_args;

use metor_db::Server;
use metor_panel::{ConnectContext, Connected, ConnectionTarget};
use startup_args::{Startup, startup_args};
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

    let app = match startup_args(std::env::args_os().skip(1)) {
        Ok(Startup::Open(path)) => metor_panel::PanelApp::open_recording(path),
        Ok(Startup::Choose) => metor_panel::PanelApp::choose_session(),
        Ok(Startup::RecordTo(path)) => match metor_panel::PanelApp::record_to(path) {
            Ok(app) => app,
            Err(error) => {
                eprintln!("Could not create recording: {error}");
                std::process::exit(1);
            }
        },
        Ok(Startup::Temporary) => metor_panel::PanelApp::temporary().unwrap_or_else(|error| {
            eprintln!("Could not create temporary session: {error}");
            std::process::exit(1);
        }),
        Ok(Startup::Help) => {
            println!(
                "Usage: metor-panel [--temporary | --record-to PATH | --open PATH]\n\nWithout arguments, choose a connection and optional recording location."
            );
            return;
        }
        Err(error) => {
            eprintln!("{error}\nUsage: metor-panel [--temporary | --record-to PATH | --open PATH]");
            std::process::exit(2);
        }
    };

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

    app.connection(sandbox)
        .connection_source(metor_panel::connections::mdns_source())
        .run();
}
