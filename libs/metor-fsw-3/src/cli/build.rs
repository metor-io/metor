//! `metor build`: evaluate a deployment, resolve every peer reference, and
//! write the envelope a target runs from.

use std::path::PathBuf;

use super::run::{RunError, prepare};

/// `metor build`'s arguments.
#[derive(clap::Args, Debug, Default)]
pub struct Build {
    /// The target file to evaluate; `./target.py` by default.
    pub target: Option<PathBuf>,
    /// Where to write the resolved envelope; stdout by default.
    #[arg(short, long)]
    pub out: Option<PathBuf>,
}

/// Evaluates and resolves the deployment, then writes it as JSON.
pub fn build(args: Build) -> Result<(), RunError> {
    let config = prepare(args.target.as_deref())?;
    let text = serde_json::to_string_pretty(&config).map_err(super::config::ConfigError::Decode)?;
    match args.out {
        Some(path) => std::fs::write(path, text + "\n")?,
        None => println!("{text}"),
    }
    Ok(())
}
