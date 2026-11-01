//! A target's wiring as boxes in step order, with edges in lanes to their
//! left.

use anstyle::Style;
use std::io::{self, Write};

use super::lanes::{self, Span};
use super::wiring::{Node, Wiring};
use crate::cli::ui::theme::Theme;
use crate::cli::ui::{glyph, glyph::line};
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

/// A lane's source: a node and an output index, 0 for a whole system.
type Key = (usize, usize);

/// The lines of a picture with the node each belongs to, the rows lanes
/// leave and enter by [`Key`], and the `idle` nodes left out.
#[derive(Debug, Default)]
struct Layout {
    parts: Vec<(usize, Part)>,
    sources: Vec<(Key, usize)>,
    sinks: Vec<(Key, usize)>,
    idle: Vec<usize>,
}

impl Layout {
    /// Adds `part` to `node` and returns its row.
    fn push(&mut self, node: usize, part: Part) -> usize {
        self.parts.push((node, part));
        self.parts.len() - 1
    }

    /// A span from each source row to the rows its key enters.
    fn spans(&self) -> Vec<Span> {
        let span = |&(key, source): &(Key, usize)| Span {
            source,
            sinks: self
                .sinks
                .iter()
                .filter(|(k, _)| *k == key)
                .map(|(_, r)| *r)
                .collect(),
        };
        self.sources.iter().map(span).collect()
    }

    /// The rows of `node` for `detail`. By system, its feeders run in
    /// ascending `rank`.
    fn add(&mut self, wiring: &Wiring<'_>, node: usize, detail: Detail, rank: &[usize]) {
        self.push(node, Part::Top);
        self.push(node, Part::Title);
        match detail {
            Detail::Systems => self.add_systems(wiring, node, rank),
            Detail::Ports => self.add_ports(wiring, node),
        }
        self.push(node, Part::Bottom);
    }

    fn add_systems(&mut self, wiring: &Wiring<'_>, node: usize, rank: &[usize]) {
        let mut feeders = feeders(wiring, node);
        feeders.sort_by_key(|&f| rank.get(f).copied().unwrap_or(0));
        for source in feeders {
            let row = self.push(node, group(wiring, source, node));
            self.sinks.push(((source, 0), row));
        }
        let outputs = &wiring.nodes[node].outputs;
        if !outputs.is_empty() {
            let row = self.push(
                node,
                Part::Output(format!("{} {}", line::OUT, fold(outputs, LIST))),
            );
            self.sources.push(((node, 0), row));
        }
    }

    fn add_ports(&mut self, wiring: &Wiring<'_>, node: usize) {
        for (input, port) in wiring.nodes[node].inputs.iter().enumerate() {
            let part = Part::Input {
                ports: port.to_string(),
                from: None,
            };
            let row = self.push(node, part);
            let edges = wiring
                .edges
                .iter()
                .filter(|e| e.to == node && e.input == input);
            self.sinks.extend(edges.map(|e| ((e.from, e.output), row)));
        }
        for (output, port) in wiring.nodes[node].outputs.iter().enumerate() {
            let row = self.push(node, Part::Output(format!("{port} {}", line::OUT)));
            self.sources.push(((node, output), row));
        }
    }
}

/// Whether `node` has neither an edge nor a note, and so gets no box.
fn is_idle(wiring: &Wiring<'_>, notes: &[Option<String>], node: usize) -> bool {
    let ports = &wiring.nodes[node];
    let noted = notes.get(node).is_some_and(Option::is_some);
    ports.inputs.is_empty() && ports.outputs.is_empty() && !noted
}

/// A box per system that isn't idle, its rows for `detail`.
fn layout(wiring: &Wiring<'_>, notes: &[Option<String>], detail: Detail, rank: &[usize]) -> Layout {
    let mut layout = Layout::default();
    for node in 0..wiring.nodes.len() {
        if is_idle(wiring, notes, node) {
            layout.idle.push(node);
        } else {
            layout.add(wiring, node, detail, rank);
        }
    }
    layout
}

/// The layout for `detail`, its spans, and their columns. Each box's
/// feeders are ordered innermost lane first, so a lane ending in the box
/// ends before an outer one taps across it.
fn lay_out(
    wiring: &Wiring<'_>,
    notes: &[Option<String>],
    detail: Detail,
) -> (Layout, Vec<Span>, Vec<usize>) {
    let first = layout(wiring, notes, detail, &[]);
    let mut rank = vec![0; wiring.nodes.len()];
    for (((node, _), _), column) in first.sources.iter().zip(lanes::pack(&first.spans())) {
        rank[*node] = column;
    }
    let layout = layout(wiring, notes, detail, &rank);
    let spans = layout.spans();
    let columns = lanes::pack(&spans);
    (layout, spans, columns)
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
    let edges: Vec<_> = wiring
        .edges
        .iter()
        .filter(|e| e.from == source && e.to == node)
        .collect();
    let mut names: Vec<&str> = Vec::new();
    for edge in &edges {
        let (_, name) = wiring.ports(edge);
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let id = wiring.nodes[source].id;
    let from = match edges.as_slice() {
        [edge] => {
            let (output, input) = wiring.ports(edge);
            let full = format!("{id}.{output}");
            if input == output || input == full {
                id.to_string()
            } else {
                full
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
    let (layout, spans, columns) = lay_out(wiring, notes, detail);
    let grid = lanes::draw(layout.parts.len(), &spans, &columns);
    let size = Size::of(wiring, &layout);
    let lane = theme.accent;
    for ((node, part), lanes) in layout.parts.iter().zip(&grid) {
        let lanes: String = lanes.iter().collect();
        write!(out, "{indent}{lane}{lanes}{lane:#}")?;
        let note = notes.get(*node).and_then(Option::as_deref);
        write_part(out, part, &wiring.nodes[*node], note, size, theme)?;
        writeln!(out)?;
    }
    let indent = format!("{indent}{}", " ".repeat(lanes::width(&columns)));
    write_idle(out, wiring, &layout.idle, theme, &indent)
}

/// `a, b  kind` for each kind among `idle`, in step order.
fn write_idle(
    out: &mut impl Write,
    wiring: &Wiring<'_>,
    idle: &[usize],
    theme: &Theme,
    indent: &str,
) -> io::Result<()> {
    let muted = theme.muted;
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
    fn of(wiring: &Wiring<'_>, layout: &Layout) -> Self {
        let widest = |f: &dyn Fn(&Node<'_>, &Part) -> usize| {
            let parts = layout.parts.iter();
            parts
                .map(|(node, part)| f(&wiring.nodes[*node], part))
                .max()
                .unwrap_or(0)
        };
        let id = widest(&|node, _| width(node.id));
        let title = id + 2 + widest(&|node, _| width(node.kind));
        let ports = widest(&|_, part| match part {
            Part::Input { ports, .. } => width(ports),
            _ => 0,
        });
        let rest = widest(&|_, part| match part {
            Part::Input { from, .. } => width(&source(from.as_deref())),
            Part::Output(text) => width(text).saturating_sub(ports),
            _ => 0,
        });
        Self {
            id,
            ports,
            inner: title.max(ports + rest),
        }
    }
}

/// `  ← from`, written after an input row's ports.
fn source(from: Option<&str>) -> String {
    from.map(|from| format!("  {} {from}", glyph::FROM))
        .unwrap_or_default()
}

fn write_part(
    out: &mut impl Write,
    part: &Part,
    node: &Node<'_>,
    note: Option<&str>,
    size: Size,
    theme: &Theme,
) -> io::Result<()> {
    let muted = theme.muted;
    let bar = format!("{muted}{}{muted:#}", line::VERTICAL);
    let tee = format!("{muted}{}{muted:#}", line::TEE_LEFT);
    let rule = line::HORIZONTAL.to_string().repeat(size.inner + 2);
    let inner = size.inner;
    match part {
        Part::Top => write!(
            out,
            "{muted}{}{rule}{}{muted:#}",
            line::DOWN_RIGHT,
            line::DOWN_LEFT
        ),
        Part::Bottom => write!(
            out,
            "{muted}{}{rule}{}{muted:#}",
            line::UP_RIGHT,
            line::UP_LEFT
        ),
        Part::Title => {
            let bold = theme.bold(Style::new());
            let (id, kind) = (node.id, node.kind);
            let (id_pad, kind_pad) = (pad(id, size.id), pad(kind, inner - size.id - 2));
            write!(
                out,
                "{bar} {bold}{id}{bold:#}{id_pad}  {muted}{kind}{muted:#}{kind_pad} {bar}"
            )?;
            match note {
                Some(note) => write!(out, "  {muted}{note}{muted:#}"),
                None => Ok(()),
            }
        }
        Part::Input { ports, from } => {
            let from = source(from.as_deref());
            let gap = pad(ports, size.ports);
            let rest = pad(&from, inner - size.ports);
            write!(out, "{tee} {ports}{gap}{muted}{from}{muted:#}{rest} {bar}")
        }
        Part::Output(text) => write!(
            out,
            "{tee} {muted}{text}{muted:#}{} {bar}",
            pad(text, inner)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::graph::wiring::Edge;
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

    fn drawn(
        wiring: &Wiring<'_>,
        notes: &[Option<String>],
        caps: &Caps,
        indent: &str,
        detail: Detail,
    ) -> String {
        let mut out = Vec::new();
        render(&mut out, wiring, notes, &Theme::new(caps), indent, detail)
            .expect("writes to a Vec");
        String::from_utf8(out).expect("utf-8")
    }

    fn rendered(wiring: &Wiring<'_>, caps: &Caps, detail: Detail) -> String {
        let notes = [Some("⇄ ground.cmd".into()), None, None, None];
        drawn(wiring, &notes, caps, "  ", detail)
    }

    fn plain(wiring: &Wiring<'_>, notes: &[Option<String>], detail: Detail) -> String {
        drawn(wiring, notes, &Caps::plain(), "", detail)
    }

    #[test]
    fn test_render_unicode() {
        insta::assert_snapshot!(rendered(&fixture(), &Caps::plain(), Detail::Ports));
    }

    #[test]
    fn test_render_systems_unicode() {
        insta::assert_snapshot!(rendered(&busy(), &Caps::plain(), Detail::Systems));
    }

    #[test]
    fn test_ports_spans_group_an_output_s_edges() {
        let spans = layout(&fixture(), &[], Detail::Ports, &[]).spans();
        assert_eq!(spans.len(), 3);
        assert_eq!(
            spans[1],
            Span {
                source: 7,
                sinks: vec![6, 17]
            },
            "gain loops and feeds pub"
        );
    }

    #[test]
    fn test_systems_have_a_lane_per_source_system() {
        let layout = layout(&busy(), &[], Detail::Systems, &[]);
        let spans = layout.spans();
        assert_eq!(spans.len(), 3, "plant, nav, and cmds");
        assert_eq!(spans[0].sinks.len(), 3, "plant feeds nav, ctrl, pub");
        let cmds = &spans[2];
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
            let text = rendered(&wiring, &Caps::plain(), detail);
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
        for detail in [Detail::Systems, Detail::Ports] {
            assert_eq!(plain(&wiring, &[], detail), "a, b  k\n");
        }
    }

    #[test]
    fn test_systems_fan_in_and_self_loop() {
        let wiring = Wiring {
            nodes: vec![
                node("a", "k", &["x"], &["p"]),
                node("b", "k", &[], &["p"]),
                node("c", "k", &["x"], &[]),
            ],
            edges: vec![edge(0, 0, 0, 0), edge(0, 0, 2, 0), edge(1, 0, 2, 0)],
        };
        let layout = layout(&wiring, &[], Detail::Systems, &[]);
        let spans = layout.spans();
        let span = |source| spans.iter().find(|s| s.source == source);
        assert_eq!(
            span(3).map(|s| s.sinks.clone()),
            Some(vec![2, 11]),
            "a loops and feeds c"
        );
        assert_eq!(
            span(7).map(|s| s.sinks.clone()),
            Some(vec![12]),
            "b feeds c"
        );
        assert_eq!(
            layout.parts[11..13]
                .iter()
                .map(|(_, part)| part.clone())
                .collect::<Vec<_>>(),
            [
                Part::Input {
                    ports: "x".into(),
                    from: Some("a.p".into())
                },
                Part::Input {
                    ports: "x".into(),
                    from: Some("b.p".into())
                },
            ],
            "one row per feeding system"
        );
    }

    #[test]
    fn test_render_boxes_a_noted_system_without_edges() {
        let wiring = Wiring {
            nodes: vec![node("link", "k", &[], &[]), node("io", "k", &[], &[])],
            edges: Vec::new(),
        };
        let notes = [Some("⇄ b.mirror".into())];
        assert_eq!(
            plain(&wiring, &notes, Detail::Systems),
            "╭─────────╮\n│ link  k │  ⇄ b.mirror\n╰─────────╯\nio  k\n"
        );
    }
}
