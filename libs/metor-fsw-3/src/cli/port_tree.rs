//! A system's ports as a tree, rebuilt from their dotted names.
//!
//! A port named `fss.3` is element 3 of the list `fss`, and `wheels.0.cmd`
//! is the `cmd` port of the first group in the list `wheels`.

use crate::system::PortDef;

/// One node of the tree.
#[derive(Debug, PartialEq)]
pub(super) enum PortNode<'a> {
    Leaf(&'a PortDef),
    /// Children named `0` … `N-1`, in index order.
    List(Vec<PortNode<'a>>),
    /// Named children, in port order.
    Group(Vec<(&'a str, PortNode<'a>)>),
}

/// Two ports that cannot share a tree: `port` is a leaf, and another port
/// sits under it.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct ShapeError {
    pub(super) port: String,
}

/// A node while ports are still being added.
enum Draft<'a> {
    Leaf(&'a PortDef),
    Branch(Vec<(&'a str, Draft<'a>)>),
}

/// Builds the tree of `ports`, keyed by their first segment.
pub(super) fn port_tree<'a>(
    ports: impl IntoIterator<Item = &'a PortDef>,
) -> Result<Vec<(&'a str, PortNode<'a>)>, ShapeError> {
    let mut root = Vec::new();
    for port in ports {
        insert(&mut root, port, &port.name, 0)?;
    }
    Ok(root
        .into_iter()
        .map(|(name, draft)| (name, finish(draft)))
        .collect())
}

/// Adds `port` under `branch`, at the part of its name from `at` on.
fn insert<'a>(
    branch: &mut Vec<(&'a str, Draft<'a>)>,
    port: &'a PortDef,
    name: &'a str,
    at: usize,
) -> Result<(), ShapeError> {
    let rest = &name[at..];
    let (segment, tail) = match rest.split_once('.') {
        Some((segment, _)) => (segment, Some(at + segment.len() + 1)),
        None => (rest, None),
    };
    let found = branch.iter().position(|(known, _)| *known == segment);
    let conflict = || ShapeError {
        port: name[..at + segment.len()].to_string(),
    };
    match (found, tail) {
        (None, None) => branch.push((segment, Draft::Leaf(port))),
        (None, Some(next)) => {
            let mut children = Vec::new();
            insert(&mut children, port, name, next)?;
            branch.push((segment, Draft::Branch(children)));
        }
        (Some(i), Some(next)) => match &mut branch[i].1 {
            Draft::Branch(children) => insert(children, port, name, next)?,
            Draft::Leaf(_) => return Err(conflict()),
        },
        (Some(_), None) => return Err(conflict()),
    }
    Ok(())
}

fn finish(draft: Draft<'_>) -> PortNode<'_> {
    match draft {
        Draft::Leaf(port) => PortNode::Leaf(port),
        Draft::Branch(children) if is_index_run(&children) => {
            PortNode::List(children.into_iter().map(|(_, d)| finish(d)).collect())
        }
        Draft::Branch(children) => PortNode::Group(
            children
                .into_iter()
                .map(|(name, d)| (name, finish(d)))
                .collect(),
        ),
    }
}

/// Whether the children are named `0` … `N-1`, in that order.
fn is_index_run(children: &[(&str, Draft<'_>)]) -> bool {
    children
        .iter()
        .enumerate()
        .all(|(i, (name, _))| *name == i.to_string())
}

#[cfg(test)]
mod tests {
    use metor_proto::types::ComponentId;

    use super::*;

    fn port(name: &str) -> PortDef {
        PortDef {
            name: name.to_string().into(),
            record: "imu".into(),
            id: ComponentId::new("imu"),
            max_len: 8,
            alignment: 4,
            depth: 8,
            layout: None,
            schema: crate::RecordSchema::msg("imu", crate::MsgCodec::Bytes),
        }
    }

    fn ports(names: &[&str]) -> Vec<PortDef> {
        names.iter().map(|name| port(name)).collect()
    }

    /// The tree's shape as text: `name`, `[..]`, or `{name: ..}`.
    fn shape(node: &PortNode<'_>) -> String {
        match node {
            PortNode::Leaf(port) => port.name.to_string(),
            PortNode::List(items) => {
                let items: Vec<_> = items.iter().map(shape).collect();
                format!("[{}]", items.join(", "))
            }
            PortNode::Group(children) => {
                let children: Vec<_> = children
                    .iter()
                    .map(|(name, node)| format!("{name}: {}", shape(node)))
                    .collect();
                format!("{{{}}}", children.join(", "))
            }
        }
    }

    fn shapes(names: &[&str]) -> Vec<String> {
        let ports = ports(names);
        let tree = port_tree(&ports).expect("a tree");
        tree.iter()
            .map(|(name, node)| format!("{name}={}", shape(node)))
            .collect()
    }

    #[test]
    fn test_leaves_stay_flat() {
        assert_eq!(shapes(&["imu", "gyro"]), ["imu=imu", "gyro=gyro"]);
    }

    #[test]
    fn test_index_runs_are_lists() {
        assert_eq!(
            shapes(&[
                "fss.0", "fss.1", "grid.0.0", "grid.0.1", "grid.1.0", "grid.1.1"
            ]),
            [
                "fss=[fss.0, fss.1]",
                "grid=[[grid.0.0, grid.0.1], [grid.1.0, grid.1.1]]"
            ]
        );
    }

    #[test]
    fn test_named_children_are_groups() {
        assert_eq!(
            shapes(&[
                "wheels.0.speed",
                "wheels.0.cmd",
                "wheels.1.speed",
                "wheels.1.cmd"
            ]),
            [
                "wheels=[{speed: wheels.0.speed, cmd: wheels.0.cmd}, {speed: wheels.1.speed, cmd: wheels.1.cmd}]"
            ]
        );
    }

    #[test]
    fn test_broken_runs_are_groups() {
        assert_eq!(shapes(&["a.1", "a.2"]), ["a={1: a.1, 2: a.2}"]);
        assert_eq!(shapes(&["b.1", "b.0"]), ["b={1: b.1, 0: b.0}"]);
        assert_eq!(shapes(&["c.0", "c.01"]), ["c={0: c.0, 01: c.01}"]);
    }

    #[test]
    fn test_leaf_with_children_is_refused() {
        let under = ports(&["a", "a.b"]);
        let over = ports(&["a.b.c", "a.b"]);
        assert_eq!(port_tree(&under), Err(ShapeError { port: "a".into() }));
        assert_eq!(port_tree(&over), Err(ShapeError { port: "a.b".into() }));
    }
}
