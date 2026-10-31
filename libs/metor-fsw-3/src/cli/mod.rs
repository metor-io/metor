//! The `metor` command line.
//!
//! It builds a pack's editable Python module, resolves a deployment, and runs
//! targets.

pub mod build;
pub mod cargo;
pub mod children;
pub mod console;
pub mod diagnostic;
pub mod module;
pub mod pack_dev;
mod port_class;
pub mod run;
pub mod ui;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::pack::ABI_VERSION;
use diagnostic::{Report, exit_code};
use run::RunError;
use ui::event::Event;
use ui::{OutputArgs, Ui};

#[derive(Parser)]
#[command(name = "metor", about = "Build and run metor flight software")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
    #[command(flatten)]
    pub output: OutputArgs,
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

/// Runs the command line. On failure, reports the error and exits with its
/// status. An interrupted run's summary already says so.
pub fn main() {
    let cli = Cli::parse();
    let ui = Ui::new(&cli.output);
    let Err(error) = run(cli.command, &ui) else {
        return;
    };
    if !matches!(error, RunError::Interrupted) {
        ui.emit(Event::Diagnostic(&Report::new(&error)));
    }
    std::process::exit(exit_code(&error));
}

fn run(command: Command, ui: &Ui) -> Result<(), RunError> {
    match command {
        Command::Pack(PackCommand::Dev { root }) => pack_dev::pack_dev(&root, ui)?,
        Command::AbiVersion => println!("{ABI_VERSION}"),
        Command::Run(args) => run::run(args, ui)?,
        Command::Build(args) => build::build(args, ui)?,
    }
    Ok(())
}
