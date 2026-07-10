use gpui::{
    App, ClipboardItem, Hsla, IntoElement, KeyDownEvent, Pixels, SharedString, Styled, TextRun,
    canvas, fill, point, px, size,
};

use crate::theme::{Theme, theme};

fn cursor_width() -> Pixels {
    px(1.5)
}

fn scroll_margin() -> Pixels {
    px(4.0)
}

/// Visual styling bundled so themes can tweak a text field's appearance
/// without touching its input logic.
pub struct TextFieldStyle {
    pub font_size: Pixels,
    pub line_height: Pixels,
    pub text_color: Hsla,
    pub placeholder_color: Hsla,
    pub cursor_color: Hsla,
    pub selection_color: Hsla,
}

impl TextFieldStyle {
    fn from_theme(theme: &Theme) -> Self {
        Self {
            font_size: px(12.0),
            line_height: px(20.0),
            text_color: theme.text_primary,
            placeholder_color: theme.text_tertiary,
            cursor_color: theme.text_primary,
            selection_color: theme.text_selection,
        }
    }
}

/// Minimal single-line text editor used inside the inspector.
///
/// Implements cursor movement, selection, standard shortcuts (Cmd-A/C/X/V,
/// emacs-style Ctrl-A/E/F/B/D/K on macOS) and clipboard access without
/// depending on gpui's heavier text-input widget. Clients drive repaint
/// after each [`handle_key_down`].
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum TextAlign {
    #[default]
    Left,
    Right,
}

pub struct TextField {
    pub text: String,
    pub cursor: usize,
    pub mark: usize,
    placeholder: String,
    style: TextFieldStyle,
    align: TextAlign,
}

impl TextField {
    pub fn new(placeholder: impl Into<String>, cx: &App) -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            mark: 0,
            placeholder: placeholder.into(),
            style: TextFieldStyle::from_theme(&theme(cx)),
            align: TextAlign::Left,
        }
    }

    pub fn set_placeholder(&mut self, placeholder: impl Into<String>) {
        self.placeholder = placeholder.into();
    }

    pub fn set_align(&mut self, align: TextAlign) {
        self.align = align;
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.mark = 0;
    }

    pub fn has_selection(&self) -> bool {
        self.mark != self.cursor
    }

    fn selection_range(&self) -> std::ops::Range<usize> {
        let start = self.mark.min(self.cursor);
        let end = self.mark.max(self.cursor);
        start..end
    }

    fn selected_text(&self) -> &str {
        &self.text[self.selection_range()]
    }

    fn prev_char_boundary(&self, offset: usize) -> usize {
        let mut o = offset.saturating_sub(1);
        while o > 0 && !self.text.is_char_boundary(o) {
            o -= 1;
        }
        o
    }

    fn next_char_boundary(&self, offset: usize) -> usize {
        let mut o = (offset + 1).min(self.text.len());
        while o < self.text.len() && !self.text.is_char_boundary(o) {
            o += 1;
        }
        o
    }

    fn delete_selection(&mut self) {
        let range = self.selection_range();
        self.text.replace_range(range.clone(), "");
        self.cursor = range.start;
        self.mark = self.cursor;
    }

    fn insert_text(&mut self, s: &str) {
        if self.has_selection() {
            self.delete_selection();
        }
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
        self.mark = self.cursor;
    }

    fn copy(&self, cx: &mut App) {
        if self.has_selection() {
            cx.write_to_clipboard(ClipboardItem::new_string(self.selected_text().to_string()));
        }
    }

    fn cut(&mut self, cx: &mut App) {
        if self.has_selection() {
            cx.write_to_clipboard(ClipboardItem::new_string(self.selected_text().to_string()));
            self.delete_selection();
        }
    }

    fn paste(&mut self, cx: &mut App) {
        if let Some(item) = cx.read_from_clipboard()
            && let Some(text) = item.text()
        {
            let text = text.replace('\n', "");
            self.insert_text(&text);
        }
    }

    fn move_cursor(&mut self, offset: usize, extend_selection: bool) {
        self.cursor = offset;
        if !extend_selection {
            self.mark = self.cursor;
        }
    }

    fn move_to_start(&mut self, extend_selection: bool) {
        self.move_cursor(0, extend_selection);
    }

    fn move_to_end(&mut self, extend_selection: bool) {
        self.move_cursor(self.text.len(), extend_selection);
    }

    fn move_left(&mut self, extend_selection: bool) {
        if !extend_selection && self.has_selection() {
            let start = self.selection_range().start;
            self.move_cursor(start, false);
        } else {
            self.move_cursor(self.prev_char_boundary(self.cursor), extend_selection);
        }
    }

    fn move_right(&mut self, extend_selection: bool) {
        if !extend_selection && self.has_selection() {
            let end = self.selection_range().end;
            self.move_cursor(end, false);
        } else {
            self.move_cursor(self.next_char_boundary(self.cursor), extend_selection);
        }
    }

    /// Dispatch a key event. Returns `true` when the field consumed it.
    ///
    /// `false` means the key did nothing visible and the host can interpret
    /// it (e.g. empty-field backspace pops the inspector page).
    pub fn handle_key_down(&mut self, event: &KeyDownEvent, cx: &mut App) -> bool {
        let key = event.keystroke.key.as_str();
        let mods = &event.keystroke.modifiers;

        // cmd on macOS, ctrl elsewhere — `platform` off-macOS is the
        // Win/Super key, which the OS mostly reserves for itself.
        let primary = if cfg!(target_os = "macos") {
            mods.platform
        } else {
            mods.control
        };

        if primary {
            match key {
                "a" => {
                    self.mark = 0;
                    self.cursor = self.text.len();
                    return true;
                }
                "c" => {
                    self.copy(cx);
                    return true;
                }
                "x" => {
                    self.cut(cx);
                    return true;
                }
                "v" => {
                    self.paste(cx);
                    return true;
                }
                "left" => {
                    self.move_to_start(mods.shift);
                    return true;
                }
                "right" => {
                    self.move_to_end(mods.shift);
                    return true;
                }
                "backspace" => {
                    if self.has_selection() {
                        self.delete_selection();
                    } else if self.cursor > 0 {
                        self.text.replace_range(0..self.cursor, "");
                        self.cursor = 0;
                        self.mark = 0;
                    }
                    return true;
                }
                _ => return false,
            }
        }

        #[cfg(target_os = "macos")]
        if mods.control {
            match key {
                "a" => {
                    self.move_to_start(mods.shift);
                    return true;
                }
                "e" => {
                    self.move_to_end(mods.shift);
                    return true;
                }
                "f" => {
                    self.move_right(mods.shift);
                    return true;
                }
                "b" => {
                    self.move_left(mods.shift);
                    return true;
                }
                "d" => {
                    if self.has_selection() {
                        self.delete_selection();
                    } else if self.cursor < self.text.len() {
                        let next = self.next_char_boundary(self.cursor);
                        self.text.replace_range(self.cursor..next, "");
                    }
                    return true;
                }
                "k" => {
                    if self.cursor < self.text.len() {
                        let killed = self.text[self.cursor..].to_string();
                        cx.write_to_clipboard(ClipboardItem::new_string(killed));
                        self.text.truncate(self.cursor);
                        self.mark = self.cursor;
                    }
                    return true;
                }
                _ => return false,
            }
        }

        match key {
            "backspace" => {
                if self.has_selection() {
                    self.delete_selection();
                } else if self.cursor > 0 {
                    let prev = self.prev_char_boundary(self.cursor);
                    self.text.replace_range(prev..self.cursor, "");
                    self.cursor = prev;
                    self.mark = self.cursor;
                } else {
                    // Empty field: bubble so host can pop the page.
                    return false;
                }
                true
            }
            "delete" => {
                if self.has_selection() {
                    self.delete_selection();
                } else if self.cursor < self.text.len() {
                    let next = self.next_char_boundary(self.cursor);
                    self.text.replace_range(self.cursor..next, "");
                }
                true
            }
            "left" => {
                self.move_left(mods.shift);
                true
            }
            "right" => {
                self.move_right(mods.shift);
                true
            }
            _ => {
                if let Some(ch) = &event.keystroke.key_char
                    && !mods.control
                    && !mods.alt
                {
                    self.insert_text(ch);
                    return true;
                }
                false
            }
        }
    }

    pub fn element(&self) -> impl IntoElement {
        let text = self.text.clone();
        let placeholder = self.placeholder.clone();
        let cursor = self.cursor;
        let mark = self.mark;
        let font_size = self.style.font_size;
        let line_height = self.style.line_height;
        let text_color = self.style.text_color;
        let placeholder_color = self.style.placeholder_color;
        let cursor_color = self.style.cursor_color;
        let selection_color = self.style.selection_color;
        let align = self.align;

        canvas(
            move |bounds, window, _cx| {
                let text_style = window.text_style();
                let font = text_style.font();

                let (is_placeholder, display_text, color) = if text.is_empty() {
                    (true, placeholder.clone(), placeholder_color)
                } else {
                    (false, text.clone(), text_color)
                };

                let run = TextRun {
                    len: display_text.len(),
                    font,
                    color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };

                let shaped = window.text_system().shape_line(
                    SharedString::from(display_text),
                    font_size,
                    &[run],
                    None,
                );

                (bounds, shaped, is_placeholder)
            },
            move |_, (bounds, shaped, is_placeholder), window, _cx| {
                let y_offset = (bounds.size.height - line_height).max(px(0.0)) / 2.0;
                let base_y = bounds.origin.y + y_offset;

                let cursor_x = if is_placeholder {
                    px(0.0)
                } else {
                    shaped.x_for_index(cursor)
                };

                // Horizontal scroll follows the cursor so long values stay
                // legible within the field's visible width.
                let visible_width = bounds.size.width;
                let scroll_offset = if cursor_x > visible_width - scroll_margin() {
                    cursor_x - visible_width + scroll_margin()
                } else {
                    px(0.0)
                };
                let max_scroll = (shaped.width - visible_width).max(px(0.0));
                let scroll_offset = scroll_offset.min(max_scroll).max(px(0.0));

                // When right-aligned and the text fits, anchor it to the
                // right edge. If it overflows the field, fall back to the
                // left-anchored cursor-follow so the cursor stays visible.
                let align_offset = match align {
                    TextAlign::Left => px(0.0),
                    TextAlign::Right => (visible_width - shaped.width).max(px(0.0)),
                };

                let text_origin = point(bounds.origin.x + align_offset - scroll_offset, base_y);

                window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
                    if !is_placeholder && mark != cursor {
                        let sel_start = mark.min(cursor);
                        let sel_end = mark.max(cursor);
                        let sel_x_start = shaped.x_for_index(sel_start);
                        let sel_x_end = shaped.x_for_index(sel_end);

                        let sel_bounds = gpui::Bounds::new(
                            point(
                                bounds.origin.x + align_offset + sel_x_start - scroll_offset,
                                base_y,
                            ),
                            size(sel_x_end - sel_x_start, line_height),
                        );
                        window.paint_quad(fill(sel_bounds, selection_color));
                    }

                    let _ = shaped.paint(text_origin, line_height, window, _cx);

                    let cursor_screen_x = bounds.origin.x + align_offset + cursor_x
                        - scroll_offset
                        - cursor_width() / 2.0;
                    let cursor_bounds = gpui::Bounds::new(
                        point(cursor_screen_x, base_y),
                        size(cursor_width(), line_height),
                    );
                    window.paint_quad(fill(cursor_bounds, cursor_color));
                });
            },
        )
        .w_full()
        .h(line_height)
    }
}
