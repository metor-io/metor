//! `metor graph` draws a deployment's wiring.

pub mod export;
pub mod lanes;
pub mod picture;
pub mod wiring;

use std::io::{self, Write};
use std::path::PathBuf;

use super::completions;
use super::run::{RunError, prepare};
use super::show::{self, Selection, select, split_args};
use super::ui::event::Event;
use super::ui::theme::Theme;
use super::ui::{Format, Ui};
use crate::deploy::ResolvedDeployment;
use picture::Detail;
use wiring::{At, Link, links};

/// The arguments to `metor graph`.
#[derive(clap::Args, Debug, Default)]
pub struct Graph {
    /// A target file, then a target's name. Without a file, the nearest
    /// `target.py` is drawn.
    #[arg(num_args = 0..=2, value_name = "FILE|TARGET", add = completions::files_or_targets())]
    pub args: Vec<String>,
    /// Write text for another tool instead of a picture.
    #[arg(long = "as", value_enum, value_name = "FORMAT")]
    pub text: Option<Text>,
    /// Draw a lane per port instead of one per system.
    #[arg(long)]
    pub ports: bool,
}

/// A text format `--as` writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Text {
    Mermaid,
    Dot,
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
        .map(|&t| (target_name(&config, t), wiring::wiring(&config.targets[t])))
        .collect();
    let within = within(&links, &chosen);
    let text = match (args.text, ui.format) {
        (Some(Text::Mermaid), _) => export::mermaid(&targets, &within),
        (Some(Text::Dot), _) => export::dot(&targets, &within),
        (None, Format::Json) => {
            ui.emit(Event::Data("graph", &export::json(&targets, &within)));
            return Ok(());
        }
        (None, Format::Human) => {
            let theme = Theme::new(&ui.stdout_caps());
            let detail = match args.ports {
                true => Detail::Ports,
                false => Detail::Systems,
            };
            let picture = Picture {
                config: &config,
                chosen: &chosen,
                links: &links,
                theme: &theme,
                detail,
            };
            return write_stdout(|out| draw(out, &picture, &targets));
        }
    };
    write_stdout(|out| out.write_all(text.as_bytes()))
}

fn write_stdout(
    write: impl FnOnce(&mut io::StdoutLock<'static>) -> io::Result<()>,
) -> Result<(), RunError> {
    write(&mut io::stdout().lock()).map_err(|source| RunError::Io {
        path: PathBuf::from("stdout"),
        source,
    })
}

/// What a human picture draws, and how.
pub struct Picture<'a> {
    pub config: &'a ResolvedDeployment,
    pub chosen: &'a [usize],
    pub links: &'a [Link],
    pub theme: &'a Theme,
    pub detail: Detail,
}

/// Each target in `picture.chosen`, with its wiring in `targets`: its title
/// line, then its picture.
pub fn draw(
    out: &mut impl Write,
    picture: &Picture<'_>,
    targets: &[export::Target<'_>],
) -> io::Result<()> {
    let Picture {
        config,
        chosen,
        links,
        theme,
        detail,
    } = *picture;
    for (index, (&t, (_, wiring))) in chosen.iter().zip(targets).enumerate() {
        if index > 0 {
            writeln!(out)?;
        }
        show::write_title(out, &config.targets[t], t, theme)?;
        writeln!(out)?;
        let notes = notes(config, links, t, theme.glyphs.link);
        picture::render(out, wiring, &notes, theme, "  ", detail)?;
    }
    Ok(())
}

/// `fsw`, or `target` for a deployment's one unnamed target.
fn target_name(config: &ResolvedDeployment, target: usize) -> &str {
    Some(config.targets[target].namespace())
        .filter(|ns| !ns.is_empty())
        .unwrap_or("target")
}

/// `fsw.plant`.
fn system_name(config: &ResolvedDeployment, (t, s): At) -> String {
    let id = &config.targets[t].coordinator.systems[s].id;
    format!("{}.{id}", target_name(config, t))
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
                .filter_map(
                    |link| match ((target, s) == link.dial, (target, s) == link.listen) {
                        (true, _) => Some(system_name(config, link.listen)),
                        (_, true) => Some(system_name(config, link.dial)),
                        _ => None,
                    },
                )
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
