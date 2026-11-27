//! The bundled icon set.

use std::borrow::Cow;

use gpui::{AssetSource, Hsla, Result, SharedString, Styled, Svg, svg};

use crate::ui_px;

/// [`AssetSource`] for the icon set compiled into the binary.
///
/// Install it with `Application::with_assets` so `svg()` elements can reference icons by their
/// `icons/...` path without filesystem access. gpui takes one asset source per app, so an app
/// with assets of its own serves these from its source through [`IconAssets::load`].
pub struct IconAssets;

impl AssetSource for IconAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(Icon::ALL
            .iter()
            .find(|icon| icon.asset_path() == path)
            .map(|icon| Cow::Borrowed(icon.bytes())))
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        Ok(vec![])
    }
}

/// Declares [`Icon`] from one list, so a variant, its asset path and its bytes can't disagree.
macro_rules! icons {
    ($($variant:ident => $file:literal,)*) => {
        /// Typed handle for every bundled icon.
        ///
        /// Use variants instead of raw asset paths so the compiler catches renames and removed
        /// assets.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum Icon {
            $($variant,)*
        }

        impl Icon {
            /// Every icon, in declaration order.
            pub const ALL: &[Icon] = &[$(Icon::$variant,)*];

            /// The path [`IconAssets`] serves the icon at.
            pub fn asset_path(self) -> &'static str {
                match self {
                    $(Icon::$variant => concat!("icons/", $file, ".svg"),)*
                }
            }

            fn bytes(self) -> &'static [u8] {
                match self {
                    $(Icon::$variant => include_bytes!(concat!("../assets/icons/", $file, ".svg")),)*
                }
            }
        }
    };
}

icons! {
    Abort => "abort",
    Add => "add",
    Chart => "chart",
    ChevronDown => "chevron_down",
    ChevronLeft => "chevron_left",
    ChevronRight => "chevron_right",
    ChevronUp => "chevron_up",
    Close => "close",
    Container => "container",
    Dashboard => "dashboard",
    Dot => "dot",
    Entity => "entity",
    ExitFullscreen => "exit-fullscreen",
    Folder => "folder",
    FrameBack => "frame_back",
    Gauge => "gauge",
    FrameForward => "frame_forward",
    Fullscreen => "fullscreen",
    IpAddr => "ip-addr",
    JumpToEnd => "jump_to_end",
    JumpToStart => "jump_to_start",
    Label => "label",
    LeftSideBar => "left-side-bar",
    Lightning => "lightning",
    Link => "link",
    Lock => "lock",
    LockOpen => "lock_open",
    Loop => "loop",
    Outline => "outline",
    Maximize => "maximize",
    Monitor => "monitor",
    Pause => "pause",
    Play => "play",
    Plot => "plot",
    Restore => "restore",
    RightSideBar => "right-side-bar",
    Scrub => "scrub",
    SplitDown => "split-down",
    SplitRight => "split-right",
    SkipForward => "skip-forward",
    Search => "search",
    Setting => "setting",
    Stop => "stop",
    Subtract => "subtract",
    Tile3dViewer => "tile_3d_viewer",
    TileGraph => "tile_graph",
    VerticalChevrons => "vertical-chevrons",
    Viewport => "viewport",
    Favorite => "favorite",
    Check => "check",
    Bell => "bell",
    BellOff => "bell-off",
    Job => "job",
    Runner => "runner",
    Webhook => "webhook",
    Queue => "queue",
    Dag => "dag",
    Gantt => "gantt",
    Cancel => "cancel",
    Rerun => "rerun",
    Log => "log",
}

impl Icon {
    /// Build an [`Svg`] sized in pixels and tinted with `color`.
    pub fn svg(self, size: f32, color: Hsla) -> Svg {
        svg()
            .path(SharedString::new_static(self.asset_path()))
            .size(ui_px(size))
            .text_color(color)
    }
}

#[cfg(test)]
mod tests {
    use gpui::AssetSource;

    use super::{Icon, IconAssets};

    #[test]
    fn every_icon_is_bundled() {
        for &icon in Icon::ALL {
            let bytes = IconAssets.load(icon.asset_path()).unwrap();
            assert!(bytes.is_some_and(|b| b.starts_with(b"<svg")), "{icon:?}");
        }
    }
}
