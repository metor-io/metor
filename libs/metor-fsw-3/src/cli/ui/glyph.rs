//! The glyphs the renderers draw with.

pub const OK: &str = "✓";
pub const FAIL: &str = "✗";
pub const WARN: &str = "▲";
pub const RUNNING: &str = "●";
pub const STOPPED: &str = "■";
pub const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
pub const ELLIPSIS: &str = "…";
pub const FROM: &str = "←";
pub const LINK: &str = "⇄";
pub const DOT: &str = "·";
pub const AGAIN: &str = "↻";
pub const BULLET: &str = "•";

/// Box-drawing characters for `graph`.
pub mod line {
    pub const HORIZONTAL: char = '─';
    pub const VERTICAL: char = '│';
    pub const DOWN_RIGHT: char = '╭';
    pub const DOWN_LEFT: char = '╮';
    pub const UP_RIGHT: char = '╰';
    pub const UP_LEFT: char = '╯';
    pub const TEE_RIGHT: char = '├';
    pub const TEE_LEFT: char = '┤';
    pub const CROSS: char = '┼';
    pub const TEE_UP: char = '┴';
    pub const TEE_DOWN: char = '┬';
    pub const ARROW: char = '▶';
    pub const OUT: char = '▸';
}
