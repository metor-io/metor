use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, Context, Div, FocusHandle, Focusable, Hsla, IntoElement, MouseButton, SharedString,
    Stateful, Window, div, prelude::*, px,
};
use metor_db::DB;
use metor_proto::types::{ComponentId, ElementValue, Timestamp};
use metor_proto_wkt::{OccurrenceId, Severity};
use regex::Regex;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::alarms::latch::{Latch, TileState};
use crate::alarms::severity_index;
#[allow(unused_imports)]
use crate::inspect;
use crate::inspector::plot_preview::shift_hover_listener;

use super::binding::{ElementRef, active_severity, component_meta, spawn_on_stream};
use super::format_number;
use super::tooltip::TooltipText;
use crate::inspector::rows::{DefaultActionRow, InspectorRow};
use crate::theme::{Theme, theme};

/// Which side of a component's value is the alarm.
///
/// `Off` is what the `*.healthy` idiom needs: a system reporting `true` is
/// calm, and the one reporting `false` is the only lit tile on the wall.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, facet::Facet)]
#[repr(u8)]
pub enum AlarmWhen {
    #[default]
    On,
    Off,
}

/// What the glob matches, and therefore where a tile's state comes from.
///
/// `Components` derives the condition locally from a boolean the panel reads;
/// `Alarms` matches [`AlarmDef::name`] and defers everything — colour, latch,
/// acknowledgment — to the shared alarm store, so the tile agrees with the
/// alarm panel, the titlebar, and every other console.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, facet::Facet)]
#[repr(u8)]
pub enum AnnunciatorSource {
    #[default]
    Components,
    Alarms,
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct AnnunciatorConfig {
    pub pattern: String,
    pub color: Option<Hsla>,
    pub source: AnnunciatorSource,
    pub alarm_when: AlarmWhen,
    pub show_labels: bool,
    pub show_values: bool,
    pub latch: bool,
    pub columns: usize,
}

/// One matched component's tile.
struct Tile {
    id: ComponentId,
    name: SharedString,
    /// `name` with the glob's literal parts removed — the part the pattern's
    /// wildcards actually matched, which is the only part that varies.
    label: SharedString,
    unit: SharedString,
    is_bool: bool,
    element_names: Vec<SharedString>,
    latest_on: Option<bool>,
    latest_value: Option<f64>,
    latch: Latch,
    _task: gpui::Task<()>,
}

/// ISA-18.1 annunciator: one tile per component — or per declared alarm —
/// matching `pattern`.
///
/// `pattern` uses the same glob syntax as the component browser (`*`, `?`).
/// The watcher task re-evaluates the regex every time a new component
/// registers, so producers that come up after the annunciator is placed still
/// appear without needing the user to retype.
///
/// Component-sourced latch state is per view and deliberately unpersisted:
/// nothing outside this panel declared those conditions alarms, so restoring a
/// latch from a layout file would assert a trip that may never have happened
/// this session. Alarm-sourced tiles hold no local state at all — the store
/// owns their latch, and the acks they publish are wire events.
#[derive(facet::Facet)]
pub struct Annunciator {
    pub pattern: SharedString,
    pub color: Hsla,
    #[facet(inspect::variants = "Components,Alarms")]
    pub source: AnnunciatorSource,
    /// Which side of a component value is the alarm. Meaningless for
    /// `Alarms`, where the control system decides.
    #[facet(inspect::variants = "On,Off")]
    pub alarm_when: AlarmWhen,
    pub show_labels: bool,
    pub show_values: bool,
    pub latch: bool,
    /// Tiles per row; `0` wraps to fit instead.
    #[facet(inspect::range(min = "0", max = "12"))]
    pub columns: usize,
    #[facet(opaque)]
    db: Arc<DB>,
    /// Pattern the current `regex` was compiled from. Compared against
    /// `pattern` at render time so direct field writes (via the reflective
    /// inspector) trigger a recompile without going through `set_pattern`.
    #[facet(skip)]
    compiled_pattern: SharedString,
    /// Polarity the tiles' latches were folded under, compared the same way.
    #[facet(skip)]
    bound_alarm_when: AlarmWhen,
    /// Source the current tile set was built from, compared the same way so
    /// flipping it in the inspector rebuilds the tiles that frame.
    #[facet(skip)]
    bound_source: AnnunciatorSource,
    #[facet(opaque)]
    regex: Option<Regex>,
    #[facet(opaque)]
    tiles: Vec<Tile>,
    #[facet(opaque)]
    focus: FocusHandle,
    #[facet(opaque)]
    _watcher: gpui::Task<()>,
}

impl Annunciator {
    pub fn new(db: Arc<DB>, pattern: SharedString, cx: &mut Context<Self>) -> Self {
        Self::from_config(
            AnnunciatorConfig {
                pattern: pattern.to_string(),
                ..Default::default()
            },
            db,
            cx,
        )
    }

    pub fn from_config(cfg: AnnunciatorConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let pattern = SharedString::from(cfg.pattern);
        let regex = compile_pattern(&pattern);
        let watcher = spawn_watcher(db.clone(), cx);
        // Alarm-sourced tiles have no stream task of their own, and component-sourced
        // ones still take their severity tint from the store.
        if let Some(store) = crate::alarms::try_global(cx) {
            cx.observe(&store, |_, _, cx| cx.notify()).detach();
        }
        Self {
            compiled_pattern: pattern.clone(),
            pattern,
            color: cfg.color.unwrap_or_else(|| theme(cx).control_active),
            bound_source: cfg.source,
            source: cfg.source,
            bound_alarm_when: cfg.alarm_when,
            alarm_when: cfg.alarm_when,
            show_labels: cfg.show_labels,
            show_values: cfg.show_values,
            latch: cfg.latch,
            columns: cfg.columns,
            db,
            regex,
            tiles: Vec::new(),
            focus: cx.focus_handle(),
            _watcher: watcher,
        }
    }

    pub fn to_config(&self) -> AnnunciatorConfig {
        AnnunciatorConfig {
            pattern: self.pattern.to_string(),
            color: Some(self.color),
            source: self.source,
            alarm_when: self.alarm_when,
            show_labels: self.show_labels,
            show_values: self.show_values,
            latch: self.latch,
            columns: self.columns,
        }
    }

    pub fn pattern(&self) -> &SharedString {
        &self.pattern
    }

    pub fn set_pattern(&mut self, pattern: SharedString, cx: &mut Context<Self>) {
        if self.pattern == pattern {
            return;
        }
        self.pattern = pattern;
        self.recompile_and_reconcile(cx);
    }

    /// Recompile the regex from `self.pattern` and rebuild the tile list.
    ///
    /// Used both by `set_pattern` (programmatic) and at render time when the
    /// inspector has written `pattern` directly through the reflective field
    /// adapter. After mutation, `cx.notify()` is emitted so the next frame
    /// shows the new tile set.
    fn recompile_and_reconcile(&mut self, cx: &mut Context<Self>) {
        self.compiled_pattern = self.pattern.clone();
        self.regex = compile_pattern(&self.pattern);
        self.reconcile_tiles(cx);
    }

    /// Diff the matched component set against `self.tiles`, dropping tiles
    /// that no longer match and creating tasks for newly-matched components.
    /// Existing tiles keep their stream tasks so we don't drop in-flight
    /// values just because a sibling component registered.
    fn reconcile_tiles(&mut self, cx: &mut Context<Self>) {
        // Alarm-sourced tiles are read straight from the store at render time,
        // so there is nothing to keep a stream task for.
        if self.source == AnnunciatorSource::Alarms {
            self.tiles.clear();
            cx.notify();
            return;
        }
        let matches: Vec<(ComponentId, String)> = match &self.regex {
            Some(re) => crate::inspector::trace_picker::list_components(&self.db)
                .into_iter()
                .filter(|(_, name)| re.is_match(name))
                .collect(),
            None => Vec::new(),
        };

        self.tiles
            .retain(|t| matches.iter().any(|(id, _)| *id == t.id));

        for (id, name) in matches {
            let label = strip_glob_literals(&self.pattern, &name);
            if let Some(tile) = self.tiles.iter_mut().find(|t| t.id == id) {
                // A kept tile may have been built before its component
                // registered; re-read metadata so is_bool/element_names
                // catch up without tearing down the stream task.
                let meta = component_meta(&self.db, id);
                tile.is_bool = meta.is_bool;
                tile.unit = meta.unit;
                tile.element_names = meta.element_names;
                tile.label = label;
                continue;
            }
            let tile = build_tile(self.db.clone(), id, SharedString::from(name), label, cx);
            self.tiles.push(tile);
        }

        self.tiles.sort_by(|a, b| a.name.cmp(&b.name));
        cx.notify();
    }

    /// Re-fold every latch after the inspector flipped the polarity, so the
    /// tiles agree with the new reading of the samples already in hand.
    fn refold_polarity(&mut self) {
        self.bound_alarm_when = self.alarm_when;
        let inverted = self.alarm_when == AlarmWhen::Off;
        let now = Timestamp::now();
        for tile in &mut self.tiles {
            if let Some(on) = tile.latest_on {
                tile.latch.condition(on ^ inverted, now);
            }
        }
    }

    /// Whether `name` is in this annunciator's set.
    fn matches(&self, name: &str) -> bool {
        self.regex.as_ref().is_some_and(|re| re.is_match(name))
    }

    /// Tiles for the components the glob matches, from their local latches.
    fn component_visuals(&self, cx: &App) -> Vec<TileVisual> {
        let latching = self.latch;
        let toggles = !latching && self.alarm_when == AlarmWhen::On;
        self.tiles
            .iter()
            .map(|tile| TileVisual {
                key: tile.id.0,
                name: tile.name.clone(),
                label: self.show_labels.then(|| tile.label.clone()),
                value: (self.show_values && !tile.is_bool).then(|| value_text(tile)),
                click: match (latching, toggles && tile.is_bool) {
                    (true, _) => Some(TileClick::AckLatch(tile.id)),
                    (false, true) => Some(TileClick::Toggle(tile.id)),
                    (false, false) => None,
                },
                state: tile.latch.state(latching),
                since: tile.latch.since(),
                reported: tile.latest_on.is_some(),
                first_out: false,
                severity: active_severity(ElementRef::new(tile.id, 0), cx),
                hover: Some(tile.id),
            })
            .collect()
    }

    /// Tiles for the declared alarms the glob matches. The store owns their state, so
    /// these are rebuilt from it each frame and hold nothing in between.
    fn alarm_visuals(&self, cx: &App) -> Vec<TileVisual> {
        let Some(store) = crate::alarms::try_global(cx) else {
            return Vec::new();
        };
        let store = store.read(cx);
        let state = store.state();
        let mut visuals: Vec<TileVisual> = state
            .defs_iter()
            .filter(|def| self.matches(&def.name))
            .map(|def| {
                let point = state.point(&def.id);
                TileVisual {
                    key: tile_key(&def.id),
                    name: SharedString::from(def.name.clone()),
                    label: self
                        .show_labels
                        .then(|| strip_glob_literals(&self.pattern, &def.name)),
                    value: self.show_values.then(|| match point.value {
                        Some(value) => SharedString::from(format_number(value)),
                        None => SharedString::new_static("—"),
                    }),
                    click: point.occurrence.map(TileClick::AckAlarm),
                    state: point.state,
                    since: point.since,
                    reported: true,
                    first_out: false,
                    severity: point.severity,
                    hover: def.target.as_ref().map(|target| target.component_id),
                }
            })
            .collect();
        visuals.sort_by(|a, b| a.name.cmp(&b.name));
        visuals
    }

    fn ack(&mut self, id: ComponentId, cx: &mut Context<Self>) {
        if let Some(tile) = self.tiles.iter_mut().find(|t| t.id == id) {
            tile.latch.ack();
            cx.notify();
        }
    }

    /// Acknowledge every lit tile in *this* annunciator — never the global alarm set.
    fn ack_all(&mut self, cx: &mut Context<Self>) {
        if self.source == AnnunciatorSource::Alarms {
            let occurrences: Vec<OccurrenceId> = self
                .alarm_visuals(cx)
                .into_iter()
                .filter(|visual| visual.state != TileState::Normal)
                .filter_map(|visual| match visual.click {
                    Some(TileClick::AckAlarm(occurrence)) => Some(occurrence),
                    _ => None,
                })
                .collect();
            if let Some(store) = crate::alarms::try_global(cx) {
                let store = store.read(cx);
                for occurrence in occurrences {
                    store.acknowledge(occurrence);
                }
            }
            return;
        }
        for tile in &mut self.tiles {
            tile.latch.ack();
        }
        cx.notify();
    }

    fn toggle_tile(&mut self, id: ComponentId, cx: &mut Context<Self>) {
        let Some(tile) = self.tiles.iter().find(|t| t.id == id) else {
            return;
        };
        if !tile.is_bool {
            return;
        }
        let next = !tile.latest_on.unwrap_or(false);
        crate::inspector::edits::upsert_element_value(
            &self.db,
            tile.id,
            tile.name.clone(),
            tile.element_names.clone(),
            0,
            ElementValue::Bool(next),
            cx,
        );
        cx.notify();
    }
}

impl Focusable for Annunciator {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Annunciator {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.compiled_pattern != self.pattern {
            self.recompile_and_reconcile(cx);
        }
        if self.bound_source != self.source {
            self.bound_source = self.source;
            self.reconcile_tiles(cx);
        }
        if self.bound_alarm_when != self.alarm_when {
            self.refold_polarity();
        }
        let theme = theme(cx);

        let mut visuals = match self.source {
            AnnunciatorSource::Components => self.component_visuals(cx),
            AnnunciatorSource::Alarms => self.alarm_visuals(cx),
        };
        mark_first_out(&mut visuals);

        let fallback = self.color;
        let mut body = div()
            .flex_1()
            .flex()
            .flex_col()
            .gap(px(3.0))
            .content_start();
        match self.columns {
            0 => {
                let tiles: Vec<Stateful<Div>> = visuals
                    .iter()
                    .map(|visual| render_tile(visual, fallback, &theme, cx))
                    .collect();
                body = body.child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(3.0))
                        .children(tiles),
                );
            }
            columns => {
                for row in visuals.chunks(columns) {
                    let tiles: Vec<Stateful<Div>> = row
                        .iter()
                        .map(|visual| render_tile(visual, fallback, &theme, cx).flex_1())
                        .collect();
                    body = body.child(div().flex().flex_row().gap(px(3.0)).children(tiles));
                }
            }
        }

        let ack_all = (self.latch || self.source == AnnunciatorSource::Alarms).then(|| {
            div().flex().flex_row().justify_end().child(
                div()
                    .id("annunciator-ack-all")
                    .px(px(8.0))
                    .py(px(2.0))
                    .rounded(px(3.0))
                    .bg(theme.bg_secondary)
                    .text_size(px(11.0))
                    .text_color(theme.text_secondary)
                    .cursor_pointer()
                    .child("Ack")
                    .on_click(cx.listener(|this, _, _window, cx| this.ack_all(cx))),
            )
        });

        div()
            .track_focus(&self.focus)
            .size_full()
            .p(px(8.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .children(ack_all)
            .child(body)
    }
}

/// One tile flattened to just what painting needs, so the element pass can
/// take `&mut Context` without still borrowing the tile list.
struct TileVisual {
    /// Stable per-tile element id: the component id, or a hash of the alarm id.
    key: u64,
    name: SharedString,
    label: Option<SharedString>,
    value: Option<SharedString>,
    click: Option<TileClick>,
    state: TileState,
    /// When the pending trip began, for the first-out rule.
    since: Option<Timestamp>,
    reported: bool,
    first_out: bool,
    severity: Option<Severity>,
    /// Component the shift-hover plot preview should show, if any.
    hover: Option<ComponentId>,
}

/// What a click on a tile does. The three differ in authority: a toggle writes a
/// component, a latch ack is local to this view, and an alarm ack is a wire event.
#[derive(Clone, Copy)]
enum TileClick {
    Toggle(ComponentId),
    AckLatch(ComponentId),
    AckAlarm(OccurrenceId),
}

/// Ring the oldest pending trip — the one that caused the rest.
fn mark_first_out(visuals: &mut [TileVisual]) {
    let Some(oldest) = visuals
        .iter()
        .filter(|visual| visual.state != TileState::Normal)
        .filter_map(|visual| visual.since)
        .min()
    else {
        return;
    };
    if let Some(visual) = visuals
        .iter_mut()
        .find(|visual| visual.state != TileState::Normal && visual.since == Some(oldest))
    {
        visual.first_out = true;
    }
}

/// Element id for an alarm-sourced tile. Alarm ids are strings, and gpui wants an
/// integer discriminator that stays the same across frames.
fn tile_key(def_id: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    def_id.hash(&mut hasher);
    hasher.finish()
}

/// Alpha of the acked fill when a tile has no declared alarm to take a
/// severity tint from. Matches `Theme::alarm_tint`'s weight so acked tiles
/// read the same whether or not the control system knows about them.
const FALLBACK_TINT_ALPHA: f32 = 0.10;

fn render_tile(
    visual: &TileVisual,
    fallback: Hsla,
    theme: &Theme,
    cx: &mut Context<Annunciator>,
) -> Stateful<Div> {
    let accent = match visual.severity {
        Some(severity) => theme.alarm_color(severity_index(severity)),
        None => fallback,
    };
    let tint = match visual.severity {
        Some(severity) => theme.alarm_tint(severity_index(severity)),
        None => Theme::dim(fallback, FALLBACK_TINT_ALPHA),
    };
    let (fill, border, text) = match visual.state {
        TileState::Normal if !visual.reported => {
            (theme.bg_secondary, theme.bg_secondary, theme.text_tertiary)
        }
        TileState::Normal => (theme.bg_secondary, theme.bg_secondary, theme.text_secondary),
        TileState::AlarmUnacked => (accent, accent, theme.text_primary),
        TileState::AlarmAcked => (tint, accent, theme.text_primary),
        TileState::ClearedUnacked => (theme.bg_secondary, accent, accent),
    };

    let has_text = visual.label.is_some() || visual.value.is_some() || visual.first_out;
    let name = visual.name.clone();
    let control_active = theme.control_active;

    div()
        .id(("annunciator-tile", visual.key as usize))
        .rounded(px(3.0))
        .min_w(px(14.0))
        .min_h(px(14.0))
        .bg(fill)
        .flex()
        .flex_col()
        .justify_center()
        .when(has_text, |tile| {
            tile.px(px(6.0)).py(px(3.0)).min_w(px(56.0))
        })
        .when(!visual.first_out, |tile| {
            tile.border_1().border_color(border)
        })
        .when(visual.first_out, |tile| {
            tile.border_2().border_color(control_active).child(
                div()
                    .text_size(px(8.0))
                    .text_color(control_active)
                    .child("1st"),
            )
        })
        .children(visual.label.clone().map(|label| {
            div()
                .overflow_hidden()
                .text_ellipsis()
                .text_size(px(11.0))
                .text_color(text)
                .child(label)
        }))
        .children(
            visual
                .value
                .clone()
                .map(|value| div().text_size(px(10.0)).text_color(text).child(value)),
        )
        .when_some(visual.click, |tile, click| {
            tile.cursor_pointer().on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _window, cx| match click {
                    TileClick::Toggle(id) => this.toggle_tile(id, cx),
                    TileClick::AckLatch(id) => this.ack(id, cx),
                    TileClick::AckAlarm(occurrence) => {
                        if let Some(store) = crate::alarms::try_global(cx) {
                            store.read(cx).acknowledge(occurrence);
                        }
                    }
                }),
            )
        })
        .tooltip(move |_window, cx| TooltipText::build(name.clone(), cx))
        .when_some(visual.hover, |tile, id| {
            tile.on_mouse_move(shift_hover_listener(id, SmallVec::new()))
        })
}

fn value_text(tile: &Tile) -> SharedString {
    match (tile.latest_value, tile.unit.is_empty()) {
        (None, _) => SharedString::new_static("—"),
        (Some(value), true) => SharedString::from(format_number(value)),
        (Some(value), false) => {
            SharedString::from(format!("{} {}", format_number(value), tile.unit))
        }
    }
}

/// The part of `name` the pattern's wildcards matched.
///
/// Every matched name shares the glob's literal prefix and suffix, so those
/// characters carry no information and only crowd the tile. A pattern with no
/// wildcard, or one that would strip the name to nothing, falls back to the
/// full name — the tooltip carries it either way.
fn strip_glob_literals(pattern: &str, name: &str) -> SharedString {
    let is_wildcard = |c: char| c == '*' || c == '?';
    let (Some(first), Some(last)) = (pattern.find(is_wildcard), pattern.rfind(is_wildcard)) else {
        return SharedString::from(name.to_string());
    };
    let stripped = name
        .strip_prefix(&pattern[..first])
        .and_then(|rest| rest.strip_suffix(&pattern[last + 1..]))
        .unwrap_or(name);
    if stripped.is_empty() {
        return SharedString::from(name.to_string());
    }
    SharedString::from(stripped.to_string())
}

fn compile_pattern(pattern: &str) -> Option<Regex> {
    if pattern.is_empty() {
        return None;
    }
    Some(crate::query::glob_to_regex(pattern))
}

/// Spawn the watcher task that rebuilds the tile list whenever the DB
/// vtable generation advances (i.e. a component is registered).
///
/// Modelled on `ComponentBrowserDelegate::spawn_watcher`.
fn spawn_watcher(db: Arc<DB>, cx: &mut Context<Annunciator>) -> gpui::Task<()> {
    cx.spawn(async move |this, cx| {
        loop {
            let result = this.update(cx, |grid, cx| grid.reconcile_tiles(cx));
            if result.is_err() {
                break;
            }
            db.vtable_gen.wait().await;
            // vtable_gen bumps once per registered component, so a startup
            // burst would run the O(components) regex reconcile N times.
            // Debounce so a burst collapses into one pass (reconcile reads
            // the latest full state regardless).
            cx.background_executor().timer(VTABLE_DEBOUNCE).await;
        }
    })
}

const VTABLE_DEBOUNCE: Duration = Duration::from_millis(50);

/// Single-question wizard row for "give me a glob pattern, build an
/// [`Annunciator`] from it". Empty input is a no-op so a stray Enter doesn't
/// create an empty annunciator. Used by both the dashboard "+ widget" menu
/// and the pane's "New Panel" menu.
pub fn glob_prompt_row(
    on_submit: Arc<dyn Fn(SharedString, &mut Window, &mut App)>,
) -> Box<dyn InspectorRow> {
    Box::new(DefaultActionRow::new(
        "Glob pattern (e.g. *.health)…",
        Arc::new(move |input, window, cx| {
            if input.is_empty() {
                return;
            }
            on_submit(SharedString::from(input), window, cx);
        }),
    ))
}

/// The config a freshly-added annunciator starts from.
///
/// Deliberately unlike [`AnnunciatorConfig::default`], which exists to keep a
/// pre-rename `{pattern, color}` blob rendering as it did: a new one wants the
/// labelled grid the widget is actually for.
pub fn seeded_config(pattern: &str) -> AnnunciatorConfig {
    AnnunciatorConfig {
        pattern: pattern.to_string(),
        show_labels: true,
        columns: 4,
        ..Default::default()
    }
}

fn build_tile(
    db: Arc<DB>,
    id: ComponentId,
    name: SharedString,
    label: SharedString,
    cx: &mut Context<Annunciator>,
) -> Tile {
    let meta = component_meta(&db, id);
    let task = spawn_on_stream(db, id, cx, move |grid, on, value, cx| {
        let inverted = grid.alarm_when == AlarmWhen::Off;
        let now = Timestamp::now();
        if let Some(tile) = grid.tiles.iter_mut().find(|t| t.id == id) {
            tile.latest_on = Some(on);
            tile.latest_value = value;
            tile.latch.condition(on ^ inverted, now);
            cx.notify();
        }
    });

    Tile {
        id,
        name,
        label,
        unit: meta.unit,
        is_bool: meta.is_bool,
        element_names: meta.element_names,
        latest_on: None,
        latest_value: None,
        latch: Latch::default(),
        _task: task,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trailing_literal_is_stripped() {
        assert_eq!(strip_glob_literals("*.healthy", "eps.healthy"), "eps");
        assert_eq!(strip_glob_literals("*.healthy", "adcs.healthy"), "adcs");
    }

    #[test]
    fn a_leading_literal_is_stripped() {
        assert_eq!(
            strip_glob_literals("gnc.*", "gnc.wheels.temp"),
            "wheels.temp"
        );
    }

    #[test]
    fn a_pattern_without_wildcards_keeps_the_full_name() {
        assert_eq!(
            strip_glob_literals("eps.healthy", "eps.healthy"),
            "eps.healthy"
        );
    }

    #[test]
    fn a_name_that_would_strip_to_nothing_keeps_the_full_name() {
        assert_eq!(strip_glob_literals("eps.*", "eps."), "eps.");
        assert_eq!(strip_glob_literals("*", ""), "");
    }
}
