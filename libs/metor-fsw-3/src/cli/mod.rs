//! The `metor` binary: build a pack's editable module, or run a target.

pub mod build;
pub mod config;
pub mod module;
pub mod pack_dev;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::pack::ABI_VERSION;

#[derive(Parser)]
#[command(name = "metor", about = "Build and run metor flight software")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Pack authoring.
    #[command(subcommand)]
    Pack(PackCommand),
    /// Prints the ABI version this host speaks.
    AbiVersion,
    /// Evaluates a target file and runs the graph it describes.
    Run,
}

#[derive(Subcommand)]
pub enum PackCommand {
    /// Builds a pack and lays out its editable Python module under `.metor`.
    Dev {
        /// The pack's directory, holding its `pyproject.toml` and `Cargo.toml`.
        #[arg(default_value = ".")]
        root: PathBuf,
    },
}

/// Runs the command line, reporting a failure on stderr with a nonzero status.
pub fn main() {
    if let Err(message) = run(Cli::parse()) {
        eprintln!("metor: {message}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<(), String> {
    match cli.command {
        Command::Pack(PackCommand::Dev { root }) => {
            pack_dev::pack_dev(&root).map_err(|error| error.to_string())
        }
        Command::AbiVersion => {
            println!("{ABI_VERSION}");
            Ok(())
        }
        Command::Run => Err("`run` has not landed yet".to_string()),
    }
}
