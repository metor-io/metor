//! Reusable expandable tree view over an [`Arc<serde_json::Value>`], used by the
//! plot's event popover to show a schema-decoded message.
//!
//! Objects and arrays are disclosure rows; leaves render `key: value` in the
//! same muted key/value styling the log panel uses for structured fields.
//! Containers open to [`DEFAULT_EXPAND_DEPTH`] by default; a click toggles one.
//! Rather than store per-node open flags, the view tracks the set of paths whose
//! state differs from that default — one `HashSet`, both directions.

use std::collections::HashSet;
use std::sync::Arc;

use gpui::{AnyElement, Context, IntoElement, SharedString, Window, div, prelude::*, px};
use serde_json::Value;

use crate::icons::Icon;
use crate::theme::{Theme, theme};

/// Indent per depth level, in pixels.
const INDENT: f32 = 12.0;
/// Row height, in pixels — compact enough to skim a large object.
const ROW_HEIGHT: f32 = 18.0;
/// Depth below which containers start expanded.
const DEFAULT_EXPAND_DEPTH: usize = 2;

/// Tree view of a JSON value with per-node collapse state.
pub struct JsonTree {
    value: Arc<Value>,
    /// Paths whose open state is flipped from the depth default.
    toggled: HashSet<String>,
}

impl JsonTree {
    pub fn new(value: Arc<Value>, _cx: &mut Context<Self>) -> Self {
        Self {
            value,
            toggled: HashSet::new(),
        }
    }

    /// Swap in a new value, keeping the collapse state (paths that still exist
    /// keep their toggle; stale ones are harmless).
    pub fn set_value(&mut self, value: Arc<Value>, cx: &mut Context<Self>) {
        self.value = value;
        cx.notify();
    }

    fn is_expanded(&self, path: &str, depth: usize) -> bool {
        (depth < DEFAULT_EXPAND_DEPTH) ^ self.toggled.contains(path)
    }

    /// Append this node's row (and, when open, its children) to `rows`.
    #[allow(clippy::too_many_arguments)]
    fn push_node(
        &self,
        key: Option<SharedString>,
        value: &Value,
        path: &str,
        depth: usize,
        theme: &Theme,
        next_id: &mut usize,
        rows: &mut Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) {
        let child_count = match value {
            Value::Object(map) => Some(map.len()),
            Value::Array(arr) => Some(arr.len()),
            _ => None,
        };
        let has_children = child_count.unwrap_or(0) > 0;
        let expanded = has_children && self.is_expanded(path, depth);

        let id = *next_id;
        *next_id += 1;

        let mut row = div()
            .id(("json-tree-row", id))
            .flex()
            .flex_row()
            .items_center()
            .h(px(ROW_HEIGHT))
            .pl(px(depth as f32 * INDENT))
            .gap_1()
            .text_size(px(11.0));

        // Disclosure slot: a triangle for openable containers, else a spacer so
        // keys stay aligned across sibling rows.
        let mut slot = div().w(px(12.0)).flex_shrink_0();
        if has_children {
            let icon = if expanded {
                Icon::ChevronDown
            } else {
                Icon::ChevronRight
            };
            slot = slot.child(icon.svg_color(10.0, theme.text_secondary));
        }
        row = row.child(slot);

        if let Some(key) = key {
            row = row.child(
                div()
                    .text_color(theme.text_tertiary)
                    .child(SharedString::from(format!("{key}:"))),
            );
        }

        // Trailing text: a leaf's value, or a container summary when it's closed
        // or empty.
        let trailing = if child_count.is_some() {
            if expanded {
                None
            } else {
                Some((summary(value), theme.text_secondary))
            }
        } else {
            Some((leaf_text(value), theme.text_primary))
        };
        if let Some((text, color)) = trailing {
            row = row.child(div().text_color(color).child(SharedString::from(text)));
        }

        if has_children {
            let path = path.to_string();
            row = row
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    if !this.toggled.remove(&path) {
                        this.toggled.insert(path.clone());
                    }
                    cx.notify();
                }));
        }

        rows.push(row.into_any_element());

        if !expanded {
            return;
        }
        match value {
            Value::Object(map) => {
                for (k, v) in map {
                    let child_path = format!("{path}/{k}");
                    self.push_node(
                        Some(k.clone().into()),
                        v,
                        &child_path,
                        depth + 1,
                        theme,
                        next_id,
                        rows,
                        cx,
                    );
                }
            }
            Value::Array(arr) => {
                for (i, v) in arr.iter().enumerate() {
                    let child_path = format!("{path}/{i}");
                    self.push_node(
                        Some(format!("[{i}]").into()),
                        v,
                        &child_path,
                        depth + 1,
                        theme,
                        next_id,
                        rows,
                        cx,
                    );
                }
            }
            _ => {}
        }
    }
}

impl Render for JsonTree {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let value = self.value.clone();
        let mut rows: Vec<AnyElement> = Vec::new();
        let mut next_id = 0usize;
        self.push_node(None, &value, "", 0, &theme, &mut next_id, &mut rows, cx);

        div()
            .flex()
            .flex_col()
            .text_color(theme.text_primary)
            .children(rows)
    }
}

/// Closed-container summary: `{…} 5 fields` / `[…] 12 items`.
fn summary(value: &Value) -> String {
    match value {
        Value::Object(map) => format!("{{…}} {}", count_label(map.len(), "field")),
        Value::Array(arr) => format!("[…] {}", count_label(arr.len(), "item")),
        _ => String::new(),
    }
}

/// One-line rendering of a scalar leaf; strings are quoted.
fn leaf_text(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => format!("\"{s}\""),
        // Containers never reach here.
        _ => String::new(),
    }
}

/// `"1 field"` / `"3 fields"` — singular for one, `s`-plural otherwise.
fn count_label(n: usize, singular: &str) -> String {
    if n == 1 {
        format!("1 {singular}")
    } else {
        format!("{n} {singular}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn summary_counts_and_pluralizes() {
        assert_eq!(summary(&json!({"a": 1})), "{…} 1 field");
        assert_eq!(summary(&json!({"a": 1, "b": 2})), "{…} 2 fields");
        assert_eq!(summary(&json!([1, 2, 3])), "[…] 3 items");
        assert_eq!(summary(&json!([])), "[…] 0 items");
    }

    #[test]
    fn leaf_text_quotes_strings_only() {
        assert_eq!(leaf_text(&json!("hi")), "\"hi\"");
        assert_eq!(leaf_text(&json!(42)), "42");
        assert_eq!(leaf_text(&json!(true)), "true");
        assert_eq!(leaf_text(&json!(null)), "null");
    }
}
