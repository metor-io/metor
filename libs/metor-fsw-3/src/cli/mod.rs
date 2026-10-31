//! The `metor` command line.
//!
//! It builds a pack's editable Python module, resolves a deployment, and runs
//! targets.

pub mod build;
pub mod cargo;
pub mod check;
pub mod children;
pub mod console;
pub mod diagnostic;
pub mod doctor;
pub mod module;
pub mod pack_dev;
mod port_class;
pub mod relay;
pub mod run;
pub mod show;
pub mod ui;
pub mod units;

use std::path::PathBuf;

use clap::builder::Styles;
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};

use crate::pack::ABI_VERSION;
use diagnostic::{Report, exit_code};
use run::RunError;
use ui::caps::{Caps, ColorChoice, Probe};
use ui::event::Event;
use ui::theme::{Theme, Token};
use ui::{OutputArgs, Ui};

#[derive(Parser)]
#[command(
    name = "metor",
    version,
    about = "Build and run metor flight software",
    after_help = EXAMPLES
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
    #[command(flatten)]
    pub output: OutputArgs,
}

#[derive(Subcommand)]
pub enum Command {
    /// Evaluates a target file and runs its targets.
    #[command(after_help = RUN_EXAMPLES)]
    Run(run::Run),
    /// Builds everything `run` would, and stops before the first cycle.
    #[command(
        after_help = "Examples:\n  metor check                  the nearest target.py\n  metor check deploy.py"
    )]
    Check(check::Check),
    /// Prints the deployment's targets, systems, and wiring.
    #[command(after_help = SHOW_EXAMPLES)]
    Show(show::Show),
    /// Evaluates a target file and writes out the resolved deployment.
    #[command(
        after_help = "Examples:\n  metor build -o deploy.json   resolve once, run anywhere\n  metor run deploy.json"
    )]
    Build(build::Build),
    /// Checks the tools a project needs.
    Doctor,
    /// Commands for pack authors.
    #[command(subcommand)]
    Pack(PackCommand),
    /// Prints the pack ABI version this binary supports.
    #[command(hide = true)]
    AbiVersion,
}

const EXAMPLES: &str = "Examples:
  metor run                    run the nearest target.py
  metor check                  find errors without running
  metor show fsw               list a target's systems
  metor doctor                 check python, cargo, and wasm32";

const RUN_EXAMPLES: &str = "Examples:
  metor run                            the nearest target.py
  metor run --target fsw               one target of a deployment
  metor run --sim-dt 10ms --for 30s    30 s of simulated time
  metor run --wall 50hz --cycles 500   pace at 50 Hz, stop after 500 cycles";

const SHOW_EXAMPLES: &str = "Examples:
  metor show                   every target
  metor show fsw               one target
  metor show fsw.nav           one system, with its params and grants";

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
    let early = Early::scan(std::env::args_os().skip(1));
    let probe = Probe::detect(early.query);
    let cli = parse(&probe, &early);
    let ui = Ui::new(&cli.output, &probe);
    let Err(error) = run(cli.command, &ui) else {
        return;
    };
    if !matches!(error, RunError::Interrupted) {
        ui.emit(Event::Diagnostic(&Report::new(&error)));
    }
    std::process::exit(exit_code(&error));
}

/// Parses the command line, with help styled for the terminal `probe`
/// describes.
fn parse(probe: &Probe, early: &Early) -> Cli {
    let caps = Caps::resolve(probe, early.color, early.ascii);
    let color = match early.color {
        ColorChoice::Auto => clap::ColorChoice::Auto,
        ColorChoice::Always => clap::ColorChoice::Always,
        ColorChoice::Never => clap::ColorChoice::Never,
    };
    let command = Cli::command()
        .color(color)
        .styles(styles(&Theme::new(&caps)));
    let matches = command.get_matches();
    Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit())
}

/// The output flags, read before clap parses, so help and the terminal
/// probe honour them.
#[derive(Debug, PartialEq, Eq)]
struct Early {
    color: ColorChoice,
    ascii: bool,
    /// Whether the terminal's background is worth asking for: not for JSON,
    /// help, or the version.
    query: bool,
}

impl Early {
    fn scan(args: impl IntoIterator<Item = std::ffi::OsString>) -> Self {
        let args: Vec<String> = args
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let value = |flag: &str| {
            let prefix = format!("{flag}=");
            args.iter()
                .enumerate()
                .find_map(|(i, arg)| match arg.strip_prefix(&prefix) {
                    Some(value) => Some(value.to_string()),
                    None if arg == flag => args.get(i + 1).cloned(),
                    None => None,
                })
        };
        let color = match value("--color").as_deref() {
            Some("always") => ColorChoice::Always,
            Some("never") => ColorChoice::Never,
            _ => ColorChoice::Auto,
        };
        let quick = ["-h", "--help", "-V", "--version", "help"];
        let json = value("--format").as_deref() == Some("json");
        Self {
            color,
            ascii: args.iter().any(|arg| arg == "--ascii"),
            query: !json && !args.iter().any(|arg| quick.contains(&arg.as_str())),
        }
    }
}

/// Help's styles from `theme`.
fn styles(theme: &Theme) -> Styles {
    Styles::styled()
        .header(theme.bold(theme.style(Token::Accent)))
        .usage(theme.bold(theme.style(Token::Accent)))
        .literal(theme.bold(theme.style(Token::Text)))
        .placeholder(theme.style(Token::Muted))
        .error(theme.bold(theme.style(Token::Error)))
        .valid(theme.style(Token::Ok))
        .invalid(theme.style(Token::Warn))
}

fn run(command: Command, ui: &Ui) -> Result<(), RunError> {
    match command {
        Command::Pack(PackCommand::Dev { root }) => pack_dev::pack_dev(&root, ui)?,
        Command::AbiVersion => println!("{ABI_VERSION}"),
        Command::Doctor => doctor::doctor(ui)?,
        Command::Run(args) => run::run(args, ui)?,
        Command::Build(args) => build::build(args, ui)?,
        Command::Check(args) => check::check(args, ui)?,
        Command::Show(args) => show::show(args, ui)?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(args: &[&str]) -> Early {
        Early::scan(args.iter().map(std::ffi::OsString::from))
    }

    #[test]
    fn test_early_reads_output_flags() {
        let early = scan(&["run", "--color", "never", "--ascii"]);
        assert_eq!(early.color, ColorChoice::Never);
        assert!(early.ascii && early.query);
        assert_eq!(scan(&["--color=always"]).color, ColorChoice::Always);
        assert_eq!(scan(&["run", "--color"]).color, ColorChoice::Auto);
        assert!(!scan(&["run", "--format", "json"]).query);
        assert!(!scan(&["--format=json"]).query);
        assert!(!scan(&["run", "--help"]).query);
        assert!(!scan(&["-V"]).query);
    }

    #[test]
    fn test_cli_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn test_help_has_examples_and_plain_styles_add_no_escapes() {
        let theme = Theme::new(&Caps::plain());
        let mut command = Cli::command().styles(styles(&theme));
        let help = command.render_help().ansi().to_string();
        assert!(help.contains("metor check"), "{help}");
        assert!(!help.contains('\x1b'), "{help:?}");
        let run = command.find_subcommand_mut("run").expect("run exists");
        assert!(run.render_long_help().to_string().contains("--for 30s"));
    }
}
