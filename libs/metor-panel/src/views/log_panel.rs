//! Streaming log viewer over the global [`LogStore`](crate::logs::LogStore):
//! a virtualized table of every [`LogEvent`] off the downlink, filtered by
//! minimum level and source, with a follow-the-tail mode.
//!
//! Rows index the store by sequence number rather than copying it: the
//! delegate rebuilds a `Vec<u64>` of matching seqs whenever the store grows
//! or a filter changes, and each cell reads its record back through
//! [`LogState::by_seq`](crate::logs::LogState::by_seq) at paint time.

use gpui::{
    AnyElement, App, Context, Entity, IntoElement, SharedString, WeakEntity, Window, div,
    prelude::*, px,
};
use metor_proto_wkt::LogLevel;

use crate::logs::{self, LogState};
use crate::theme::{Theme, theme};
use crate::views::format::format_time;
use crate::views::table::{self, Column, ColumnSort, Table, TableDelegate};

/// The viewer's level floor. `All` shows everything received (the FSW side
/// already filters what it forwards); the rest hide rows below the floor.
#[derive(
    Clone, Copy, Default, PartialEq, Eq, Debug, facet::Facet, serde::Serialize, serde::Deserialize,
)]
#[repr(u8)]
pub enum LevelFilter {
    #[default]
    All,
    Debug,
    Info,
    Warn,
    Error,
}

impl LevelFilter {
    fn floor(self) -> LogLevel {
        match self {
            LevelFilter::All => LogLevel::Trace,
            LevelFilter::Debug => LogLevel::Debug,
            LevelFilter::Info => LogLevel::Info,
            LevelFilter::Warn => LogLevel::Warn,
            LevelFilter::Error => LogLevel::Error,
        }
    }

    fn of(level: LogLevel) -> Self {
        match level {
            LogLevel::Trace => LevelFilter::All,
            LogLevel::Debug => LevelFilter::Debug,
            LogLevel::Info => LevelFilter::Info,
            LogLevel::Warn => LevelFilter::Warn,
            LogLevel::Error => LevelFilter::Error,
        }
    }
}

/// The log list view. Level floor and source filter are editable through the
/// pane inspector; the header chips and source cells offer the same filters
/// in place.
#[derive(facet::Facet)]
pub struct LogView {
    pub min_level: LevelFilter,
    /// Exact source name to show, empty for all. Set by clicking a source
    /// cell (click again to clear) or through the inspector.
    pub source: String,
    pub follow: bool,
    #[facet(opaque)]
    table: Entity<Table<LogDelegate>>,
}

impl LogView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        if let Some(store) = logs::try_global(cx) {
            cx.observe(&store, |_, _, cx| cx.notify()).detach();
        }
        let weak = cx.weak_entity();
        let table = cx.new(|_| Table::new(LogDelegate::new(weak)));
        Self {
            min_level: LevelFilter::default(),
            source: String::new(),
            follow: true,
            table,
        }
    }

    /// Restore persisted filters.
    pub fn set_filters(&mut self, min_level: LevelFilter, source: String, follow: bool) {
        self.min_level = min_level;
        self.source = source;
        self.follow = follow;
    }

    fn level_chip(
        &self,
        level: LogLevel,
        count: usize,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active = self.min_level == LevelFilter::of(level);
        let color = level_color(level, theme);
        let mut chip = div()
            .id(("log-level-chip", level as usize))
            .px_2()
            .py(px(1.0))
            .rounded(px(3.0))
            .text_size(px(11.0))
            .cursor_pointer()
            .text_color(color)
            .child(SharedString::from(format!(
                "{} {}",
                count,
                level_label(level)
            )));
        if active {
            chip = chip.bg(level_tint(level, theme));
        }
        chip.on_click(cx.listener(move |this, _, _, cx| {
            // Click sets the floor to this level; clicking the active floor
            // clears back to All.
            let target = LevelFilter::of(level);
            this.min_level = if this.min_level == target {
                LevelFilter::All
            } else {
                target
            };
            cx.notify();
        }))
        .into_any_element()
    }
}

impl Render for LogView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let store = logs::try_global(cx);

        // Refresh the delegate's filtered index when the store grew or a
        // filter changed, then chase the tail if follow is on.
        let mut counts = [0usize; 5];
        let mut source_known = true;
        if let Some(store) = &store {
            let state = store.read(cx).state();
            counts = state.counts_by_level();
            source_known = self.source.is_empty() || state.sources().any(|s| s == self.source);
        }
        if let Some(store) = store.clone() {
            let follow = self.follow;
            let min = self.min_level;
            let source = self.source.clone();
            self.table.update(cx, |table, cx| {
                let rebuilt = {
                    let state = store.read(cx).state();
                    table.delegate_mut().refresh(state, min, &source)
                };
                if rebuilt {
                    if follow {
                        let rows = table.delegate().rows_count();
                        if rows > 0 {
                            table.scroll_to_row(rows - 1);
                        }
                    }
                    cx.notify();
                }
            });
        }

        let mut header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .h(px(table::HEADER_HEIGHT))
            .flex_shrink_0()
            .border_b_1()
            .border_color(theme.border_primary);

        for level in [
            LogLevel::Error,
            LogLevel::Warn,
            LogLevel::Info,
            LogLevel::Debug,
            LogLevel::Trace,
        ] {
            if counts[level as usize] == 0 {
                continue;
            }
            header = header.child(self.level_chip(level, counts[level as usize], &theme, cx));
        }

        if !self.source.is_empty() {
            let label = if source_known {
                format!("source: {}", self.source)
            } else {
                format!("source: {} (none seen)", self.source)
            };
            header = header.child(
                div()
                    .id("log-source-filter")
                    .px_2()
                    .py(px(1.0))
                    .rounded(px(3.0))
                    .bg(theme.bg_elevated)
                    .text_color(theme.text_secondary)
                    .text_size(px(11.0))
                    .cursor_pointer()
                    .child(SharedString::from(format!("{label} ✕")))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.source.clear();
                        cx.notify();
                    })),
            );
        }

        header = header.child(div().flex_grow()).child(
            div()
                .id("log-follow")
                .px_2()
                .py(px(1.0))
                .rounded(px(3.0))
                .bg(if self.follow {
                    theme.selection_bg
                } else {
                    theme.bg_elevated
                })
                .text_color(theme.text_secondary)
                .text_size(px(11.0))
                .cursor_pointer()
                .child("Follow")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.follow = !this.follow;
                    cx.notify();
                })),
        );

        let body: AnyElement = if store.is_some() {
            self.table.clone().into_any_element()
        } else {
            div()
                .flex()
                .items_center()
                .justify_center()
                .p_4()
                .text_color(theme.text_tertiary)
                .child("Log store unavailable")
                .into_any_element()
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg_primary)
            .text_color(theme.text_primary)
            .text_size(px(12.0))
            .child(header)
            .child(body)
    }
}

fn level_label(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Trace => "Trace",
        LogLevel::Debug => "Debug",
        LogLevel::Info => "Info",
        LogLevel::Warn => "Warn",
        LogLevel::Error => "Error",
    }
}

fn level_color(level: LogLevel, theme: &Theme) -> gpui::Hsla {
    match level {
        LogLevel::Error => theme.alarm_color(2),
        LogLevel::Warn => theme.alarm_color(1),
        LogLevel::Info => theme.alarm_color(0),
        LogLevel::Debug | LogLevel::Trace => theme.text_tertiary,
    }
}

fn level_tint(level: LogLevel, theme: &Theme) -> gpui::Hsla {
    match level {
        LogLevel::Error => theme.alarm_tint(2),
        LogLevel::Warn => theme.alarm_tint(1),
        LogLevel::Info => theme.alarm_tint(0),
        LogLevel::Debug | LogLevel::Trace => theme.bg_elevated,
    }
}

const COL_TIME: usize = 0;
const COL_LEVEL: usize = 1;
const COL_SOURCE: usize = 2;
const COL_MESSAGE: usize = 3;

/// Table delegate: a filtered index of store sequence numbers.
pub struct LogDelegate {
    view: WeakEntity<LogView>,
    seqs: Vec<u64>,
    /// `(store.pushed, floor, source)` the index was built against.
    built: (u64, LogLevel, String),
}

impl LogDelegate {
    fn new(view: WeakEntity<LogView>) -> Self {
        Self {
            view,
            seqs: Vec::new(),
            built: (0, LogLevel::Trace, String::new()),
        }
    }

    /// Rebuild the index if the store or filters moved. Returns whether it did.
    fn refresh(&mut self, state: &LogState, min: LevelFilter, source: &str) -> bool {
        let floor = min.floor();
        if self.built == (state.pushed(), floor, source.to_string()) {
            return false;
        }
        self.seqs.clear();
        self.seqs.extend(
            state
                .history()
                .iter()
                .filter(|rec| {
                    rec.event.level >= floor && (source.is_empty() || rec.event.source == source)
                })
                .map(|rec| rec.seq),
        );
        self.built = (state.pushed(), floor, source.to_string());
        true
    }
}

impl TableDelegate for LogDelegate {
    fn columns(&self) -> Vec<Column> {
        // All fixed widths: a flex column would fit the viewport and clip
        // long messages, while an all-fixed layout gets the table's
        // horizontal scroll.
        vec![
            Column::new("Time", 110.0),
            Column::new("Level", 64.0),
            Column::new("Source", 150.0),
            Column::new("Message", 2000.0).max_width(8000.0),
        ]
    }

    fn rows_count(&self) -> usize {
        self.seqs.len()
    }

    fn row_height(&self) -> gpui::Pixels {
        px(24.0)
    }

    fn render_cell(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<Table<Self>>,
    ) -> AnyElement {
        let theme = theme(cx);
        let Some(store) = logs::try_global(cx) else {
            return div().into_any_element();
        };
        let store = store.read(cx);
        // A seq the ring has since evicted paints empty; the next refresh
        // drops its row.
        let Some(rec) = self.seqs.get(row_ix).and_then(|&s| store.state().by_seq(s)) else {
            return div().into_any_element();
        };
        let ev = &rec.event;

        let cell = div()
            .flex()
            .items_center()
            .h_full()
            .px(px(12.0))
            .overflow_hidden()
            .whitespace_nowrap();
        match col_ix {
            COL_TIME => cell
                .text_color(theme.text_tertiary)
                .child(SharedString::from(format_time(ev.timestamp.0)))
                .into_any_element(),
            COL_LEVEL => cell
                .child(
                    div()
                        .px(px(6.0))
                        .rounded(px(3.0))
                        .bg(level_tint(ev.level, &theme))
                        .text_color(level_color(ev.level, &theme))
                        .text_size(px(10.0))
                        .child(level_label(ev.level).to_uppercase()),
                )
                .into_any_element(),
            COL_SOURCE => {
                let source = ev.source.clone();
                let view = self.view.clone();
                cell.id(("log-source", row_ix))
                    .text_color(theme.text_secondary)
                    .cursor_pointer()
                    .child(SharedString::from(source.clone()))
                    .on_click(move |_, _, cx| {
                        // Click filters to this source; click again clears.
                        let _ = view.update(cx, |view, cx| {
                            view.source = if view.source == source {
                                String::new()
                            } else {
                                source.clone()
                            };
                            cx.notify();
                        });
                    })
                    .into_any_element()
            }
            COL_MESSAGE => {
                let mut row = cell.gap_2().child(SharedString::from(ev.message.clone()));
                if let Some(span) = &ev.span {
                    row = row.child(
                        div()
                            .text_color(theme.text_tertiary)
                            .text_size(px(10.0))
                            .child(SharedString::from(span.clone())),
                    );
                }
                for (key, value) in &ev.fields {
                    row = row.child(
                        div()
                            .text_color(theme.text_tertiary)
                            .child(SharedString::from(format!("{key}={value}"))),
                    );
                }
                row.into_any_element()
            }
            _ => div().into_any_element(),
        }
    }

    fn sort_column(&mut self, _col_ix: usize, _sort: ColumnSort, _cx: &App) {}
}
