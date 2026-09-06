use super::*;
use crate::plot_events::flags::{ClusterPaint, paint_event_flags};
use crate::views::plot_common::paint_text_label;
use crate::views::table::{CELL_FONT_SIZE, CELL_PAD_X, HEADER_HEIGHT, ROW_HEIGHT};
use gpui::{BorderStyle, fill, point, quad, size};

/// Painted geometry is also the hit target, including the entire truncated chip.
pub(super) struct EventHit {
    bounds: Bounds<Pixels>,
    lane: usize,
    events: std::ops::Range<usize>,
    event: Arc<TimelineEvent>,
}

impl Timeline {
    pub(super) fn range_handle_height(height: f32) -> f32 {
        if height < 88.0 {
            (height - 1.0).max(10.0)
        } else {
            12.0 + HEADER_HEIGHT
        }
    }

    pub(super) fn visible_lanes(&self) -> usize {
        self.lanes
            .iter()
            .filter(|l| !self.config.collapsed.contains(&l.key))
            .count()
    }
    pub(super) fn lane_geometry(&self, height: f32) -> (f32, f32, usize) {
        let top = 12.0 + HEADER_HEIGHT;
        let available = (height - top - 14.0).max(ROW_HEIGHT);
        (
            top,
            (available / self.visible_lanes().max(1) as f32).clamp(ROW_HEIGHT, 52.0),
            (available / ROW_HEIGHT) as usize,
        )
    }
    pub(super) fn time_area(&self) -> Option<Bounds<Pixels>> {
        self.area.map(|mut area| {
            let gutter = Self::gutter(area);
            area.origin.x += px(gutter);
            area.size.width -= px(gutter);
            area
        })
    }
    fn gutter(area: Bounds<Pixels>) -> f32 {
        if f32::from(area.size.height) < 88.0 {
            0.0
        } else {
            (f32::from(area.size.width) * 0.25).min(140.0)
        }
    }
    fn hit_at(&self, position: Point<Pixels>) -> Option<&EventHit> {
        self.event_hits
            .iter()
            .rev()
            .find(|hit| hit.bounds.contains(&position))
    }
    pub(super) fn event_at(&self, position: Point<Pixels>, _: &App) -> Option<Arc<TimelineEvent>> {
        self.hit_at(position).map(|hit| hit.event.clone())
    }
    pub(super) fn hit_events(
        &self,
        position: Point<Pixels>,
        limit: usize,
    ) -> Option<(Vec<Arc<TimelineEvent>>, usize)> {
        let hit = self.hit_at(position)?;
        let lane = self.lanes.get(hit.lane)?;
        let events = lane.index.events.get(hit.events.clone())?;
        // Keep the representative first: it carries the most severe event in a burst.
        let mut result = vec![hit.event.clone()];
        result.extend(
            events
                .iter()
                .filter(|e| e.id != hit.event.id)
                .take(limit.saturating_sub(1))
                .cloned(),
        );
        Some((result, events.len()))
    }

    pub(super) fn paint(&mut self, area: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        self.event_hits.clear();
        let theme = crate::theme::theme(cx);
        let w = f32::from(area.size.width).max(1.0);
        let h = f32::from(area.size.height);
        let gutter = Self::gutter(area);
        let time_w = (w - gutter).max(1.0);
        let rect = |x: f32, y: f32, width: f32, height: f32| Bounds {
            origin: area.origin + point(px(x), px(y)),
            size: size(px(width.max(0.0)), px(height.max(0.0))),
        };
        let Some(view) = self.viewport else {
            paint_text_label(
                "Load data or enter a time",
                theme.text_secondary,
                |_, _| area.origin + point(px(CELL_PAD_X), px(8.0)),
                window,
                cx,
            );
            return;
        };
        let x = |t| gutter + (view.fraction(t) * f64::from(time_w)) as f32;
        let thin = h < 88.0;
        let brace_h = if thin { 10.0 } else { 12.0 };
        let ruler_y = if thin { 11.0 } else { 23.0 };
        let (lane_top, lane_h, count) = self.lane_geometry(h);
        self.lane_scroll = self
            .lane_scroll
            .min(self.visible_lanes().saturating_sub(count));
        if !thin {
            window.paint_quad(fill(rect(0.0, 0.0, w, lane_top), theme.bg_secondary));
        }
        if let Some(snapshot) = temporal::snapshot(cx)
            && let Some(extent) = snapshot.context.extent
        {
            let left = x(extent.start.0).clamp(gutter, w);
            let right = x(snapshot
                .context
                .live
                .unwrap_or(extent.end)
                .max(extent.end)
                .0)
            .clamp(gutter, w);
            window.paint_quad(fill(rect(left, 0.0, right - left, h), theme.bg_elevated));
        }
        if !thin {
            window.paint_quad(fill(
                rect(0.0, lane_top - 1.0, w, 1.0),
                theme.border_primary,
            ));
        }
        if let Some(range) = self.range(cx) {
            let left = x(range.start).clamp(gutter, w);
            let right = x(range.end).clamp(gutter, w);
            window.paint_quad(quad(
                rect(left, 1.0, right - left, brace_h - 2.0),
                px(2.0),
                theme.selection_bg,
                px(1.0),
                theme.control_active,
                BorderStyle::Solid,
            ));
        }
        let step = model::tick_step(view.span(), w);
        let config = temporal::config(cx);
        if let Some(snapshot) = temporal::snapshot(cx) {
            let origin = if config.display == temporal::TimeDisplay::Elapsed {
                temporal::display::origin(&config, &snapshot.context).map(|t| t.0)
            } else {
                None
            };
            for t in model::ticks(view, step, &config.timezone, origin) {
                {
                    let tx = x(t);
                    window.paint_quad(fill(
                        rect(
                            tx,
                            brace_h,
                            1.0,
                            if thin { 3.0 } else { lane_top - brace_h },
                        ),
                        theme.border_primary,
                    ));
                    let full =
                        temporal::display::timestamp(Timestamp(t), &config, &snapshot.context);
                    let label = if config.display == temporal::TimeDisplay::Elapsed {
                        full
                    } else if step < 86_400_000_000 {
                        full.split_whitespace().nth(1).unwrap_or(&full).to_string()
                    } else {
                        full.split_whitespace().next().unwrap_or(&full).to_string()
                    };
                    paint_text_label(
                        label,
                        theme.text_secondary,
                        |width, _| {
                            area.origin
                                + point(
                                    px((tx + 3.0).min((w - f32::from(width)).max(gutter))),
                                    px(ruler_y),
                                )
                        },
                        window,
                        cx,
                    );
                }
            }
        }

        let bin_count = ((time_w / 6.0).ceil() as usize).clamp(1, 1024);
        let mut visible_i = 0;
        for (lane_i, lane) in self.lanes.iter().enumerate() {
            if self.config.collapsed.contains(&lane.key) {
                continue;
            }
            let row_i = visible_i;
            visible_i += 1;
            if !thin && (row_i < self.lane_scroll || row_i >= self.lane_scroll + count) {
                continue;
            }
            let y = if thin {
                h - 8.0
            } else {
                lane_top + (row_i - self.lane_scroll) as f32 * lane_h
            };
            let row_h = if thin { 8.0 } else { lane_h.min(h - 14.0 - y) };
            if !thin {
                let row_bounds = rect(0.0, y, w, row_h);
                if self.hover.is_some_and(|p| row_bounds.contains(&p)) {
                    window.paint_quad(fill(row_bounds, theme.bg_secondary));
                }
                window.paint_quad(fill(
                    rect(0.0, y + row_h - 1.0, w, 1.0),
                    theme.border_primary,
                ));
                let budget =
                    ((gutter - CELL_PAD_X * 2.0) / (CELL_FONT_SIZE * 0.6)).max(1.0) as usize;
                let mut name: String = lane.name.chars().take(budget).collect();
                if lane.name.chars().count() > budget {
                    name.pop();
                    name.push('…');
                }
                paint_table_label(
                    name,
                    theme.text_secondary,
                    |_, _| {
                        area.origin + point(px(CELL_PAD_X), px(y + (row_h - CELL_FONT_SIZE) / 2.0))
                    },
                    window,
                    cx,
                );
            }
            for event in lane.index.spans_in(view.start, view.end, 256) {
                let sx = x(event.event.ts.0).clamp(gutter, w);
                let ex = x(event.end.unwrap().0).clamp(gutter, w);
                let bounds = rect(
                    sx,
                    y + if thin { 1.0 } else { 7.0 },
                    (ex - sx).max(1.0),
                    if thin { 5.0 } else { 13.0 },
                );
                window.paint_quad(quad(
                    bounds,
                    px(3.0),
                    crate::plot_events::flags::mix(theme.bg_primary, event.event.color, 0.18),
                    px(1.0),
                    event.event.color,
                    BorderStyle::Solid,
                ));
                self.event_hits.push(EventHit {
                    bounds,
                    lane: lane_i,
                    events: lane
                        .index
                        .bounds(event.event.ts.0, event.event.ts.0.saturating_add(1)),
                    event: event.clone(),
                });
            }
            let mut flags = Vec::new();
            let mut entries = Vec::new();
            for i in 0..bin_count {
                let start = view.at(i as f64 / bin_count as f64);
                let end = view
                    .at((i + 1) as f64 / bin_count as f64)
                    .max(start.saturating_add(1));
                let bounds = lane.index.bounds(start, end);
                let Some(event) = lane.index.representative(bounds.clone()) else {
                    continue;
                };
                let label = if thin {
                    String::new()
                } else if bounds.len() > 1 {
                    format!("{} +{}", event.event.short, bounds.len() - 1)
                } else {
                    event.event.short.to_string()
                };
                flags.push(ClusterPaint {
                    x: area.origin.x + px(x(event.event.ts.0)),
                    color: event.event.color,
                    label,
                });
                entries.push((bounds, event.clone()));
            }
            let flag_area = rect(
                gutter,
                y + if thin { 0.0 } else { 7.0 },
                time_w,
                (row_h - if thin { 0.0 } else { 9.0 }).max(1.0),
            );
            let hits = paint_event_flags(flag_area, &flags, window, cx);
            for (bounds, (events, event)) in hits.into_iter().zip(entries) {
                self.event_hits.push(EventHit {
                    bounds,
                    lane: lane_i,
                    events,
                    event,
                });
            }
        }
        if !thin {
            window.paint_quad(fill(rect(gutter - 1.0, 0.0, 1.0, h), theme.border_primary));
            paint_table_label(
                "Source",
                theme.text_secondary,
                |_, _| area.origin + point(px(CELL_PAD_X), px(ruler_y)),
                window,
                cx,
            );
            let overview = self.fit.unwrap_or(view).hull(view);
            window.paint_quad(fill(rect(0.0, h - 14.0, gutter, 14.0), theme.bg_secondary));
            window.paint_quad(fill(rect(0.0, h - 14.0, w, 1.0), theme.border_primary));
            let left = gutter + (overview.fraction(view.start) * f64::from(time_w)) as f32;
            let right = gutter + (overview.fraction(view.end) * f64::from(time_w)) as f32;
            window.paint_quad(quad(
                rect(left, h - 11.0, right - left, 9.0),
                px(2.0),
                theme.selection_bg,
                px(1.0),
                theme.border_primary,
                BorderStyle::Solid,
            ));
            for t in [left, right - 2.0] {
                window.paint_quad(fill(rect(t, h - 10.0, 2.0, 7.0), theme.control_active));
            }
            if self.visible_lanes() > count {
                let available = h - lane_top - 14.0;
                let total = self.visible_lanes() as f32;
                window.paint_quad(quad(
                    rect(
                        w - 4.0,
                        lane_top + available * self.lane_scroll as f32 / total,
                        3.0,
                        available * count as f32 / total,
                    ),
                    px(1.5),
                    theme.text_tertiary,
                    px(0.0),
                    theme.text_tertiary,
                    BorderStyle::Solid,
                ));
            }
        }
        // Paint the grabbers above ruler labels; their hit zone uses this same height.
        if let Some(range) = self.range(cx) {
            for tx in [
                x(range.start).clamp(gutter, w),
                (x(range.end) - 3.0).clamp(gutter, (w - 3.0).max(gutter)),
            ] {
                window.paint_quad(quad(
                    rect(tx, 1.0, 3.0, Self::range_handle_height(h) - 1.0),
                    px(1.0),
                    theme.control_active,
                    px(0.0),
                    theme.control_active,
                    BorderStyle::Solid,
                ));
            }
        }
        if !temporal::is_live(cx)
            && let Some(t) = self.time(cx).filter(|t| *t >= view.start && *t <= view.end)
        {
            let tx = x(t).clamp(gutter, (w - 1.0).max(gutter));
            window.paint_quad(fill(
                rect(tx, brace_h, 1.0, h - brace_h),
                theme.text_primary,
            ));
            window.paint_quad(quad(
                rect(tx - 3.0, brace_h, 6.0, 3.0),
                px(1.0),
                theme.text_primary,
                px(0.0),
                theme.text_primary,
                BorderStyle::Solid,
            ));
        }
        if let Some(pointer) = self
            .hover
            .filter(|p| self.time_area().is_some_and(|a| a.contains(p)))
        {
            if self.event_at(pointer, cx).is_none() {
                window.paint_quad(fill(
                    rect(
                        f32::from(pointer.x - area.origin.x),
                        brace_h,
                        1.0,
                        h - brace_h,
                    ),
                    theme.text_tertiary,
                ));
            }
        }
    }
}

/// Lane cells use the same typography as the application's dense tables.
fn paint_table_label(
    text: impl Into<gpui::SharedString>,
    color: gpui::Hsla,
    origin: impl FnOnce(Pixels, Pixels) -> Point<Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    let text = text.into();
    let font_size = px(CELL_FONT_SIZE);
    let run = gpui::TextRun {
        len: text.len(),
        font: window.text_style().font(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let shaped = window
        .text_system()
        .shape_line(text, font_size, &[run], None);
    let _ = shaped.paint(origin(shaped.width, font_size), font_size, window, cx);
}
