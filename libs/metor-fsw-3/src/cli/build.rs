//! `metor build` evaluates a deployment and writes the resolved config.
//!
//! Resolving replaces every peer reference with a concrete address or port
//! definition, so each target can run from the written file alone.

use std::path::PathBuf;

use super::run::{RunError, prepare};
use crate::deploy::ConfigError;

/// The arguments to `metor build`.
#[derive(clap::Args, Debug, Default)]
pub struct Build {
    /// The target file to evaluate. Defaults to `./target.py`.
    pub target: Option<PathBuf>,
    /// Where to write the resolved config. Defaults to stdout.
    #[arg(short, long)]
    pub out: Option<PathBuf>,
}

/// Evaluates and resolves the deployment, then writes it out as JSON.
pub fn build(args: Build) -> Result<(), RunError> {
    let config = prepare(args.target.as_deref())?;
    let text = serde_json::to_string_pretty(&config).map_err(ConfigError::Decode)?;
    match args.out {
        Some(path) => std::fs::write(path, text + "\n")?,
        None => println!("{text}"),
    }
    Ok(())
}
