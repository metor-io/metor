//! A small text editor that is also the window's input handler.

use std::cell::Cell;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use gpui::{
    App, Bounds, ClipboardItem, FocusHandle, Hsla, InputHandler, IntoElement, KeyDownEvent, Pixels,
    Point, SharedString, Styled, TextRun, UTF16Selection, WeakEntity, Window, canvas, fill, point,
    px, size,
};

use crate::{Theme, theme};

fn cursor_width() -> Pixels {
    px(1.5)
}

fn scroll_margin() -> Pixels {
    px(4.0)
}

/// Colors and metrics a [`TextField`] paints with, taken from the theme.
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

/// Which edge a [`TextField`]'s text sits against while it fits.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum TextAlign {
    #[default]
    Left,
    Right,
}

/// A small text editor, single-line unless made [`multiline`](Self::multiline).
///
/// It handles cursor movement, selection, the standard shortcuts (Cmd-A/C/X/V, and Emacs-style
/// Ctrl-A/E/F/B/D/K on macOS) and the clipboard. The host view feeds it keys through
/// [`handle_key_down`](Self::handle_key_down) and repaints after each. Drawn with
/// [`input_element`](Self::input_element), it is also the window's input handler, so input
/// methods compose into it and, on the web, paste reaches it.
pub struct TextField {
    /// The contents.
    pub text: String,
    /// The cursor, as a byte offset into `text`.
    pub cursor: usize,
    /// The other end of the selection; equal to `cursor` when nothing is selected.
    pub mark: usize,
    placeholder: String,
    style: TextFieldStyle,
    align: TextAlign,
    /// Newlines are text rather than a submit gesture, and the field paints every line instead
    /// of scrolling one horizontally.
    multiline: bool,
    /// Byte ranges to underline and their colors, such as compiler diagnostics.
    pub marks: Vec<(std::ops::Range<usize>, Hsla)>,
    /// First visible line, kept following the cursor.
    scroll_line: usize,
    /// Lines that fit, as the last paint measured them. Written from the
    /// prepaint pass and read by [`follow_cursor`](Self::follow_cursor), so
    /// scrolling follows the height the field actually has rather than one
    /// the caller guessed.
    rows: Arc<AtomicUsize>,
    /// Where the caret's baseline landed, in window coordinates, as the last
    /// paint placed it — the anchor a completion popup hangs from. Packed
    /// f32 bits so the paint closure can write it without a lock.
    caret_px: Arc<AtomicU64>,
    /// Text an input method is composing, as a byte range of `text`. It is replaced when the
    /// composition commits or changes.
    marked: Option<Range<usize>>,
    /// Whether the last paint registered this field as the window's input handler, through
    /// [`input_element`](Self::input_element). Pasting then arrives as a paste event on the web.
    platform_input: Cell<bool>,
}

impl TextField {
    /// An empty field showing `placeholder`, styled from the current theme.
    pub fn new(placeholder: impl Into<String>, cx: &App) -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            mark: 0,
            placeholder: placeholder.into(),
            style: TextFieldStyle::from_theme(&theme(cx)),
            align: TextAlign::Left,
            multiline: false,
            marks: Vec::new(),
            scroll_line: 0,
            rows: Arc::new(AtomicUsize::new(1)),
            caret_px: Arc::new(AtomicU64::new(0)),
            marked: None,
            platform_input: Cell::new(false),
        }
    }

    /// Where the caret sat below its line at the last paint, in window
    /// coordinates — the anchor for a popup that follows typing. A frame
    /// stale at worst, which is invisible under a keystroke.
    pub fn caret_position(&self) -> Point<Pixels> {
        let packed = self.caret_px.load(Ordering::Relaxed);
        let x = f32::from_bits((packed >> 32) as u32);
        let y = f32::from_bits(packed as u32);
        point(px(x), px(y))
    }

    /// Turn the field into an editor: Enter inserts a newline, Up and Down
    /// move by line, and paste keeps the newlines it was given.
    pub fn multiline(mut self) -> Self {
        self.multiline = true;
        self
    }

    /// Replaces the text, keeping the cursor where it still fits.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.cursor.min(self.text.len());
        self.mark = self.cursor;
    }

    /// Byte offset each line starts at, first included and last excluded —
    /// the index every line-wise movement is computed against.
    fn line_starts(&self) -> Vec<usize> {
        let mut starts = vec![0];
        starts.extend(
            self.text
                .bytes()
                .enumerate()
                .filter(|(_, b)| *b == b'\n')
                .map(|(i, _)| i + 1),
        );
        starts
    }

    fn line_of(&self, offset: usize) -> usize {
        self.line_starts()
            .iter()
            .rposition(|start| *start <= offset)
            .unwrap_or(0)
    }

    fn line_bounds(&self, line: usize) -> std::ops::Range<usize> {
        let starts = self.line_starts();
        let start = starts.get(line).copied().unwrap_or(self.text.len());
        let end = starts
            .get(line + 1)
            .map(|next| next - 1)
            .unwrap_or(self.text.len());
        start..end
    }

    /// Move the cursor `delta` lines, holding the column where the line is
    /// long enough and clamping to its end where it is not.
    fn move_line(&mut self, delta: isize, extend_selection: bool) {
        let line = self.line_of(self.cursor);
        let column = self.cursor - self.line_bounds(line).start;
        let target = match delta < 0 {
            true => line.saturating_sub(delta.unsigned_abs()),
            false => (line + delta as usize).min(self.line_starts().len() - 1),
        };
        let bounds = self.line_bounds(target);
        let mut at = (bounds.start + column).min(bounds.end);
        while at > bounds.start && !self.text.is_char_boundary(at) {
            at -= 1;
        }
        self.move_cursor(at, extend_selection);
    }

    /// Replaces the text shown while the field is empty.
    pub fn set_placeholder(&mut self, placeholder: impl Into<String>) {
        self.placeholder = placeholder.into();
    }

    /// Sets which edge the text sits against.
    pub fn set_align(&mut self, align: TextAlign) {
        self.align = align;
    }

    /// Removes all text.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.mark = 0;
    }

    /// Whether any text is selected.
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
            let text = match self.multiline {
                true => text,
                false => text.replace('\n', ""),
            };
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

        let primary = crate::primary_modifier(mods);

        // Line-wise movement replaces document-wise where there are lines to
        // move through; the document ends stay on cmd-up and cmd-down.
        if self.multiline {
            match (primary, key) {
                (_, "enter") if !mods.shift => {
                    self.insert_text("\n");
                    return true;
                }
                (false, "up") => {
                    self.move_line(-1, mods.shift);
                    return true;
                }
                (false, "down") => {
                    self.move_line(1, mods.shift);
                    return true;
                }
                (true, "up") => {
                    self.move_to_start(mods.shift);
                    return true;
                }
                (true, "down") => {
                    self.move_to_end(mods.shift);
                    return true;
                }
                (true, "left") => {
                    let start = self.line_bounds(self.line_of(self.cursor)).start;
                    self.move_cursor(start, mods.shift);
                    return true;
                }
                (true, "right") => {
                    let end = self.line_bounds(self.line_of(self.cursor)).end;
                    self.move_cursor(end, mods.shift);
                    return true;
                }
                _ => {}
            }
        }

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
                    // On the web the clipboard can only be read from the browser's paste event,
                    // which reaches the input handler, so the key is left to the browser.
                    if cfg!(target_family = "wasm") && self.platform_input.get() {
                        return false;
                    }
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
                    let at = match self.multiline {
                        true => self.line_bounds(self.line_of(self.cursor)).start,
                        false => 0,
                    };
                    self.move_cursor(at, mods.shift);
                    return true;
                }
                "e" => {
                    let at = match self.multiline {
                        true => self.line_bounds(self.line_of(self.cursor)).end,
                        false => self.text.len(),
                    };
                    self.move_cursor(at, mods.shift);
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
                    let end = match self.multiline {
                        true => self.line_bounds(self.line_of(self.cursor)).end,
                        false => self.text.len(),
                    };
                    if self.cursor < end {
                        let killed = self.text[self.cursor..end].to_string();
                        cx.write_to_clipboard(ClipboardItem::new_string(killed));
                        self.text.replace_range(self.cursor..end, "");
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

    /// Keep the cursor's line inside the window the last paint measured.
    pub fn follow_cursor(&mut self) {
        let line = self.line_of(self.cursor);
        let rows = self.rows.load(Ordering::Relaxed).max(1);
        if line < self.scroll_line {
            self.scroll_line = line;
        } else if line >= self.scroll_line + rows {
            self.scroll_line = line + 1 - rows;
        }
    }

    /// Paint every line, with the selection, the cursor, and any diagnostic
    /// underlines placed against the same shaped runs.
    ///
    /// Each line is shaped on its own, so a column is an `x_for_index` into
    /// that line rather than a measurement of the whole document — which is
    /// what keeps a thousand-line module from being reshaped per frame.
    pub fn lines_element(&self) -> impl IntoElement {
        let text = self.text.clone();
        let placeholder = self.placeholder.clone();
        let cursor = self.cursor;
        let mark = self.mark;
        let scroll = self.scroll_line;
        let marks = self.marks.clone();
        let font_size = self.style.font_size;
        let line_height = self.style.line_height;
        let text_color = self.style.text_color;
        let placeholder_color = self.style.placeholder_color;
        let cursor_color = self.style.cursor_color;
        let selection_color = self.style.selection_color;
        let rows_seen = self.rows.clone();
        let caret_seen = self.caret_px.clone();

        canvas(
            move |bounds, window, _cx| {
                let font = window.text_style().font();
                let showing_placeholder = text.is_empty();
                let body = if showing_placeholder {
                    placeholder.clone()
                } else {
                    text.clone()
                };
                let color = if showing_placeholder {
                    placeholder_color
                } else {
                    text_color
                };

                let rows = (f32::from(bounds.size.height) / f32::from(line_height)).ceil() as usize;
                rows_seen.store(rows.max(1), Ordering::Relaxed);
                let mut shaped = Vec::with_capacity(rows);
                let mut at = 0usize;
                for (index, line) in body.split('\n').enumerate() {
                    let start = at;
                    at += line.len() + 1;
                    if index < scroll || shaped.len() >= rows {
                        continue;
                    }
                    let run = TextRun {
                        len: line.len(),
                        font: font.clone(),
                        color,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    };
                    shaped.push((
                        start,
                        line.len(),
                        window.text_system().shape_line(
                            SharedString::from(line.to_string()),
                            font_size,
                            &[run],
                            None,
                        ),
                    ));
                }
                (bounds, shaped, showing_placeholder)
            },
            move |_, (bounds, shaped, showing_placeholder), window, cx| {
                window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
                    for (row, (start, len, line)) in shaped.iter().enumerate() {
                        let y = bounds.origin.y + line_height * row as f32;
                        let origin = point(bounds.origin.x, y);
                        let x_at = |offset: usize| {
                            line.x_for_index(offset.saturating_sub(*start).min(*len))
                        };
                        let overlaps = |range: &std::ops::Range<usize>| {
                            range.start <= start + len && range.end >= *start
                        };

                        if !showing_placeholder && mark != cursor {
                            let selection = mark.min(cursor)..mark.max(cursor);
                            if overlaps(&selection) {
                                // A selection running past this line's end
                                // shows the newline it swallowed, so a
                                // multi-line drag reads as one region.
                                let to = match selection.end > start + len {
                                    true => line.width + px(4.0),
                                    false => x_at(selection.end),
                                };
                                let from = x_at(selection.start);
                                window.paint_quad(fill(
                                    gpui::Bounds::new(
                                        point(origin.x + from, y),
                                        size(to - from, line_height),
                                    ),
                                    selection_color,
                                ));
                            }
                        }

                        for (range, color) in &marks {
                            if !overlaps(range) || range.start == range.end {
                                continue;
                            }
                            let from = x_at(range.start);
                            let to = x_at(range.end).max(from + px(2.0));
                            window.paint_quad(fill(
                                gpui::Bounds::new(
                                    point(origin.x + from, y + line_height - px(2.0)),
                                    size(to - from, px(1.5)),
                                ),
                                *color,
                            ));
                        }

                        let _ = line.paint(
                            origin,
                            line_height,
                            gpui::TextAlign::Left,
                            None,
                            window,
                            cx,
                        );

                        if !showing_placeholder && (*start..=start + len).contains(&cursor) {
                            let caret_x = origin.x + x_at(cursor);
                            let packed = ((f32::from(caret_x).to_bits() as u64) << 32)
                                | f32::from(y + line_height).to_bits() as u64;
                            caret_seen.store(packed, Ordering::Relaxed);
                            window.paint_quad(fill(
                                gpui::Bounds::new(
                                    point(caret_x - cursor_width() / 2.0, y),
                                    size(cursor_width(), line_height),
                                ),
                                cursor_color,
                            ));
                        }
                    }
                });
            },
        )
        .size_full()
    }

    /// The field as an element that takes keys only through its host's key handler.
    pub fn element(&self) -> impl IntoElement {
        self.platform_input.set(false);
        self.render_element(None)
    }

    /// The field as an element that is also the window's input handler while `focus` is
    /// focused, so input methods can compose text into it and, on the web, paste reaches it.
    ///
    /// `view` is the host, which says through [`TextFieldHost`] which field input goes to. Only
    /// that field should be drawn this way, since a window has one input handler.
    pub fn input_element<V: TextFieldHost>(
        &self,
        focus: &FocusHandle,
        view: WeakEntity<V>,
    ) -> impl IntoElement {
        self.platform_input.set(true);
        let handle = focus.clone();
        let register: RegisterInput = Box::new(move |bounds, window, cx| {
            window.handle_input(&handle, TextFieldInput { view, bounds }, cx);
        });
        self.render_element(Some((focus.clone(), register)))
    }

    fn render_element(&self, input: Option<(FocusHandle, RegisterInput)>) -> impl IntoElement {
        let text = self.text.clone();
        let marked = self.marked.clone();
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
            move |_, (bounds, shaped, is_placeholder), window, cx| {
                // A field drawn as the input handler shows its caret only while focused; one drawn
                // plainly is drawn by its host only while it is being edited.
                let show_caret = input
                    .as_ref()
                    .is_none_or(|(focus, _)| focus.is_focused(window));
                if let Some((_, register)) = input {
                    register(bounds, window, cx);
                }
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

                    let _ = shaped.paint(
                        text_origin,
                        line_height,
                        gpui::TextAlign::Left,
                        None,
                        window,
                        cx,
                    );

                    if let Some(marked) = marked.filter(|_| !is_placeholder) {
                        let start = shaped.x_for_index(marked.start);
                        let end = shaped.x_for_index(marked.end);
                        let underline = gpui::Bounds::new(
                            point(text_origin.x + start, base_y + line_height - px(2.0)),
                            size(end - start, px(1.0)),
                        );
                        window.paint_quad(fill(underline, cursor_color));
                    }

                    if show_caret {
                        let cursor_screen_x = bounds.origin.x + align_offset + cursor_x
                            - scroll_offset
                            - cursor_width() / 2.0;
                        let cursor_bounds = gpui::Bounds::new(
                            point(cursor_screen_x, base_y),
                            size(cursor_width(), line_height),
                        );
                        window.paint_quad(fill(cursor_bounds, cursor_color));
                    }
                });
            },
        )
        .w_full()
        .h(line_height)
    }
}

/// Registers a field's input handler from its paint, with the field's bounds.
type RegisterInput = Box<dyn FnOnce(Bounds<Pixels>, &mut Window, &mut App)>;

/// The platform text-input protocol, which counts in UTF-16 code units.
impl TextField {
    fn offset_to_utf16(&self, offset: usize) -> usize {
        self.text[..offset.min(self.text.len())]
            .chars()
            .map(char::len_utf16)
            .sum()
    }

    fn offset_from_utf16(&self, utf16: usize) -> usize {
        let mut units = 0;
        for (offset, ch) in self.text.char_indices() {
            if units >= utf16 {
                return offset;
            }
            units += ch.len_utf16();
        }
        self.text.len()
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }

    /// Text without newlines unless the field is multiline, as typing and pasting insert it.
    fn accepted(&self, text: &str) -> String {
        match self.multiline {
            true => text.to_string(),
            false => text.replace('\n', ""),
        }
    }

    /// The selection, in UTF-16 units.
    pub fn selected_text_range(&self) -> UTF16Selection {
        UTF16Selection {
            range: self.range_to_utf16(&self.selection_range()),
            reversed: self.cursor < self.mark,
        }
    }

    /// The text an input method is composing, in UTF-16 units.
    pub fn marked_text_range(&self) -> Option<Range<usize>> {
        self.marked.as_ref().map(|range| self.range_to_utf16(range))
    }

    /// The text in `range_utf16`, clamped to the field.
    pub fn text_for_range(
        &self,
        range_utf16: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
    ) -> String {
        let range = self.range_from_utf16(&range_utf16);
        *adjusted = Some(self.range_to_utf16(&range));
        self.text[range].to_string()
    }

    /// Replace `range_utf16`, or the composition, or else the selection, with `text`, and end
    /// any composition.
    pub fn replace_text_in_range(&mut self, range_utf16: Option<Range<usize>>, text: &str) {
        let range = range_utf16
            .map(|range| self.range_from_utf16(&range))
            .or(self.marked.take())
            .unwrap_or_else(|| self.selection_range());
        let text = self.accepted(text);
        self.text.replace_range(range.clone(), &text);
        self.cursor = range.start + text.len();
        self.mark = self.cursor;
        self.marked = None;
    }

    /// Like [`replace_text_in_range`](Self::replace_text_in_range), but the new text stays
    /// marked as a composition, with `selected_utf16` (relative to it) selected.
    pub fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        selected_utf16: Option<Range<usize>>,
    ) {
        let range = range_utf16
            .map(|range| self.range_from_utf16(&range))
            .or(self.marked.clone())
            .unwrap_or_else(|| self.selection_range());
        let text = self.accepted(text);
        self.text.replace_range(range.clone(), &text);
        self.marked = (!text.is_empty()).then(|| range.start..range.start + text.len());
        let start_utf16 = self.offset_to_utf16(range.start);
        match selected_utf16 {
            Some(selected) => {
                self.mark = self.offset_from_utf16(start_utf16 + selected.start);
                self.cursor = self.offset_from_utf16(start_utf16 + selected.end);
            }
            None => {
                self.cursor = range.start + text.len();
                self.mark = self.cursor;
            }
        }
    }

    /// Commit the composition as ordinary text.
    pub fn unmark_text(&mut self) {
        self.marked = None;
    }
}

/// Connects a [`TextField`] inside a host view to the window's text input, so input methods
/// and, on the web, paste can edit it.
struct TextFieldInput<V: TextFieldHost> {
    view: WeakEntity<V>,
    /// Where the field was painted, which input methods place their candidate window by.
    bounds: Bounds<Pixels>,
}

/// A view that holds a [`TextField`] drawn with [`TextField::input_element`].
pub trait TextFieldHost: Sized + 'static {
    /// The field platform input edits now, if any.
    fn text_field(&mut self) -> Option<&mut TextField>;

    /// Called after platform input changed the field's text, as the host's own key handler
    /// would after a keystroke.
    fn text_field_changed(&mut self, _cx: &mut gpui::Context<Self>) {}
}

impl<V: TextFieldHost> TextFieldInput<V> {
    /// Run `f` on the host's field. When `edits`, tell the host its text changed.
    fn with_field<R>(
        &self,
        edits: bool,
        cx: &mut App,
        f: impl FnOnce(&mut TextField) -> R,
    ) -> Option<R> {
        self.view
            .update(cx, |view, cx| {
                let result = view.text_field().map(f);
                if edits && result.is_some() {
                    view.text_field_changed(cx);
                    cx.notify();
                }
                result
            })
            .ok()
            .flatten()
    }
}

impl<V: TextFieldHost> InputHandler for TextFieldInput<V> {
    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        cx: &mut App,
    ) -> Option<UTF16Selection> {
        self.with_field(false, cx, |field| field.selected_text_range())
    }

    fn marked_text_range(&mut self, _window: &mut Window, cx: &mut App) -> Option<Range<usize>> {
        self.with_field(false, cx, |field| field.marked_text_range())
            .flatten()
    }

    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Option<String> {
        self.with_field(false, cx, |field| {
            field.text_for_range(range_utf16, adjusted_range)
        })
    }

    fn replace_text_in_range(
        &mut self,
        replacement_range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut App,
    ) {
        self.with_field(true, cx, |field| {
            field.replace_text_in_range(replacement_range, text)
        });
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut App,
    ) {
        self.with_field(true, cx, |field| {
            field.replace_and_mark_text_in_range(range_utf16, new_text, new_selected_range)
        });
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut App) {
        self.with_field(true, cx, TextField::unmark_text);
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Option<Bounds<Pixels>> {
        // The caret's last painted position stands in for the range: input methods only use it
        // to place their candidate window beside the text.
        let caret = self.with_field(false, cx, |field| field.caret_position())?;
        let line = self.bounds.size.height;
        Some(Bounds::new(
            point(caret.x, caret.y - line),
            size(px(1.0), line),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<usize> {
        None
    }
}

#[cfg(test)]
mod input_tests {
    use super::*;

    fn field(text: &str, cx: &App) -> TextField {
        let mut field = TextField::new("", cx);
        field.set_text(text);
        field
    }

    #[gpui::test]
    fn composition_replaces_itself_and_commits(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::install_themes(cx, Vec::new(), None);
            let mut field = field("ab", cx);
            field.cursor = 1;
            field.mark = 1;
            field.replace_and_mark_text_in_range(None, "k", Some(1..1));
            assert_eq!(field.text, "akb");
            assert_eq!(field.marked_text_range(), Some(1..2));
            field.replace_and_mark_text_in_range(None, "かな", Some(2..2));
            assert_eq!(field.text, "aかなb");
            assert_eq!(field.marked_text_range(), Some(1..3));
            field.replace_text_in_range(None, "仮名");
            assert_eq!(field.text, "a仮名b");
            assert_eq!(field.marked_text_range(), None);
            assert_eq!(field.selected_text_range().range, 3..3);
        });
    }

    #[gpui::test]
    fn ranges_count_utf16_units(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::install_themes(cx, Vec::new(), None);
            let mut field = field("a😀b", cx);
            let mut adjusted = None;
            assert_eq!(field.text_for_range(1..3, &mut adjusted), "😀");
            assert_eq!(adjusted, Some(1..3));
            field.replace_text_in_range(Some(3..4), "c\nd");
            assert_eq!(field.text, "a😀cd", "a single-line field drops newlines");
        });
    }
}
