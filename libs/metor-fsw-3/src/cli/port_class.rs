//! The typed Python accessors of a frame's fields: a keyword per top-level
//! field on the record's `__init__`, and a port class with a property per
//! field.

use serde::Serialize;

/// One `__init__` keyword of a frame's record class.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Keyword {
    /// The Python name, which may end in `_`.
    pub name: String,
    /// The field's own name.
    pub key: String,
    pub annotation: String,
}

/// One property of a port class.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Accessor {
    pub name: String,
    pub returns: String,
    pub body: String,
}

/// A port class, or a group class nested under one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct PortClass {
    pub name: String,
    pub base: String,
    pub accessors: Vec<Accessor>,
}

/// A frame's keywords and port classes. The first class is the port class.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct Accessors {
    pub keywords: Vec<Keyword>,
    pub classes: Vec<PortClass>,
}

/// The public names of an `OutPort` or `Field`, and `self`, which a property
/// must not shadow.
const RESERVED_NAMES: [&str; 10] = [
    "self",
    "ref",
    "record",
    "target",
    "kind",
    "name",
    "field",
    "port",
    "path",
    "signature",
];

/// Python's hard keywords.
const KEYWORDS: [&str; 35] = [
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

/// A view of a frame's leaves, `(path, signature)` in vtable order.
struct Tree<'a> {
    leaves: &'a [(String, String)],
}

impl Accessors {
    /// Builds the accessors of record class `class` over its leaves.
    pub(crate) fn of(class: &str, leaves: &[(String, String)]) -> Self {
        let tree = Tree { leaves };
        let port = format!("{class}Port");
        let mut classes = Vec::new();
        tree.collect(&port, &format!("OutPort[{class}]"), "", &mut classes);
        let keywords = tree
            .children("")
            .into_iter()
            .filter_map(|segment| {
                Some(Keyword {
                    name: py_name(&segment)?,
                    annotation: field_type(&tree.signature(&segment)),
                    key: segment,
                })
            })
            .collect();
        Self { keywords, classes }
    }
}

impl Tree<'_> {
    /// Adds the class for `prefix`, then the class of each group under it.
    fn collect(&self, name: &str, base: &str, prefix: &str, out: &mut Vec<PortClass>) {
        let index = out.len();
        out.push(PortClass {
            name: name.to_string(),
            base: base.to_string(),
            accessors: Vec::new(),
        });
        for segment in self.children(prefix) {
            let Some(py) = py_name(&segment) else {
                continue;
            };
            let path = join(prefix, &segment);
            let accessor = self.accessor(name, prefix, &py, &path, out);
            out[index].accessors.push(accessor);
        }
    }

    fn accessor(
        &self,
        parent: &str,
        prefix: &str,
        py: &str,
        path: &str,
        out: &mut Vec<PortClass>,
    ) -> Accessor {
        let port = if prefix.is_empty() {
            "self"
        } else {
            "self.port"
        };
        let signature = self.signature(path);
        if self.is_leaf(path) {
            return Accessor {
                name: py.to_string(),
                returns: field_type(&signature),
                body: format!("{port}.field({})", quoted(path)),
            };
        }
        let group = format!("{parent}_{py}");
        self.collect(&group, &field_type(&signature), path, out);
        Accessor {
            name: py.to_string(),
            body: format!("{group}({port}, {}, {})", quoted(path), quoted(&signature)),
            returns: group,
        }
    }

    /// The next segment of every leaf under `prefix`, in first-seen order.
    fn children(&self, prefix: &str) -> Vec<String> {
        let mut segments: Vec<String> = Vec::new();
        for (path, _) in self.leaves {
            let Some(rest) = relative(path, prefix) else {
                continue;
            };
            let segment = rest.split('.').next().unwrap_or(rest);
            if !segments.iter().any(|known| known == segment) {
                segments.push(segment.to_string());
            }
        }
        segments
    }

    fn is_leaf(&self, path: &str) -> bool {
        self.leaves.iter().any(|(leaf, _)| leaf == path)
    }

    /// A leaf's signature, or a group's `{a:f64,b:f64[3]}` over its leaves'
    /// relative paths, sorted.
    fn signature(&self, path: &str) -> String {
        if let Some((_, signature)) = self.leaves.iter().find(|(leaf, _)| leaf == path) {
            return signature.clone();
        }
        let mut group: Vec<(&str, &str)> = self
            .leaves
            .iter()
            .filter_map(|(leaf, signature)| Some((relative(leaf, path)?, signature.as_str())))
            .collect();
        group.sort_unstable();
        let parts: Vec<String> = group.iter().map(|(p, s)| format!("{p}:{s}")).collect();
        format!("{{{}}}", parts.join(","))
    }
}

/// The part of `path` under `prefix.`, or all of it under the root.
fn relative<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    if prefix.is_empty() {
        return Some(path);
    }
    path.strip_prefix(prefix)?.strip_prefix('.')
}

fn join(prefix: &str, segment: &str) -> String {
    if prefix.is_empty() {
        segment.to_string()
    } else {
        format!("{prefix}.{segment}")
    }
}

/// The Python name of a segment: itself, with `_` after a keyword or a
/// reserved name, or `None` when it is not an identifier.
pub(crate) fn py_name(segment: &str) -> Option<String> {
    let mut chars = segment.chars();
    let first = chars.next()?;
    let ident = (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !ident {
        return None;
    }
    if KEYWORDS.contains(&segment) || RESERVED_NAMES.contains(&segment) {
        return Some(format!("{segment}_"));
    }
    Some(segment.to_string())
}

fn field_type(signature: &str) -> String {
    format!("Field[Literal[{}]]", quoted(signature))
}

fn quoted(text: &str) -> String {
    serde_json::Value::String(text.to_string()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaves(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(p, s)| (p.to_string(), s.to_string()))
            .collect()
    }

    #[test]
    fn test_nested_group() {
        let a = Accessors::of(
            "W",
            &leaves(&[("w.b", "f64"), ("w.a.x", "u8"), ("z", "f64")]),
        );
        let names: Vec<_> = a.classes.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["WPort", "WPort_w", "WPort_w_a"]);
        assert_eq!(
            a.keywords[0].annotation,
            "Field[Literal[\"{a.x:u8,b:f64}\"]]"
        );
        assert_eq!(a.classes[1].base, a.keywords[0].annotation);
        assert_eq!(a.classes[0].accessors[0].returns, "WPort_w");
        assert_eq!(
            a.classes[0].accessors[0].body,
            "WPort_w(self, \"w\", \"{a.x:u8,b:f64}\")"
        );
        assert_eq!(a.classes[1].accessors[0].body, "self.port.field(\"w.b\")");
        assert_eq!(
            a.classes[1].accessors[1].body,
            "WPort_w_a(self.port, \"w.a\", \"{x:u8}\")"
        );
    }

    #[test]
    fn test_reserved_names() {
        let a = Accessors::of("K", &leaves(&[("class", "f64"), ("name", "f64")]));
        let pairs: Vec<_> = a
            .keywords
            .iter()
            .map(|k| (k.name.as_str(), k.key.as_str()))
            .collect();
        assert_eq!(pairs, [("class_", "class"), ("name_", "name")]);
    }

    #[test]
    fn test_numeric_segments_get_no_property() {
        let a = Accessors::of("C", &leaves(&[("css.0", "f64"), ("css.1", "f64")]));
        assert_eq!(
            a.keywords[0].annotation,
            "Field[Literal[\"{0:f64,1:f64}\"]]"
        );
        assert_eq!(a.classes.len(), 2);
        assert!(a.classes[1].accessors.is_empty());
        assert_eq!(py_name("0"), None);
        assert_eq!(py_name(""), None);
        assert_eq!(py_name("a-b"), None);
    }
}
