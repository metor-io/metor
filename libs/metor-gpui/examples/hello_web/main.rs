//! The smallest gpui_web app: one window with styled text. It proves the pinned gpui fork runs
//! single-threaded in a browser.

#![cfg_attr(target_family = "wasm", no_main)]

use gpui::{
    App, Bounds, Context, Window, WindowBounds, WindowOptions, div, prelude::*, px, rgb, size,
};

struct Hello;

impl Render for Hello {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(0x1b1b1f))
            .text_color(rgb(0xe8e8ec))
            .font_family("IoskeleyMono Nerd Font")
            .child("hello from gpui_web")
    }
}

fn run() {
    gpui_platform::application().run(|cx: &mut App| {
        cx.text_system()
            .add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!(
                "../../assets/fonts/IoskeleyMonoNerdFont-Regular.ttf"
            ))])
            .expect("register font");
        let bounds = Bounds::centered(None, size(px(480.), px(320.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| Hello),
        )
        .expect("open window");
        cx.activate(true);
    });
}

#[cfg(not(target_family = "wasm"))]
fn main() {
    run();
}

#[cfg(target_family = "wasm")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn start() {
    gpui_platform::web_init();
    run();
}
