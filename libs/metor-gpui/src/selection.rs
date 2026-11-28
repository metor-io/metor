//! Read-only text the reader can select and copy, as in a browser.
//!
//! A window has at most one selection, kept in a global. It covers a run of [`SelectableText`]s
//! that share a group, such as the lines of a log, in index order. Dragging selects, a double
//! click selects a word and a triple click the whole text. The host calls [`copy`] from its
//! key handler on the primary-modifier `c`.

use std::collections::BTreeMap;
use std::ops::Range;

use gpui::{
    App, Bounds, CursorStyle, DispatchPhase, Element, ElementId, Global, GlobalElementId,
    HighlightStyle, Hitbox, HitboxBehavior, InspectorElementId, IntoElement, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, SharedString, StyledText, Window, fill,
    point, px, size,
};

use crate::theme;

/// One end of a selection: a text's place in its group and a byte offset into it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct End {
    group: ElementId,
    index: usize,
    offset: usize,
}

impl End {
    fn key(&self) -> (usize, usize) {
        (self.index, self.offset)
    }
}

/// The window's selection.
#[derive(Default)]
struct TextSelection {
    /// Where the selection started; `None` when nothing is selected.
    anchor: Option<End>,
    /// Where it ends, following the pointer while dragging.
    head: Option<End>,
    /// Whether the left button went down on a selectable text and is still held.
    dragging: bool,
    /// Whether the pointer moved the head since the button went down, so the release is a drag
    /// rather than a click.
    moved: bool,
    /// The text of every member of the selected group painted since the selection started, so
    /// a copy covers lines that have scrolled out of view.
    texts: BTreeMap<usize, SharedString>,
}

impl Global for TextSelection {}

impl TextSelection {
    /// The selection's ends in group order, when it is not empty.
    fn ordered(&self) -> Option<(&End, &End)> {
        let (anchor, head) = (self.anchor.as_ref()?, self.head.as_ref()?);
        if anchor.group != head.group || anchor == head {
            return None;
        }
        Some(if anchor.key() <= head.key() {
            (anchor, head)
        } else {
            (head, anchor)
        })
    }

    /// The bytes of member `index` of `group` that are selected, and whether the selection runs
    /// on past its end.
    fn range_in(
        &self,
        group: &ElementId,
        index: usize,
        len: usize,
    ) -> Option<(Range<usize>, bool)> {
        let (start, end) = self.ordered()?;
        if &start.group != group || index < start.index || index > end.index {
            return None;
        }
        let from = if index == start.index {
            start.offset
        } else {
            0
        };
        let to = if index == end.index { end.offset } else { len };
        Some((from.min(len)..to.min(len), index < end.index))
    }

    fn selected_text(&self) -> Option<String> {
        let (start, end) = self.ordered()?;
        let mut lines = Vec::new();
        for (&index, text) in self.texts.range(start.index..=end.index) {
            let from = if index == start.index {
                start.offset
            } else {
                0
            };
            let to = if index == end.index {
                end.offset
            } else {
                text.len()
            };
            lines.push(
                text.get(from.min(to)..to.min(text.len()))
                    .unwrap_or_default(),
            );
        }
        Some(lines.join("\n")).filter(|text| !text.is_empty())
    }

    fn start(&mut self, at: End) {
        if self
            .anchor
            .as_ref()
            .is_none_or(|anchor| anchor.group != at.group)
        {
            self.texts.clear();
        }
        self.anchor = Some(at.clone());
        self.head = Some(at);
    }

    fn clear(&mut self) {
        self.anchor = None;
        self.head = None;
        self.texts.clear();
    }
}

/// Copies the selection to the clipboard. Returns whether anything was selected.
pub fn copy(cx: &mut App) -> bool {
    let Some(text) = cx
        .try_global::<TextSelection>()
        .and_then(TextSelection::selected_text)
    else {
        return false;
    };
    cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
    true
}

/// Drops the selection.
pub fn clear(cx: &mut App) {
    if let Some(selection) = cx.try_global::<TextSelection>()
        && selection.anchor.is_some()
    {
        cx.global_mut::<TextSelection>().clear();
        cx.refresh_windows();
    }
}

/// Text the reader can select with the pointer and copy.
///
/// It lays out like [`StyledText`], taking its font, color, wrapping and truncation from the
/// parent. A drag that selects text stops the release there, so a clickable parent isn't
/// clicked as well.
pub struct SelectableText {
    group: ElementId,
    index: usize,
    /// Whether other texts share the group, so the selection may continue past this one.
    grouped: bool,
    text: SharedString,
    styled: StyledText,
}

impl SelectableText {
    /// A text that is selected on its own; `id` must be unique in the window.
    pub fn new(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Self {
        let text = text.into();
        Self {
            group: id.into(),
            index: 0,
            grouped: false,
            styled: StyledText::new(text.clone()),
            text,
        }
    }

    /// Member `index` of `group`. A selection runs across a group's members in index order,
    /// such as the lines of a log.
    pub fn line(group: impl Into<ElementId>, index: usize, text: impl Into<SharedString>) -> Self {
        Self {
            index,
            grouped: true,
            ..Self::new(group, text)
        }
    }

    /// Styles ranges of the text, as [`StyledText::with_highlights`] does.
    pub fn with_highlights(
        mut self,
        highlights: impl IntoIterator<Item = (Range<usize>, HighlightStyle)>,
    ) -> Self {
        self.styled = self.styled.with_highlights(highlights);
        self
    }

    /// Paints the selection behind the bytes `range`, continuing to the right edge when
    /// `continues`.
    fn paint_selection(
        &self,
        range: Range<usize>,
        continues: bool,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &App,
    ) {
        let layout = self.styled.layout();
        let line_height = layout.line_height();
        let color = theme(cx).text_selection;
        let (Some(from), Some(to)) = (
            layout.position_for_index(range.start),
            layout.position_for_index(range.end),
        ) else {
            return;
        };
        let to_x = if continues {
            layout
                .line_layouts()
                .last()
                .map_or(to.x, |line| bounds.left() + line.width())
                + px(4.0)
        } else {
            to.x
        };
        let mut paint = |left: Pixels, top: Pixels, right: Pixels| {
            if right > left {
                window.paint_quad(fill(
                    Bounds::new(point(left, top), size(right - left, line_height)),
                    color,
                ));
            }
        };
        if from.y == to.y {
            paint(from.x, from.y, to_x);
            return;
        }
        // A selection over wrapped lines: the rest of the first, every line between, and the
        // start of the last.
        paint(from.x, from.y, bounds.right());
        let mut y = from.y + line_height;
        while y < to.y {
            paint(bounds.left(), y, bounds.right());
            y += line_height;
        }
        paint(bounds.left(), to.y, to_x);
    }
}

impl Element for SelectableText {
    type RequestLayoutState = ();
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        self.styled.request_layout(None, inspector_id, window, cx)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Hitbox {
        self.styled
            .prepaint(None, inspector_id, bounds, state, window, cx);
        window.insert_hitbox(bounds, HitboxBehavior::Normal)
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut (),
        hitbox: &mut Hitbox,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.set_cursor_style(CursorStyle::IBeam, hitbox);
        let selection = cx.default_global::<TextSelection>();
        let selected = selection.range_in(&self.group, self.index, self.text.len());
        if selection
            .anchor
            .as_ref()
            .is_some_and(|anchor| anchor.group == self.group)
        {
            selection.texts.insert(self.index, self.text.clone());
        }
        if let Some((range, continues)) = selected
            && (!range.is_empty() || continues)
        {
            self.paint_selection(range, continues, bounds, window, cx);
        }
        self.styled
            .paint(None, inspector_id, bounds, state, &mut (), window, cx);
        self.listen(bounds, hitbox, window);
    }
}

impl SelectableText {
    fn listen(&self, bounds: Bounds<Pixels>, hitbox: &Hitbox, window: &mut Window) {
        // Any press drops the old selection; the text under the pointer then starts a new one.
        window.on_mouse_event(|event: &MouseDownEvent, phase, _, cx| {
            if phase == DispatchPhase::Capture
                && event.button == MouseButton::Left
                && cx
                    .try_global::<TextSelection>()
                    .is_some_and(|s| s.anchor.is_some())
            {
                cx.global_mut::<TextSelection>().clear();
            }
        });

        let this = self.clone_for_listeners();
        let hitbox = hitbox.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble
                || event.button != MouseButton::Left
                || !hitbox.is_hovered(window)
            {
                return;
            }
            let offset = this.offset_at(event.position);
            let selection = cx.default_global::<TextSelection>();
            let (from, to) = match event.click_count {
                2 => word_at(&this.text, offset),
                n if n >= 3 => (0, this.text.len()),
                _ => (offset, offset),
            };
            selection.start(this.end(from));
            selection.head = Some(this.end(to));
            selection.texts.insert(this.index, this.text.clone());
            selection.dragging = event.click_count == 1;
            selection.moved = false;
            window.refresh();
        });

        let this = self.clone_for_listeners();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble || event.pressed_button != Some(MouseButton::Left) {
                return;
            }
            let Some(selection) = cx.try_global::<TextSelection>() else {
                return;
            };
            let Some(anchor) = selection.anchor.as_ref().filter(|_| selection.dragging) else {
                return;
            };
            if anchor.group != this.group {
                return;
            }
            let y = event.position.y;
            let offset = if y >= bounds.top() && y < bounds.bottom() {
                this.offset_at(event.position)
            } else if this.grouped || anchor.index != this.index {
                return;
            } else if y < bounds.top() {
                0
            } else {
                this.text.len()
            };
            let head = this.end(offset);
            let selection = cx.global_mut::<TextSelection>();
            if selection.head.as_ref() != Some(&head) {
                selection.head = Some(head);
                selection.moved = true;
                window.refresh();
            }
        });

        window.on_mouse_event(|event: &MouseUpEvent, phase, _, cx| {
            if phase != DispatchPhase::Capture || event.button != MouseButton::Left {
                return;
            }
            let Some(selection) = cx.try_global::<TextSelection>() else {
                return;
            };
            if !selection.dragging {
                return;
            }
            let dragged = selection.moved && selection.ordered().is_some();
            let selection = cx.global_mut::<TextSelection>();
            selection.dragging = false;
            selection.moved = false;
            if dragged {
                cx.stop_propagation();
            }
        });
    }

    /// The parts of `self` the mouse listeners read, since the element is consumed by paint.
    fn clone_for_listeners(&self) -> Listener {
        Listener {
            group: self.group.clone(),
            index: self.index,
            grouped: self.grouped,
            text: self.text.clone(),
            layout: self.styled.layout().clone(),
        }
    }
}

/// What a [`SelectableText`]'s mouse listeners need of it after paint.
struct Listener {
    group: ElementId,
    index: usize,
    grouped: bool,
    text: SharedString,
    layout: gpui::TextLayout,
}

impl Listener {
    fn offset_at(&self, position: gpui::Point<Pixels>) -> usize {
        match self.layout.index_for_position(position) {
            Ok(offset) | Err(offset) => offset.min(self.text.len()),
        }
    }

    fn end(&self, offset: usize) -> End {
        End {
            group: self.group.clone(),
            index: self.index,
            offset,
        }
    }
}

impl IntoElement for SelectableText {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

/// The byte range of the word around `offset`: a run of letters, digits and underscores, or of
/// any other non-space characters.
fn word_at(text: &str, offset: usize) -> (usize, usize) {
    let class = |c: char| {
        if c.is_alphanumeric() || c == '_' {
            1
        } else if c.is_whitespace() {
            0
        } else {
            2
        }
    };
    let offset = offset.min(text.len());
    let Some(at) = text[offset..]
        .chars()
        .next()
        .or_else(|| text[..offset].chars().next_back())
    else {
        return (0, 0);
    };
    let kind = class(at);
    let start = text[..offset]
        .char_indices()
        .rev()
        .take_while(|&(_, c)| class(c) == kind)
        .last()
        .map_or(offset, |(i, _)| i);
    let end = text[offset..]
        .char_indices()
        .find(|&(_, c)| class(c) != kind)
        .map_or(text.len(), |(i, _)| offset + i);
    (start, end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn end(index: usize, offset: usize) -> End {
        End {
            group: "log".into(),
            index,
            offset,
        }
    }

    #[test]
    fn words_are_runs_of_one_class() {
        assert_eq!(word_at("cargo build --release", 2), (0, 5));
        assert_eq!(word_at("cargo build --release", 13), (12, 14));
        assert_eq!(word_at("job_42 done", 6), (6, 7));
        assert_eq!(word_at("abc", 3), (0, 3));
        assert_eq!(word_at("", 0), (0, 0));
    }

    #[test]
    fn a_selection_spans_lines_in_order_whichever_way_it_was_dragged() {
        let mut selection = TextSelection::default();
        selection.start(end(3, 2));
        selection.head = Some(end(1, 4));
        for (index, text) in [(1, "first line"), (2, "second"), (3, "third")] {
            selection.texts.insert(index, text.into());
        }
        assert_eq!(selection.selected_text().unwrap(), "t line\nsecond\nth");
        let group = ElementId::from("log");
        assert_eq!(selection.range_in(&group, 1, 10), Some((4..10, true)));
        assert_eq!(selection.range_in(&group, 2, 6), Some((0..6, true)));
        assert_eq!(selection.range_in(&group, 3, 5), Some((0..2, false)));
        assert_eq!(selection.range_in(&group, 4, 5), None);
    }

    #[test]
    fn an_empty_selection_copies_nothing() {
        let mut selection = TextSelection::default();
        selection.start(end(1, 2));
        selection.texts.insert(1, "text".into());
        assert_eq!(selection.selected_text(), None);
    }
}
