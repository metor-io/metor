//! The plot legend as its own cached view.
//!
//! A live plot re-renders every frame its data moves, and without a boundary
//! its legend (a wrapping row of chips, each a shaped label) is rebuilt, laid
//! out and painted with it. As a cached view the legend replays its last
//! frame instead, re-rendering only when a trace it shows changes, its trace
//! list changes, or its width does.
//!
//! A cached view is laid out by its parent at a fixed style, but the legend's
//! height depends on how its chips wrap. So it measures itself: each render
//! records the height its content laid out at, and when that differs from the
//! height it was given, it takes the new one on the next frame.

use gpui::{
    AnyElement, App, Context, Entity, Hsla, IntoElement, Pixels, Rems, SharedString,
    StyleRefinement, Styled, Window, canvas, div, prelude::*, px,
};

/// Height the legend takes before it has measured itself: one row of chips.
const FIRST_GUESS: Pixels = px(18.0);

pub(crate) struct PlotLegend<T: 'static, N: 'static> {
    traces: Vec<Entity<T>>,
    notify: Entity<N>,
    left: Rems,
    describe: fn(&T) -> (SharedString, Hsla, bool),
    toggle_visible: fn(&mut T),
    height: Pixels,
}

impl<T: 'static, N: 'static> PlotLegend<T, N> {
    /// `notify` is the line plot to refresh after a visibility toggle.
    pub(crate) fn new(
        notify: Entity<N>,
        describe: fn(&T) -> (SharedString, Hsla, bool),
        toggle_visible: fn(&mut T),
    ) -> Self {
        Self {
            traces: Vec::new(),
            notify,
            left: Rems(0.0),
            describe,
            toggle_visible,
            height: FIRST_GUESS,
        }
    }

    /// Show `traces`, inset by `left`. Re-renders only on a change.
    pub(crate) fn set(&mut self, traces: &[Entity<T>], left: Rems, cx: &mut Context<Self>) {
        if self.traces != traces || self.left != left {
            self.traces = traces.to_vec();
            self.left = left;
            cx.notify();
        }
    }

    /// The legend as a child element, at the height it last measured.
    pub(crate) fn element(this: &Entity<Self>, cx: &App) -> AnyElement {
        let height = this.read(cx).height;
        this.clone()
            .cached(StyleRefinement::default().w_full().h(height).flex_none())
            .into_any_element()
    }
}

impl<T: 'static, N: 'static> Render for PlotLegend<T, N> {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity().downgrade();
        let height = self.height;
        div()
            .w_full()
            .relative()
            .child(super::plot_legend(
                &self.traces,
                self.notify.clone(),
                self.left,
                self.describe,
                self.toggle_visible,
                cx,
            ))
            .child(
                // Spans the content, so its bounds are the height the chips
                // wrapped to at this width.
                canvas(
                    move |bounds, _window, cx| {
                        let measured = bounds.size.height;
                        if measured != height {
                            // After this draw: a notify sent during it would
                            // be dropped with the frame's invalidations.
                            cx.defer(move |cx| {
                                let _ = this.update(cx, |legend, cx| {
                                    if legend.height != measured {
                                        legend.height = measured;
                                        cx.notify();
                                    }
                                });
                            });
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .inset_0(),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use gpui::{Hsla, SharedString, StyleRefinement, TestAppContext, div, prelude::*, px};

    use super::PlotLegend;

    /// Counts legend renders: `describe` runs once per chip per render.
    static DESCRIBED: AtomicUsize = AtomicUsize::new(0);

    struct Chip(SharedString);

    fn describe(chip: &Chip) -> (SharedString, Hsla, bool) {
        DESCRIBED.fetch_add(1, Ordering::Relaxed);
        (chip.0.clone(), gpui::red(), true)
    }

    /// Stands in for the line plot: re-renders whenever it is notified.
    struct Data(u32);

    impl Render for Data {
        fn render(
            &mut self,
            _window: &mut gpui::Window,
            _cx: &mut gpui::Context<Self>,
        ) -> impl IntoElement {
            div().size(px(self.0 as f32 + 1.0))
        }
    }

    /// Stands in for the plot: the data and the legend under one cached tile.
    struct Plot {
        data: gpui::Entity<Data>,
        legend: gpui::Entity<PlotLegend<Chip, Data>>,
    }

    impl Render for Plot {
        fn render(
            &mut self,
            _window: &mut gpui::Window,
            cx: &mut gpui::Context<Self>,
        ) -> impl IntoElement {
            div()
                .flex()
                .flex_col()
                .size_full()
                .child(div().flex_1().child(self.data.clone()))
                .child(PlotLegend::element(&self.legend, cx))
        }
    }

    struct Tile(gpui::Entity<Plot>);

    impl Render for Tile {
        fn render(
            &mut self,
            _window: &mut gpui::Window,
            _cx: &mut gpui::Context<Self>,
        ) -> impl IntoElement {
            div().w(px(120.0)).h(px(300.0)).child(
                self.0
                    .clone()
                    .cached(StyleRefinement::default().size_full()),
            )
        }
    }

    #[gpui::test]
    fn legend_stays_cached_while_the_plot_redraws(cx: &mut TestAppContext) {
        let (tile, cx) = cx.add_window_view(|_, cx| {
            crate::theme::set_theme(cx, Arc::new(crate::theme::DARK.clone()));
            let data = cx.new(|_| Data(0));
            let legend = cx.new(|cx| {
                let mut legend = PlotLegend::new(data.clone(), describe, |_| {});
                let chips: Vec<_> = (0..6)
                    .map(|i| cx.new(|_| Chip(format!("trace number {i}").into())))
                    .collect();
                legend.set(&chips, gpui::Rems(0.0), cx);
                legend
            });
            Tile(cx.new(|_| Plot { data, legend }))
        });
        let draw = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.run_until_parked();
        };
        let (data, legend) = cx.update(|_, cx| {
            let plot = tile.read(cx).0.read(cx);
            (plot.data.clone(), plot.legend.clone())
        });

        // The first frame lays the chips out at the guessed height, the
        // second takes the measured one; after that the legend is settled.
        for _ in 0..3 {
            draw(cx);
        }
        let height = cx.update(|_, cx| legend.read(cx).height);
        assert!(
            height > super::FIRST_GUESS,
            "six chips wrap to several rows in 120px, got {height:?}"
        );

        let settled = DESCRIBED.load(Ordering::Relaxed);
        for n in 1..=5 {
            data.update(cx, |data, cx| {
                data.0 = n;
                cx.notify();
            });
            draw(cx);
        }
        assert_eq!(
            DESCRIBED.load(Ordering::Relaxed),
            settled,
            "the plot redrew five times without the legend rendering"
        );
        assert_eq!(cx.update(|_, cx| legend.read(cx).height), height);
    }
}
