use super::*;

impl TimeSeriesPlot {
    fn plot_surface(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        div()
            .id((
                "time-series-plot",
                cx.entity().entity_id().as_u64() as usize,
            ))
            .flex_1()
            .min_h_0()
            .relative()
            .on_hover(cx.listener(|this, hovered: &bool, _window, cx| {
                if !*hovered {
                    this.clear_hover(cx);
                }
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::plot_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::plot_mouse_up))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                    this.clear_hover(cx);
                    this.open_cursor_inspector_at(event.position, window, cx);
                }),
            )
            .on_mouse_move(cx.listener(Self::plot_mouse_move))
            .on_scroll_wheel(cx.listener(Self::plot_scroll))
    }

    fn plot_mouse_down(
        &mut self,
        event: &gpui::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_hover(cx);
        if event.click_count == 2 {
            self.reset_view(cx);
            return;
        }
        // Alt+left-drag opens a measurement cursor —
        // gpui's mac trackpad backend delivers right-
        // mouse-down but not the matching drag/up, so
        // the gesture has to ride on the left button.
        if event.modifiers.alt {
            self.start_cursor_drag(event, window, cx);
            return;
        }
        // A click on a flag pins its popover; any other plot click
        // clears an open pin before falling through to pan setup.
        if let Some(pa) = self.last_plot_area
            && let Some(idx) = self.event_cluster_at(event.position, pa)
        {
            let x_ts = self.event_clusters[idx].ts().0;
            self.pinned_event = Some(PinnedEvent { x_ts, selected: 0 });
            self.sync_pinned_json_tree(cx);
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if self.pinned_event.take().is_some() {
            self.event_json_tree = None;
            cx.notify();
        }
        let axis_count = self.line_plot.read(cx).axes.len();
        let zone = self
            .last_plot_area
            .map(|pa| axis_zone(event.position, pa, axis_count))
            .unwrap_or(AxisZone::Plot);
        self.drag_start = Some(event.position);
        self.drag_start_view = self.line_plot.read(cx).effective_view(cx);
        self.drag_zone = zone;
    }

    fn plot_mouse_up(
        &mut self,
        event: &gpui::MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.end_panel_drag() {
            return;
        }
        if self.cursor_drag.is_some() {
            self.finish_cursor_drag(event, window, cx);
            return;
        }
        self.drag_start = None;
        self.drag_start_view = None;
    }

    fn plot_mouse_move(
        &mut self,
        event: &gpui::MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !event.dragging() {
            self.update_hover(event, cx);
            return;
        }
        if self.advance_panel_drag(event.position, cx) {
            return;
        }
        if self.cursor_drag.is_some() {
            self.handle_cursor_drag_move(event, cx);
            return;
        }
        let (Some(start), Some(start_view), Some(pa)) = (
            self.drag_start,
            self.drag_start_view.clone(),
            self.last_plot_area,
        ) else {
            return;
        };

        let dx = event.position.x - start.x;
        let dy = event.position.y - start.y;
        let (nx, ny) = start_view.x_bounds().screen_delta_to_norm(pa, dx, dy);
        let new_view = match self.drag_zone {
            AxisZone::Plot => start_view.offset_x(-nx).offset_y_all(ny),
            AxisZone::XAxis => start_view.offset_x(-nx),
            AxisZone::YAxis(i) => start_view.offset_axis_y(i, ny),
        };
        self.line_plot
            .update(cx, |lp, cx| lp.set_view_override(Some(new_view), cx));
    }

    fn plot_scroll(
        &mut self,
        event: &gpui::ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_hover(cx);
        let Some(view) = self.line_plot.read(cx).effective_view(cx) else {
            return;
        };
        let Some(pa) = self.last_plot_area else {
            return;
        };

        let delta = event.delta.pixel_delta(px(20.0));
        let zoom_amount = f32::from(-delta.y) as f64 / 200.0;
        let factor = (1.0_f64 + zoom_amount).clamp(0.5, 2.0);

        let zone = axis_zone(event.position, pa, view.axis_count());
        let (ax, ay) = view.x_bounds().screen_anchor(pa, event.position);
        let new_view = match zone {
            AxisZone::Plot => view.zoom_x(factor, ax).zoom_y_all(factor, 1.0 - ay),
            AxisZone::XAxis => view.zoom_x(factor, ax),
            AxisZone::YAxis(i) => view.zoom_axis_y(i, factor, 1.0 - ay),
        };
        self.line_plot
            .update(cx, |lp, cx| lp.set_view_override(Some(new_view), cx));
        cx.stop_propagation();
    }

    fn render_underlay(&self, cx: &mut Context<Self>) -> AnyElement {
        let underlay_lp = self.line_plot.clone();
        canvas(
            {
                let this = cx.entity().downgrade();
                move |bounds, _window, cx| {
                    let lp = underlay_lp.read(cx);
                    let axis_count = lp.axes.len();
                    let view = lp.effective_view(cx);
                    let data_start = lp.data_start().unwrap_or(0.0);
                    let fmt = lp.x_time_format;
                    let tint = alarm_plot_tint(lp, cx);
                    let _ = this.update(cx, |this, _| {
                        this.last_plot_area = Some(plot_area(bounds, axis_count));
                    });
                    (bounds, view, data_start, fmt, tint)
                }
            },
            move |_, (bounds, view, data_start, fmt, tint), window, cx| {
                if let Some(view) = view {
                    paint_underlay(bounds, &view, data_start, fmt, tint, window, cx);
                }
            },
        )
        .absolute()
        .inset_0()
        .into_any_element()
    }

    fn render_overlay(&self, _cx: &mut Context<Self>) -> AnyElement {
        let overlay_lp = self.line_plot.clone();
        canvas(
            move |bounds, _window, cx| {
                let lp = overlay_lp.read(cx);
                let view = lp.effective_view(cx);
                let colors = lp.axis_colors(cx);
                // Per-trace axis markers: value at the visible left edge,
                // only meaningful once there's more than one axis.
                let mut markers: Vec<(usize, f64, Hsla)> = Vec::new();
                if let Some(v) = &view
                    && v.axis_count() > 1
                {
                    let left = Timestamp(v.x.0 as i64);
                    for trace in lp.traces() {
                        let cfg = trace.read(cx);
                        if !cfg.visible {
                            continue;
                        }
                        if let Some(val) = lp.trace_value_at(trace.entity_id(), left, cx) {
                            markers.push((cfg.axis_index, val, cfg.color));
                        }
                    }
                }
                let limit_lines = alarm_limit_lines(lp, cx);
                (
                    bounds,
                    view,
                    colors,
                    markers,
                    limit_lines,
                    lp.data_start().unwrap_or(0.0),
                    lp.x_time_format,
                )
            },
            move |_, (bounds, view, colors, markers, limit_lines, data_start, fmt), window, cx| {
                if let Some(view) = view {
                    paint_overlay(
                        bounds,
                        &view,
                        &colors,
                        &markers,
                        &limit_lines,
                        data_start,
                        fmt,
                        window,
                        cx,
                    );
                    crate::temporal::paint_playhead(
                        plot_area(bounds, view.axis_count()),
                        view.x,
                        window,
                        cx,
                    );
                }
            },
        )
        .absolute()
        .inset_0()
        .into_any_element()
    }

    fn render_event_flags(&self, cx: &mut Context<Self>) -> AnyElement {
        let flags_weak = cx.entity().downgrade();
        canvas(
            move |bounds, _window, cx| {
                let mut out = None;
                let _ = flags_weak.update(cx, |this, cx| {
                    let Some(view) = this.line_plot.read(cx).effective_view(cx) else {
                        return;
                    };
                    let pa = plot_area(bounds, view.axis_count());
                    let clusters = this.build_event_clusters(&view, pa, cx);
                    let theme = crate::theme::theme(cx);
                    let paints: Vec<ClusterPaint> = clusters
                        .iter()
                        .map(|c| ClusterPaint {
                            x: c.x,
                            color: c.color(&theme),
                            label: c.chip_label(),
                        })
                        .collect();
                    this.event_clusters = clusters;
                    out = Some((view, paints));
                });
                (bounds, out)
            },
            move |_, (bounds, data), window, cx| {
                if let Some((view, paints)) = data {
                    paint_event_flags(bounds, &view, &paints, window, cx);
                }
            },
        )
        .absolute()
        .inset_0()
        .into_any_element()
    }

    fn render_cursors(&self, cx: &mut Context<Self>) -> AnyElement {
        let cursors_lp = self.line_plot.clone();
        let cursors_weak = cx.entity().downgrade();
        canvas(
            move |bounds, _window, cx| {
                let view = cursors_lp.read(cx).effective_view(cx);
                let mut snapshots: Vec<CursorPaint> = Vec::new();
                let _ = cursors_weak.update(cx, |this, cx| {
                    let active_id = this.cursor_drag.as_ref().map(|d| d.cursor.read(cx).id);
                    let lp = this.line_plot.read(cx);
                    for cursor in &this.cursors {
                        let c = cursor.read(cx);
                        let mut markers: Vec<(f64, f64, Hsla)> = Vec::new();
                        for trace in lp.traces() {
                            let cfg = trace.read(cx);
                            if !cfg.visible {
                                continue;
                            }
                            let v_start = lp.trace_value_at(trace.entity_id(), c.t_start, cx);
                            let v_end = lp.trace_value_at(trace.entity_id(), c.t_end, cx);
                            if let (Some(vs), Some(ve)) = (v_start, v_end) {
                                // Pre-normalize to [0,1] against the
                                // trace's axis so paint can map with a
                                // single fixed 0..1 Y range.
                                let (ns, ne) = match &view {
                                    Some(v) => {
                                        let b = v.axis_bounds(cfg.axis_index);
                                        let h = (b.max_y - b.min_y).max(1e-12);
                                        ((vs - b.min_y) / h, (ve - b.min_y) / h)
                                    }
                                    None => (vs, ve),
                                };
                                markers.push((ns, ne, cfg.color));
                            }
                        }
                        snapshots.push(CursorPaint {
                            t_start: c.t_start,
                            t_end: c.t_end,
                            active: Some(c.id) == active_id,
                            trace_markers: markers,
                        });
                    }
                });
                (bounds, view, snapshots)
            },
            move |_, (bounds, view, cursors), window, cx| {
                if let Some(view) = view {
                    // Markers carry normalized [0,1] Y; lines use X only.
                    paint_cursors(bounds, &view, &cursors, window, cx);
                }
            },
        )
        .absolute()
        .inset_0()
        .into_any_element()
    }

    fn render_hover(&self, cx: &mut Context<Self>) -> AnyElement {
        let hover_weak = cx.entity().downgrade();
        canvas(
            move |bounds, _window, cx| {
                let mut out = None;
                let _ = hover_weak.update(cx, |this, cx| {
                    let Some(pointer) = this.hover else {
                        return;
                    };
                    let Some(view) = this.line_plot.read(cx).effective_view(cx) else {
                        return;
                    };
                    // Same lookup as the readout box, pre-normalized
                    // per axis the way `CursorPaint` markers are.
                    let markers = this
                        .hover_samples(cx)
                        .map(|(_, samples)| {
                            samples
                                .into_iter()
                                .map(|s| {
                                    let b = view.axis_bounds(s.axis_index);
                                    let h = (b.max_y - b.min_y).max(1e-12);
                                    (s.ts.0 as f64, (s.value - b.min_y) / h, s.color)
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    out = Some((
                        view,
                        HoverPaint {
                            pointer_x: pointer.x,
                            markers,
                        },
                    ));
                });
                (bounds, out)
            },
            move |_, (bounds, data), window, cx| {
                if let Some((view, hover)) = data {
                    paint_hover_crosshair(bounds, &view, hover.pointer_x, window, cx);
                    paint_hover_markers(bounds, &view, &hover.markers, window);
                }
            },
        )
        .absolute()
        .inset_0()
        .into_any_element()
    }
}

impl Render for TimeSeriesPlot {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::theme(cx);
        // Resolve this frame's event sources (and refresh their repaint subs)
        // before building canvases, so the flag canvas can query them in paint.
        self.sync_event_sources(cx);
        let trace_entities: Vec<Entity<Trace>> = self.line_plot.read(cx).traces().to_vec();
        let show_legend = trace_entities.len() >= 2;
        // Drives the left chrome width: one Y_LABEL_WIDTH column per axis.
        let axis_count = self.line_plot.read(cx).axes.len();
        let chrome_left = px(left_margin(axis_count));

        let mut root = div().flex().flex_col().size_full().bg(theme.bg_secondary);

        let mut inner = self
            .plot_surface(cx)
            .child(self.render_underlay(cx))
            .child(
                div()
                    .absolute()
                    .left(chrome_left)
                    .top(px(PADDING))
                    .right(px(PADDING))
                    .bottom(px(X_LABEL_HEIGHT + PADDING))
                    .child(self.line_plot.clone()),
            )
            .child(self.render_overlay(cx))
            .child(self.render_event_flags(cx))
            .child(self.render_cursors(cx))
            .child(self.render_hover(cx));

        if self.line_plot.read(cx).x_range.as_custom().is_some() {
            inner = inner.child(
                div()
                    .absolute()
                    .top_0()
                    .right_0()
                    .text_size(px(10.0))
                    .text_color(theme.text_tertiary)
                    .child("Independent range"),
            );
        }
        // Measurement panels: a native div tree positioned on top of
        // the cursor overlays. In Track mode this is one mini panel per
        // cursor; in Pinned mode it's a single consolidated panel.
        let panels = measurement_panel::render_panels(self, cx);
        for panel in panels {
            // Panel origins are plot-area-local; the outer container's
            // origin is offset by (left_margin, PADDING).
            let left = panel.origin.x + chrome_left;
            let top = panel.origin.y + px(PADDING);
            inner = inner.child(div().absolute().left(left).top(top).child(panel.element));
        }

        if let Some(readout) = self.hover_readout(cx) {
            inner = inner.child(readout);
        }

        // Event-flag popovers: a pinned one wins over the transient hover one.
        if let Some(popover) = self.event_pinned_popover(cx) {
            inner = inner.child(popover);
        } else if let Some(popover) = self.event_hover_popover(cx) {
            inner = inner.child(popover);
        }

        root = root.child(inner);

        if show_legend {
            root = root.child(crate::views::plot_common::plot_legend(
                &trace_entities,
                self.line_plot.clone(),
                chrome_left,
                |trace| (trace.label.clone(), trace.color, trace.visible),
                |trace| trace.visible = !trace.visible,
                cx,
            ));
        }

        root
    }
}
