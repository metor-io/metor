//! The deployment's wiring as Mermaid, DOT, or JSON, for docs and tools.

use core::fmt::Write;

use serde_json::{Value, json};

use super::wiring::{Link, Wiring};

/// A target's name and wiring.
pub type Target<'a> = (&'a str, Wiring<'a>);

/// A Mermaid flowchart: a subgraph per target, an edge per input, and a
/// dotted edge per link.
pub fn mermaid(targets: &[Target<'_>], links: &[Link]) -> String {
    let mut out = String::from("flowchart TD\n");
    for (t, (name, wiring)) in targets.iter().enumerate() {
        let _ = writeln!(out, "  subgraph t{t}[\"{}\"]", escape(name));
        for (s, node) in wiring.nodes.iter().enumerate() {
            let _ = writeln!(
                out,
                "    t{t}_{s}[\"{}<br/><small>{}</small>\"]",
                escape(node.id),
                escape(node.kind)
            );
        }
        out.push_str("  end\n");
    }
    for (t, (_, wiring)) in targets.iter().enumerate() {
        for edge in &wiring.edges {
            let label = escape(&label(wiring, edge));
            let _ = writeln!(out, "  t{t}_{} -->|{label}| t{t}_{}", edge.from, edge.to);
        }
    }
    for link in links {
        let (d, l) = (link.dial, link.listen);
        let _ = writeln!(out, "  t{}_{} <-.-> t{}_{}", d.0, d.1, l.0, l.1);
    }
    out
}

/// A DOT digraph: a cluster per target, an edge per input, and a dashed
/// two-way edge per link.
pub fn dot(targets: &[Target<'_>], links: &[Link]) -> String {
    let mut out = String::from("digraph deployment {\n  node [shape=box];\n");
    for (t, (name, wiring)) in targets.iter().enumerate() {
        let _ = writeln!(
            out,
            "  subgraph cluster_{t} {{\n    label=\"{}\";",
            quote(name)
        );
        for (s, node) in wiring.nodes.iter().enumerate() {
            let _ = writeln!(
                out,
                "    t{t}_{s} [label=\"{}\\n{}\"];",
                quote(node.id),
                quote(node.kind)
            );
        }
        out.push_str("  }\n");
    }
    for (t, (_, wiring)) in targets.iter().enumerate() {
        for edge in &wiring.edges {
            let label = quote(&label(wiring, edge));
            let _ = writeln!(
                out,
                "  t{t}_{} -> t{t}_{} [label=\"{label}\"];",
                edge.from, edge.to
            );
        }
    }
    for link in links {
        let (d, l) = (link.dial, link.listen);
        let _ = writeln!(
            out,
            "  t{}_{} -> t{}_{} [style=dashed, dir=both];",
            d.0, d.1, l.0, l.1
        );
    }
    out.push_str("}\n");
    out
}

/// Every target's systems and edges, named `target.system.port`.
pub fn json(targets: &[Target<'_>], links: &[Link]) -> Value {
    let system = |t: usize, s: usize| {
        let (name, wiring) = &targets[t];
        format!("{name}.{}", wiring.nodes[s].id)
    };
    let mut edges = Vec::new();
    for (t, (_, wiring)) in targets.iter().enumerate() {
        for edge in &wiring.edges {
            let output = wiring.nodes[edge.from].outputs[edge.output];
            let input = wiring.nodes[edge.to].inputs[edge.input];
            edges.push(json!({
                "from": format!("{}.{output}", system(t, edge.from)),
                "to": format!("{}.{input}", system(t, edge.to)),
            }));
        }
    }
    let targets: Vec<Value> = targets
        .iter()
        .map(|(name, wiring)| {
            let systems: Vec<Value> = wiring
                .nodes
                .iter()
                .map(|node| json!({"id": node.id, "kind": node.kind}))
                .collect();
            json!({"name": name, "systems": systems})
        })
        .collect();
    let links: Vec<Value> = links
        .iter()
        .map(|link| {
            json!({
                "dial": system(link.dial.0, link.dial.1),
                "listen": system(link.listen.0, link.listen.1),
            })
        })
        .collect();
    json!({"targets": targets, "edges": edges, "links": links})
}

/// `port`, or `output → input` where the names differ.
fn label(wiring: &Wiring<'_>, edge: &super::wiring::Edge) -> String {
    let output = wiring.nodes[edge.from].outputs[edge.output];
    let input = wiring.nodes[edge.to].inputs[edge.input];
    match output == input {
        true => output.to_string(),
        false => format!("{output} → {input}"),
    }
}

/// Text safe inside a Mermaid string.
fn escape(text: &str) -> String {
    text.replace('"', "#quot;")
}

/// Text safe inside a DOT string.
fn quote(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::graph::wiring::{Edge, Node};

    fn targets() -> Vec<Target<'static>> {
        let node = |id, kind, inputs: &[&'static str], outputs: &[&'static str]| Node {
            id,
            kind,
            inputs: inputs.to_vec(),
            outputs: outputs.to_vec(),
        };
        vec![
            (
                "plant",
                Wiring {
                    nodes: vec![
                        node("plant", "adcs.Plant", &[], &["sensors"]),
                        node("pub", "fsw.publish", &["sensors"], &[]),
                    ],
                    edges: vec![Edge {
                        from: 0,
                        output: 0,
                        to: 1,
                        input: 0,
                    }],
                },
            ),
            (
                "fsw",
                Wiring {
                    nodes: vec![
                        node("plant", "fsw.subscribe", &[], &["sensors"]),
                        node("nav", "adcs.\"Nav\"", &["imu"], &[]),
                    ],
                    edges: vec![Edge {
                        from: 0,
                        output: 0,
                        to: 1,
                        input: 0,
                    }],
                },
            ),
        ]
    }

    const LINKS: [Link; 1] = [Link {
        dial: (1, 0),
        listen: (0, 1),
    }];

    #[test]
    fn test_mermaid() {
        insta::assert_snapshot!(mermaid(&targets(), &LINKS));
    }

    #[test]
    fn test_dot() {
        insta::assert_snapshot!(dot(&targets(), &LINKS));
    }

    #[test]
    fn test_json_names_ports_in_full() {
        let value = json(&targets(), &LINKS);
        assert_eq!(
            value["edges"][1],
            json!({"from": "fsw.plant.sensors", "to": "fsw.nav.imu"})
        );
        assert_eq!(
            value["links"][0],
            json!({"dial": "fsw.plant", "listen": "plant.pub"})
        );
        assert_eq!(
            json(&[], &[]),
            json!({"targets": [], "edges": [], "links": []})
        );
    }
}
