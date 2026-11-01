//! The CLI's output: what the terminal can show, the theme, and the
//! renderers commands report events through.

pub mod caps;
pub mod event;
pub mod glyph;
pub mod human;
pub mod json;
pub mod table;
pub mod theme;

use std::sync::{Arc, Mutex};

use caps::{Caps, ColorChoice};
use event::Event;
use human::Human;
use json::Json;

/// The columns `text` takes in a terminal.
pub fn width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

/// The spaces that pad `text` to `columns`.
pub fn pad(text: &str, columns: usize) -> String {
    " ".repeat(columns.saturating_sub(width(text)))
}

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
    #[arg(
        long,
        value_enum,
        default_value_t,
        global = true,
        help_heading = "Output"
    )]
    pub color: ColorChoice,
    /// How to write events: styled text, or one JSON object per line on
    /// stdout.
    #[arg(
        long,
        value_enum,
        default_value_t,
        global = true,
        help_heading = "Output"
    )]
    pub format: Format,
    /// Show cargo's output and the files each step writes.
    #[arg(short, long, global = true, help_heading = "Output")]
    pub verbose: bool,
}

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
    color: ColorChoice,
    pub format: Format,
    pub verbose: bool,
    /// Drop the deployment's phases and `Running`, which the parent of a
    /// multi-target run shows once.
    child: bool,
}

impl Ui {
    /// Picks the renderer for `args` on a `terminal`. With `--verbose`,
    /// cargo writes to the terminal too, so nothing spins.
    pub fn new(args: &OutputArgs, terminal: Caps) -> Self {
        let mut caps = match args.format {
            Format::Human => terminal,
            Format::Json => Caps::plain(),
        };
        caps.live &= !args.verbose;
        let reporter: Box<dyn Reporter> = match args.format {
            Format::Human => Box::new(Human::new(std::io::stderr(), caps)),
            Format::Json => Box::new(Json::new(std::io::stdout())),
        };
        Self {
            color: args.color,
            format: args.format,
            verbose: args.verbose,
            child: crate::cli::children::is_child(),
            ..Self::with(reporter, caps)
        }
    }

    /// What stdout can show, under the same `--color`.
    pub fn stdout_caps(&self) -> Caps {
        match self.format {
            Format::Human => Caps::stdout(self.color, self.caps),
            Format::Json => Caps::plain(),
        }
    }

    /// The terminal's width when stdout is a terminal.
    pub fn stdout_width(&self) -> Option<usize> {
        use std::io::IsTerminal;
        std::io::stdout().is_terminal().then(|| {
            terminal_size::terminal_size().map_or(usize::from(self.caps.width), |(width, _)| {
                usize::from(width.0)
            })
        })
    }

    /// A handle that drops every event.
    pub fn silent() -> Self {
        Self::with(Box::new(Silent), Caps::plain())
    }

    pub fn with(reporter: Box<dyn Reporter>, caps: Caps) -> Self {
        Self {
            reporter: Arc::new(Mutex::new(reporter)),
            caps,
            color: ColorChoice::Auto,
            format: Format::Human,
            verbose: false,
            child: false,
        }
    }

    /// Renders `event`.
    pub fn emit(&self, event: Event<'_>) {
        let deployment = matches!(event, Event::Phase(_) | Event::Running(_));
        if self.child && deployment {
            return;
        }
        let mut reporter = self.reporter.lock().unwrap_or_else(|e| e.into_inner());
        reporter.event(&event);
    }
}

#[cfg(test)]
mod tests;
