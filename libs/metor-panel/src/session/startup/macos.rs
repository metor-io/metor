//! Native backdrop for the splash's translucent sidebar.
use gpui::Window;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
    NSAutoresizingMaskOptions, NSView, NSVisualEffectBlendingMode, NSVisualEffectMaterial,
    NSVisualEffectState, NSVisualEffectView, NSWindowOrderingMode,
};
use wgpu::rwh::{HasWindowHandle, RawWindowHandle};

/// Install once during window creation. The native hierarchy owns the effect
/// view; GPUI's opaque connections surface masks the backdrop on the right.
pub(super) fn install_sidebar_blur(
    window: &Window,
    theme: &crate::theme::Theme,
) -> Result<(), String> {
    let main_thread = MainThreadMarker::new().ok_or("Splash blur requires the AppKit thread")?;
    let handle = HasWindowHandle::window_handle(window).map_err(|error| error.to_string())?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return Err("Splash window has no AppKit view".into());
    };

    // GPUI supplies a live NSView, borrowed only while its Window is alive.
    let render_view = unsafe { &*handle.ns_view.as_ptr().cast::<NSView>() };
    let container =
        unsafe { render_view.superview() }.ok_or("Splash render view has no native container")?;
    let effect = NSVisualEffectView::initWithFrame(
        NSVisualEffectView::alloc(main_thread),
        container.bounds(),
    );

    // The panel's theme can differ from the system appearance inherited
    // by AppKit, which otherwise gives a dark panel a pale sidebar.
    let appearance_name = unsafe {
        if theme.bg_elevated.l < 0.5 {
            NSAppearanceNameDarkAqua
        } else {
            NSAppearanceNameAqua
        }
    };
    let appearance = NSAppearance::appearanceNamed(appearance_name)
        .ok_or("Could not resolve the splash's native appearance")?;
    effect.setAppearance(Some(&appearance));

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
