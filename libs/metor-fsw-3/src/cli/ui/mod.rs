//! The CLI's output: what the terminal can show, the theme, and the
//! renderers commands report events through.

pub mod caps;
pub mod event;
pub mod human;
pub mod json;
pub mod theme;

use std::sync::{Arc, Mutex};

use caps::{Caps, ColorChoice, Probe};
use event::Event;
use human::Human;
use json::Json;

/// Takes events and renders them.
pub trait Reporter: Send {
    fn event(&mut self, event: &Event<'_>);
}

/// The `--format` flag.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    #[default]
    Human,
    Json,
}

/// The output flags every command takes.
#[derive(clap::Args, Clone, Debug, Default)]
pub struct OutputArgs {
    /// When to use colour.
    #[arg(long, value_enum, default_value_t, global = true)]
    pub color: ColorChoice,
    /// How to write events: styled text, or one JSON object per line on
    /// stdout.
    #[arg(long, value_enum, default_value_t, global = true)]
    pub format: Format,
    /// Draw with ASCII only.
    #[arg(long, global = true)]
    pub ascii: bool,
}

/// A handle commands report through. Clones share one renderer.
#[derive(Clone)]
pub struct Ui {
    reporter: Arc<Mutex<Box<dyn Reporter>>>,
    pub caps: Caps,
}

impl Ui {
    /// Detects the terminal and picks the renderer for `args`.
    pub fn new(args: &OutputArgs) -> Self {
        let caps = match args.format {
            Format::Human => Caps::resolve(&Probe::detect(), args.color, args.ascii),
            Format::Json => Caps::plain(),
        };
        let reporter: Box<dyn Reporter> = match args.format {
            Format::Human => Box::new(Human::new(std::io::stderr(), caps)),
            Format::Json => Box::new(Json::new(std::io::stdout())),
        };
        Self::with(reporter, caps)
    }

    pub fn with(reporter: Box<dyn Reporter>, caps: Caps) -> Self {
        Self {
            reporter: Arc::new(Mutex::new(reporter)),
            caps,
        }
    }

    /// Renders `event`.
    pub fn emit(&self, event: Event<'_>) {
        let mut reporter = self.reporter.lock().unwrap_or_else(|e| e.into_inner());
        reporter.event(&event);
    }
}

#[cfg(test)]
mod tests;
