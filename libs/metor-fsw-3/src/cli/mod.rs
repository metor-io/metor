//! The `metor` command line.
//!
//! It builds a pack's editable Python module.

pub mod cargo;
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
    /// Commands for pack authors.
    #[command(subcommand)]
    Pack(PackCommand),
    /// Prints the pack ABI version this binary supports.
    AbiVersion,
}

#[derive(Subcommand)]
pub enum PackCommand {
    /// Builds a pack and writes its editable Python module under `.metor`.
    Dev {
        /// The pack's directory, which contains its `pyproject.toml` and
        /// `Cargo.toml`.
        #[arg(default_value = ".")]
        root: PathBuf,
    },
}

/// Runs the command line. On failure, prints the error to stderr and exits
/// with status 1.
pub fn main() {
    if let Err(message) = run(Cli::parse()) {
        eprintln!("metor: {message}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        Command::Pack(PackCommand::Dev { root }) => pack_dev::pack_dev(&root)?,
        Command::AbiVersion => println!("{ABI_VERSION}"),
    }
    Ok(())
}
