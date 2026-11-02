//! `metor build` evaluates a deployment and writes the resolved config.
//!
//! Resolving replaces every peer reference with a concrete address or port
//! definition, so each target can run from the written file alone.

use std::path::PathBuf;

use super::run::{RunError, prepare};
use super::ui::event::Event;
use super::ui::{Format, Ui};
use crate::deploy::ConfigError;

/// The arguments to `metor build`.
#[derive(clap::Args, Debug, Default)]
pub struct Build {
    /// The target file to evaluate. Defaults to the nearest `target.py` in
    /// this directory or one above it.
    #[arg(add = super::completions::files())]
    pub target: Option<PathBuf>,
    /// Where to write the resolved config. Defaults to stdout.
    #[arg(short, long)]
    pub out: Option<PathBuf>,
}

/// Evaluates and resolves the deployment, then writes it out as JSON.
///
/// Without `--out`, it goes to stdout: pretty, or with `--format json` as
/// one `config` event in the stream.
pub fn build(args: Build, ui: &Ui) -> Result<(), RunError> {
    let config = prepare(args.target.as_deref(), ui)?;
    let value = serde_json::to_value(&config).map_err(ConfigError::Decode)?;
    let text = || serde_json::to_string_pretty(&value).map_err(ConfigError::Decode);
    match (args.out, ui.format) {
        (Some(path), _) => {
            std::fs::write(&path, text()? + "\n").map_err(|source| RunError::Io { path, source })?
        }
        (None, Format::Json) => ui.emit(Event::Data("config", &value)),
        (None, Format::Human) => println!("{}", text()?),
    }
    Ok(())
}
