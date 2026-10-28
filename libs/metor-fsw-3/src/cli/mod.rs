//! The `metor` command line.
//!
//! It builds a pack's editable Python module, resolves a deployment, and runs
//! targets.

pub mod build;
pub mod cargo;
pub mod children;
pub mod module;
pub mod pack_dev;
pub mod run;

use std::ffi::OsString;
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
    /// Evaluates a target file and runs its targets.
    Run(run::Run),
    /// Evaluates a target file and writes out the resolved deployment.
    Build(build::Build),
    /// Runs `metor-<name>`, found next to this binary or on `PATH`, with the
    /// remaining arguments.
    #[command(external_subcommand)]
    External(Vec<OsString>),
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
        Command::Run(args) => run::run(args)?,
        Command::Build(args) => build::build(args)?,
        Command::External(args) => external(&args)?,
    }
    Ok(())
}

/// Runs the `metor-<name>` binary for an external subcommand and exits with
/// its status.
fn external(args: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let Some((name, rest)) = args.split_first() else {
        return Err("no subcommand".into());
    };
    let mut program = OsString::from("metor-");
    program.push(name);
    let sibling = std::env::current_exe()?.with_file_name(&program);
    let program = if sibling.is_file() { sibling.into_os_string() } else { program };
    let status = std::process::Command::new(&program)
        .args(rest)
        .status()
        .map_err(|e| format!("running {}: {e}", program.to_string_lossy()))?;
    std::process::exit(status.code().unwrap_or(1));
}
