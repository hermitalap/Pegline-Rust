//! The note editor's text model: bytes, caret, selection, undo and the pure
//! editing commands (NOTE-EDITOR.md §5.4, §6.4).
//!
//! The source string is the single truth. Every offset is a byte offset into
//! that string and is always kept on a UTF-8 character boundary. The display
//! projection lives in [`super::display`]; nothing here knows about Markdown or
//! pixels, so all of it is unit-testable without a desktop.

use std::ops::Range;

/// How far the caret moves for one key press.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Motion {
    Left,
    Right,
    WordLeft,
    WordRight,
    LineStart,
    LineEnd,
    DocStart,
    DocEnd,
}

/// An edit run that undo coalesces. Consecutive typing within 500 ms is one
/// undo step; a deletion run is its own step.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EditKind {
    Insert,
    Delete,
    Replace,
}

/// One undo step: the whole state before the edit. Notes are small, so a
/// snapshot is cheaper to get right than an inverse-operation log.
#[derive(Clone, Debug)]
struct Snapshot {
    text: String,
    caret: usize,
    anchor: usize,
}

/// Undo depth (NOTE-EDITOR.md §6.4).
const UNDO_LIMIT: usize = 100;
/// Typing inside this window coalesces into one undo step.
const COALESCE_SECONDS: f64 = 0.5;

/// The editable document.
#[derive(Clone, Debug)]
pub struct Doc {
    text: String,
    caret: usize,
    anchor: usize,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    last_edit: EditKind,
    last_edit_at: f64,
}

impl Doc {
    /// A document from file text: BOM-free and with `\n` line endings.
    pub fn new(text: impl Into<String>) -> Self {
        let text = normalize(&text.into());
        let caret = text.len();
        Self {
            text,
            caret,
            anchor: caret,
            undo: Vec::new(),
            redo: Vec::new(),
            last_edit: EditKind::Insert,
            last_edit_at: f64::NEG_INFINITY,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn len(&self) -> usize {
        self.text.len()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// True when the document has no visible character at all.
    pub fn is_blank(&self) -> bool {
        self.text.trim().is_empty()
    }

    pub fn caret(&self) -> usize {
        self.caret
    }

    pub fn anchor(&self) -> usize {
        self.anchor
    }

    /// The non-empty selection, in source byte order.
    pub fn selection(&self) -> Option<Range<usize>> {
        if self.caret == self.anchor {
            return None;
        }
        Some(self.caret.min(self.anchor)..self.caret.max(self.anchor))
    }

    pub fn has_selection(&self) -> bool {
        self.caret != self.anchor
    }

    /// Selects everything.
    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.caret = self.text.len();
    }

    /// Places the caret at `offset`, clamped to a character boundary. With
    /// `extend` the selection anchor stays put (Shift+click / Shift+arrows).
    pub fn set_caret(&mut self, offset: usize, extend: bool) {
        let offset = floor_boundary(&self.text, offset.min(self.text.len()));
        self.caret = offset;
        if !extend {
            self.anchor = offset;
        }
    }

    /// Moves the caret by one [`Motion`].
    pub fn move_caret(&mut self, motion: Motion, extend: bool) {
        let target = match motion {
            Motion::Left => previous_char(&self.text, self.caret),
            Motion::Right => next_char(&self.text, self.caret),
            Motion::WordLeft => word_left(&self.text, self.caret),
            Motion::WordRight => word_right(&self.text, self.caret),
            Motion::LineStart => line_start(&self.text, self.caret),
            Motion::LineEnd => line_end(&self.text, self.caret),
            Motion::DocStart => 0,
            Motion::DocEnd => self.text.len(),
        };
        self.set_caret(target, extend);
    }

    /// The source range of the line the caret sits on, without the newline.
    pub fn caret_line(&self) -> Range<usize> {
        line_range(&self.text, self.caret)
    }

    /// The line containing `offset`, without the newline.
    pub fn line_at(&self, offset: usize) -> Range<usize> {
        line_range(&self.text, offset.min(self.text.len()))
    }

    /// Inserts text, replacing the selection, and moves the caret after it.
    ///
    /// `now` is the wall clock used to coalesce typing into one undo step.
    pub fn insert(&mut self, text: &str, now: f64) {
        let text = normalize(text);
        if text.is_empty() {
            return;
        }
        self.snapshot(EditKind::Insert, now);
        let range = self.selection();
        let at = range
            .as_ref()
            .map(|range| range.start)
            .unwrap_or(self.caret);
        if let Some(range) = range {
            self.text.replace_range(range, &text);
        } else {
            self.text.insert_str(at, &text);
        }
        self.caret = at + text.len();
        self.anchor = self.caret;
    }

    /// Removes the selection, or the character before the caret.
    pub fn backspace(&mut self, now: f64) {
        if let Some(range) = self.selection() {
            self.snapshot(EditKind::Delete, now);
            self.text.replace_range(range.clone(), "");
            self.caret = range.start;
            self.anchor = self.caret;
            return;
        }
        let start = previous_char(&self.text, self.caret);
        if start == self.caret {
            return;
        }
        self.snapshot(EditKind::Delete, now);
        self.text.replace_range(start..self.caret, "");
        self.caret = start;
        self.anchor = start;
    }

    /// Removes the selection, or the character after the caret.
    pub fn delete_forward(&mut self, now: f64) {
        if let Some(range) = self.selection() {
            self.snapshot(EditKind::Delete, now);
            self.text.replace_range(range.clone(), "");
            self.caret = range.start;
            self.anchor = self.caret;
            return;
        }
        let end = next_char(&self.text, self.caret);
        if end == self.caret {
            return;
        }
        self.snapshot(EditKind::Delete, now);
        self.text.replace_range(self.caret..end, "");
        self.anchor = self.caret;
    }

    /// Removes `range` as one edit and puts the caret at its start.
    ///
    /// The list commands use this to drop a whole marker (`- `, `2. `) with one
    /// Backspace or by pressing Enter on an empty item.
    pub fn delete_range(&mut self, range: Range<usize>, now: f64) {
        let start = floor_boundary(&self.text, range.start.min(self.text.len()));
        let end = floor_boundary(&self.text, range.end.min(self.text.len()));
        if start >= end {
            return;
        }
        self.snapshot(EditKind::Delete, now);
        self.text.replace_range(start..end, "");
        self.caret = start;
        self.anchor = start;
    }

    /// The selected source text, or `None` when nothing is selected.
    pub fn selected_text(&self) -> Option<String> {
        self.selection().map(|range| self.text[range].to_string())
    }

    /// Removes and returns the selection (Ctrl+X).
    pub fn cut(&mut self, now: f64) -> Option<String> {
        let text = self.selected_text()?;
        self.backspace(now);
        Some(text)
    }

    /// Inserts clipboard text at the caret (Ctrl+V), replacing a selection.
    pub fn paste(&mut self, text: &str, now: f64) {
        self.insert(text, now);
    }

    /// Wraps the selection in `marker` (Ctrl+B/I/backtick). With no selection
    /// the markers are inserted and the caret sits between them.
    pub fn toggle_marker(&mut self, marker: &str, now: f64) {
        if marker.is_empty() {
            return;
        }
        self.snapshot(EditKind::Replace, now);
        if let Some(range) = self.selection() {
            let inner = self.text[range.clone()].to_string();
            let replacement = format!("{marker}{inner}{marker}");
            self.text.replace_range(range.clone(), &replacement);
            // Keep the original text selected, not the markers.
            self.anchor = range.start + marker.len();
            self.caret = self.anchor + inner.len();
        } else {
            let at = self.caret;
            self.text.insert_str(at, &format!("{marker}{marker}"));
            self.caret = at + marker.len();
            self.anchor = self.caret;
        }
    }

    /// Undoes the last edit run. Returns false when there is nothing to undo.
    pub fn undo(&mut self) -> bool {
        let Some(snapshot) = self.undo.pop() else {
            return false;
        };
        self.push_redo();
        self.restore(snapshot);
        true
    }

    /// Redoes the last undone edit run.
    pub fn redo(&mut self) -> bool {
        let Some(snapshot) = self.redo.pop() else {
            return false;
        };
        self.push_snapshot(Snapshot {
            text: self.text.clone(),
            caret: self.caret,
            anchor: self.anchor,
        });
        self.restore(snapshot);
        true
    }

    fn push_redo(&mut self) {
        self.redo.push(Snapshot {
            text: self.text.clone(),
            caret: self.caret,
            anchor: self.anchor,
        });
    }

    fn restore(&mut self, snapshot: Snapshot) {
        self.text = snapshot.text;
        self.caret = floor_boundary(&self.text, snapshot.caret.min(self.text.len()));
        self.anchor = floor_boundary(&self.text, snapshot.anchor.min(self.text.len()));
        self.last_edit_at = f64::NEG_INFINITY;
    }

    fn snapshot(&mut self, kind: EditKind, now: f64) {
        let coalesce = kind == self.last_edit
            && kind != EditKind::Replace
            && now - self.last_edit_at < COALESCE_SECONDS
            && !self.undo.is_empty();
        if !coalesce {
            self.push_snapshot(Snapshot {
                text: self.text.clone(),
                caret: self.caret,
                anchor: self.anchor,
            });
            self.redo.clear();
        }
        self.last_edit = kind;
        self.last_edit_at = now;
    }

    fn push_snapshot(&mut self, snapshot: Snapshot) {
        self.undo.push(snapshot);
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
    }

    /// Scroll/focus state is not part of the model; this only reports whether a
    /// commit would change the file.
    pub fn differs_from(&self, original: &str) -> bool {
        let original = normalize(original);
        self.text != original
    }
}

/// Normalises line endings and strips a UTF-8 BOM. The file format stays
/// CRLF+BOM (NOTE-EDITOR.md §8.1); the in-memory text is always LF.
pub fn normalize(text: &str) -> String {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            other => out.push(other),
        }
    }
    out
}

/// Converts LF text to the CRLF the note files use.
pub fn to_crlf(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for character in text.chars() {
        if character == '\n' {
            out.push('\r');
        }
        out.push(character);
    }
    out
}

/// The character boundary at or before `offset`.
pub fn floor_boundary(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    let mut offset = offset;
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// The previous character boundary.
pub fn previous_char(text: &str, offset: usize) -> usize {
    let offset = floor_boundary(text, offset);
    if offset == 0 {
        return 0;
    }
    let mut previous = offset - 1;
    while previous > 0 && !text.is_char_boundary(previous) {
        previous -= 1;
    }
    previous
}

/// The next character boundary.
pub fn next_char(text: &str, offset: usize) -> usize {
    let offset = floor_boundary(text, offset);
    if offset >= text.len() {
        return text.len();
    }
    let mut next = offset + 1;
    while next < text.len() && !text.is_char_boundary(next) {
        next += 1;
    }
    next
}

/// Whether `character` counts as part of a word for Ctrl+Left/Right.
fn is_word(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

/// The start of the word before the caret.
pub fn word_left(text: &str, offset: usize) -> usize {
    let mut index = floor_boundary(text, offset);
    // Skip separators, then the word itself.
    while index > 0 {
        let previous = previous_char(text, index);
        let character = text[previous..index].chars().next().unwrap_or(' ');
        if is_word(character) {
            break;
        }
        index = previous;
    }
    while index > 0 {
        let previous = previous_char(text, index);
        let character = text[previous..index].chars().next().unwrap_or(' ');
        if !is_word(character) {
            break;
        }
        index = previous;
    }
    index
}

/// The start of the next word after the caret. From inside a word this lands on
/// the following word, not on the current one's last character.
pub fn word_right(text: &str, offset: usize) -> usize {
    let mut index = floor_boundary(text, offset);
    while index < text.len() {
        let next = next_char(text, index);
        let character = text[index..next].chars().next().unwrap_or(' ');
        if !is_word(character) {
            break;
        }
        index = next;
    }
    while index < text.len() {
        let next = next_char(text, index);
        let character = text[index..next].chars().next().unwrap_or(' ');
        if is_word(character) {
            break;
        }
        index = next;
    }
    index
}

/// The whole word containing `offset`, for a double-click. Returns an empty
/// range when the character at the caret is not a word character.
pub fn word_range(text: &str, offset: usize) -> Range<usize> {
    let offset = floor_boundary(text, offset.min(text.len()));
    let mut start = offset;
    while start > 0 {
        let previous = previous_char(text, start);
        let character = text[previous..start].chars().next().unwrap_or(' ');
        if !is_word(character) {
            break;
        }
        start = previous;
    }
    let mut end = offset;
    while end < text.len() {
        let next = next_char(text, end);
        let character = text[end..next].chars().next().unwrap_or(' ');
        if !is_word(character) {
            break;
        }
        end = next;
    }
    start..end
}

/// Start of the line containing `offset`.
pub fn line_start(text: &str, offset: usize) -> usize {
    let offset = floor_boundary(text, offset);
    text[..offset]
        .rfind('\n')
        .map(|index| index + 1)
        .unwrap_or(0)
}

/// End of the line containing `offset` (before its newline).
pub fn line_end(text: &str, offset: usize) -> usize {
    let offset = floor_boundary(text, offset);
    text[offset..]
        .find('\n')
        .map(|index| offset + index)
        .unwrap_or(text.len())
}

/// The line range around `offset`, excluding the newline.
pub fn line_range(text: &str, offset: usize) -> Range<usize> {
    let start = line_start(text, offset);
    let end = line_end(text, offset);
    start..end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(text: &str) -> Doc {
        Doc::new(text)
    }

    #[test]
    fn normalises_line_endings_and_bom() {
        assert_eq!(normalize("\u{feff}a\r\nb\rc\n"), "a\nb\nc\n");
        assert_eq!(to_crlf("a\nb\n"), "a\r\nb\r\n");
        let doc = make("a\r\nb");
        assert_eq!(doc.text(), "a\nb");
        assert_eq!(doc.caret(), 3);
    }

    #[test]
    fn insertion_and_selection_replacement() {
        let mut doc = make("hello");
        doc.set_caret(0, false);
        doc.insert("well ", 1.0);
        assert_eq!(doc.text(), "well hello");
        assert_eq!(doc.caret(), 5);

        doc.set_caret(0, false);
        doc.set_caret(4, true);
        assert_eq!(doc.selected_text().as_deref(), Some("well"));
        doc.insert("good", 2.0);
        assert_eq!(doc.text(), "good hello");
        assert_eq!(doc.caret(), 4);
        assert!(!doc.has_selection());
    }

    #[test]
    fn caret_moves_on_character_boundaries() {
        let mut doc = make("aé中");
        doc.set_caret(0, false);
        doc.move_caret(Motion::Right, false);
        assert_eq!(doc.caret(), 1);
        doc.move_caret(Motion::Right, false);
        assert_eq!(doc.caret(), 3);
        doc.move_caret(Motion::Right, false);
        assert_eq!(doc.caret(), 6);
        doc.move_caret(Motion::Left, false);
        assert_eq!(doc.caret(), 3);
        // A byte offset inside a character snaps back to its start.
        doc.set_caret(4, false);
        assert_eq!(doc.caret(), 3);
    }

    #[test]
    fn backspace_deletes_whole_characters() {
        let mut doc = make("a中");
        doc.set_caret(doc.len(), false);
        doc.backspace(1.0);
        assert_eq!(doc.text(), "a");
        doc.backspace(2.0);
        assert_eq!(doc.text(), "");
        doc.backspace(3.0);
        assert_eq!(doc.text(), "");
    }

    #[test]
    fn forward_delete_and_word_motion() {
        let mut doc = make("one two three");
        doc.set_caret(0, false);
        assert_eq!(doc.caret(), 0);
        doc.move_caret(Motion::WordRight, false);
        assert_eq!(doc.caret(), 4);
        doc.move_caret(Motion::WordRight, false);
        assert_eq!(doc.caret(), 8);
        doc.move_caret(Motion::WordLeft, false);
        assert_eq!(doc.caret(), 4);
        doc.delete_forward(1.0);
        assert_eq!(doc.text(), "one wo three");
    }

    #[test]
    fn double_click_word_range_covers_alnum_and_underscore() {
        let text = "one two_three four";
        assert_eq!(word_range(text, 1), 0..3);
        assert_eq!(word_range(text, 5), 4..13);
        assert_eq!(word_range(text, 13), 4..13);
        assert_eq!(word_range(text, 15), 14..18);
        assert_eq!(word_range(text, 3), 0..3);
    }

    #[test]
    fn line_and_document_motion() {
        let mut doc = make("alpha\nbeta\ngamma");
        doc.set_caret(8, false); // inside "beta"
        doc.move_caret(Motion::LineStart, false);
        assert_eq!(doc.caret(), 6);
        doc.move_caret(Motion::LineEnd, false);
        assert_eq!(doc.caret(), 10);
        doc.move_caret(Motion::DocEnd, false);
        assert_eq!(doc.caret(), doc.len());
        doc.move_caret(Motion::DocStart, false);
        assert_eq!(doc.caret(), 0);
        assert_eq!(doc.line_at(8), 6..10);
    }

    #[test]
    fn undo_coalesces_typing_and_redo_replays_it() {
        let mut doc = make("");
        doc.insert("a", 0.0);
        doc.insert("b", 0.1);
        doc.insert("c", 0.2);
        assert_eq!(doc.text(), "abc");
        assert!(doc.undo());
        assert_eq!(doc.text(), "");
        assert!(doc.redo());
        assert_eq!(doc.text(), "abc");
        // A pause starts a new undo step.
        let mut doc = make("");
        doc.insert("a", 0.0);
        doc.insert("b", 1.0);
        assert!(doc.undo());
        assert_eq!(doc.text(), "a");
    }

    #[test]
    fn undo_restores_selection_and_redo_is_cleared_by_a_new_edit() {
        let mut doc = make("abcdef");
        doc.set_caret(1, false);
        doc.set_caret(4, true);
        doc.backspace(0.0);
        assert_eq!(doc.text(), "aef");
        assert!(doc.undo());
        assert_eq!(doc.text(), "abcdef");
        assert_eq!(doc.selection(), Some(1..4));
        doc.set_caret(0, false);
        doc.insert("z", 1.0);
        assert!(!doc.redo());
    }

    #[test]
    fn undo_is_limited_to_one_hundred_steps() {
        let mut doc = make("");
        for index in 0..150 {
            doc.insert("x", index as f64 * 2.0);
        }
        let mut steps = 0;
        while doc.undo() {
            steps += 1;
        }
        assert_eq!(steps, UNDO_LIMIT);
    }

    #[test]
    fn marker_toggle_wraps_a_selection_and_places_the_caret_between_markers() {
        let mut doc = make("bold text");
        doc.set_caret(0, false);
        doc.set_caret(4, true);
        doc.toggle_marker("**", 0.0);
        assert_eq!(doc.text(), "**bold** text");
        assert_eq!(doc.selected_text().as_deref(), Some("bold"));

        let mut doc = make("x");
        doc.set_caret(1, false);
        doc.toggle_marker("`", 1.0);
        assert_eq!(doc.text(), "x``");
        assert_eq!(doc.caret(), 2);
    }

    #[test]
    fn cut_returns_source_and_paste_normalises_crlf() {
        let mut doc = make("hello world");
        doc.set_caret(6, false);
        doc.set_caret(11, true);
        assert_eq!(doc.cut(0.0).as_deref(), Some("world"));
        assert_eq!(doc.text(), "hello ");
        doc.paste("a\r\nb", 1.0);
        assert_eq!(doc.text(), "hello a\nb");
    }

    #[test]
    fn delete_range_removes_one_span_and_undo_brings_it_back() {
        let mut doc = make("- one\n- two");
        // Drop the second marker: "- one\n- two" -> "- one\ntwo".
        doc.delete_range(6..8, 0.0);
        assert_eq!(doc.text(), "- one\ntwo");
        assert_eq!(doc.caret(), 6);
        assert!(!doc.has_selection());
        assert!(doc.undo());
        assert_eq!(doc.text(), "- one\n- two");
        // Undo restores the state before the edit, caret included.
        assert_eq!(doc.caret(), doc.len());
        // An empty or out-of-range span is a no-op.
        let before = doc.text().to_string();
        doc.delete_range(3..3, 1.0);
        doc.delete_range(100..120, 1.0);
        assert_eq!(doc.text(), before);
    }

    #[test]
    fn blank_and_diff_checks() {
        let doc = make("  \n\t\n");
        assert!(doc.is_blank());
        let doc = make("hello");
        assert!(!doc.differs_from("hello"));
        assert!(doc.differs_from("hello "));
    }

    #[test]
    fn line_ranges_handle_the_trailing_newline() {
        let text = "a\n\nb";
        assert_eq!(line_range(text, 0), 0..1);
        assert_eq!(line_range(text, 2), 2..2);
        assert_eq!(line_range(text, 3), 3..4);
        let text = "a\n";
        assert_eq!(line_range(text, 2), 2..2);
    }
}
