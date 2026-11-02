//! `metor graph` draws a deployment's wiring.

pub mod export;
pub mod lanes;
pub mod picture;
pub mod wiring;

use std::io::{self, Write};

use super::completions;
use super::run::{RunError, prepare, print};
use super::show::{self, Selection, select, split_args, target_name};
use super::ui::event::Event;
use super::ui::theme::Theme;
use super::ui::{Format, Ui};
use crate::cli::ui::glyph;
use crate::deploy::ResolvedDeployment;
use picture::Detail;
use wiring::{At, Link, links};

/// The arguments to `metor graph`.
#[derive(clap::Args, Debug, Default)]
pub struct Graph {
    /// A target file, then a target's name. Without a file, the nearest
    /// `target.py` is drawn.
    #[arg(num_args = 0..=2, value_name = "FILE|TARGET", add = completions::names(&[completions::TARGET], true))]
    pub args: Vec<String>,
    /// Write text for another tool instead of a picture.
    #[arg(long = "as", value_enum, value_name = "FORMAT")]
    pub text: Option<Text>,
    /// Draw a lane per port instead of one per system.
    #[arg(long, conflicts_with = "text")]
    pub ports: bool,
}

/// A text format `--as` writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Text {
    Mermaid,
}

/// Draws the deployment, or the target `args` names, to stdout.
pub fn graph(args: Graph, ui: &Ui) -> Result<(), RunError> {
    let (file, name) = split_args(&args.args);
    let config = prepare(file.as_deref(), ui)?;
    let chosen = match select(&config, name.as_deref())? {
        Selection::All => (0..config.targets.len()).collect(),
        Selection::Target(target) | Selection::System(target, _) => vec![target],
    };
    let links = links(&config);
    let targets: Vec<export::Target<'_>> = chosen
        .iter()
        .map(|&t| {
            (
                target_name(&config.targets[t]),
                wiring::wiring(&config.targets[t]),
            )
        })
        .collect();
    let within = within(&links, &chosen);
    let detail = match args.ports {
        true => Detail::Ports,
        false => Detail::Systems,
    };
    match (args.text, ui.format) {
        (Some(Text::Mermaid), _) => print(ui, |out, _, _| {
            out.write_all(export::mermaid(&targets, &within).as_bytes())
        }),
        (None, Format::Json) => {
            ui.emit(Event::Data("graph", &export::json(&targets, &within)));
            Ok(())
        }
        (None, Format::Human) => print(ui, |out, theme, _| {
            draw(out, &config, &links, &chosen, theme, detail)
        }),
    }
}

/// Each target in `chosen`: its title line, then its picture.
fn draw(
    out: &mut impl Write,
    config: &ResolvedDeployment,
    links: &[Link],
    chosen: &[usize],
    theme: &Theme,
    detail: Detail,
) -> io::Result<()> {
    for (index, &t) in chosen.iter().enumerate() {
        if index > 0 {
            writeln!(out)?;
        }
        show::write_title(out, &config.targets[t], t, theme)?;
        writeln!(out)?;
        let notes = notes(config, links, t, glyph::LINK);
        let wiring = wiring::wiring(&config.targets[t]);
        picture::render(out, &wiring, &notes, theme, "  ", detail)?;
    }
    Ok(())
}

/// `fsw.plant`.
fn system_name(config: &ResolvedDeployment, (t, s): At) -> String {
    let id = &config.targets[t].coordinator.systems[s].id;
    format!("{}.{id}", target_name(&config.targets[t]))
}

/// Beside each system of `target` with a link, the systems at its other end.
fn notes(
    config: &ResolvedDeployment,
    links: &[Link],
    target: usize,
    glyph: &str,
) -> Vec<Option<String>> {
    let systems = config.targets[target].coordinator.systems.len();
    (0..systems)
        .map(|s| {
            let peers: Vec<String> = links
                .iter()
                .filter_map(|link| match (target, s) {
                    at if at == link.dial => Some(link.listen),
                    at if at == link.listen => Some(link.dial),
                    _ => None,
                })
                .map(|at| system_name(config, at))
                .collect();
            (!peers.is_empty()).then(|| format!("{glyph} {}", peers.join(", ")))
        })
        .collect()
}

/// The links with both ends in `chosen`, renumbered to positions in it.
fn within(links: &[Link], chosen: &[usize]) -> Vec<Link> {
    let position = |(t, s): At| Some((chosen.iter().position(|&c| c == t)?, s));
    links
        .iter()
        .filter_map(|link| {
            Some(Link {
                dial: position(link.dial)?,
                listen: position(link.listen)?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_within_drops_and_renumbers() {
        let links = [
            Link {
                dial: (2, 0),
                listen: (0, 1),
            },
            Link {
                dial: (1, 0),
                listen: (0, 1),
            },
        ];
        assert_eq!(
            within(&links, &[0, 2]),
            [Link {
                dial: (1, 0),
                listen: (0, 1)
            }]
        );
        assert!(within(&links, &[2]).is_empty());
    }
}
