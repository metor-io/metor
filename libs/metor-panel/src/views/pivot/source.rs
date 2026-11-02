//! What a pivot table takes for its rows.
//!
//! A table gathers instances from every source it names and keeps the
//! union. The most reliable source is the frame tag the producer stamps on
//! each field (`frame` / `frame.root`), so every instance of a frame type
//! lands in one table no matter where its port sits. Where nothing is
//! tagged, a [`RowPattern`] names instances by path, a shape gathers
//! subtrees having at least an exemplar's fields, and hand-picked paths
//! cover anything that fits no rule. A filter and an exclude list then
//! narrow the union by path.
//!
//! Sources resolve against the uncompressed tree, where every path prefix
//! is a node: an instance is a subtree, and one whose only child is a
//! single field would vanish into a compressed chain.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use gpui::SharedString;
use metor_db::DB;
use regex::Regex;

use super::model::{Captures, Frame};
use crate::query::Query;
use crate::views::component_browser::component_tree::{ComponentNode, natural_cmp, node_at};

/// Every tagged frame instance in the db: type name → roots. A frame map's
/// entries are instances of their own type, `{frame}.{map}`.
#[derive(Default, Clone)]
pub(crate) struct FrameIndex {
    roots: HashMap<SharedString, BTreeSet<SharedString>>,
}

impl FrameIndex {
    pub fn from_db(db: &DB) -> Self {
        db.with_state(|state| {
            let tags: Vec<(String, String)> = state
                .component_metadata_iter()
                .filter(|(_, meta)| !meta.is_hidden())
                .flat_map(|(_, meta)| meta.frame_instances())
                .collect();
            Self::from_tags(tags.iter().map(|(t, r)| (t.as_str(), r.as_str())))
        })
    }

    pub fn from_tags<'a>(tags: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let mut roots: HashMap<SharedString, BTreeSet<SharedString>> = HashMap::new();
        for (name, root) in tags {
            roots
                .entry(SharedString::from(name.to_string()))
                .or_default()
                .insert(SharedString::from(root.to_string()));
        }
        Self { roots }
    }

    /// The tagged frame types, in natural order.
    pub fn types(&self) -> Vec<SharedString> {
        let mut out: Vec<SharedString> = self.roots.keys().cloned().collect();
        out.sort_by(|a, b| natural_cmp(a, b));
        out
    }

    /// The roots tagged with `name`, matched case-insensitively.
    fn roots(&self, name: &str) -> impl Iterator<Item = &SharedString> {
        self.roots
            .iter()
            .filter(move |(k, _)| k.eq_ignore_ascii_case(name))
            .flat_map(|(_, roots)| roots)
    }
}

/// A path pattern naming instances, with named captures that become key
/// columns.
///
/// The glob form works a segment at a time: `{name}` captures one segment,
/// `{name**}` one or more, `*` matches within a segment and `**` across
/// them, so `dut{dut}.psu` reads `dut2.psu` as `dut = 2`. A pattern
/// wrapped in slashes is a regex whose named groups capture instead. Both
/// must match the whole path, case-insensitively, like every other filter
/// in the panel.
#[derive(Clone, Debug)]
pub(crate) struct RowPattern {
    regex: Regex,
    captures: Vec<SharedString>,
}

impl RowPattern {
    /// `None` for an empty pattern; an error names what's wrong with a
    /// malformed one.
    pub fn parse(text: &str) -> Result<Option<Self>, String> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(None);
        }
        let source = match text
            .strip_prefix('/')
            .and_then(|rest| rest.strip_suffix('/'))
        {
            Some(regex) if !regex.is_empty() => format!("(?i)^(?:{regex})$"),
            _ => glob_source(text)?,
        };
        let regex = Regex::new(&source).map_err(|e| e.to_string())?;
        let captures = regex
            .capture_names()
            .flatten()
            .map(|n| SharedString::from(n.to_string()))
            .collect();
        Ok(Some(Self { regex, captures }))
    }

    /// The capture names, in the order they appear.
    pub fn capture_names(&self) -> &[SharedString] {
        &self.captures
    }

    /// What each capture took in `path`; `None` when the path doesn't
    /// match.
    pub fn captures(&self, path: &str) -> Option<Captures> {
        let caps = self.regex.captures(path)?;
        Some(
            self.captures
                .iter()
                .map(|n| {
                    caps.name(n)
                        .map(|m| SharedString::from(m.as_str().to_string()))
                        .unwrap_or_default()
                })
                .collect(),
        )
    }
}

fn glob_source(text: &str) -> Result<String, String> {
    let mut out = String::from("(?i)^");
    let mut rest = text;
    while let Some(ch) = rest.chars().next() {
        if let Some(inner) = rest.strip_prefix('{') {
            let close = inner
                .find('}')
                .ok_or_else(|| "unclosed `{` in pattern".to_string())?;
            let spec = &inner[..close];
            let (name, deep) = match spec.strip_suffix("**") {
                Some(name) => (name, true),
                None => (spec, false),
            };
            let valid = name
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !valid {
                return Err(format!("`{{{spec}}}` isn't a capture name"));
            }
            let body = if deep { ".+" } else { "[^.]+" };
            out.push_str(&format!("(?P<{name}>{body})"));
            rest = &inner[close + 1..];
        } else if let Some(after) = rest.strip_prefix("**") {
            out.push_str(".*");
            rest = after;
        } else {
            match ch {
                '*' => out.push_str("[^.]*"),
                '?' => out.push_str("[^.]"),
                other => out.push_str(&regex::escape(other.encode_utf8(&mut [0; 4]))),
            }
            rest = &rest[ch.len_utf8()..];
        }
    }
    out.push('$');
    Ok(out)
}

/// Every source a table names, compiled. Empty sources contribute nothing,
/// so a fresh table lists nothing until one is set.
#[derive(Default)]
pub(crate) struct Sources {
    /// A frame type gathered by its producer's tag.
    pub frame: SharedString,
    pub pattern: Option<RowPattern>,
    /// Fields an instance must have, relative to itself; extras are fine.
    pub shape: Vec<SharedString>,
    /// Hand-picked instance paths.
    pub instances: Vec<SharedString>,
    /// Keeps only instances whose path matches.
    pub filter: Query,
    /// Drops instances whose path matches any one term.
    pub exclude: Vec<Query>,
}

impl Sources {
    /// Whitespace-separated terms, each a substring or glob on its own.
    pub fn parse_exclude(text: &str) -> Vec<Query> {
        text.split_whitespace().map(Query::parse).collect()
    }

    /// The instances every source finds in `tree` (uncompressed), filtered,
    /// in natural path order. A pattern's captures apply to any instance
    /// whose path it matches, whichever source found it.
    pub fn resolve(&self, tree: &Arc<ComponentNode>, frames: &FrameIndex) -> Vec<Frame> {
        let mut found: BTreeMap<SharedString, Arc<ComponentNode>> = BTreeMap::new();
        let mut take = |path: &SharedString| {
            if found.contains_key(path) {
                return;
            }
            if let Some(node) = node_at(tree, path).filter(|n| !n.children.is_empty()) {
                found.insert(path.clone(), node);
            }
        };
        if !self.frame.trim().is_empty() {
            for root in frames.roots(self.frame.trim()) {
                take(root);
            }
        }
        for path in &self.instances {
            take(path);
        }
        if self.pattern.is_some() || !self.shape.is_empty() {
            let mut stack: Vec<Arc<ComponentNode>> = tree.children.values().cloned().collect();
            while let Some(node) = stack.pop() {
                if node.children.is_empty() {
                    continue;
                }
                let by_pattern = self
                    .pattern
                    .as_ref()
                    .is_some_and(|p| p.regex.is_match(&node.full_name));
                if by_pattern || (!self.shape.is_empty() && has_fields(&node, &self.shape)) {
                    found.entry(node.full_name.clone()).or_insert(node.clone());
                }
                stack.extend(node.children.values().cloned());
            }
        }
        let mut out: Vec<Frame> = found
            .into_iter()
            .filter(|(path, _)| self.filter.matches_name(path))
            .filter(|(path, _)| !self.exclude.iter().any(|q| q.matches_name(path)))
            .map(|(path, node)| Frame {
                captures: self
                    .pattern
                    .as_ref()
                    .and_then(|p| p.captures(&path))
                    .unwrap_or_else(|| self.blank_captures()),
                label: path,
                node,
            })
            .collect();
        out.sort_by(|a, b| natural_cmp(&a.label, &b.label));
        out
    }

    fn blank_captures(&self) -> Captures {
        let n = self.pattern.as_ref().map_or(0, |p| p.captures.len());
        (0..n).map(|_| SharedString::default()).collect()
    }
}

/// Whether every field of `fields` is a component under `node`.
fn has_fields(node: &Arc<ComponentNode>, fields: &[SharedString]) -> bool {
    fields
        .iter()
        .all(|f| node_at(node, f).and_then(|n| n.component_id).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::pivot::model::tests::{branch, leaf};

    /// Uncompressed: two PSUs under DUTs (one with an extra field), a
    /// third PSU missing a field, and a port nested a level deeper.
    fn tree() -> Arc<ComponentNode> {
        branch(
            "",
            vec![
                branch(
                    "dut1",
                    vec![
                        branch(
                            "dut1.psu",
                            vec![leaf("dut1.psu.current"), leaf("dut1.psu.voltage")],
                        ),
                        leaf("dut1.serial"),
                    ],
                ),
                branch(
                    "dut2",
                    vec![branch(
                        "dut2.psu",
                        vec![
                            leaf("dut2.psu.current"),
                            leaf("dut2.psu.voltage"),
                            leaf("dut2.psu.temp"),
                        ],
                    )],
                ),
                branch(
                    "dut3",
                    vec![branch("dut3.psu", vec![leaf("dut3.psu.current")])],
                ),
            ],
        )
    }

    fn labels(frames: &[Frame]) -> Vec<&str> {
        frames.iter().map(|f| f.label.as_ref()).collect()
    }

    fn pattern(text: &str) -> Option<RowPattern> {
        RowPattern::parse(text).unwrap()
    }

    #[test]
    fn a_glob_capture_takes_one_segment_and_names_a_key() {
        let p = pattern("dut{dut}.psu").unwrap();
        assert_eq!(p.capture_names(), ["dut"]);
        assert_eq!(p.captures("DUT12.psu").unwrap().as_slice(), ["12"]);
        assert!(p.captures("dut1.bay.psu").is_none());
        let deep = pattern("{sys**}.psu").unwrap();
        assert_eq!(deep.captures("a.b.psu").unwrap().as_slice(), ["a.b"]);
    }

    #[test]
    fn a_slashed_pattern_is_a_regex_with_named_groups() {
        let p = pattern(r"/dut(?<n>\d+)\.psu/").unwrap();
        assert_eq!(p.capture_names(), ["n"]);
        assert_eq!(p.captures("dut3.psu").unwrap().as_slice(), ["3"]);
        assert!(p.captures("dutx.psu").is_none());
    }

    #[test]
    fn malformed_patterns_say_why() {
        assert!(RowPattern::parse("dut{").is_err());
        assert!(RowPattern::parse("dut{1x}").is_err());
        assert!(RowPattern::parse("/(/").is_err());
        assert!(RowPattern::parse("  ").unwrap().is_none());
    }

    #[test]
    fn a_frame_tag_gathers_its_roots() {
        let frames =
            FrameIndex::from_tags([("psu", "dut1.psu"), ("psu", "dut3.psu"), ("imu", "dut2")]);
        let sources = Sources {
            frame: "PSU".into(),
            ..Default::default()
        };
        assert_eq!(
            labels(&sources.resolve(&tree(), &frames)),
            ["dut1.psu", "dut3.psu"]
        );
        assert_eq!(frames.types(), ["imu", "psu"]);
    }

    #[test]
    fn a_shape_takes_any_subtree_with_at_least_its_fields() {
        let sources = Sources {
            shape: vec!["current".into(), "voltage".into()],
            ..Default::default()
        };
        assert_eq!(
            labels(&sources.resolve(&tree(), &FrameIndex::default())),
            ["dut1.psu", "dut2.psu"]
        );
    }

    #[test]
    fn sources_union_and_captures_reach_every_matching_row() {
        let sources = Sources {
            pattern: pattern("dut{dut}.psu"),
            instances: vec!["dut1".into(), "missing".into()],
            ..Default::default()
        };
        let rows = sources.resolve(&tree(), &FrameIndex::default());
        assert_eq!(labels(&rows), ["dut1", "dut1.psu", "dut2.psu", "dut3.psu"]);
        assert_eq!(rows[0].captures.as_slice(), [""]);
        assert_eq!(rows[2].captures.as_slice(), ["2"]);
    }

    #[test]
    fn filter_and_exclude_narrow_by_path() {
        let sources = Sources {
            pattern: pattern("*.psu"),
            filter: Query::parse("dut"),
            exclude: Sources::parse_exclude("dut2 *3.psu"),
            ..Default::default()
        };
        assert_eq!(
            labels(&sources.resolve(&tree(), &FrameIndex::default())),
            ["dut1.psu"]
        );
    }

    #[test]
    fn nothing_named_lists_nothing() {
        assert!(
            Sources::default()
                .resolve(&tree(), &FrameIndex::default())
                .is_empty()
        );
    }
}
