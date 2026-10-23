mod startup_args;

use metor_db::Server;
use metor_panel::{ConnectContext, Connected, ConnectionTarget};
use startup_args::{Startup, USAGE, startup_args};
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

    let args = match startup_args(std::env::args_os().skip(1)) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error}\n{USAGE}");
            std::process::exit(2);
        }
    };
    let app = match args.mode {
        Startup::Open(path) => metor_panel::PanelApp::open_recording(path),
        Startup::Choose => metor_panel::PanelApp::choose_session(),
        Startup::RecordTo(path) => metor_panel::PanelApp::record_to(path)
            .unwrap_or_else(|error| exit("Could not create recording", &error)),
        Startup::Temporary => metor_panel::PanelApp::temporary()
            .unwrap_or_else(|error| exit("Could not create temporary session", &error)),
        Startup::Connect(addr) => {
            let addr = startup_args::resolve(&addr).unwrap_or_else(|e| exit("Bad address", &e));
            metor_panel::PanelApp::temporary()
                .unwrap_or_else(|error| exit("Could not create temporary session", &error))
                .remote(addr)
        }
        Startup::Help => {
            println!(
                "{USAGE}\n\nWithout a mode, choose a connection and optional recording location.\n--connect mirrors a running db in a temporary session; --preset loads a layout file."
            );
            return;
        }
    };
    let app = match args.preset {
        Some(path) => match std::fs::read_to_string(&path) {
            Ok(json) => app.layout(json),
            Err(error) => exit(&format!("Could not read preset {}", path.display()), &error),
        },
        None => app,
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

fn exit(what: &str, error: &dyn std::fmt::Display) -> ! {
    eprintln!("{what}: {error}");
    std::process::exit(1);
}
