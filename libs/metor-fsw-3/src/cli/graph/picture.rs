//! A target's wiring as boxes in step order, with edges in lanes to their
//! left.

use std::io::{self, Write};

use super::lanes::{self, Span};
use super::wiring::Wiring;
use crate::cli::ui::theme::{Theme, Token};

/// What one line of a box shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Part<'a> {
    Top,
    Title,
    Input(&'a str),
    Output(&'a str),
    Bottom,
}

/// Each line of the picture: the node it belongs to and what it shows.
fn parts<'a>(wiring: &Wiring<'a>) -> Vec<(usize, Part<'a>)> {
    let mut parts = Vec::new();
    for (index, node) in wiring.nodes.iter().enumerate() {
        parts.push((index, Part::Top));
        parts.push((index, Part::Title));
        parts.extend(node.inputs.iter().map(|port| (index, Part::Input(port))));
        parts.extend(node.outputs.iter().map(|port| (index, Part::Output(port))));
        parts.push((index, Part::Bottom));
    }
    parts
}

/// One span per output that feeds an input, in rows of [`parts`].
fn spans(wiring: &Wiring<'_>) -> Vec<Span> {
    let mut starts = Vec::with_capacity(wiring.nodes.len());
    let mut row = 0;
    for node in &wiring.nodes {
        starts.push(row);
        row += 3 + node.inputs.len() + node.outputs.len();
    }
    let input_row = |node: usize, input: usize| starts[node] + 2 + input;
    let output_row =
        |node: usize, output: usize| starts[node] + 2 + wiring.nodes[node].inputs.len() + output;
    let mut spans: Vec<((usize, usize), Span)> = Vec::new();
    for edge in &wiring.edges {
        let key = (edge.from, edge.output);
        let sink = input_row(edge.to, edge.input);
        match spans.iter_mut().find(|(k, _)| *k == key) {
            Some((_, span)) => span.sinks.push(sink),
            None => spans.push((
                key,
                Span {
                    source: output_row(edge.from, edge.output),
                    sinks: vec![sink],
                },
            )),
        }
    }
    spans.into_iter().map(|(_, span)| span).collect()
}

/// Writes the boxes and lanes of `wiring`, each line indented by `indent`.
/// `notes[node]`, if any, is written beside that node's title.
pub fn render(
    out: &mut impl Write,
    wiring: &Wiring<'_>,
    notes: &[Option<String>],
    theme: &Theme,
    indent: &str,
) -> io::Result<()> {
    let parts = parts(wiring);
    let spans = spans(wiring);
    let columns = lanes::pack(&spans);
    let grid = lanes::draw(parts.len(), &spans, &columns, &theme.glyphs.lines);
    let size = Size::of(wiring);
    let lane = theme.style(Token::Accent);
    for ((node, part), lanes) in parts.iter().zip(&grid) {
        let lanes: String = lanes.iter().collect();
        write!(out, "{indent}{lane}{lanes}{lane:#}")?;
        let note = notes.get(*node).and_then(Option::as_deref);
        let node = &wiring.nodes[*node];
        write_part(out, *part, node.id, node.kind, note, size, theme)?;
        writeln!(out)?;
    }
    Ok(())
}

/// The widths every box shares, so the column lines up.
#[derive(Clone, Copy, Debug)]
struct Size {
    id: usize,
    inner: usize,
}

impl Size {
    fn of(wiring: &Wiring<'_>) -> Self {
        let width = |text: &str| text.chars().count();
        let id = wiring.nodes.iter().map(|n| width(n.id)).max().unwrap_or(0);
        let title = wiring
            .nodes
            .iter()
            .map(|n| id + 2 + width(n.kind))
            .max()
            .unwrap_or(0);
        let ports = wiring
            .nodes
            .iter()
            .flat_map(|n| n.inputs.iter().chain(&n.outputs))
            .map(|port| width(port) + 2)
            .max()
            .unwrap_or(0);
        Self {
            id,
            inner: title.max(ports),
        }
    }
}

fn write_part(
    out: &mut impl Write,
    part: Part<'_>,
    id: &str,
    kind: &str,
    note: Option<&str>,
    size: Size,
    theme: &Theme,
) -> io::Result<()> {
    let lines = &theme.glyphs.lines;
    let border = theme.style(Token::Muted);
    let rule = || lines.horizontal.to_string().repeat(size.inner + 2);
    let inner = size.inner;
    match part {
        Part::Top => write!(
            out,
            "{border}{}{}{}{border:#}",
            lines.down_right,
            rule(),
            lines.down_left
        ),
        Part::Bottom => write!(
            out,
            "{border}{}{}{}{border:#}",
            lines.up_right,
            rule(),
            lines.up_left
        ),
        Part::Title => {
            let bold = theme.bold(theme.style(Token::Text));
            let muted = theme.style(Token::Muted);
            let kind_width = inner - size.id - 2;
            write!(
                out,
                "{border}{}{border:#} {bold}{id:<id_width$}{bold:#}  {muted}{kind:<kind_width$}{muted:#} {border}{}{border:#}",
                lines.vertical,
                lines.vertical,
                id_width = size.id,
            )?;
            match note {
                Some(note) => write!(out, "  {muted}{note}{muted:#}"),
                None => Ok(()),
            }
        }
        Part::Input(port) => write!(
            out,
            "{border}{}{border:#} {port:<inner$} {border}{}{border:#}",
            lines.tee_left, lines.vertical
        ),
        Part::Output(port) => {
            let muted = theme.style(Token::Muted);
            let pad = inner - port.chars().count() - 2;
            write!(
                out,
                "{border}{}{border:#} {muted}{port} {}{muted:#}{:pad$} {border}{}{border:#}",
                lines.tee_left, lines.out, "", lines.vertical
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::graph::wiring::{Edge, Node};
    use crate::cli::ui::caps::Caps;

    fn node<'a>(id: &'a str, kind: &'a str, inputs: &[&'a str], outputs: &[&'a str]) -> Node<'a> {
        Node {
            id,
            kind,
            inputs: inputs.to_vec(),
            outputs: outputs.to_vec(),
        }
    }

    /// The fixture's shape: a subscriber, a self loop, and a fan-in.
    fn fixture() -> Wiring<'static> {
        Wiring {
            nodes: vec![
                node("cmds", "fsw.subscribe", &[], &["ping"]),
                node("gain", "echo.Gain", &["input"], &["output"]),
                node("echo", "echo.Echo", &["input"], &["output"]),
                node("pub", "fsw.publish", &["echo", "gain"], &[]),
            ],
            edges: vec![
                Edge {
                    from: 1,
                    output: 0,
                    to: 1,
                    input: 0,
                },
                Edge {
                    from: 0,
                    output: 0,
                    to: 2,
                    input: 0,
                },
                Edge {
                    from: 2,
                    output: 0,
                    to: 3,
                    input: 0,
                },
                Edge {
                    from: 1,
                    output: 0,
                    to: 3,
                    input: 1,
                },
            ],
        }
    }

    fn rendered(caps: &Caps) -> String {
        let mut out = Vec::new();
        let notes = [Some("⇄ ground.cmd".into()), None, None, None];
        render(&mut out, &fixture(), &notes, &Theme::new(caps), "  ").expect("writes to a Vec");
        String::from_utf8(out).expect("utf-8")
    }

    fn unicode() -> Caps {
        Caps {
            unicode: true,
            ..Caps::plain()
        }
    }

    #[test]
    fn test_render_unicode() {
        insta::assert_snapshot!(rendered(&unicode()));
    }

    #[test]
    fn test_render_ascii() {
        let caps = Caps {
            unicode: false,
            ..Caps::plain()
        };
        insta::assert_snapshot!(rendered(&caps));
    }

    #[test]
    fn test_spans_group_an_output_s_edges() {
        let spans = spans(&fixture());
        assert_eq!(spans.len(), 3);
        assert_eq!(
            spans[0],
            Span {
                source: 7,
                sinks: vec![6, 17]
            },
            "gain loops and feeds pub"
        );
    }

    #[test]
    fn test_render_without_edges_is_boxes() {
        let wiring = Wiring {
            nodes: vec![node("a", "k", &[], &[])],
            edges: Vec::new(),
        };
        let mut out = Vec::new();
        render(&mut out, &wiring, &[], &Theme::new(&unicode()), "").expect("writes to a Vec");
        assert_eq!(
            String::from_utf8(out).expect("utf-8"),
            "╭──────╮\n│ a  k │\n╰──────╯\n"
        );
    }
}
