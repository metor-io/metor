//! `metor check` finds what `metor run` would fail on, without running.
//!
//! It builds the dev packs, evaluates and resolves the deployment, and
//! builds each target's coordinator, then stops before the first cycle.

use std::path::PathBuf;

use super::run::{Run, RunError, load, phased, prepare};
use super::ui::Ui;
use super::ui::event::Verb;

/// The arguments to `metor check`.
#[derive(clap::Args, Debug, Default)]
pub struct Check {
    /// The target file to evaluate. Defaults to the nearest `target.py` in
    /// this directory or one above it.
    pub target: Option<PathBuf>,
}

/// Checks every target of the deployment, each as a phase.
pub fn check(args: Check, ui: &Ui) -> Result<(), RunError> {
    let config = prepare(args.target.as_deref(), ui)?;
    for (target, namespace) in config.targets.iter().zip(config.namespaces()) {
        let name = if namespace.is_empty() { "target" } else { namespace };
        phased(ui, Verb::Check, name, || {
            load(target, &Run::default(), None).map(drop)
        })?;
    }
    Ok(())
}
