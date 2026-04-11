//! Minimal 3D viewer example. Opens a window containing a single
//! [`Viewer3d`], which boots the shared Bevy bridge, spawns a test scene
//! (directional light + blue cube), and blits rendered frames into GPUI.
//!
//! Useful for smoke-testing the bridge without standing up a full DB-backed
//! metor-panel session.

use std::sync::Arc;

use gpui::{
    App, Application, Bounds, Context, Entity, IntoElement, Render, WindowBounds, WindowOptions,
    div, prelude::*, px, size,
};
use metor_panel::elements::viewer_3d::Viewer3d;
use metor_panel::theme::{ActiveTheme, DARK};

struct Root {
    left: Entity<Viewer3d>,
    right: Entity<Viewer3d>,
}

impl Root {
    fn new(cx: &mut Context<Self>) -> Self {
        let left = cx.new(Viewer3d::new);
        let right = cx.new(Viewer3d::new);
        // Optional: set METOR_VIEWER3D_GLB=<path> to load a GLTF file into
        // the left viewer. The right viewer keeps showing the default cube,
        // which helps verify render-layer isolation.
        if let Ok(path) = std::env::var("METOR_VIEWER3D_GLB") {
            left.read(cx).load_gltf(path);
        }
        Self { left, right }
    }
}

impl Render for Root {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        _cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .size_full()
            .bg(gpui::rgb(0x0a0a10))
            .flex()
            .flex_row()
            .child(div().flex_1().child(self.left.clone()))
            .child(div().w(px(1.0)).bg(gpui::rgb(0x1a1a24)))
            .child(div().flex_1().child(self.right.clone()))
    }
}

fn main() {
    Application::new()
        .with_assets(metor_panel::icons::IconAssets)
        .run(move |cx: &mut App| {
            cx.set_global(ActiveTheme(Arc::new(DARK.clone())));
            let bounds = Bounds::centered(None, size(px(800.0), px(600.0)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |_window, cx| cx.new(Root::new),
            )
            .unwrap();
            cx.activate(true);
        });
}
