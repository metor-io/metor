//! Minimal 3D viewer example. Opens a window with two side-by-side
//! [`Viewer3d`]s, both wired to the shared Bevy bridge.
//!
//! By default both viewers render an empty clear color — the viewer no
//! longer ships a placeholder cube, models are added explicitly. Set
//! `METOR_VIEWER3D_GLB=<path>` to load a GLTF/GLB into the left viewer
//! programmatically. The right viewer stays empty so you can verify
//! per-viewer render-layer isolation.

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
        // the left viewer.
        if let Ok(path) = std::env::var("METOR_VIEWER3D_GLB") {
            left.update(cx, |viewer, cx| {
                viewer.add_model("env-glb", path, cx);
            });
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
