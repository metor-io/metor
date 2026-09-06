use super::*;
use std::{cell::Cell, rc::Rc, time::Duration};

struct Host {
    inspector: gpui::Entity<Inspector>,
    focus: FocusHandle,
    clicks: Rc<Cell<usize>>,
}

impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let clicks = self.clicks.clone();
        div()
            .id("motion-test-host")
            .relative()
            .size_full()
            .track_focus(&self.focus)
            .child(div().absolute().size_full().on_mouse_down(
                gpui::MouseButton::Left,
                move |_, _, _| {
                    clicks.set(clicks.get() + 1);
                },
            ))
            .child(self.inspector.clone())
    }
}

#[gpui::test]
fn exiting_menu_releases_pointer_input_and_does_not_steal_new_focus(cx: &mut gpui::TestAppContext) {
    let changes = Rc::new(Cell::new(0));
    let clicks = Rc::new(Cell::new(0));
    let (host, cx) = cx.add_window_view(|window, cx| {
        crate::theme::set_theme(cx, Arc::new(crate::theme::DARK.clone()));
        let focus = cx.focus_handle();
        let inspector = cx.new(|cx| {
            let changes = changes.clone();
            let mut inspector = Inspector::new(
                vec![Box::new(rows::BoolRow::new(
                    "Toggle",
                    false,
                    Arc::new(move |_, _, _| {
                        changes.set(changes.get() + 1);
                    }),
                ))],
                InspectorMode::Anchored(gpui::point(px(40.0), px(40.0))),
                cx,
            );
            inspector.set_parent_focus(focus.clone());
            inspector.fade.finish();
            inspector
        });
        inspector.focus_handle(cx).focus(window);
        Host {
            inspector,
            focus,
            clicks: clicks.clone(),
        }
    });
    cx.refresh().unwrap();
    let inspector = cx.update(|_, cx| host.read(cx).inspector.clone());
    let row_position = cx.update(|_, cx| {
        let bounds = inspector.read(cx).panel_bounds.unwrap();
        gpui::point(bounds.origin.x + px(20.0), bounds.bottom() - px(14.0))
    });
    cx.simulate_click(row_position, gpui::Modifiers::default());
    assert_eq!(changes.get(), 1, "open row receives clicks");
    assert_eq!(clicks.get(), 0, "open menu occludes the background");

    let next_focus = cx.update(|window, cx| {
        inspector.update(cx, |inspector, cx| {
            inspector.dismiss(window, cx);
            // Keep this test in the exiting phase irrespective of machine speed.
            inspector.fade = Fade::settled(1.0);
            inspector.fade.exit(Duration::from_secs(60));
        });
        let focus = cx.focus_handle();
        host.update(cx, |host, cx| {
            host.focus = focus.clone();
            cx.notify();
        });
        focus.focus(window);
        focus
    });
    cx.refresh().unwrap();
    cx.simulate_click(row_position, gpui::Modifiers::default());
    cx.simulate_mouse_move(
        row_position,
        Some(gpui::MouseButton::Left),
        gpui::Modifiers::default(),
    );
    assert_eq!(
        changes.get(),
        1,
        "closing checkbox cannot toggle or drag-paint"
    );
    assert_eq!(
        clicks.get(),
        1,
        "closing visual passes clicks to the background"
    );

    cx.update(|_, cx| {
        inspector.update(cx, |inspector, cx| {
            assert!(!inspector.exit_complete);
            inspector.fade.finish();
            cx.notify();
        })
    });
    cx.refresh().unwrap();
    cx.update(|window, cx| {
        assert!(inspector.read(cx).exit_complete);
        assert!(
            next_focus.is_focused(window),
            "visual cleanup must not move focus"
        );
    });
}

#[gpui::test]
fn command_executes_once_even_when_confirmed_during_entry(cx: &mut gpui::TestAppContext) {
    let count = Rc::new(Cell::new(0));
    let cx = cx.add_empty_window();
    cx.update(|window, cx| {
        crate::theme::set_theme(cx, Arc::new(crate::theme::DARK.clone()));
        let callback_count = count.clone();
        let inspector = cx.new(|cx| {
            Inspector::new(
                vec![Box::new(rows::CommandRow::new(
                    "Run",
                    Arc::new(move |_, _| {
                        callback_count.set(callback_count.get() + 1);
                    }),
                ))],
                InspectorMode::Centered,
                cx,
            )
        });
        inspector.update(cx, |inspector, cx| {
            inspector.confirm(window, cx);
            inspector.confirm(window, cx);
            assert!(inspector.dismissed);
            assert_eq!(count.get(), 1);
        });
    });
}
