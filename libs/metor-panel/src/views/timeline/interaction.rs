use super::*;

impl Timeline {
    pub(super) fn scroll(&mut self, event: &gpui::ScrollWheelEvent, cx: &mut Context<Self>) {
        let delta = event.delta.pixel_delta(px(20.0));
        let d = if f32::from(delta.x).abs() > f32::from(delta.y).abs() {
            delta.x
        } else {
            delta.y
        };
        if f32::from(d) == 0.0 {
            return;
        }
        self.clear_hover(cx);
        if event.modifiers.shift {
            let Some(area) = self.area.filter(|a| f32::from(a.size.height) >= 88.0) else {
                return;
            };
            let (_, _, count) = self.lane_geometry(f32::from(area.size.height));
            let max = self.visible_lanes().saturating_sub(count);
            if max == 0 {
                return;
            }
            self.lane_scroll = (self.lane_scroll as i64 - f32::from(d).signum() as i64)
                .clamp(0, max as i64) as usize;
            cx.notify();
        } else if event.modifiers.control {
            if let (Some(view), Some(area)) = (self.viewport, self.time_area()) {
                let span = view.span() * f64::from(f32::from(d))
                    / f64::from(f32::from(area.size.width).max(1.0));
                self.navigate(Navigation::Manual(view.shifted(-span.round() as i128)), cx);
            }
        } else {
            // Match the plot's zoom direction and sensitivity, restricted to time.
            self.zoom(
                (1.0 - f64::from(f32::from(d)) / 200.0).clamp(0.5, 2.0),
                self.fraction(event.position.x).clamp(0.0, 1.0),
                cx,
            );
        }
        cx.stop_propagation();
    }

    pub(super) fn emit(&mut self, action: TimeAction, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((_, edit)) = &self.edit {
            edit(action, window, cx);
        } else if let Err(error) = temporal::dispatch(action, cx) {
            self.preview_error = Some(error);
        }
    }
    pub(super) fn fraction(&self, x: Pixels) -> f64 {
        self.time_area().map_or(0.5, |a| {
            f64::from(f32::from(x - a.origin.x)) / f64::from(f32::from(a.size.width).max(1.0))
        })
    }
    pub(super) fn zoom(&mut self, factor: f64, fraction: f64, cx: &mut Context<Self>) {
        if let Some(view) = self.viewport {
            let zoom = view.zoom(factor, fraction);
            let navigation =
                if factor > 1.0 && self.fit.is_some_and(|f| zoom.span() >= f.span() * 1.02) {
                    Navigation::Fit
                } else {
                    Navigation::Manual(zoom)
                };
            self.navigate(navigation, cx);
        }
    }
    pub(super) fn pointer_down(
        &mut self,
        e: &gpui::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_hover(cx);
        self.focus.focus(window);
        let (Some(area), Some(view)) = (self.area, self.viewport) else {
            return;
        };
        let Some(time_area) = self.time_area() else {
            return;
        };
        if e.position.x < time_area.origin.x {
            return;
        }
        let y = f32::from(e.position.y - area.origin.y);
        let x = self.fraction(e.position.x);
        let range = self.range(cx);
        let target = self.edit.as_ref().map(|(target, _)| *target);
        let range_enabled = target != Some(EditTarget::View);
        let range_zone = if f32::from(area.size.height) < 88.0 {
            10.0
        } else {
            12.0
        };
        let tolerance = 7.0 / f64::from(f32::from(time_area.size.width).max(1.0));
        let handle = range
            .filter(|_| y < Self::range_handle_height(f32::from(area.size.height)))
            .and_then(|r| {
                let start = (x - view.fraction(r.start)).abs();
                let end = (x - view.fraction(r.end)).abs();
                if start.min(end) > tolerance {
                    None
                } else if start <= end {
                    Some(DragKind::Start)
                } else {
                    Some(DragKind::End)
                }
            });
        let overview = self.fit.unwrap_or(view).hull(view);
        let kind = if e.button == MouseButton::Middle {
            DragKind::Pan
        } else if f32::from(area.size.height) >= 88.0 && y > f32::from(area.size.height) - 14.0 {
            let tolerance = 6.0 / f64::from(f32::from(time_area.size.width).max(1.0));
            if (x - overview.fraction(view.start)).abs() < tolerance {
                DragKind::OverviewStart
            } else if (x - overview.fraction(view.end)).abs() < tolerance {
                DragKind::OverviewEnd
            } else {
                DragKind::Overview
            }
        } else if range_enabled && (y < range_zone || handle.is_some() || e.modifiers.shift) {
            match target {
                Some(EditTarget::Start) => DragKind::Start,
                Some(EditTarget::End) => DragKind::End,
                _ => {
                    if let Some(r) = range {
                        if let Some(handle) = handle {
                            handle
                        } else if !e.modifiers.shift
                            && x > view.fraction(r.start)
                            && x < view.fraction(r.end)
                        {
                            DragKind::Move
                        } else {
                            DragKind::Brush
                        }
                    } else {
                        DragKind::Brush
                    }
                }
            }
        } else if target
            .is_some_and(|t| matches!(t, EditTarget::Range | EditTarget::Start | EditTarget::End))
        {
            DragKind::Pan
        } else {
            DragKind::Seek
        };
        if e.click_count == 2 {
            self.pending_seek = None;
            self.click = None;
            self.drag = None;
            self.draft = None;
            self.navigate(Navigation::Fit, cx);
            return;
        }
        if y >= 44.0
            && y < f32::from(area.size.height) - 14.0
            && f32::from(area.size.height) >= 88.0
            && !e.modifiers.shift
            && e.button == MouseButton::Left
        {
            if let Some(event) = self.event_at(e.position, cx) {
                self.selected = Some(event);
                self.inspect_events(e.position, window, cx);
                cx.stop_propagation();
                cx.notify();
                return;
            }
        }
        let (time, time_expr) = if kind == DragKind::Brush {
            self.snap(view.at(x), view, e.modifiers.alt, cx)
        } else {
            (view.at(x), TimeExpr::fixed(Timestamp(view.at(x))))
        };
        self.published_range = None;
        self.drag = Some(Drag {
            kind,
            position: e.position,
            last_position: e.position,
            viewport: view,
            overview,
            range,
            spec: self.range_spec(cx),
            time,
            time_expr,
            edge_delta: 0.0,
            edge_since: None,
            moved: false,
        });
        self.click = None;
        cx.stop_propagation();
        cx.notify();
    }
    pub(super) fn snap(
        &self,
        time: i64,
        view: Interval,
        bypass: bool,
        cx: &App,
    ) -> (i64, TimeExpr) {
        let fixed = (time, TimeExpr::fixed(Timestamp(time)));
        if !self.config.snap || bypass {
            return fixed;
        }
        let tolerance = view.span() * 6.0
            / self
                .time_area()
                .map_or(500.0, |a| f64::from(f32::from(a.size.width).max(1.0)));
        let mut candidates = Vec::new();
        if let Some(s) = temporal::snapshot(cx) {
            if let Some(ref r) = s.context.extent {
                candidates.push((r.start.0, TimeExpr::new(temporal::Anchor::DataStart, 0)));
                // Data-end snaps mean "follow the head", not a fixed instant.
                candidates.push((
                    r.end.0,
                    if s.context.live.is_some() {
                        TimeExpr::LIVE
                    } else {
                        TimeExpr::new(temporal::Anchor::DataEnd, 0)
                    },
                ));
            }
            if let Some(live) = s.context.live {
                candidates.push((live.0, TimeExpr::LIVE));
            }
            if let Some(t0) = temporal::display::origin(&temporal::config(cx), &s.context) {
                candidates.push((t0.0, TimeExpr::fixed(t0)));
            }
        }
        for lane in &self.lanes {
            for direction in [-1, 1] {
                if let Some(e) = lane.index.nearest(time, direction) {
                    candidates.push((e.event.ts.0, TimeExpr::fixed(e.event.ts)));
                }
            }
        }
        let distance = |(t, _): &(i64, TimeExpr)| (i128::from(*t) - i128::from(time)).abs();
        // At the head, Live wins over dense event flags in the same few pixels.
        let candidate = candidates
            .iter()
            .filter(|(_, expr)| *expr == TimeExpr::LIVE)
            .filter(|candidate| distance(candidate) as f64 <= tolerance)
            .min_by_key(|candidate| distance(candidate))
            .or_else(|| {
                candidates
                    .iter()
                    .min_by_key(|candidate| distance(candidate))
            })
            .filter(|candidate| distance(candidate) as f64 <= tolerance);
        candidate
            .map(|(t, expr)| {
                (
                    temporal::snapshot(cx)
                        .and_then(|s| expr.resolve(&s.context).ok())
                        .map_or(*t, |t| t.0),
                    *expr,
                )
            })
            .unwrap_or(fixed)
    }

    pub(super) fn pointer_move(&mut self, e: &gpui::MouseMoveEvent, cx: &mut Context<Self>) {
        self.hover = (e.pressed_button.is_none()
            && self.drag.is_none()
            && self.time_area().is_some_and(|a| a.contains(&e.position)))
        .then_some(e.position);
        if self.drag.is_some() && e.pressed_button.is_none() {
            self.drag = None;
            self.draft = None;
            self.pending_seek = None;
        }
        self.move_drag(e.position, e.modifiers.alt, cx);
        cx.notify();
    }
    pub(super) fn move_drag(
        &mut self,
        position: Point<Pixels>,
        bypass: bool,
        cx: &mut Context<Self>,
    ) {
        let fraction = self.fraction(position.x);
        let Some(mut drag) = self.drag.take() else {
            return;
        };
        drag.last_position = position;
        drag.moved |=
            f32::from(position.x - drag.position.x).abs() >= 3.0 || drag.edge_delta.abs() > 0.0;
        let (t, expr) = self.snap(
            clamp(i128::from(drag.viewport.at(fraction)) + drag.edge_delta.round() as i128),
            drag.viewport,
            bypass,
            cx,
        );
        let delta = i128::from(t) - i128::from(drag.time);
        match drag.kind {
            DragKind::Seek if drag.moved => self.pending_seek = Some(t),
            DragKind::Brush if drag.moved => {
                if t != drag.time {
                    self.draft = Some(if t > drag.time {
                        TimeRangeSpec {
                            start: drag.time_expr,
                            end: expr,
                        }
                    } else {
                        TimeRangeSpec {
                            start: expr,
                            end: drag.time_expr,
                        }
                    });
                }
            }
            DragKind::Start => {
                if let Some(r) = drag.range {
                    self.draft = Some(TimeRangeSpec {
                        start: if t < r.end {
                            expr
                        } else {
                            TimeExpr::fixed(Timestamp(r.end.saturating_sub(1)))
                        },
                        end: drag.spec.end,
                    });
                }
            }
            DragKind::End => {
                if let Some(r) = drag.range {
                    self.draft = Some(TimeRangeSpec {
                        start: drag.spec.start,
                        end: if t > r.start {
                            expr
                        } else {
                            TimeExpr::fixed(Timestamp(r.start.saturating_add(1)))
                        },
                    });
                }
            }
            DragKind::Move => {
                if let Some(r) = drag.range {
                    // Move the interval as a unit; snapping its end to Live
                    // makes a floating window with the same duration.
                    let raw = clamp(
                        i128::from(drag.viewport.at(fraction)) + drag.edge_delta.round() as i128,
                    );
                    let r = r.shifted(i128::from(raw) - i128::from(drag.time));
                    let (end, end_expr) = self.snap(r.end, drag.viewport, bypass, cx);
                    self.draft = Some(
                        if end_expr == TimeExpr::LIVE
                            && let Ok(offset) =
                                i64::try_from(i128::from(r.start) - i128::from(r.end))
                        {
                            TimeRangeSpec {
                                start: TimeExpr::new(temporal::Anchor::Live, offset),
                                end: TimeExpr::LIVE,
                            }
                        } else {
                            let r = r.shifted(i128::from(end) - i128::from(r.end));
                            TimeRangeSpec::fixed(Timestamp(r.start)..Timestamp(r.end))
                        },
                    );
                }
            }
            DragKind::Pan => {
                let v = drag.viewport.shifted(-delta);
                self.viewport = Some(v);
                self.config.navigation = Navigation::Manual(v);
            }
            DragKind::Overview | DragKind::OverviewStart | DragKind::OverviewEnd => {
                let t = drag.overview.at(fraction);
                let v = match drag.kind {
                    DragKind::OverviewStart => Interval::new(
                        t.min(drag.viewport.end.saturating_sub(1_000)),
                        drag.viewport.end,
                    ),
                    DragKind::OverviewEnd => Interval::new(
                        drag.viewport.start,
                        t.max(drag.viewport.start.saturating_add(1_000)),
                    ),
                    _ => drag.viewport.shifted(
                        i128::from(t)
                            - i128::from(drag.overview.at(self.fraction(drag.position.x))),
                    ),
                };
                self.viewport = Some(v);
                self.config.navigation = Navigation::Manual(v);
            }
            _ => {}
        }
        self.drag = Some(drag);
    }
    pub(super) fn release(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        if drag.kind == DragKind::Seek && !drag.moved {
            self.click = Some((drag.time, Instant::now()));
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if let Some(t) = self.pending_seek.take() {
            self.emit(TimeAction::Seek(TimeExpr::fixed(Timestamp(t))), window, cx);
        }
        self.flush_range(window, cx);
        self.draft = None;
        cx.stop_propagation();
        cx.notify();
    }
    pub(super) fn animate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_frame).as_secs_f64().min(0.05);
        self.last_frame = now;
        if !window.is_window_active() {
            self.drag = None;
            self.draft = None;
            self.pending_seek = None;
            self.click = None;
        }
        if self
            .click
            .is_some_and(|(_, started)| now.duration_since(started).as_millis() >= 220)
        {
            let (time, _) = self.click.take().unwrap();
            self.pending_seek = Some(time);
        }
        let mut moved = false;
        let time_area = self.time_area();
        if let Some(drag) = &mut self.drag
            && !matches!(
                drag.kind,
                DragKind::Pan
                    | DragKind::Overview
                    | DragKind::OverviewStart
                    | DragKind::OverviewEnd
            )
            && let Some(time_area) = time_area
        {
            let fraction = f64::from(
                f32::from(drag.last_position.x - time_area.origin.x)
                    / f32::from(time_area.size.width).max(1.0),
            );
            let speed = model::edge_speed(fraction, f32::from(time_area.size.width));
            if speed == 0.0 {
                drag.edge_since = None;
            } else {
                let start = *drag.edge_since.get_or_insert(now);
                if now.duration_since(start).as_millis() >= 150 {
                    drag.edge_delta += speed * drag.viewport.span() * elapsed;
                    let shift = drag.edge_delta.round() as i128;
                    let view = if self.config.navigation == Navigation::Fit {
                        Interval::new(
                            clamp(i128::from(drag.viewport.start) + shift.min(0)),
                            clamp(i128::from(drag.viewport.end) + shift.max(0)),
                        )
                    } else {
                        drag.viewport.shifted(shift)
                    };
                    self.viewport = Some(view);
                    if self.config.navigation == Navigation::Fit {
                        self.fit = Some(self.fit.unwrap_or(view).hull(view));
                    } else {
                        self.config.navigation = Navigation::Manual(view);
                    }
                    moved = true;
                }
            }
        }
        if moved && let Some(position) = self.drag.as_ref().map(|d| d.last_position) {
            self.move_drag(position, false, cx);
        }
        if let Some(t) = self.pending_seek.take() {
            self.emit(TimeAction::Seek(TimeExpr::fixed(Timestamp(t))), window, cx);
        }
        self.flush_range(window, cx);
    }
    /// Coalesce range pointer events into one update per frame; release flushes
    /// the final position without applying the same range a second time.
    pub(super) fn flush_range(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(range) = self
            .draft
            .filter(|range| Some(*range) != self.published_range)
        {
            self.emit(TimeAction::Range(range), window, cx);
            self.published_range = Some(range);
        }
    }
}
