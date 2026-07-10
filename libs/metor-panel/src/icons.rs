use std::borrow::Cow;

use gpui::{AssetSource, Hsla, Result, SharedString, Styled, Svg, px, svg};

/// [`AssetSource`] for the icon set compiled into the binary.
///
/// Installed once via `Application::with_assets` so `svg()` elements can
/// reference icons by their `icons/...` path without filesystem access.
pub struct IconAssets;

impl AssetSource for IconAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let bytes: Option<&'static [u8]> = match path {
            "icons/add.svg" => Some(include_bytes!("../assets/icons/add.svg")),
            "icons/chart.svg" => Some(include_bytes!("../assets/icons/chart.svg")),
            "icons/chevron_down.svg" => Some(include_bytes!("../assets/icons/chevron_down.svg")),
            "icons/chevron_left.svg" => Some(include_bytes!("../assets/icons/chevron_left.svg")),
            "icons/chevron_right.svg" => Some(include_bytes!("../assets/icons/chevron_right.svg")),
            "icons/chevron_up.svg" => Some(include_bytes!("../assets/icons/chevron_up.svg")),
            "icons/close.svg" => Some(include_bytes!("../assets/icons/close.svg")),
            "icons/container.svg" => Some(include_bytes!("../assets/icons/container.svg")),
            "icons/entity.svg" => Some(include_bytes!("../assets/icons/entity.svg")),
            "icons/exit-fullscreen.svg" => {
                Some(include_bytes!("../assets/icons/exit-fullscreen.svg"))
            }
            "icons/folder.svg" => Some(include_bytes!("../assets/icons/folder.svg")),
            "icons/frame_back.svg" => Some(include_bytes!("../assets/icons/frame_back.svg")),
            "icons/frame_forward.svg" => Some(include_bytes!("../assets/icons/frame_forward.svg")),
            "icons/fullscreen.svg" => Some(include_bytes!("../assets/icons/fullscreen.svg")),
            "icons/ip-addr.svg" => Some(include_bytes!("../assets/icons/ip-addr.svg")),
            "icons/jump_to_end.svg" => Some(include_bytes!("../assets/icons/jump_to_end.svg")),
            "icons/jump_to_start.svg" => Some(include_bytes!("../assets/icons/jump_to_start.svg")),
            "icons/left-side-bar.svg" => Some(include_bytes!("../assets/icons/left-side-bar.svg")),
            "icons/lightning.svg" => Some(include_bytes!("../assets/icons/lightning.svg")),
            "icons/link.svg" => Some(include_bytes!("../assets/icons/link.svg")),
            "icons/lock.svg" => Some(include_bytes!("../assets/icons/lock.svg")),
            "icons/lock_open.svg" => Some(include_bytes!("../assets/icons/lock_open.svg")),
            "icons/loop.svg" => Some(include_bytes!("../assets/icons/loop.svg")),
            "icons/maximize.svg" => Some(include_bytes!("../assets/icons/maximize.svg")),
            "icons/pause.svg" => Some(include_bytes!("../assets/icons/pause.svg")),
            "icons/play.svg" => Some(include_bytes!("../assets/icons/play.svg")),
            "icons/plot.svg" => Some(include_bytes!("../assets/icons/plot.svg")),
            "icons/restore.svg" => Some(include_bytes!("../assets/icons/restore.svg")),
            "icons/right-side-bar.svg" => {
                Some(include_bytes!("../assets/icons/right-side-bar.svg"))
            }
            "icons/scrub.svg" => Some(include_bytes!("../assets/icons/scrub.svg")),
            "icons/search.svg" => Some(include_bytes!("../assets/icons/search.svg")),
            "icons/setting.svg" => Some(include_bytes!("../assets/icons/setting.svg")),
            "icons/subtract.svg" => Some(include_bytes!("../assets/icons/subtract.svg")),
            "icons/tile_3d_viewer.svg" => {
                Some(include_bytes!("../assets/icons/tile_3d_viewer.svg"))
            }
            "icons/tile_graph.svg" => Some(include_bytes!("../assets/icons/tile_graph.svg")),
            "icons/vertical-chevrons.svg" => {
                Some(include_bytes!("../assets/icons/vertical-chevrons.svg"))
            }
            "icons/viewport.svg" => Some(include_bytes!("../assets/icons/viewport.svg")),
            _ => None,
        };
        Ok(bytes.map(Cow::Borrowed))
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        Ok(vec![])
    }
}

/// Typed handle for every icon bundled with the panel.
///
/// Use variants instead of raw asset paths so the compiler catches renames
/// and removed assets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Add,
    Chart,
    ChevronDown,
    ChevronLeft,
    ChevronRight,
    ChevronUp,
    Close,
    Container,
    Entity,
    ExitFullscreen,
    Folder,
    FrameBack,
    FrameForward,
    Fullscreen,
    IpAddr,
    JumpToEnd,
    JumpToStart,
    LeftSideBar,
    Lightning,
    Link,
    Lock,
    LockOpen,
    Loop,
    Maximize,
    Pause,
    Play,
    Plot,
    Restore,
    RightSideBar,
    Scrub,
    Search,
    Setting,
    Subtract,
    Tile3dViewer,
    TileGraph,
    VerticalChevrons,
    Viewport,
}

impl Icon {
    fn asset_path(self) -> &'static str {
        match self {
            Icon::Add => "icons/add.svg",
            Icon::Chart => "icons/chart.svg",
            Icon::ChevronDown => "icons/chevron_down.svg",
            Icon::ChevronLeft => "icons/chevron_left.svg",
            Icon::ChevronRight => "icons/chevron_right.svg",
            Icon::ChevronUp => "icons/chevron_up.svg",
            Icon::Close => "icons/close.svg",
            Icon::Container => "icons/container.svg",
            Icon::Entity => "icons/entity.svg",
            Icon::ExitFullscreen => "icons/exit-fullscreen.svg",
            Icon::Folder => "icons/folder.svg",
            Icon::FrameBack => "icons/frame_back.svg",
            Icon::FrameForward => "icons/frame_forward.svg",
            Icon::Fullscreen => "icons/fullscreen.svg",
            Icon::IpAddr => "icons/ip-addr.svg",
            Icon::JumpToEnd => "icons/jump_to_end.svg",
            Icon::JumpToStart => "icons/jump_to_start.svg",
            Icon::LeftSideBar => "icons/left-side-bar.svg",
            Icon::Lightning => "icons/lightning.svg",
            Icon::Link => "icons/link.svg",
            Icon::Lock => "icons/lock.svg",
            Icon::LockOpen => "icons/lock_open.svg",
            Icon::Loop => "icons/loop.svg",
            Icon::Maximize => "icons/maximize.svg",
            Icon::Pause => "icons/pause.svg",
            Icon::Play => "icons/play.svg",
            Icon::Plot => "icons/plot.svg",
            Icon::Restore => "icons/restore.svg",
            Icon::RightSideBar => "icons/right-side-bar.svg",
            Icon::Scrub => "icons/scrub.svg",
            Icon::Search => "icons/search.svg",
            Icon::Setting => "icons/setting.svg",
            Icon::Subtract => "icons/subtract.svg",
            Icon::Tile3dViewer => "icons/tile_3d_viewer.svg",
            Icon::TileGraph => "icons/tile_graph.svg",
            Icon::VerticalChevrons => "icons/vertical-chevrons.svg",
            Icon::Viewport => "icons/viewport.svg",
        }
    }

    /// Build an [`Svg`] sized in pixels and tinted with the dark theme's
    /// secondary-text color. Callers that need a different tint should use
    /// [`Icon::svg_color`].
    pub fn svg(self, size: f32) -> Svg {
        svg()
            .path(SharedString::from(self.asset_path()))
            .size(px(size))
            .text_color(crate::theme::DARK.text_secondary)
    }

    /// Build an [`Svg`] sized in pixels that inherits the surrounding text
    /// color, so an ancestor's hover style can recolor the glyph.
    pub fn svg_inherit(self, size: f32) -> Svg {
        svg()
            .path(SharedString::from(self.asset_path()))
            .size(px(size))
    }

    /// Build an [`Svg`] sized in pixels and tinted with an explicit color.
    pub fn svg_color(self, size: f32, color: Hsla) -> Svg {
        svg()
            .path(SharedString::from(self.asset_path()))
            .size(px(size))
            .text_color(color)
    }
}
