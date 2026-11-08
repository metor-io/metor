//! The Python a pack module declares for a system's port tree: keywords,
//! attributes, and the classes for groups of ports.

use serde::Serialize;

use super::port_tree::{PortNode, ShapeError};

/// An `__init__` keyword.
#[derive(Debug, Serialize)]
pub(super) struct Keyword {
    pub(super) name: String,
    pub(super) annotation: String,
    /// The default, rendered as ` = <literal>`.
    pub(super) default: &'static str,
}

/// A class attribute.
#[derive(Debug, Serialize)]
pub(super) struct Attr {
    pub(super) name: String,
    pub(super) annotation: String,
}

/// An input port by its dotted path, with its record class.
#[derive(Debug, Serialize)]
pub(super) struct Leaf {
    pub(super) name: String,
    pub(super) record: String,
}

/// A class nested on the system's: a group of inputs, or of outputs.
#[derive(Debug, Serialize)]
pub(super) struct GroupClass {
    pub(super) name: String,
    /// `PortGroup` or `OutGroup`.
    pub(super) base: &'static str,
    pub(super) keywords: Vec<Keyword>,
    /// The group's inputs, by their path under the group.
    pub(super) input_ports: Vec<Leaf>,
    pub(super) attrs: Vec<Attr>,
}

/// Renders one system's port tree, collecting its group classes.
pub(super) struct Stubs<'s> {
    system: &'s str,
    /// Renders a leaf input's record class.
    record: &'s dyn Fn(&str) -> String,
    /// Renders a leaf output's port type.
    port: &'s dyn Fn(&crate::PortDef) -> String,
    pub(super) groups: Vec<GroupClass>,
}

type Children<'a> = [(&'a str, PortNode<'a>)];

impl<'s> Stubs<'s> {
    pub(super) fn new(
        system: &'s str,
        record: &'s dyn Fn(&str) -> String,
        port: &'s dyn Fn(&crate::PortDef) -> String,
    ) -> Self {
        Self {
            system,
            record,
            port,
            groups: Vec::new(),
        }
    }

    /// One keyword per child of `prefix`; `prefix` is empty at the root.
    pub(super) fn keywords(
        &mut self,
        children: &Children<'_>,
        prefix: &str,
    ) -> Result<Vec<Keyword>, ShapeError> {
        children
            .iter()
            .map(|(name, node)| {
                let path = join(prefix, name);
                check_identifier(name, &path)?;
                let annotation = self.input(node, &path)?;
                Ok(match node {
                    PortNode::Leaf(_) => keyword(name, annotation, " = ()"),
                    _ => keyword(name, format!("{annotation} | None"), " = None"),
                })
            })
            .collect()
    }

    /// One attribute per child of `prefix`; `prefix` is empty at the root.
    pub(super) fn attrs(
        &mut self,
        children: &Children<'_>,
        prefix: &str,
    ) -> Result<Vec<Attr>, ShapeError> {
        children
            .iter()
            .map(|(name, node)| {
                let path = join(prefix, name);
                check_identifier(name, &path)?;
                Ok(Attr {
                    name: (*name).to_string(),
                    annotation: self.output(node, &path)?,
                })
            })
            .collect()
    }

    /// `Sources[T]`, a `Sequence`, or the group's class.
    fn input(&mut self, node: &PortNode<'_>, path: &str) -> Result<String, ShapeError> {
        match node {
            PortNode::Leaf(port) => Ok(format!("Sources[{}]", (self.record)(&port.record))),
            PortNode::List(items) => Ok(format!("Sequence[{}]", self.input_element(items, path)?)),
            PortNode::Group(children) => {
                let name = group_name(path);
                if !self.has_group(&name) {
                    let class = GroupClass {
                        keywords: self.keywords(children, path)?,
                        input_ports: leaves(children, "", self.record),
                        attrs: Vec::new(),
                        name: name.clone(),
                        base: "PortGroup",
                    };
                    self.groups.push(class);
                }
                Ok(format!("{}.{name}", self.system))
            }
        }
    }

    /// The leaf's port type, a `tuple`, or the group's class.
    fn output(&mut self, node: &PortNode<'_>, path: &str) -> Result<String, ShapeError> {
        match node {
            PortNode::Leaf(port) => Ok((self.port)(port)),
            PortNode::List(items) => {
                Ok(format!("tuple[{}, ...]", self.output_element(items, path)?))
            }
            PortNode::Group(children) => {
                let name = format!("{}Out", group_name(path));
                if !self.has_group(&name) {
                    let class = GroupClass {
                        attrs: self.attrs(children, path)?,
                        keywords: Vec::new(),
                        input_ports: Vec::new(),
                        name: name.clone(),
                        base: "OutGroup",
                    };
                    self.groups.push(class);
                }
                Ok(format!("{}.{name}", self.system))
            }
        }
    }

    fn input_element(&mut self, items: &[PortNode<'_>], path: &str) -> Result<String, ShapeError> {
        match items.first() {
            Some(first) => self.input(first, &join(path, "0")),
            None => Ok("object".into()),
        }
    }

    fn output_element(&mut self, items: &[PortNode<'_>], path: &str) -> Result<String, ShapeError> {
        match items.first() {
            Some(first) => self.output(first, &join(path, "0")),
            None => Ok("object".into()),
        }
    }

    fn has_group(&self, name: &str) -> bool {
        self.groups.iter().any(|group| group.name == name)
    }
}

fn keyword(name: &str, annotation: String, default: &'static str) -> Keyword {
    Keyword {
        name: name.to_string(),
        annotation,
        default,
    }
}

/// Every leaf under `children`, by its path from the group.
fn leaves(children: &Children<'_>, prefix: &str, record: &dyn Fn(&str) -> String) -> Vec<Leaf> {
    let mut out = Vec::new();
    for (name, node) in children {
        push_leaves(node, &join(prefix, name), record, &mut out);
    }
    out
}

fn push_leaves(
    node: &PortNode<'_>,
    path: &str,
    record: &dyn Fn(&str) -> String,
    out: &mut Vec<Leaf>,
) {
    match node {
        PortNode::Leaf(port) => out.push(Leaf {
            name: path.to_string(),
            record: record(&port.record),
        }),
        PortNode::List(items) => {
            for (i, item) in items.iter().enumerate() {
                push_leaves(item, &join(path, &i.to_string()), record, out);
            }
        }
        PortNode::Group(children) => out.extend(leaves(children, path, record)),
    }
}

/// The class name of the group at `path`: its named segments in Pascal case.
///
/// `wheels.2` is `Wheels`, and `sensors.imu` is `SensorsImu`.
fn group_name(path: &str) -> String {
    path.split('.')
        .filter(|segment| !segment.starts_with(|c: char| c.is_ascii_digit()))
        .map(super::module::pascal)
        .collect()
}

fn join(prefix: &str, name: &str) -> String {
    match prefix {
        "" => name.to_string(),
        prefix => format!("{prefix}.{name}"),
    }
}

/// Refuses a keyword or attribute that Python cannot spell.
fn check_identifier(name: &str, path: &str) -> Result<(), ShapeError> {
    let mut chars = name.chars();
    let first = chars.next().is_some_and(|c| c == '_' || c.is_alphabetic());
    match first && chars.all(|c| c == '_' || c.is_alphanumeric()) {
        true => Ok(()),
        false => Err(ShapeError {
            port: path.to_string(),
        }),
    }
}
