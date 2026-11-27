//! The native blurred backdrop behind a window's translucent sidebar: the
//! splash's, and a workspace window's side tabs. AppKit draws it beneath
//! gpui's surface, so whatever gpui paints opaque masks it and only the
//! translucent sidebar shows it through.
use gpui::Window;
use objc2::{MainThreadMarker, MainThreadOnly, rc::Retained};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
    NSAutoresizingMaskOptions, NSView, NSVisualEffectBlendingMode, NSVisualEffectMaterial,
    NSVisualEffectState, NSVisualEffectView, NSWindowOrderingMode,
};
use wgpu::rwh::{HasWindowHandle, RawWindowHandle};

fn render_view(window: &Window) -> Result<&NSView, String> {
    let handle = HasWindowHandle::window_handle(window).map_err(|error| error.to_string())?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return Err("Window has no AppKit view".into());
    };
    // GPUI supplies a live NSView, borrowed only while its Window is alive.
    Ok(unsafe { &*handle.ns_view.as_ptr().cast::<NSView>() })
}

/// The panel's theme can differ from the system appearance inherited by
/// AppKit, which otherwise gives a dark panel a pale sidebar.
fn match_theme(effect: &NSVisualEffectView, theme: &metor_gpui::Theme) -> Result<(), String> {
    let name = unsafe {
        if theme.bg_elevated.l < 0.5 {
            NSAppearanceNameDarkAqua
        } else {
            NSAppearanceNameAqua
        }
    };
    let appearance =
        NSAppearance::appearanceNamed(name).ok_or("Could not resolve the native appearance")?;
    effect.setAppearance(Some(&appearance));
    Ok(())
}

/// Install once during window creation, on a window opened with a
/// transparent background.
pub(crate) fn install(window: &Window, theme: &metor_gpui::Theme) -> Result<(), String> {
    let main_thread = MainThreadMarker::new().ok_or("The backdrop requires the AppKit thread")?;
    let render_view = render_view(window)?;
    let container =
        unsafe { render_view.superview() }.ok_or("Render view has no native container")?;
    let effect = NSVisualEffectView::initWithFrame(
        NSVisualEffectView::alloc(main_thread),
        container.bounds(),
    );
    match_theme(&effect, theme)?;

    // Leave AppKit's material layers intact; GPUI's built-in BlurredView
    // overrides updateLayer and strips parts of the native effect.
    effect.setMaterial(NSVisualEffectMaterial::Sidebar);
    effect.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
    effect.setState(NSVisualEffectState::Active);
    effect.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );

    // The container retains the view after the local Retained is dropped.
    container.addSubview_positioned_relativeTo(
        &effect,
        NSWindowOrderingMode::Below,
        Some(render_view),
    );
    Ok(())
}

/// Follow a theme switch, which can cross between light and dark.
pub(crate) fn retheme(window: &Window, theme: &metor_gpui::Theme) {
    let Some(container) = render_view(window)
        .ok()
        .and_then(|view| unsafe { view.superview() })
    else {
        return;
    };
    for view in container.subviews() {
        if let Ok(effect) = Retained::downcast::<NSVisualEffectView>(view) {
            let _ = match_theme(&effect, theme);
        }
    }
}
