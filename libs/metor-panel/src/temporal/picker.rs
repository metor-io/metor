//! Time pages use the same query/row actions in the palette and anchored inspector.
use super::{
    TemporalConfig, TimeAction, TimeExpr, TimeRangeSpec,
    model::{self, ParseContext},
};
use crate::inspector::rows::header::HeaderRow;
use crate::inspector::{
    InspectorMode, InspectorRequest,
    rows::{CommandRow, InspectorRow, NavRow, RowAction, render_label_row},
};
use gpui::{AnyElement, App, AppContext, Focusable, SharedString, Window};
use metor_proto::types::Timestamp;
use std::{cell::RefCell, rc::Rc, sync::Arc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    View,
    Range,
    Start,
    End,
    Zone,
    Format,
    T0,
    Scope,
    Clock,
    Rate,
    Step,
}
impl Target {
    fn label(self) -> &'static str {
        match self {
            Self::View => "Set view time",
            Self::Range => "Set visible range",
            Self::Start => "Start endpoint",
            Self::End => "End endpoint",
            Self::Zone => "Timezone",
            Self::Format => "Time display format",
            Self::T0 => "T0 reference",
            Self::Scope => "Data scope prefix",
            Self::Clock => "Clock source",
            Self::Rate => "Playback speed",
            Self::Step => "Step size",
        }
    }
    fn text(self, c: &TemporalConfig) -> String {
        match self {
            Self::View => c.view.to_string(),
            Self::Range => c.range.to_string(),
            Self::Start => c.range.start.to_string(),
            Self::End => c.range.end.to_string(),
            Self::Zone => c.timezone.clone(),
            Self::Format => c.display.label().into(),
            Self::T0 => {
                c.t0.map(|t| model::timestamp_text(Timestamp(t), "UTC"))
                    .unwrap_or_else(|| "data start".into())
            }
            Self::Scope => {
                if c.scope_prefix.is_empty() {
                    "all".into()
                } else {
                    c.scope_prefix.clone()
                }
            }
            Self::Clock => c
                .source_clock
                .map(|id| format!("id:{}", id.0))
                .unwrap_or(if c.wall_clock { "wall" } else { "session" }.into()),
            Self::Rate => format!("{}x", c.rate),
            Self::Step => model::format_duration(c.step_micros),
        }
    }

    fn edit_text(self, config: &TemporalConfig, context: &super::TimeContext) -> String {
        // UTC keeps absolute editor text unambiguous and parseable across DST folds.
        let mut display = config.clone();
        display.timezone = "UTC".into();
        let expr = |expr: TimeExpr| {
            if matches!(expr.anchor, super::Anchor::Timestamp(_)) {
                expr.resolve(context)
                    .map(|t| super::display::timestamp(t, &display, context))
                    .unwrap_or_else(|_| expr.to_string())
            } else {
                expr.to_string()
            }
        };
        match self {
            Self::View => expr(config.view),
            Self::Start => expr(config.range.start),
            Self::End => expr(config.range.end),
            Self::Range
                if matches!(config.range.start.anchor, super::Anchor::Timestamp(_))
                    || matches!(config.range.end.anchor, super::Anchor::Timestamp(_)) =>
            {
                format!("{} .. {}", expr(config.range.start), expr(config.range.end))
            }
            Self::T0 if config.t0.is_some() => {
                display.display = super::TimeDisplay::Timestamp;
                super::display::timestamp(Timestamp(config.t0.unwrap()), &display, context)
            }
            _ => self.text(config),
        }
    }
}

pub(crate) fn editor(target: Target, cx: &App) -> Vec<Box<dyn InspectorRow>> {
    let config = super::config(cx);
    let now = Timestamp::now();
    let context = ParseContext::new(&config.timezone, now, super::view_time(cx).unwrap_or(now))
        .unwrap_or_else(|_| ParseContext::utc());
    let base = Rc::new(RefCell::new(config.clone()));
    let provider = Provider {
        target,
        context,
        base,
        initial: super::snapshot(cx).map_or_else(
            || target.text(&config),
            |s| target.edit_text(&config, &s.context),
        ),
        initial_exact: target.text(&config),
        local: None,
        notice: None,
        accessory: Default::default(),
    };
    let mut rows: Vec<Box<dyn InspectorRow>> = vec![Box::new(provider.clone())];
    rows.extend(provider.candidates("", 0, cx));
    rows
}

/// Per-panel overrides use the same parser, completions, and explicit commit row.
pub(crate) fn local_range(current: &str, set: RangeSetter, cx: &App) -> Vec<Box<dyn InspectorRow>> {
    let mut config = super::config(cx);
    let now = Timestamp::now();
    let context = ParseContext::new(&config.timezone, now, super::view_time(cx).unwrap_or(now))
        .unwrap_or_else(|_| ParseContext::utc());
    config.range = current
        .parse::<crate::views::time_series::TimeRangeBehavior>()
        .map(TimeRangeSpec::from)
        .unwrap_or(config.range);
    let initial_exact = config.range.to_string();
    let initial = super::snapshot(cx).map_or_else(
        || initial_exact.clone(),
        |s| Target::Range.edit_text(&config, &s.context),
    );
    vec![Box::new(Provider {
        target: Target::Range,
        context,
        base: Rc::new(RefCell::new(config)),
        initial_exact,
        initial,
        local: Some(set),
        notice: None,
        accessory: Default::default(),
    })]
}

fn navigation(target: Target, cx: &App) -> Box<dyn InspectorRow> {
    let row = NavRow::new(
        target.label(),
        summary(target, cx),
        Box::new(move |cx| editor(target, cx)),
    );
    if target != Target::View {
        return Box::new(row);
    }
    let cache = RefCell::new(None::<gpui::Entity<crate::views::Timeline>>);
    Box::new(row.with_accessory(Box::new(move |cx| {
        let mut cache = cache.borrow_mut();
        let entity = if let Some(entity) = &*cache {
            entity.clone()
        } else {
            let db = super::controller(cx)?.read(cx).db.clone();
            let edit = Arc::new(|action: TimeAction, _: &mut Window, cx: &mut App| {
                // The overview edits in place, keeping the same menu and drag target.
                let _ = apply_action(action, cx);
            });
            let entity = cx.new(|cx| {
                crate::views::Timeline::preview(
                    db,
                    crate::views::timeline::EditTarget::Both,
                    edit,
                    cx,
                )
            });
            *cache = Some(entity.clone());
            entity
        };
        let weak = entity.downgrade();
        Some(crate::inspector::rows::AccessorySpec {
            view: entity.clone().into(),
            focus: entity.focus_handle(cx),
            dragging: Arc::new(move |cx| weak.upgrade().is_some_and(|e| e.read(cx).is_dragging())),
        })
    })))
}

fn summary(target: Target, cx: &App) -> String {
    let config = super::config(cx);
    let Some(snapshot) = super::snapshot(cx) else {
        return target.text(&config);
    };
    if target == Target::Range {
        return super::display::range(config.range, &config, &snapshot.context);
    }
    if target == Target::T0
        && let Some(t0) = config.t0
    {
        let mut absolute = config.clone();
        absolute.display = super::TimeDisplay::Timestamp;
        return super::display::timestamp(Timestamp(t0), &absolute, &snapshot.context);
    }
    let expr = match target {
        Target::View => Some(config.view),
        Target::Start => Some(config.range.start),
        Target::End => Some(config.range.end),
        _ => None,
    };
    if let Some(expr) = expr
        && matches!(expr.anchor, super::Anchor::Timestamp(_))
        && let Ok(t) = expr.resolve(&snapshot.context)
    {
        return super::display::timestamp(t, &config, &snapshot.context);
    }
    target.text(&config)
}

pub(crate) fn rows(cx: &App) -> Vec<Box<dyn InspectorRow>> {
    let mut rows = vec![navigation(Target::View, cx), navigation(Target::Range, cx)];
    for (label, action) in transport_actions() {
        rows.push(action_row(label, action));
    }
    rows.push(Box::new(NavRow::new(
        "More time settings",
        "",
        Box::new(|cx| {
            [
                Target::Start,
                Target::End,
                Target::Zone,
                Target::Format,
                Target::T0,
                Target::Scope,
                Target::Clock,
                Target::Rate,
                Target::Step,
            ]
            .into_iter()
            .map(|t| navigation(t, cx))
            .collect()
        }),
    )));
    rows
}

fn transport_actions() -> Vec<(&'static str, TimeAction)> {
    vec![
        ("Sync all plots to global range", TimeAction::SyncPlots),
        ("Pause", TimeAction::Pause),
        ("Play", TimeAction::Play { from_start: false }),
        (
            "Play from range start",
            TimeAction::Play { from_start: true },
        ),
        ("Go live", TimeAction::Live),
        ("Step forward", TimeAction::Step(1)),
        ("Step backward", TimeAction::Step(-1)),
        ("Pin both range endpoints", TimeAction::Pin),
    ]
}

pub(crate) fn plot_actions(x: (f64, f64)) -> Vec<Box<dyn InspectorRow>> {
    vec![
        action_row(
            "Use this zoom as global time range",
            TimeAction::Range(TimeRangeSpec::fixed(
                Timestamp(x.0 as i64)..Timestamp(x.1 as i64),
            )),
        ),
        action_row("Sync all plots to global range", TimeAction::SyncPlots),
    ]
}

pub(crate) fn open_plot_actions(
    x: (f64, f64),
    position: gpui::Point<gpui::Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    let mut rows = plot_actions(x);
    rows.push(Box::new(NavRow::new(
        "Time controls",
        "",
        Box::new(self::rows),
    )));
    if let Some(open) = crate::inspector::open_inspector(cx) {
        open(
            InspectorRequest {
                rows,
                mode: InspectorMode::Anchored(position),
            },
            window,
            cx,
        );
    }
    cx.stop_propagation();
}

fn action_row(label: &str, action: TimeAction) -> Box<dyn InspectorRow> {
    Box::new(CommandRow::action(
        SharedString::from(label.to_string()),
        Arc::new(move |_, cx| match super::dispatch(action.clone(), cx) {
            Ok(()) => RowAction::Dismiss,
            Err(error) => RowAction::Cascade(vec![Box::new(HeaderRow::new(error))]),
        }),
    ))
}

pub(crate) fn open(target: Option<Target>, mode: InspectorMode, window: &mut Window, cx: &mut App) {
    if let Some(open) = crate::inspector::open_inspector(cx) {
        open(
            InspectorRequest {
                rows: target.map_or_else(|| rows(cx), |t| editor(t, cx)),
                mode,
            },
            window,
            cx,
        );
    }
}

pub(crate) fn register(cx: &mut App) {
    use crate::inspector::palette::{Category, InspectionItem, ItemRegistry};
    ItemRegistry::register(
        cx,
        Category::Custom("Time".into()),
        Arc::new(|cx| {
            let mut items = vec![InspectionItem::SubMenu {
                label: "Time controls".into(),
                summary: "".into(),
                build: Arc::new(rows),
            }];
            for target in [
                Target::View,
                Target::Range,
                Target::Format,
                Target::T0,
                Target::Zone,
                Target::Rate,
            ] {
                items.push(InspectionItem::SubMenu {
                    label: format!("Time: {}…", target.label()).into(),
                    summary: summary(target, cx).into(),
                    build: Arc::new(move |cx| editor(target, cx)),
                });
            }
            for (label, action) in transport_actions() {
                items.push(InspectionItem::Command {
                    label: format!("Time: {label}").into(),
                    callback: Arc::new(move |window, cx| {
                        if super::dispatch(action.clone(), cx).is_err() {
                            open(None, InspectorMode::Centered, window, cx);
                        }
                    }),
                });
            }
            items
        }),
    );
}

type RangeSetter = Arc<dyn Fn(crate::views::time_series::TimeRangeBehavior, &mut Window, &mut App)>;
#[derive(Clone)]
struct Provider {
    target: Target,
    context: ParseContext,
    base: Rc<RefCell<TemporalConfig>>,
    initial: String,
    initial_exact: String,
    local: Option<RangeSetter>,
    notice: Option<String>,
    accessory: Rc<RefCell<Option<(gpui::Entity<crate::views::Timeline>, String, u64)>>>,
}
/// Avoid restarting playback or re-resolving an unchanged expression on finish.
fn apply_action(action: TimeAction, cx: &mut App) -> Result<(), String> {
    let current = super::config(cx);
    let unchanged = match &action {
        TimeAction::Range(range) => *range == current.range,
        TimeAction::Seek(time) => {
            *time == current.view && !super::snapshot(cx).is_some_and(|s| s.playing)
        }
        _ => false,
    };
    if unchanged {
        Ok(())
    } else {
        super::dispatch(action, cx)
    }
}

impl Provider {
    fn live(&self) -> bool {
        self.local.is_none()
            && matches!(
                self.target,
                Target::View | Target::Range | Target::Start | Target::End
            )
    }
    fn apply_edit(&self, query: &str, cx: &mut App) {
        if !self.live() || self.conflict(&super::config(cx)) {
            return;
        }
        if let Ok((action, _)) = self.action(query, cx) {
            if apply_action(action, cx).is_ok() {
                // Our own live changes must not look like an external edit on Enter.
                *self.base.borrow_mut() = super::config(cx);
            }
        }
    }

    fn action(&self, query: &str, cx: &App) -> Result<(TimeAction, String), String> {
        // Reopening or accepting an untouched compact field retains stored microseconds.
        let query = if query == self.initial {
            &self.initial_exact
        } else {
            query
        };
        let snapshot = super::snapshot(cx).ok_or("Time controller unavailable")?;
        let config = super::config(cx);
        let expanded = if matches!(
            self.target,
            Target::View | Target::Range | Target::Start | Target::End
        ) {
            super::display::expand_input(query, &config, &snapshot.context)?
        } else {
            query.to_string()
        };
        let query = expanded.as_str();
        let (action, preview) = match self.target {
            Target::View => {
                let expr = model::parse_instant(query, &self.context, false)?;
                let t = expr.resolve(&snapshot.context)?;
                (
                    TimeAction::Seek(expr),
                    format!(
                        "{} · {}",
                        super::display::label(t, cx),
                        if expr == TimeExpr::LIVE {
                            "Live"
                        } else {
                            "pauses view time"
                        }
                    ),
                )
            }
            Target::Range | Target::Start | Target::End => {
                let range = match self.target {
                    Target::Start => TimeRangeSpec {
                        start: model::parse_instant(query, &self.context, true)?,
                        end: config.range.end,
                    },
                    Target::End => TimeRangeSpec {
                        start: config.range.start,
                        end: model::parse_instant(query, &self.context, true)?,
                    },
                    _ => model::parse_range(query, &self.context)?,
                };
                let r = range.resolve(&snapshot.context)?;
                (
                    TimeAction::Range(range),
                    format!(
                        "{} → {}",
                        super::display::label(r.start, cx),
                        super::display::label(r.end, cx)
                    ),
                )
            }
            Target::Format => {
                let display = match query.trim().to_ascii_lowercase().as_str() {
                    "timestamp" | "absolute" => super::TimeDisplay::Timestamp,
                    "elapsed / t0" | "elapsed" | "offset" | "t0" => super::TimeDisplay::Elapsed,
                    _ => return Err("Choose timestamp or elapsed / T0".into()),
                };
                let mut preview_config = config.clone();
                preview_config.display = display;
                let preview = snapshot
                    .context
                    .view
                    .map(|t| super::display::timestamp(t, &preview_config, &snapshot.context))
                    .unwrap_or_else(|| "No selected time".into());
                (TimeAction::Display(display), preview)
            }
            Target::T0 => {
                let t0 = if query.trim().eq_ignore_ascii_case("data start") {
                    None
                } else {
                    Some(
                        model::parse_instant(query, &self.context, true)?
                            .resolve(&snapshot.context)?
                            .0,
                    )
                };
                (
                    TimeAction::T0(t0),
                    if t0.is_none() {
                        "Zero follows data start"
                    } else {
                        "Fixed zero; selected time stays unchanged"
                    }
                    .into(),
                )
            }
            Target::Zone => {
                ParseContext::new(query.trim(), Timestamp::now(), Timestamp::now())?;
                (
                    TimeAction::Timezone(query.trim().into()),
                    "Display timezone; committed instants remain unchanged".into(),
                )
            }
            Target::Scope => (
                TimeAction::Scope(if query.trim() == "all" {
                    String::new()
                } else {
                    query.trim().into()
                }),
                "Component-name prefix; all selects the complete session".into(),
            ),
            Target::Clock => {
                let id = super::controller(cx)
                    .ok_or("Time controller unavailable")?
                    .read(cx)
                    .clock_source(if query.trim() == "wall" {
                        "session"
                    } else {
                        query.trim()
                    })?;
                (
                    if query.trim() == "wall" {
                        TimeAction::WallClock
                    } else {
                        TimeAction::Clock(id)
                    },
                    "Live follows telemetry timestamps; wall selects the computer clock".into(),
                )
            }
            Target::Rate => {
                let rate: f64 = query
                    .trim()
                    .trim_end_matches(['x', '×'])
                    .parse()
                    .map_err(|_| "Enter a speed such as 2x")?;
                if !rate.is_finite() || rate <= 0.0 || rate > 100.0 {
                    return Err("Rate must be greater than zero and at most 100x".into());
                }
                (TimeAction::Rate(rate), format!("{rate}× recorded time"))
            }
            Target::Step => {
                let step = model::duration(query)?;
                if step <= 0 {
                    return Err("Step must be positive".into());
                }
                (TimeAction::StepSize(step), model::format_duration(step))
            }
        };
        Ok((action, preview))
    }
    fn conflict(&self, current: &TemporalConfig) -> bool {
        let old = self.base.borrow();
        let dependencies = old.scope_prefix != current.scope_prefix
            || old.source_clock != current.source_clock
            || old.wall_clock != current.wall_clock
            || old.timezone != current.timezone
            || old.t0 != current.t0;
        let field = match self.target {
            Target::Start | Target::End | Target::Range => old.range != current.range,
            Target::View => old.view != current.view,
            _ => self.target.text(&old) != self.target.text(current),
        };
        dependencies || field
    }
    fn commit(&self, query: &str, window: &mut Window, cx: &mut App) -> RowAction {
        if let Some(set) = &self.local {
            return match self.action(query, cx) {
                Ok((TimeAction::Range(range), _)) => {
                    set(range.into(), window, cx);
                    RowAction::Dismiss
                }
                _ => RowAction::Handled,
            };
        }
        let current = super::config(cx);
        if self.conflict(&current) {
            *self.base.borrow_mut() = current;
            let mut refreshed = self.clone();
            refreshed.notice =
                Some("Time settings changed. Review the new preview before applying.".into());
            refreshed.context = ParseContext::new(
                &super::config(cx).timezone,
                Timestamp::now(),
                super::view_time(cx).unwrap_or_else(Timestamp::now),
            )
            .unwrap_or_else(|_| ParseContext::utc());
            return RowAction::CascadeWith {
                rows: vec![
                    Box::new(HeaderRow::new(
                        "Time settings changed. Review the new preview before applying.",
                    )),
                    Box::new(refreshed),
                ],
                query: query.into(),
            };
        }
        match self
            .action(query, cx)
            .and_then(|(action, _)| apply_action(action, cx))
        {
            Ok(()) => RowAction::Dismiss,
            Err(_) => RowAction::Handled,
        }
    }
    fn candidates(&self, query: &str, cursor: usize, cx: &App) -> Vec<Box<dyn InspectorRow>> {
        let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();
        if let Some(notice) = &self.notice {
            rows.push(Box::new(HeaderRow::new(notice.clone())));
        }
        if !query.trim().is_empty() {
            match self.action(query, cx) {
                Ok((action, preview)) => {
                    let config = super::config(cx);
                    let label = match (&action, super::snapshot(cx)) {
                        (TimeAction::Range(range), Some(s)) => {
                            super::display::range(*range, &config, &s.context)
                        }
                        (TimeAction::Seek(expr), Some(s))
                            if matches!(expr.anchor, super::Anchor::Timestamp(_)) =>
                        {
                            expr.resolve(&s.context)
                                .map(|t| super::display::label(t, cx))
                                .unwrap_or_else(|_| query.into())
                        }
                        (TimeAction::T0(Some(t)), Some(s)) => {
                            let absolute = TemporalConfig {
                                display: super::TimeDisplay::Timestamp,
                                ..config
                            };
                            super::display::timestamp(Timestamp(*t), &absolute, &s.context)
                        }
                        _ => query.into(),
                    };
                    rows.push(Box::new(TimeCandidate {
                        label: format!("{}: {}", self.target.label(), label).into(),
                        text: query.into(),
                        cursor: query.len(),
                        commit: Some(self.clone()),
                    }));
                    rows.push(Box::new(HeaderRow::new(preview)));
                    if let Some(extent) = super::snapshot(cx).and_then(|s| s.context.extent) {
                        rows.push(Box::new(HeaderRow::new(format!(
                            "Coverage: {} → {}",
                            super::display::label(extent.start, cx),
                            super::display::label(extent.end, cx)
                        ))));
                    }
                }
                Err(error) => rows.push(Box::new(HeaderRow::new(error))),
            }
        }
        if self.target == Target::Clock
            && let Some(controller) = super::controller(cx)
        {
            for (id, name) in controller.read(cx).source_names(query.trim()) {
                let text = format!("id:{}", id.0);
                rows.push(Box::new(TimeCandidate {
                    label: name.into(),
                    cursor: text.len(),
                    text,
                    commit: None,
                }));
            }
        }
        for candidate in complete(query, cursor, self.target, &self.context) {
            rows.push(Box::new(TimeCandidate {
                label: candidate.0.into(),
                commit: self.action(&candidate.1, cx).ok().map(|_| self.clone()),
                text: candidate.1,
                cursor: candidate.2,
            }));
        }
        rows
    }
}

impl InspectorRow for Provider {
    fn supports_exit_fade(&self) -> bool {
        true
    }

    fn query_edited(&self, query: &str, cx: &mut App) {
        self.apply_edit(query, cx);
    }

    fn accessory(
        &self,
        query: &str,
        cx: &mut App,
    ) -> Option<crate::inspector::rows::AccessorySpec> {
        use crate::views::timeline::{EditTarget, Timeline};
        let target = match self.target {
            Target::View => EditTarget::View,
            Target::Range => EditTarget::Range,
            Target::Start => EditTarget::Start,
            Target::End => EditTarget::End,
            _ => return None,
        };
        let mut cached = self.accessory.borrow_mut();
        if cached.is_none() {
            let db = super::controller(cx)?.read(cx).db.clone();
            let field = self.target;
            let callback = Arc::new(
                move |action: TimeAction, window: &mut Window, cx: &mut App| {
                    let mut config = super::config(cx);
                    match (field, action) {
                        (Target::View, TimeAction::Seek(time)) => config.view = time,
                        (Target::Range | Target::Start | Target::End, TimeAction::Range(range)) => {
                            config.range = range
                        }
                        _ => return,
                    };
                    let text = super::snapshot(cx).map_or_else(
                        || field.text(&config),
                        |s| field.edit_text(&config, &s.context),
                    );
                    window.dispatch_action(
                        Box::new(crate::inspector::EditInspectorQuery { text }),
                        cx,
                    );
                },
            );
            let entity = cx.new(|cx| Timeline::preview(db, target, callback, cx));
            *cached = Some((entity, String::new(), u64::MAX));
        }
        let (entity, old_query, revision) = cached.as_mut()?;
        let current_revision = self.query_revision(cx);
        if query != old_query || *revision != current_revision {
            let value = self.action(query, cx).map(|(action, _)| action);
            entity.update(cx, |timeline, cx| timeline.set_preview(value, cx));
            *old_query = query.into();
            *revision = current_revision;
        }
        let weak = entity.downgrade();
        Some(crate::inspector::rows::AccessorySpec {
            view: entity.clone().into(),
            focus: entity.focus_handle(cx),
            dragging: Arc::new(move |cx| weak.upgrade().is_some_and(|e| e.read(cx).is_dragging())),
        })
    }
    fn query_placeholder(&self) -> Option<&str> {
        Some(match self.target {
            Target::Range => "Range: last 5m, first 5m, or YYYY-MM-DD",
            Target::View | Target::Start | Target::End => {
                "Time: live, T+5m, or YYYY-MM-DD HH:MM:SS"
            }
            _ => self.target.label(),
        })
    }
    fn label(&self) -> &str {
        if self.live() {
            "Time updates as you edit; Tab completes, Enter finishes"
        } else {
            "Type a time expression; Tab completes, Enter applies"
        }
    }
    fn render_row(&self, _: usize, _: bool, window: &mut Window, cx: &mut App) -> AnyElement {
        HeaderRow::new(self.label().to_string()).render_row(0, false, window, cx)
    }
    fn activate(&mut self, _: &mut Window, _: &mut App) -> RowAction {
        RowAction::Handled
    }
    fn is_header(&self) -> bool {
        true
    }
    fn initial_query(&self) -> Option<String> {
        Some(self.initial.clone())
    }
    fn query_revision(&self, cx: &App) -> u64 {
        cx.try_global::<super::TemporalRevision>()
            .map_or(0, |r| r.0)
    }
    fn query_rows(
        &self,
        query: &str,
        cursor: usize,
        cx: &mut App,
    ) -> Option<Vec<Box<dyn InspectorRow>>> {
        Some(self.candidates(query, cursor, cx))
    }
}

struct TimeCandidate {
    label: SharedString,
    text: String,
    cursor: usize,
    commit: Option<Provider>,
}
impl InspectorRow for TimeCandidate {
    fn supports_exit_fade(&self) -> bool {
        true
    }

    fn label(&self) -> &str {
        &self.label
    }
    fn render_row(
        &self,
        i: usize,
        selected: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        render_label_row(
            i,
            selected,
            self.label.clone(),
            Some(
                if self.commit.is_some() {
                    "Enter"
                } else {
                    "Tab"
                }
                .into(),
            ),
            crate::theme::theme(cx).text_primary,
            window,
            cx,
        )
    }
    fn activate(&mut self, window: &mut Window, cx: &mut App) -> RowAction {
        if let Some(provider) = &self.commit {
            provider.commit(&self.text, window, cx)
        } else {
            self.insert("", window, cx)
        }
    }
    fn insert(&mut self, _: &str, _: &mut Window, _: &mut App) -> RowAction {
        RowAction::ReplaceQuery {
            text: self.text.clone(),
            cursor: self.cursor,
        }
    }
}

/// Replacement candidates preserve text after the cursor and never commit.
fn complete(
    query: &str,
    cursor: usize,
    target: Target,
    context: &ParseContext,
) -> Vec<(String, String, usize)> {
    let mut cursor = cursor.min(query.len());
    while !query.is_char_boundary(cursor) {
        cursor -= 1;
    }
    let before = &query[..cursor];
    let presets: &[&str] = match target {
        Target::Range => &[
            "full range",
            "last 5m",
            "last 15m",
            "last 1h",
            "first 5m",
            "5m ending at view time",
        ],
        Target::Zone => &[
            "UTC",
            "Local",
            "America/Los_Angeles",
            "America/New_York",
            "Europe/London",
            "Europe/Paris",
            "Asia/Tokyo",
        ],
        Target::Format => &["timestamp", "elapsed / T0"],
        Target::T0 => &["data start", "view time"],
        Target::Clock => &["session", "wall"],
        Target::Scope => &["all"],
        Target::Rate => &["0.1x", "0.25x", "0.5x", "1x", "2x", "5x", "10x"],
        Target::Step => &["1ms", "100ms", "1s", "5s", "1m"],
        _ => &["live", "data start", "data end", "T0", "T+5m", "T-30s"],
    };
    let mut candidates = Vec::new();
    let mut add = |label: String, start: usize, end: usize, insert: String| {
        if start > end || !query.is_char_boundary(start) || !query.is_char_boundary(end) {
            return;
        }
        let text = format!("{}{}{}", &query[..start], insert, &query[end..]);
        let caret = start + insert.len();
        if text != query && !candidates.iter().any(|(_, existing, _)| existing == &text) {
            candidates.push((label, text, caret));
        }
    };
    for preset in presets {
        if preset.starts_with(&before.to_ascii_lowercase())
            || preset
                .to_ascii_lowercase()
                .starts_with(&before.to_ascii_lowercase())
        {
            add(preset.to_string(), 0, cursor, preset.to_string());
        }
    }
    if matches!(
        target,
        Target::Range | Target::View | Target::Start | Target::End | Target::T0
    ) {
        for (label, date) in [
            (
                if target == Target::Range {
                    "Entire day"
                } else {
                    "Midnight"
                },
                context.view_date,
            ),
            ("Today", context.today),
        ] {
            let text = if target == Target::Range {
                date.to_string()
            } else {
                format!("{date} 00:00:00")
            };
            if before.is_empty()
                || text.starts_with(before)
                || label
                    .to_ascii_lowercase()
                    .starts_with(&before.to_ascii_lowercase())
            {
                add(format!("{label} · {date}"), 0, cursor, text);
            }
        }
    }
    let token_start = before.rfind(char::is_whitespace).map_or(0, |i| i + 1);
    let token_end = query[cursor..]
        .find(char::is_whitespace)
        .map_or(query.len(), |i| cursor + i);
    let token = &query[token_start..cursor];
    if !token.is_empty()
        && token.chars().all(|c| c.is_ascii_digit() || c == '.')
        && !token.contains("..")
    {
        for unit in ["m", "s", "h", "ms", "us", "d"] {
            let text = format!("{token}{unit}");
            add(text.clone(), token_start, token_end, text);
        }
    }
    let endpoint_start = before.rfind("..").map(|i| i + 2).unwrap_or(0);
    let endpoint = before[endpoint_start..].trim_start();
    let offset = cursor - endpoint.len();
    if target == Target::Range || matches!(target, Target::View | Target::Start | Target::End) {
        for anchor in [
            "data start",
            "data end",
            "live",
            "view time",
            "T0",
            "T+5m",
            "T-30s",
        ] {
            if target == Target::View && anchor == "view time" {
                continue;
            }
            if !(target == Target::Range && query.is_empty())
                && anchor
                    .to_ascii_lowercase()
                    .starts_with(&endpoint.to_ascii_lowercase())
            {
                add(anchor.into(), offset, cursor, anchor.into());
            }
        }
        if before.ends_with("+ ") || before.ends_with("- ") {
            for d in ["30s", "1m", "5m"] {
                add(d.into(), cursor, cursor, d.into());
            }
        }
        for zone in [
            "UTC",
            "America/Los_Angeles",
            "Europe/London",
            "+00:00",
            "-07:00",
            "-08:00",
        ] {
            if before.contains(':')
                && (token.is_empty()
                    || zone
                        .to_ascii_lowercase()
                        .starts_with(&token.to_ascii_lowercase()))
            {
                add(zone.into(), token_start, token_end, zone.into());
            }
        }
        let date_prefix = token;
        if date_prefix.len() == 5
            && date_prefix.ends_with('-')
            && date_prefix[..4].parse::<i32>().is_ok()
        {
            for month in 1..=12 {
                let date = format!("{date_prefix}{month:02}-");
                add(date.clone(), token_start, token_end, date);
            }
        }
        if date_prefix.len() == 8 && date_prefix.ends_with('-') {
            for day in 1..=31 {
                let date = format!("{date_prefix}{day:02}");
                if let Ok(d) = date.parse::<jiff::civil::Date>() {
                    add(
                        format!("{date} · {:?}", d.weekday()),
                        token_start,
                        token_end,
                        date,
                    );
                }
            }
        }
        if before.ends_with("today ")
            || before.ends_with("yesterday ")
            || token.parse::<jiff::civil::Date>().is_ok()
        {
            let insertion = if before.ends_with(' ') {
                "00:00:00"
            } else {
                " 00:00:00"
            };
            add(
                "Start of day · 00:00:00".into(),
                cursor,
                cursor,
                insertion.into(),
            );
        }
        // Complete one civil-time segment without touching a following zone.
        if token.len() >= 3 && token.as_bytes().get(2) == Some(&b':') && !token.contains('.') {
            let parts: Vec<_> = token.split(':').collect();
            if parts.len() <= 3 && parts[0].parse::<u8>().is_ok_and(|h| h < 24) {
                let prefix = &token[..token.rfind(':').unwrap() + 1];
                let partial = parts.last().copied().unwrap_or("");
                if partial.len() < 2 {
                    for n in 0..60 {
                        let digits = format!("{n:02}");
                        if digits.starts_with(partial) {
                            let time = format!(
                                "{prefix}{digits}{}",
                                if parts.len() == 2 { ":00" } else { "" }
                            );
                            add(time.clone(), token_start, token_end, time);
                        }
                    }
                }
            }
        }
        if before.is_empty() {
            let date = format!("{} 00:00:00", context.view_date);
            add(date.clone(), 0, 0, date);
        }
    }
    candidates.truncate(40);
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn compact_editor_labels_preserve_untouched_microseconds(cx: &mut gpui::TestAppContext) {
        let temp = tempfile::tempdir().unwrap();
        let db = Arc::new(metor_db::DB::create(temp.path().join("db")).unwrap());
        cx.update(|cx| {
            super::super::TemporalController::init(db, cx);
            for action in [
                TimeAction::Seek(TimeExpr::fixed(Timestamp(1_123_456))),
                TimeAction::Range(TimeRangeSpec::fixed(
                    Timestamp(1_123_456)..Timestamp(61_550_789),
                )),
                TimeAction::T0(Some(1_123_456)),
            ] {
                super::super::dispatch(action, cx).unwrap();
            }
            for display in [
                super::super::TimeDisplay::Timestamp,
                super::super::TimeDisplay::Elapsed,
            ] {
                super::super::dispatch(TimeAction::Display(display), cx).unwrap();
                let original = super::super::config(cx);
                for target in [
                    Target::View,
                    Target::Range,
                    Target::Start,
                    Target::End,
                    Target::T0,
                ] {
                    let rows = editor(target, cx);
                    let query = rows[0].initial_query().unwrap();
                    assert!(!query.contains("+00:00"), "{query}");
                    assert!(!query.contains(".123456"), "{query}");
                    assert!(!query.contains(".550789"), "{query}");
                    let candidates = rows[0].query_rows(&query, query.len(), cx).unwrap();
                    let label = candidates[0].label();
                    assert!(!label.contains("+00:00"), "{label}");
                    assert!(!label.contains(".123456"), "{label}");
                    rows[0].query_edited(&query, cx);
                    assert_eq!(super::super::config(cx), original);
                }
            }
        });
    }

    #[test]
    fn completions_preserve_suffixes_unicode_and_subsecond_precision() {
        let context = ParseContext::utc();
        let query = "data start + 2 .. data end";
        let cursor = query.find("2 ").unwrap() + 1;
        let completion = complete(query, cursor, Target::Range, &context)
            .into_iter()
            .find(|c| c.0 == "2m")
            .unwrap();
        assert_eq!(completion.1, "data start + 2m .. data end");
        assert_eq!(&completion.1[..completion.2], "data start + 2m");
        let query = "2026-09-05 14:32:10.123457 U";
        let completion = complete(query, query.len(), Target::View, &context)
            .into_iter()
            .find(|c| c.0 == "UTC")
            .unwrap();
        assert!(completion.1.contains(".123457 UTC"));
        assert!(model::parse_instant(&completion.1, &context, false).is_ok());
        let unicode = "data start ↔ live";
        for cursor in 0..=unicode.len() {
            let _ = complete(unicode, cursor, Target::Range, &context);
        }
        assert!(
            complete("2026-02-", 8, Target::View, &context)
                .iter()
                .all(|c| !c.1.ends_with("-29") && !c.1.ends_with("-30") && !c.1.ends_with("-31"))
        );
        assert!(
            complete("14:3 UTC", 4, Target::View, &context)
                .iter()
                .any(|c| c.1 == "14:30:00 UTC")
        );
    }

    #[gpui::test]
    fn live_endpoints_preserve_other_anchor_and_invalid_edits_keep_last_value(
        cx: &mut gpui::TestAppContext,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let db = Arc::new(metor_db::DB::create(temp.path().join("db")).unwrap());
        cx.update(|cx| {
            super::super::TemporalController::init(db, cx);
            super::super::dispatch(
                TimeAction::Range(TimeRangeSpec {
                    start: TimeExpr::fixed(Timestamp(0)),
                    end: TimeExpr::LIVE,
                }),
                cx,
            )
            .unwrap();
            let rows = editor(Target::Start, cx);
            rows[0].query_edited("1970-01-01 00:00:10 UTC", cx);
            let valid = super::super::config(cx);
            assert_eq!(valid.range.start, TimeExpr::fixed(Timestamp(10_000_000)));
            assert_eq!(valid.range.end, TimeExpr::LIVE);
            rows[0].query_edited("1970-", cx);
            assert_eq!(super::super::config(cx), valid);
            // Completion/clock refreshes alone never publish a new value.
            rows[0].query_rows("1970-01-01 00:00:20 UTC", 23, cx);
            assert_eq!(super::super::config(cx), valid);
            let rows = editor(Target::End, cx);
            rows[0].query_edited("1970-01-01 00:00:05 UTC", cx);
            assert_eq!(
                super::super::config(cx),
                valid,
                "reversed ranges are not applied"
            );
            rows[0].query_edited("1970-01-01 00:00:30 UTC", cx);
            assert_eq!(super::super::config(cx).range.start, valid.range.start);
            assert_eq!(
                super::super::config(cx).range.end,
                TimeExpr::fixed(Timestamp(30_000_000))
            );
        });
    }

    #[gpui::test]
    fn completion_inserts_commit_applies_and_same_field_conflict_requires_review(
        cx: &mut gpui::TestAppContext,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let db = Arc::new(metor_db::DB::create(temp.path().join("db")).unwrap());
        let cx = cx.add_empty_window();
        cx.update(|window, cx| {
            super::super::TemporalController::init(db, cx);
            let rows = editor(Target::Range, cx);
            let provider = &rows[0];
            let mut candidates = provider.query_rows("last 2.5m", 9, cx).unwrap();
            let before = super::super::config(cx);
            assert!(matches!(
                candidates[0].insert("", window, cx),
                RowAction::ReplaceQuery { .. }
            ));
            assert_eq!(super::super::config(cx), before);
            assert!(matches!(
                candidates[0].activate(window, cx),
                RowAction::Dismiss
            ));
            assert_eq!(super::super::config(cx).range.end, TimeExpr::LIVE);

            let rows = editor(Target::View, cx);
            let mut candidate = rows[0]
                .query_rows("2026-09-05 14:00:00 UTC", 23, cx)
                .unwrap()
                .remove(0);
            super::super::dispatch(TimeAction::Rate(2.0), cx).unwrap();
            assert!(matches!(candidate.activate(window, cx), RowAction::Dismiss));
            assert_eq!(super::super::config(cx).rate, 2.0);

            let rows = editor(Target::View, cx);
            let mut candidate = rows[0]
                .query_rows("2026-09-05 15:00:00 UTC", 23, cx)
                .unwrap()
                .remove(0);
            super::super::dispatch(TimeAction::Live, cx).unwrap();
            assert!(matches!(
                candidate.activate(window, cx),
                RowAction::CascadeWith { .. }
            ));
            assert!(super::super::is_live(cx));
        });
    }
}
