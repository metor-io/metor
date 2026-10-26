//! What an empty pane shows: a few large buttons for the views people open
//! first, styled like the connection picker's favourite cards.
//!
//! Outline and Dashboard need no setup and land straight away. Time Series
//! needs its traces picked first, which takes the database and a window, so
//! the pane asks the app to open that wizard ([`PaneEvent::NewTimeSeries`]).
use gpui::{AnyElement, Context, SharedString, div, prelude::*, px};

use super::pane::{Pane, PaneEvent};
use crate::icons::Icon;
use crate::theme::theme;

/// One button: what it opens, and how.
struct Starter {
    label: &'static str,
    detail: &'static str,
    icon: Icon,
    open: fn(&mut Pane, &mut Context<Pane>),
}

const STARTERS: [Starter; 3] = [
    Starter {
        label: "Outline",
        detail: "Browse every component",
        icon: Icon::Outline,
        open: |pane, cx| pane.add_registered("component_outline", cx),
    },
    Starter {
        label: "Time Series",
        detail: "Plot values over time",
        icon: Icon::Plot,
        open: |_, cx| cx.emit(PaneEvent::NewTimeSeries),
    },
    Starter {
        label: "Dashboard",
        detail: "Lay out instruments",
        icon: Icon::Dashboard,
        open: |pane, cx| pane.add_registered("dashboard", cx),
    },
];

impl Pane {
    /// Add a registered view with its default config as a new tab.
    pub(super) fn add_registered(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(registry) = cx.try_global::<super::ItemRegistry>().cloned() else {
            return;
        };
        if let Some(item) = registry.deserialize(key, "{}", cx) {
            self.add_item(item, cx);
        }
    }

    pub(super) fn render_starters(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx);
        let mut cards = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .justify_center()
            .gap(px(10.0));
        for (ix, starter) in STARTERS.iter().enumerate() {
            let open = starter.open;
            cards = cards.child(
                div()
                    .id(("pane-starter", ix))
                    .debug_selector(move || format!("pane-starter-{ix}"))
                    .w(px(150.0))
                    .p(px(12.0))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(6.0))
                    .bg(theme.bg_secondary)
                    .border_1()
                    .border_color(theme.border_primary)
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.bg_elevated))
                    .on_click(cx.listener(move |pane, _, _, cx| open(pane, cx)))
                    .child(starter.icon.svg(24.0, theme.text_secondary))
                    .child(
                        div()
                            .text_size(px(13.0))
                            .text_color(theme.text_primary)
                            .child(SharedString::new_static(starter.label)),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.text_tertiary)
                            .child(SharedString::new_static(starter.detail)),
                    ),
            );
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(14.0))
            .p(px(16.0))
            .child(cards)
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme.text_tertiary)
                    .child(SharedString::new_static(
                        "More views in the command palette",
                    )),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use gpui::{App, Entity, IntoElement, Render, Window};

    use super::*;
    use crate::tiles::{ItemRegistry, PaneItem, PaneItemHandle};

    #[derive(serde::Serialize, serde::Deserialize, Default)]
    struct StubConfig {}

    struct Stub(&'static str);

    impl Render for Stub {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    impl PaneItem for Stub {
        type Config = StubConfig;

        fn tab_title(&self, _: &App) -> SharedString {
            SharedString::new_static(self.0)
        }

        fn serialization_key() -> &'static str {
            "stub"
        }

        fn to_config(&self, _: &App) -> StubConfig {
            StubConfig {}
        }
    }

    fn click_starter(selector: &'static str, cx: &mut gpui::VisualTestContext) {
        let bounds = cx.debug_bounds(selector).expect("starter is drawn");
        cx.simulate_click(bounds.center(), Default::default());
    }

    #[gpui::test]
    fn an_empty_pane_offers_the_common_views(cx: &mut gpui::TestAppContext) {
        let (pane, cx) = cx.add_window_view(|_, cx| {
            crate::theme::set_theme(cx, std::sync::Arc::new(crate::theme::DARK.clone()));
            let mut registry = ItemRegistry::default();
            for key in ["component_outline", "dashboard"] {
                registry.register_erased(key, move |_, cx| {
                    Some(Box::new(cx.new(|_| Stub(key))) as Box<dyn PaneItemHandle>)
                });
            }
            cx.set_global(registry);
            Pane::new(Vec::new(), cx)
        });
        let asked = Rc::new(Cell::new(false));
        let _subscription = cx.update(|_, cx| {
            let asked = asked.clone();
            cx.subscribe(&pane, move |_: Entity<Pane>, event: &PaneEvent, _| {
                if matches!(event, PaneEvent::NewTimeSeries) {
                    asked.set(true);
                }
            })
        });
        cx.run_until_parked();
        let titles = |cx: &mut gpui::VisualTestContext| -> Vec<SharedString> {
            cx.update(|_, cx| {
                pane.read(cx)
                    .items()
                    .iter()
                    .map(|item| item.tab_title(cx))
                    .collect()
            })
        };

        click_starter("pane-starter-1", cx);
        assert!(asked.get(), "time series asks the app for its wizard");
        assert!(titles(cx).is_empty());

        click_starter("pane-starter-0", cx);
        assert_eq!(titles(cx), ["component_outline"]);
        // A pane with a tab shows that tab, not the starters.
        assert!(cx.debug_bounds("pane-starter-2").is_none());
    }
}
