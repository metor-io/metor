//! The host's half of expression completion: ranking and display.
//!
//! `metor_expr::complete` collects broadly and does not rank — matching what
//! the operator typed against what is legal is the host's job, with the same
//! nucleo matcher the rest of the panel filters by. This module is that
//! half, shared by the picker rows and the canvas popup so a candidate looks
//! and sorts the same wherever it appears.

use std::sync::Arc;

use gpui::{App, Pixels, SharedString, Window, div, prelude::*, px};
use metor_db::DB;
use metor_expr::complete::{CompletionItem, CompletionKind, Completions};

use crate::dynamic::resolver::DbResolver;
use crate::theme::theme;

thread_local! {
    /// Resolver snapshot cached against the DB's vtable generation, so a
    /// completion per keystroke does not re-lock and re-sort the component
    /// tree. Mirrors `COMPONENT_LIST_CACHE` in the trace picker.
    static RESOLVER_CACHE: std::cell::RefCell<Option<(u64, Arc<DbResolver>)>> =
        const { std::cell::RefCell::new(None) };
}

/// The component tree as the completion engine should see it, cached until a
/// component appears or disappears.
pub(crate) fn resolver(db: &DB) -> Arc<DbResolver> {
    let generation = db.vtable_gen.latest();
    let cached = RESOLVER_CACHE.with(|cache| match &*cache.borrow() {
        Some((at, resolver)) if *at == generation => Some(resolver.clone()),
        _ => None,
    });
    cached.unwrap_or_else(|| {
        let fresh = Arc::new(DbResolver::snapshot(db));
        RESOLVER_CACHE.with(|cache| *cache.borrow_mut() = Some((generation, fresh.clone())));
        fresh
    })
}

/// Order the engine's candidates for display, best first.
///
/// Fuzzy score dominates — an operator who typed `pos` wants `position`
/// first whatever its kind — with the kind tier breaking ties and the label
/// after that, so equal matches never reshuffle between keystrokes. A
/// non-empty prefix drops candidates it does not match at all; an empty one
/// keeps everything in tier order.
pub(crate) fn rank(completions: &mut Completions) {
    use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};

    // The prefix is matched literally: fzf's query operators are for search
    // boxes, and `^` or `!` typed here is expression text, not syntax.
    let unfiltered = completions.prefix.is_empty();
    let pattern = Pattern::new(
        &completions.prefix,
        CaseMatching::Ignore,
        Normalization::Smart,
        AtomKind::Fuzzy,
    );
    rank_by(completions, true, pattern, unfiltered);
}

/// Order candidates for a *search* query — the picker's plain-name mode,
/// where the field is a filter rather than an expression. Matching follows
/// the inspector's own rules (`Pattern::parse`, default config), so a spaced
/// query narrows by every word exactly as the fuzzy filter always has.
pub(crate) fn rank_search(completions: &mut Completions, query: &str) {
    use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};

    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    rank_by(completions, false, pattern, query.trim().is_empty());
}

fn rank_by(
    completions: &mut Completions,
    prefer_prefix: bool,
    pattern: nucleo_matcher::pattern::Pattern,
    unfiltered: bool,
) {
    use nucleo_matcher::{Config, Matcher, Utf32Str};

    let items = &mut completions.items;
    if unfiltered {
        items.sort_by_key(|a| tier(a.kind));
        return;
    }

    let mut config = Config::DEFAULT;
    config.prefer_prefix = prefer_prefix;
    let mut matcher = Matcher::new(config);

    let mut buf = Vec::new();
    let mut scored: Vec<(CompletionItem, u32)> = std::mem::take(items)
        .into_iter()
        .filter_map(|item| {
            let haystack = Utf32Str::new(&item.label, &mut buf);
            let score = pattern.score(haystack, &mut matcher)?;
            Some((item, score))
        })
        .collect();
    scored.sort_by(|(a, sa), (b, sb)| {
        sb.cmp(sa)
            .then(tier(a.kind).cmp(&tier(b.kind)))
            .then_with(|| a.label.cmp(&b.label))
    });
    *items = scored.into_iter().map(|(item, _)| item).collect();
}

/// Kinds by how often an operator wants them: the data first, the language
/// last.
fn tier(kind: CompletionKind) -> u8 {
    match kind {
        CompletionKind::Component | CompletionKind::Field | CompletionKind::Local => 0,
        CompletionKind::Function => 1,
        CompletionKind::Builtin => 2,
        CompletionKind::Keyword => 3,
    }
}

/// A candidate's line: kind glyph, label, and the type dimmed at the far
/// side. The picker wraps this in `row_base`; the canvas popup stacks it in
/// its own panel — one spelling of a candidate either way.
pub(crate) fn candidate_content(
    item: &CompletionItem,
    budget: Option<Pixels>,
    window: &Window,
    cx: &App,
) -> gpui::Div {
    let theme = theme(cx);
    // The glyph and the detail take their share before the label elides.
    let budget = budget
        .map(|b| b - crate::inspector::rows::measure(&item.detail, px(11.0), window) - px(36.0));
    let glyph = match item.kind {
        CompletionKind::Component => "◆",
        CompletionKind::Field | CompletionKind::Local => "▪",
        CompletionKind::Function | CompletionKind::Builtin => "ƒ",
        CompletionKind::Keyword => "∘",
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .w_full()
        .min_w_0()
        .child(
            div()
                .text_size(px(11.0))
                .text_color(theme.text_tertiary)
                .child(SharedString::new_static(glyph)),
        )
        .child(crate::inspector::rows::path_label(
            &item.label,
            theme.text_primary,
            budget,
            window,
            cx,
        ))
        .child(
            div()
                .text_size(px(11.0))
                .text_color(theme.text_tertiary)
                .child(SharedString::from(item.detail.clone())),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use metor_expr::Span;

    fn item(label: &str, kind: CompletionKind) -> CompletionItem {
        CompletionItem {
            label: label.to_string(),
            detail: String::new(),
            kind,
            insert: label.to_string(),
            caret: None,
        }
    }

    #[test]
    fn prefix_matches_filter_and_order() {
        let mut c = Completions {
            replace: Span { start: 0, end: 2 },
            prefix: "om".to_string(),
            items: vec![
                item("round", CompletionKind::Builtin),
                item("adcs.omega_b", CompletionKind::Component),
                item("power.bus_v", CompletionKind::Component),
            ],
        };
        rank(&mut c);
        let labels: Vec<&str> = c.items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["adcs.omega_b"], "non-matches drop");
    }

    #[test]
    fn empty_prefix_keeps_everything_in_tier_order() {
        let mut c = Completions {
            replace: Span { start: 0, end: 0 },
            prefix: String::new(),
            items: vec![
                item("sqrt", CompletionKind::Builtin),
                item("adcs.omega_b", CompletionKind::Component),
            ],
        };
        rank(&mut c);
        assert_eq!(c.items[0].label, "adcs.omega_b");
        assert_eq!(c.items.len(), 2);
    }

    #[test]
    fn search_splits_words_like_the_inspector() {
        let mut c = Completions {
            replace: Span { start: 0, end: 1 },
            prefix: "b".to_string(),
            items: vec![
                item("adcs.omega_b", CompletionKind::Component),
                item("power.bus_v", CompletionKind::Component),
            ],
        };
        rank_search(&mut c, "omega b");
        let labels: Vec<&str> = c.items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["adcs.omega_b"], "each word narrows");
    }

    #[test]
    fn prefix_prefers_prefix_matches() {
        let mut c = Completions {
            replace: Span { start: 0, end: 3 },
            prefix: "sin".to_string(),
            items: vec![
                item("asin", CompletionKind::Builtin),
                item("sin", CompletionKind::Builtin),
                item("sinh", CompletionKind::Builtin),
            ],
        };
        rank(&mut c);
        assert_eq!(c.items[0].label, "sin", "exact match first");
    }
}
