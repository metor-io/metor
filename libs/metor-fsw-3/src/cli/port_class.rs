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
                    annotation: tree.keyword_type(&segment),
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
            return leaf_accessor(py, port, path, &signature);
        }
        let group = format!("{parent}_{py}");
        let base = match self.index_run(path) {
            Some(child) => format!(
                "ArrayField[Literal[{}], {}]",
                quoted(&signature),
                value_type(&child)
            ),
            None => field_type(&signature),
        };
        self.collect(&group, &base, path, out);
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

    /// A keyword's annotation: a field, or for an array a mapping from index
    /// to a field of its element. A scalar `f64` also takes a `Calibrated`.
    fn keyword_type(&self, path: &str) -> String {
        let whole = mapped_type(&self.signature(path));
        let element = match self.leaves.iter().find(|(leaf, _)| leaf == path) {
            Some((_, signature)) => element_signature(signature),
            None => self.index_run(path),
        };
        match element {
            Some(element) => format!("{whole} | Mapping[int, {}]", mapped_type(&element)),
            None => whole,
        }
    }

    /// The one signature of a group's children when they are exactly
    /// `0..N` in order.
    fn index_run(&self, path: &str) -> Option<String> {
        let children = self.children(path);
        let indexed = children
            .iter()
            .enumerate()
            .all(|(i, child)| *child == i.to_string());
        let first = self.signature(&join(path, children.first()?));
        let shared = children
            .iter()
            .all(|child| self.signature(&join(path, child)) == first);
        (indexed && shared).then_some(first)
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

fn leaf_accessor(py: &str, port: &str, path: &str, signature: &str) -> Accessor {
    let (returns, body) = match array_type(signature) {
        Some(array) => (
            array,
            format!(
                "ArrayField({port}, {}, {})",
                quoted(path),
                quoted(signature)
            ),
        ),
        None => (
            field_type(signature),
            format!("{port}.field({})", quoted(path)),
        ),
    };
    Accessor {
        name: py.to_string(),
        returns,
        body,
    }
}

/// `"f64[6]"` for `"f64[6,6]"`, `"f64"` for `"f64[6]"`, `None` for a scalar
/// or a group.
fn element_signature(signature: &str) -> Option<String> {
    let (ty, dims) = signature.strip_suffix(']')?.split_once('[')?;
    Some(match dims.split_once(',') {
        Some((_, rest)) => format!("{ty}[{rest}]"),
        None => ty.to_string(),
    })
}

/// The `ArrayField` type of a shaped leaf, typed down to its scalars.
fn array_type(signature: &str) -> Option<String> {
    let element = element_signature(signature)?;
    Some(format!(
        "ArrayField[Literal[{}], {}]",
        quoted(signature),
        value_type(&element)
    ))
}

/// The type of a field of `signature`: an `ArrayField` when shaped.
fn value_type(signature: &str) -> String {
    array_type(signature).unwrap_or_else(|| field_type(signature))
}

/// What a mapping of `signature` takes: a field, or for a scalar `f64` also
/// a `Calibrated`.
fn mapped_type(signature: &str) -> String {
    match signature {
        "f64" => format!("{} | Calibrated", field_type(signature)),
        _ => field_type(signature),
    }
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
            "Field[Literal[\"{0:f64,1:f64}\"]] | Mapping[int, Field[Literal[\"f64\"]] | Calibrated]"
        );
        assert_eq!(a.classes.len(), 2);
        assert!(a.classes[1].accessors.is_empty());
        assert_eq!(py_name("0"), None);
        assert_eq!(py_name(""), None);
        assert_eq!(py_name("a-b"), None);
    }

    #[test]
    fn test_shaped_leaf_is_an_array_field() {
        let a = Accessors::of("A", &leaves(&[("v", "f64[4]"), ("s", "f64")]));
        let v = &a.classes[0].accessors[0];
        assert_eq!(
            v.returns,
            "ArrayField[Literal[\"f64[4]\"], Field[Literal[\"f64\"]]]"
        );
        assert_eq!(v.body, "ArrayField(self, \"v\", \"f64[4]\")");
        assert_eq!(
            a.keywords[0].annotation,
            "Field[Literal[\"f64[4]\"]] | Mapping[int, Field[Literal[\"f64\"]] | Calibrated]"
        );
        assert_eq!(a.classes[0].accessors[1].returns, "Field[Literal[\"f64\"]]");
        assert_eq!(
            a.keywords[1].annotation,
            "Field[Literal[\"f64\"]] | Calibrated"
        );
    }

    #[test]
    fn test_matrix_nests() {
        let a = Accessors::of("M", &leaves(&[("p", "f64[6,6]")]));
        assert_eq!(
            a.classes[0].accessors[0].returns,
            "ArrayField[Literal[\"f64[6,6]\"], \
             ArrayField[Literal[\"f64[6]\"], Field[Literal[\"f64\"]]]]"
        );
        assert_eq!(
            a.keywords[0].annotation,
            "Field[Literal[\"f64[6,6]\"]] | Mapping[int, Field[Literal[\"f64[6]\"]]]"
        );
    }

    #[test]
    fn test_index_run_group_is_an_array_field() {
        let a = Accessors::of("B", &leaves(&[("open.0", "bool"), ("open.1", "bool")]));
        let sig = "Literal[\"{0:bool,1:bool}\"]";
        assert_eq!(
            a.classes[1].base,
            format!("ArrayField[{sig}, Field[Literal[\"bool\"]]]")
        );
        assert_eq!(
            a.keywords[0].annotation,
            format!("Field[{sig}] | Mapping[int, Field[Literal[\"bool\"]]]")
        );
    }

    #[test]
    fn test_groups_that_are_not_index_runs() {
        let gap = Accessors::of("G", &leaves(&[("g.0", "f64"), ("g.2", "f64")]));
        let mixed = Accessors::of("H", &leaves(&[("h.0", "f64"), ("h.1", "u8")]));
        let named = Accessors::of("N", &leaves(&[("n.a", "f64"), ("n.b", "f64")]));
        for a in [gap, mixed, named] {
            assert!(
                a.classes[1].base.starts_with("Field["),
                "{}",
                a.classes[1].base
            );
            assert!(!a.keywords[0].annotation.contains("Mapping"));
        }
        assert_eq!(element_signature("{a:f64}"), None);
        assert_eq!(element_signature("f64"), None);
    }

    #[test]
    fn test_only_scalar_f64_takes_calibrated() {
        let a = Accessors::of(
            "F",
            &leaves(&[("x", "f32"), ("n", "i32"), ("g.0", "f64"), ("g.1", "f64")]),
        );
        assert_eq!(a.keywords[0].annotation, "Field[Literal[\"f32\"]]");
        assert_eq!(a.keywords[1].annotation, "Field[Literal[\"i32\"]]");
        assert_eq!(
            a.keywords[2].annotation,
            "Field[Literal[\"{0:f64,1:f64}\"]] | Mapping[int, Field[Literal[\"f64\"]] | Calibrated]"
        );
        let matrix = Accessors::of("M", &leaves(&[("p", "f64[6,6]")]));
        assert!(!matrix.keywords[0].annotation.contains("Calibrated"));
    }
}
