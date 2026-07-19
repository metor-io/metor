//! Terminal presentation for the `metor-fsw` CLI.
//!
//! Two pieces: [`init_tracing`] installs the subscriber that turns the build
//! pipeline's tracing output into a live display — active `build`-target
//! spans render as a pinned progress line (spinner, label, the crate cargo is
//! currently compiling, elapsed time) while `cargo`-target events stream
//! above it — and [`print_preflight`] lists the systems and slots a mission
//! will run before it starts. Both degrade to plain text when stderr is not a
//! terminal: indicatif hides its bars and events print as ordinary lines,
//! and the pre-flight colors are gated on stream support.

use std::io::IsTerminal;
use std::path::Path;

use owo_colors::{OwoColorize, Stream};
use tracing_indicatif::IndicatifLayer;
use tracing_indicatif::style::ProgressStyle;
use tracing_subscriber::filter::{Targets, filter_fn};
use tracing_subscriber::layer::{Layer, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;

use crate::wiring::{ClockSpec, Wiring};

/// Install the CLI's tracing subscriber. `build`-target spans get the pinned
/// progress line; `cargo` and `build` events pass the default filter, which
/// `RUST_LOG` overrides for debugging.
pub(super) fn init_tracing() {
    let indicatif_layer = IndicatifLayer::new().with_progress_style(
        ProgressStyle::with_template("{span_child_prefix}{spinner} {msg} {elapsed}")
            .expect("static template parses"),
    );
    let writer = indicatif_layer.get_stderr_writer();
    let events = std::env::var("RUST_LOG")
        .ok()
        .and_then(|spec| spec.parse::<Targets>().ok())
        .unwrap_or_else(|| {
            Targets::new()
                .with_target("cargo", tracing::Level::INFO)
                .with_target("build", tracing::Level::INFO)
                .with_default(tracing::Level::WARN)
        });
    tracing_subscriber::registry()
        .with(indicatif_layer.with_filter(filter_fn(|meta| meta.target() == "build")))
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .without_time()
                .with_target(false)
                .with_level(false)
                .with_ansi(std::io::stderr().is_terminal())
                .with_filter(events),
        )
        .init();
}

/// One two-line entry in the pre-flight listing, with the bar color that
/// signifies its kind.
struct Entry {
    name: String,
    ty: String,
    detail: String,
    bar: owo_colors::Style,
}

/// Print the ordered list of systems and slots `run` is about to start, to
/// stderr: a header with the effective clock, then one two-line entry each —
/// instance name and type, with the source (pack distribution, crate, or
/// builtin; a slot's allowed occupants) dimmed underneath. Each entry carries
/// a colored bar keying its kind: magenta for pack-loaded systems, yellow
/// for worker-process systems, cyan for builtins, green for slots. The order
/// is [`Wiring::systems`] then [`Wiring::slots`], which is also resolve's
/// registration order.
pub(super) fn print_preflight(wiring: &Wiring, target: &Path) {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| target.display().to_string());
    let clock = match wiring.coordinator.clock {
        ClockSpec::Wall => "wall clock".to_string(),
        ClockSpec::Simulated { dt_secs } => format!("sim dt {} ms", trim(dt_secs * 1e3)),
    };
    let slots = match wiring.slots.len() {
        0 => String::new(),
        n => format!(" · {n} slot(s)"),
    };
    eprintln!();
    eprintln!(
        "  {} · {} system(s){slots} · {} Hz · {}",
        name.if_supports_color(Stream::Stderr, |t| t.bold()),
        wiring.systems.len(),
        trim(wiring.coordinator.cycle_rate),
        clock,
    );
    eprintln!();

    let mut entries: Vec<Entry> = wiring
        .systems
        .iter()
        .map(|sys| {
            let bar = if sys.process {
                owo_colors::Style::new().bright_yellow()
            } else if sys.artifact.is_some() {
                owo_colors::Style::new().bright_magenta()
            } else {
                owo_colors::Style::new().bright_cyan()
            };
            let mut detail = source_of(wiring, sys.artifact.as_deref());
            if sys.process {
                detail.push_str(" · process");
            }
            Entry {
                name: sys.name.clone(),
                ty: sys
                    .ty
                    .clone()
                    .unwrap_or_else(|| "(sole pack entry)".to_string()),
                detail,
                bar,
            }
        })
        .collect();
    for slot in &wiring.slots {
        // The allowed occupants, the initial one (if any) marked.
        let occupants: Vec<String> = slot
            .allow
            .iter()
            .map(|a| {
                let initial = slot
                    .initial
                    .as_ref()
                    .is_some_and(|i| i.occupant == a.occupant);
                if initial {
                    format!("▸{}", a.occupant)
                } else {
                    a.occupant.clone()
                }
            })
            .collect();
        let mut detail = format!("allow {}", occupants.join(", "));
        if slot.process {
            detail.push_str(" · process");
        }
        entries.push(Entry {
            name: slot.name.clone(),
            ty: "slot".to_string(),
            detail,
            bar: owo_colors::Style::new().bright_green(),
        });
    }

    let width = entries.iter().map(|e| e.name.len()).max().unwrap_or(0);
    for e in &entries {
        let bar = "┃".if_supports_color(Stream::Stderr, |t| t.style(e.bar));
        // Pad before styling: ANSI escapes would defeat a format width.
        let pad = " ".repeat(width - e.name.len());
        eprintln!(
            "  {bar} {}{pad}  {}",
            e.name.if_supports_color(Stream::Stderr, |t| t.bold()),
            e.ty,
        );
        eprintln!(
            "  {bar} {:width$}  {}",
            "",
            e.detail.if_supports_color(Stream::Stderr, |t| t.dimmed()),
        );
    }
    eprintln!();
}

/// Where a system comes from, for the pre-flight listing: its artifact's
/// distribution (`adcs-pack 1.2.0`), bare crate, or the static registry.
fn source_of(wiring: &Wiring, artifact: Option<&str>) -> String {
    let Some(id) = artifact else {
        return "builtin".to_string();
    };
    match wiring.artifacts.iter().find(|a| a.id == id) {
        Some(a) => match &a.dist {
            Some(dist) => format!("{} {}", dist.name, dist.version),
            None => format!("crate {}", a.crate_name),
        },
        // Unreachable after validation, but the listing is display-only.
        None => id.to_string(),
    }
}

/// `f64` display without trailing fraction noise: `120`, not `120.000`.
fn trim(value: f64) -> String {
    let text = format!("{value:.3}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}
