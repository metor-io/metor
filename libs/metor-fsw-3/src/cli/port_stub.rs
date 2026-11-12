//! The Python a pack module declares for a system's port tree: keywords,
//! attributes, and the classes for groups of ports.

use serde::Serialize;

use super::port_tree::{PortNode, ShapeError};

/// An `__init__` keyword.
#[derive(Debug, PartialEq, Serialize)]
pub(super) struct Keyword {
    pub(super) name: String,
    pub(super) annotation: String,
    /// The default, rendered as ` = <literal>`.
    pub(super) default: &'static str,
}

/// A class attribute.
#[derive(Debug, PartialEq, Serialize)]
pub(super) struct Attr {
    pub(super) name: String,
    pub(super) annotation: String,
}

/// An input port by its dotted path, with its record class.
#[derive(Debug, PartialEq, Serialize)]
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
    /// The port path the class was first built for.
    #[serde(skip)]
    pub(super) path: String,
}

impl GroupClass {
    /// Whether `other` renders the same class.
    fn same_as(&self, other: &Self) -> bool {
        (self.name == other.name && self.base == other.base)
            && (self.keywords == other.keywords && self.input_ports == other.input_ports)
            && self.attrs == other.attrs
    }
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
                let class = GroupClass {
                    keywords: self.keywords(children, path)?,
                    input_ports: leaves(children, "", self.record),
                    attrs: Vec::new(),
                    name: group_name(path),
                    base: "PortGroup",
                    path: path.to_string(),
                };
                self.add_group(class)
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
                let class = GroupClass {
                    attrs: self.attrs(children, path)?,
                    keywords: Vec::new(),
                    input_ports: Vec::new(),
                    name: format!("{}Out", group_name(path)),
                    base: "OutGroup",
                    path: path.to_string(),
                };
                self.add_group(class)
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

    /// Keeps `class` once, and returns how the system's stub names it.
    ///
    /// Fails when another group already took the class name.
    fn add_group(&mut self, class: GroupClass) -> Result<String, ShapeError> {
        let named = format!("{}.{}", self.system, class.name);
        match self.groups.iter().find(|group| group.name == class.name) {
            Some(known) if known.same_as(&class) => {}
            Some(known) => {
                return Err(ShapeError {
                    port: class.path,
                    other: Some(known.path.clone()),
                });
            }
            None => self.groups.push(class),
        }
        Ok(named)
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

/// Python's keywords, and `self`, which `__init__` takes first.
const PYTHON_KEYWORDS: [&str; 36] = [
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield", "self",
];

/// Refuses a keyword or attribute that Python cannot spell.
fn check_identifier(name: &str, path: &str) -> Result<(), ShapeError> {
    let mut chars = name.chars();
    let first = chars.next().is_some_and(|c| c == '_' || c.is_alphabetic());
    let spelled = first && chars.all(|c| c == '_' || c.is_alphanumeric());
    match spelled && !PYTHON_KEYWORDS.contains(&name) {
        true => Ok(()),
        false => Err(ShapeError::at(path)),
    }
}
