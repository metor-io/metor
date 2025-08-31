use clap::Parser;
use std::net::SocketAddr;
use tracing_subscriber::EnvFilter;

mod editor;

#[derive(Parser, Clone)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    addr: Option<SocketAddr>,
}

impl Cli {
    pub fn from_os_args() -> Self {
        Self::parse()
    }

    pub fn run(self) -> miette::Result<()> {
        let filter = if std::env::var("RUST_LOG").is_ok() {
            EnvFilter::builder().from_env_lossy()
        } else {
            EnvFilter::builder().parse_lossy("metor=info,impeller=info,error")
        };

        let _ = tracing_subscriber::fmt::fmt()
            .with_target(false)
            .with_env_filter(filter)
            .with_timer(tracing_subscriber::fmt::time::ChronoLocal::new(
                "%Y-%m-%d %H:%M:%S%.3f".to_string(),
            ))
            .try_init();

        self.editor()
    }

    fn dirs(&self) -> Result<directories::ProjectDirs, std::io::Error> {
        directories::ProjectDirs::from("io", "metor", "ui").ok_or(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "failed to get data directory",
        ))
    }
}
