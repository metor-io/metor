//! The CLI's output: what the terminal can show, the theme, and the
//! renderers commands report events through.

pub mod caps;
pub mod event;
pub mod human;
pub mod json;
pub mod logline;
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
    /// Show cargo's output and the files each step writes.
    #[arg(short, long, global = true)]
    pub verbose: bool,
}

/// How often a live line redraws.
const TICK: core::time::Duration = core::time::Duration::from_millis(100);

/// Drops every event.
struct Silent;

impl Reporter for Silent {
    fn event(&mut self, _event: &Event<'_>) {}
}

/// A handle commands report through. Clones share one renderer.
#[derive(Clone)]
pub struct Ui {
    reporter: Arc<Mutex<Box<dyn Reporter>>>,
    pub caps: Caps,
    pub format: Format,
    pub verbose: bool,
}

impl Ui {
    /// Detects the terminal and picks the renderer for `args`.
    ///
    /// A live terminal gets a thread that ticks its spinner. With
    /// `--verbose`, cargo writes to the terminal too, so nothing redraws.
    pub fn new(args: &OutputArgs) -> Self {
        let mut caps = match args.format {
            Format::Human => Caps::resolve(&Probe::detect(), args.color, args.ascii),
            Format::Json => Caps::plain(),
        };
        caps.live &= !args.verbose;
        let reporter: Box<dyn Reporter> = match args.format {
            Format::Human => Box::new(Human::new(std::io::stderr(), caps)),
            Format::Json => Box::new(Json::new(std::io::stdout())),
        };
        let ui = Self {
            format: args.format,
            verbose: args.verbose,
            ..Self::with(reporter, caps)
        };
        if caps.live {
            let ticker = ui.clone();
            std::thread::spawn(move || {
                loop {
                    std::thread::sleep(TICK);
                    ticker.emit(Event::Tick);
                }
            });
        }
        ui
    }

    /// A handle that drops every event.
    pub fn silent() -> Self {
        Self::with(Box::new(Silent), Caps::plain())
    }

    pub fn with(reporter: Box<dyn Reporter>, caps: Caps) -> Self {
        Self {
            reporter: Arc::new(Mutex::new(reporter)),
            caps,
            format: Format::Human,
            verbose: false,
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
