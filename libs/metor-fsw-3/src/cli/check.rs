//! `metor check` finds what `metor run` would fail on, without running.
//!
//! It builds the dev packs, evaluates and resolves the deployment, and
//! builds each target's coordinator, then stops before the first cycle.

use std::path::PathBuf;

use super::run::{Run, RunError, load, phased, prepare};
use super::show::target_name;
use super::ui::Ui;
use super::ui::event::Verb;

/// The arguments to `metor check`.
#[derive(clap::Args, Debug, Default)]
pub struct Check {
    /// The target file to evaluate. Defaults to the nearest `target.py` in
    /// this directory or one above it.
    #[arg(add = super::completions::files())]
    pub target: Option<PathBuf>,
}

/// Checks every target of the deployment, each as a phase.
pub fn check(args: Check, ui: &Ui) -> Result<(), RunError> {
    let config = prepare(args.target.as_deref(), ui)?;
    for target in &config.targets {
        phased(ui, Verb::Check, target_name(target), || {
            load(target, &Run::default(), None).map(drop)
        })?;
    }
    Ok(())
}
