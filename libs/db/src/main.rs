use std::{io::Write, net::SocketAddr, path::PathBuf};

use clap::{Parser, Subcommand};
use metor_proto::vtable;
use metor_db::Server;
use miette::IntoDiagnostic;
use postcard_c_codegen::SchemaExt;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Clone)]
#[command(version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Clone)]
enum Commands {
    #[command(about = "Run the Metor database server")]
    Run(RunArgs),
    #[command(about = "Run a Lua script or launch a REPL")]
    Lua(metor_proto_cli::Args),
    #[command(about = "Generate C++ header files")]
    GenCpp,
}

#[derive(clap::Args, Clone, Debug)]
struct RunArgs {
    #[clap(default_value = "[::]:2240", help = "Address to bind the server to")]
    addr: SocketAddr,
    #[clap(help = "Path to the data directory")]
    path: Option<PathBuf>,
    #[clap(long, help = "Path to the configuration file")]
    pub config: Option<PathBuf>,
    #[clap(long, hide = true)]
    reset: bool,
}

#[stellarator::main]
async fn main() -> miette::Result<()> {
    let filter = if std::env::var("RUST_LOG").is_ok() {
        EnvFilter::builder().from_env_lossy()
    } else {
        EnvFilter::builder().parse_lossy("metor_db=info")
    };

    let _ = tracing_subscriber::fmt::fmt()
        .with_target(false)
        .with_env_filter(filter)
        .with_timer(tracing_subscriber::fmt::time::ChronoLocal::new(
            "%Y-%m-%d %H:%M:%S%.3f".to_string(),
        ))
        .try_init();
    let args = Cli::parse();
    match args.command {
        Commands::Run(RunArgs {
            addr,
            path,
            config,
            reset,
        }) => {
            let path = path.unwrap_or_else(|| {
                let dirs =
                    directories::ProjectDirs::from("systems", "metor", "db").expect("no dirs");
                dirs.data_dir().join("data")
            });
            if reset && path.exists() {
                info!(?path, "resetting db");
                std::fs::remove_dir_all(&path).unwrap_or_else(|_| {
                    tracing::warn!("failed to remove existing data directory");
                });
            }
            info!(?path, "starting db");
            let server = Server::new(path, addr).into_diagnostic()?;
            let db = stellarator::spawn(server.run());
            if let Some(lua_config) = config {
                let args = metor_proto_cli::Args {
                    path: Some(lua_config),
                };
                metor_proto_cli::run(args)
                    .await
                    .map_err(|e| miette::miette!(e))?;
            }
            db.await.unwrap().into_diagnostic()
        }
        Commands::Lua(args) => metor_proto_cli::run(args)
            .await
            .map_err(|e| miette::miette!(e)),
        Commands::GenCpp => {
            let header = postcard_c_codegen::hpp_header(
                "ELODIN_DB",
                [
                    include_str!("../../postcard-c/postcard.h").to_string(),
                    metor_proto_wkt::InitialTimestamp::to_cpp()?,
                    metor_proto_wkt::FixedRateBehavior::to_cpp()?,
                    metor_proto_wkt::StreamBehavior::to_cpp()?,
                    metor_proto_wkt::Stream::to_cpp()?,
                    metor_proto_wkt::MsgStream::to_cpp()?,
                    vtable::Field::to_cpp()?,
                    vtable::Op::to_cpp()?,
                    vtable::OpRef::to_cpp()?,
                    metor_proto::types::PrimType::to_cpp()?,
                    vtable::VTable::<Vec<vtable::Op>, Vec<u8>, Vec<vtable::Field>>::to_cpp()?,
                    metor_proto_wkt::VTableMsg::to_cpp()?,
                    metor_proto_wkt::VTableStream::to_cpp()?,
                    metor_proto_wkt::ComponentMetadata::to_cpp()?,
                    metor_proto_wkt::SetComponentMetadata::to_cpp()?,
                    include_str!("../cpp/helpers.hpp").to_string(),
                    include_str!("../cpp/vtable.hpp").to_string(),
                ],
            )?;
            std::io::stdout()
                .write_all(header.as_bytes())
                .into_diagnostic()?;
            Ok(())
        }
    }
}
