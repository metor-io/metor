//! A target's wiring as boxes in step order, with edges in lanes to their
//! left.

use std::io::{self, Write};

use super::lanes::{self, Span};
use super::wiring::{Edge, Wiring};
use crate::cli::ui::theme::{Theme, Token};
use crate::cli::ui::{pad, width};

/// How finely a picture draws edges.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Detail {
    /// One lane per source system, and one row per system feeding a box.
    #[default]
    Systems,
    /// One lane per output port, and one row per port.
    Ports,
}

/// The widest a list of port names runs before it ends in `+N`.
const LIST: usize = 32;

/// What one line of a box shows.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Part {
    Top,
    Title,
    /// Input port names, and the system they come from.
    Input {
        ports: String,
        from: Option<String>,
    },
    Output(String),
    Bottom,
}

/// The lines of a picture, the node each belongs to, and the lanes between
/// them, and the `idle` nodes left out.
#[derive(Debug, Default)]
struct Layout {
    parts: Vec<(usize, Part)>,
    spans: Vec<Span>,
    idle: Vec<usize>,
}

impl Layout {
    /// Adds `part` to `node` and returns its row.
    fn push(&mut self, node: usize, part: Part) -> usize {
        self.parts.push((node, part));
        self.parts.len() - 1
    }
}

/// Whether `node` has neither an edge nor a note, and so gets no box.
fn is_idle(wiring: &Wiring<'_>, notes: &[Option<String>], node: usize) -> bool {
    let ports = &wiring.nodes[node];
    let noted = notes.get(node).is_some_and(Option::is_some);
    ports.inputs.is_empty() && ports.outputs.is_empty() && !noted
}

/// A row per port, and a lane per output.
fn by_port(wiring: &Wiring<'_>, notes: &[Option<String>], out: char) -> Layout {
    let mut layout = Layout::default();
    let mut inputs = vec![Vec::new(); wiring.nodes.len()];
    let mut outputs = vec![Vec::new(); wiring.nodes.len()];
    for (index, node) in wiring.nodes.iter().enumerate() {
        if is_idle(wiring, notes, index) {
            layout.idle.push(index);
            continue;
        }
        layout.push(index, Part::Top);
        layout.push(index, Part::Title);
        for port in &node.inputs {
            let ports = port.to_string();
            inputs[index].push(layout.push(index, Part::Input { ports, from: None }));
        }
        for port in &node.outputs {
            outputs[index].push(layout.push(index, Part::Output(format!("{port} {out}"))));
        }
        layout.push(index, Part::Bottom);
    }
    let mut keyed: Vec<((usize, usize), Span)> = Vec::new();
    for edge in &wiring.edges {
        let sink = inputs[edge.to][edge.input];
        let key = (edge.from, edge.output);
        match keyed.iter_mut().find(|(k, _)| *k == key) {
            Some((_, span)) => span.sinks.push(sink),
            None => keyed.push((
                key,
                Span {
                    source: outputs[edge.from][edge.output],
                    sinks: vec![sink],
                },
            )),
        }
    }
    layout.spans = keyed.into_iter().map(|(_, span)| span).collect();
    layout
}

/// A row per system feeding a box, one row for a box's outputs, and a lane
/// per source system.
fn by_system(wiring: &Wiring<'_>, notes: &[Option<String>], out: char) -> Layout {
    let mut layout = Layout::default();
    let mut sources = vec![None; wiring.nodes.len()];
    let mut sinks: Vec<(usize, usize)> = Vec::new();
    for (index, node) in wiring.nodes.iter().enumerate() {
        if is_idle(wiring, notes, index) {
            layout.idle.push(index);
            continue;
        }
        layout.push(index, Part::Top);
        layout.push(index, Part::Title);
        for source in feeders(wiring, index) {
            let part = group(wiring, source, index);
            sinks.push((source, layout.push(index, part)));
        }
        if !node.outputs.is_empty() {
            let part = Part::Output(format!("{out} {}", fold(&node.outputs, LIST)));
            sources[index] = Some(layout.push(index, part));
        }
        layout.push(index, Part::Bottom);
    }
    layout.spans = sources
        .iter()
        .enumerate()
        .filter_map(|(node, row)| {
            let sinks = sinks.iter().filter(|(s, _)| *s == node).map(|(_, r)| *r);
            Some(Span {
                source: (*row)?,
                sinks: sinks.collect(),
            })
        })
        .collect();
    layout
}

/// The systems feeding `node`, in the order of their first edge.
fn feeders(wiring: &Wiring<'_>, node: usize) -> Vec<usize> {
    let mut feeders = Vec::new();
    for edge in wiring.edges.iter().filter(|e| e.to == node) {
        if !feeders.contains(&edge.from) {
            feeders.push(edge.from);
        }
    }
    feeders
}

/// The row for the inputs of `node` that `source` feeds. A lone port named
/// neither `output` nor `source.output` names the output it comes from.
fn group(wiring: &Wiring<'_>, source: usize, node: usize) -> Part {
    let edges: Vec<&Edge> = wiring
        .edges
        .iter()
        .filter(|e| e.from == source && e.to == node)
        .collect();
    let mut names: Vec<&str> = Vec::new();
    for edge in &edges {
        let name = wiring.nodes[node].inputs[edge.input];
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let id = wiring.nodes[source].id;
    let from = match edges.as_slice() {
        [edge] => {
            let output = wiring.nodes[source].outputs[edge.output];
            let full = format!("{id}.{output}");
            match names[0] == output || names[0] == full {
                true => id.to_string(),
                false => full,
            }
        }
        _ => id.to_string(),
    };
    Part::Input {
        ports: fold(&names, LIST),
        from: Some(from),
    }
}

/// `names` joined by spaces, as many as fit in `budget` columns, then `+N`
/// for the rest. At least one name is kept.
fn fold(names: &[&str], budget: usize) -> String {
    let text = |kept: usize| {
        let mut text = names[..kept].join(" ");
        if kept < names.len() {
            text.push_str(&format!(" +{}", names.len() - kept));
        }
        text
    };
    (1..=names.len())
        .rev()
        .map(text)
        .find(|text| width(text) <= budget)
        .unwrap_or_else(|| text(names.len().min(1)))
}

/// Writes the boxes and lanes of `wiring`, each line indented by `indent`.
/// `notes[node]`, if any, is written beside that node's title. Systems
/// without an edge or a note follow on a line per kind.
pub fn render(
    out: &mut impl Write,
    wiring: &Wiring<'_>,
    notes: &[Option<String>],
    theme: &Theme,
    indent: &str,
    detail: Detail,
) -> io::Result<()> {
    let lines = &theme.glyphs.lines;
    let layout = match detail {
        Detail::Systems => by_system(wiring, notes, lines.out),
        Detail::Ports => by_port(wiring, notes, lines.out),
    };
    let columns = lanes::pack(&layout.spans);
    let grid = lanes::draw(layout.parts.len(), &layout.spans, &columns, lines);
    let size = Size::of(wiring, &layout, theme.glyphs.from);
    let lane = theme.style(Token::Accent);
    for ((node, part), lanes) in layout.parts.iter().zip(&grid) {
        let lanes: String = lanes.iter().collect();
        write!(out, "{indent}{lane}{lanes}{lane:#}")?;
        let note = notes.get(*node).and_then(Option::as_deref);
        let node = &wiring.nodes[*node];
        write_part(out, part, node.id, node.kind, note, size, theme)?;
        writeln!(out)?;
    }
    let margin = " ".repeat(lanes::width(&columns));
    write_idle(
        out,
        wiring,
        &layout.idle,
        theme,
        &format!("{indent}{margin}"),
    )
}

/// `a, b  kind` for each kind among `idle`, in step order.
fn write_idle(
    out: &mut impl Write,
    wiring: &Wiring<'_>,
    idle: &[usize],
    theme: &Theme,
    indent: &str,
) -> io::Result<()> {
    let muted = theme.style(Token::Muted);
    let mut kinds: Vec<&str> = Vec::new();
    for &node in idle {
        let kind = wiring.nodes[node].kind;
        if kinds.contains(&kind) {
            continue;
        }
        kinds.push(kind);
        let ids: Vec<&str> = idle
            .iter()
            .map(|&n| &wiring.nodes[n])
            .filter(|n| n.kind == kind)
            .map(|n| n.id)
            .collect();
        writeln!(out, "{indent}{muted}{}  {kind}{muted:#}", ids.join(", "))?;
    }
    Ok(())
}

/// The widths every box shares, so the column lines up.
#[derive(Clone, Copy, Debug)]
struct Size {
    id: usize,
    /// Where an input row's source starts.
    ports: usize,
    inner: usize,
}

impl Size {
    fn of(wiring: &Wiring<'_>, layout: &Layout, glyph: &str) -> Self {
        let boxed = || layout.parts.iter().map(|(node, _)| &wiring.nodes[*node]);
        let id = boxed().map(|n| width(n.id)).max().unwrap_or(0);
        let title = boxed().map(|n| id + 2 + width(n.kind)).max().unwrap_or(0);
        let inputs = layout.parts.iter().filter_map(|(_, part)| match part {
            Part::Input { ports, from } => Some((ports, from)),
            _ => None,
        });
        let ports = inputs.clone().map(|(p, _)| width(p)).max().unwrap_or(0);
        let from = inputs
            .filter_map(|(_, from)| Some(2 + width(glyph) + 1 + width(from.as_deref()?)))
            .max()
            .unwrap_or(0);
        let outputs = layout.parts.iter().filter_map(|(_, part)| match part {
            Part::Output(text) => Some(width(text)),
            _ => None,
        });
        Self {
            id,
            ports,
            inner: title.max(ports + from).max(outputs.max().unwrap_or(0)),
        }
    }
}

fn write_part(
    out: &mut impl Write,
    part: &Part,
    id: &str,
    kind: &str,
    note: Option<&str>,
    size: Size,
    theme: &Theme,
) -> io::Result<()> {
    let lines = &theme.glyphs.lines;
    let border = theme.style(Token::Muted);
    let muted = theme.style(Token::Muted);
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
            let (id_pad, kind_pad) = (pad(id, size.id), pad(kind, inner - size.id - 2));
            write!(
                out,
                "{border}{}{border:#} {bold}{id}{bold:#}{id_pad}  {muted}{kind}{muted:#}{kind_pad} {border}{}{border:#}",
                lines.vertical, lines.vertical,
            )?;
            match note {
                Some(note) => write!(out, "  {muted}{note}{muted:#}"),
                None => Ok(()),
            }
        }
        Part::Input { ports, from } => {
            write!(out, "{border}{}{border:#} {ports}", lines.tee_left)?;
            let used = match from {
                Some(from) => {
                    let glyph = theme.glyphs.from;
                    let gap = pad(ports, size.ports);
                    write!(out, "{gap}  {muted}{glyph} {from}{muted:#}")?;
                    size.ports + 2 + width(glyph) + 1 + width(from)
                }
                None => width(ports),
            };
            let rest = " ".repeat(inner - used);
            write!(out, "{rest} {border}{}{border:#}", lines.vertical)
        }
        Part::Output(text) => write!(
            out,
            "{border}{}{border:#} {muted}{text}{muted:#}{} {border}{}{border:#}",
            lines.tee_left,
            pad(text, inner),
            lines.vertical
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::graph::wiring::Node;
    use crate::cli::ui::caps::Caps;

    fn node<'a>(id: &'a str, kind: &'a str, inputs: &[&'a str], outputs: &[&'a str]) -> Node<'a> {
        Node {
            id,
            kind,
            inputs: inputs.to_vec(),
            outputs: outputs.to_vec(),
        }
    }

    fn edge(from: usize, output: usize, to: usize, input: usize) -> Edge {
        Edge {
            from,
            output,
            to,
            input,
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
                edge(1, 0, 1, 0),
                edge(0, 0, 2, 0),
                edge(2, 0, 3, 0),
                edge(1, 0, 3, 1),
            ],
        }
    }

    /// A subscriber feeding several systems, a publisher fed by all, a
    /// renamed port, a loop, and systems without edges.
    fn busy() -> Wiring<'static> {
        Wiring {
            nodes: vec![
                node("plant", "fsw.subscribe", &[], &["sensors", "gps", "status"]),
                node(
                    "nav",
                    "adcs.Nav",
                    &["sensors", "gps"],
                    &["estimate", "status"],
                ),
                node("ctrl", "adcs.Ctrl", &["att", "sensors", "cmd"], &[]),
                node(
                    "pub",
                    "fsw.publish",
                    &["plant.status", "nav.estimate", "nav.status"],
                    &[],
                ),
                node("cmds", "fsw.subscribe", &[], &["cmd"]),
                node("io_a", "fsw.io", &[], &[]),
                node("io_b", "fsw.io", &[], &[]),
            ],
            edges: vec![
                edge(0, 0, 1, 0),
                edge(0, 1, 1, 1),
                edge(1, 0, 2, 0),
                edge(0, 0, 2, 1),
                edge(4, 0, 2, 2),
                edge(0, 2, 3, 0),
                edge(1, 0, 3, 1),
                edge(1, 1, 3, 2),
            ],
        }
    }

    fn rendered(wiring: &Wiring<'_>, caps: &Caps, detail: Detail) -> String {
        let mut out = Vec::new();
        let notes = [Some("⇄ ground.cmd".into()), None, None, None];
        render(&mut out, wiring, &notes, &Theme::new(caps), "  ", detail).expect("writes to a Vec");
        String::from_utf8(out).expect("utf-8")
    }

    fn unicode() -> Caps {
        Caps {
            unicode: true,
            ..Caps::plain()
        }
    }

    fn ascii() -> Caps {
        Caps {
            unicode: false,
            ..Caps::plain()
        }
    }

    #[test]
    fn test_render_unicode() {
        insta::assert_snapshot!(rendered(&fixture(), &unicode(), Detail::Ports));
    }

    #[test]
    fn test_render_ascii() {
        insta::assert_snapshot!(rendered(&fixture(), &ascii(), Detail::Ports));
    }

    #[test]
    fn test_render_systems_unicode() {
        insta::assert_snapshot!(rendered(&busy(), &unicode(), Detail::Systems));
    }

    #[test]
    fn test_render_systems_ascii() {
        insta::assert_snapshot!(rendered(&busy(), &ascii(), Detail::Systems));
    }

    #[test]
    fn test_by_port_spans_group_an_output_s_edges() {
        let layout = by_port(&fixture(), &[], '>');
        assert_eq!(layout.spans.len(), 3);
        assert_eq!(
            layout.spans[0],
            Span {
                source: 7,
                sinks: vec![6, 17]
            },
            "gain loops and feeds pub"
        );
    }

    #[test]
    fn test_by_system_has_a_lane_per_source_system() {
        let layout = by_system(&busy(), &[], '>');
        assert_eq!(layout.spans.len(), 3, "plant, nav, and cmds");
        assert_eq!(layout.spans[0].sinks.len(), 3, "plant feeds nav, ctrl, pub");
        let cmds = &layout.spans[2];
        assert!(cmds.sinks[0] < cmds.source, "cmds loops up");
        assert_eq!(layout.idle, [5, 6]);
    }

    #[test]
    fn test_group_names_a_lone_renamed_source() {
        let wiring = busy();
        let from = |source, node| match group(&wiring, source, node) {
            Part::Input { from, .. } => from,
            _ => None,
        };
        assert_eq!(from(1, 2).as_deref(), Some("nav.estimate"));
        assert_eq!(from(0, 1).as_deref(), Some("plant"), "two ports");
        assert_eq!(from(4, 2).as_deref(), Some("cmds"), "same name");
        assert_eq!(from(0, 3).as_deref(), Some("plant"), "published name");
    }

    #[test]
    fn test_fold_keeps_what_fits() {
        assert_eq!(fold(&["a", "b", "c"], 10), "a b c");
        assert_eq!(fold(&["aaaa", "bbbb", "cccc"], 10), "aaaa +2");
        assert_eq!(fold(&["aaaaaaaaaaaa", "b"], 4), "aaaaaaaaaaaa +1");
        assert_eq!(fold(&[], 4), "");
    }

    #[test]
    fn test_render_pads_wide_names_by_columns() {
        let wiring = Wiring {
            nodes: vec![
                node("中文", "k", &[], &["x"]),
                node("ab", "k", &["中文"], &[]),
            ],
            edges: vec![edge(0, 0, 1, 0)],
        };
        let edge = |c| c == '│' || c == '╮' || c == '╯';
        for detail in [Detail::Systems, Detail::Ports] {
            let text = rendered(&wiring, &unicode(), detail);
            let widths: Vec<usize> = text
                .lines()
                .map(|line| width(line.trim_end_matches(|c| !edge(c))))
                .collect();
            assert!(widths.iter().all(|w| *w == widths[0]), "{text}");
        }
    }

    #[test]
    fn test_render_without_edges_lists_systems() {
        let wiring = Wiring {
            nodes: vec![node("a", "k", &[], &[]), node("b", "k", &[], &[])],
            edges: Vec::new(),
        };
        let mut out = Vec::new();
        render(
            &mut out,
            &wiring,
            &[],
            &Theme::new(&unicode()),
            "",
            Detail::Systems,
        )
        .expect("writes to a Vec");
        assert_eq!(String::from_utf8(out).expect("utf-8"), "a, b  k\n");
    }

    #[test]
    fn test_render_boxes_a_noted_system_without_edges() {
        let wiring = Wiring {
            nodes: vec![node("link", "k", &[], &[]), node("io", "k", &[], &[])],
            edges: Vec::new(),
        };
        let notes = [Some("⇄ b.mirror".into())];
        let mut out = Vec::new();
        render(
            &mut out,
            &wiring,
            &notes,
            &Theme::new(&unicode()),
            "",
            Detail::Systems,
        )
        .expect("writes to a Vec");
        assert_eq!(
            String::from_utf8(out).expect("utf-8"),
            "╭─────────╮\n│ link  k │  ⇄ b.mirror\n╰─────────╯\nio  k\n"
        );
    }
}
