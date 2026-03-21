use gpui::{
    App, Application, Bounds, Context, SharedString, Window, WindowBounds, WindowOptions, div,
    prelude::*, px, rgb, size,
};

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1280.), px(720.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| {},
        )
        .unwrap();
    });
}
