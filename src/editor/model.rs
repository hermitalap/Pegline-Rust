//! Pure drawing model for the annotation editor.
//!
//! Everything in this module is arithmetic on device pixels: no Win32, no
//! rendering. The window owns a [`Doc`], applies gestures to it and renders it;
//! tests can drive the same operations without a desktop.

/// The annotation tools (BEHAVIOR §11). The eraser removes whole marks under
/// its disc; the mosaic is still available as the fifth tool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tool {
    Arrow,
    Rectangle,
    Text,
    Eraser,
    Mosaic,
}

/// The six pen colours: red, gold, green, blue, black, white.
pub const COLORS: [(u32, &str); 6] = [
    (0xE8_1E_1E, "red"),
    (0xFF_D7_00, "gold"),
    (0x32CD32, "green"),
    (0x1E_90_FF, "blue"),
    (0x00_00_00, "black"),
    (0xFF_FF_FF, "white"),
];

/// The three line widths, device pixels at 100% scale.
pub const WIDTHS: [f32; 3] = [4.0, 8.0, 12.0];

/// One mark on the picture.
#[derive(Clone, Debug, PartialEq)]
pub enum Mark {
    Arrow {
        from: (f32, f32),
        to: (f32, f32),
        color: u32,
        width: f32,
    },
    Rectangle {
        rect: (f32, f32, f32, f32),
        color: u32,
        width: f32,
    },
    Text {
        origin: (f32, f32),
        text: String,
        color: u32,
        size: f32,
    },
    Mosaic {
        rect: (f32, f32, f32, f32),
        /// Device-pixel size of one block in the source image.
        block: u32,
    },
}

/// An IME composition in progress: the pinyin shown inside the text box while
/// the input method has not produced a result yet. It never enters `marks`, the
/// undo history or the saved PNG — it lives only as long as the IME is
/// composing, and only [`Doc::commit_composition`] turns it into text.
#[derive(Clone, Debug, PartialEq)]
pub struct Composition {
    text: String,
    /// Caret offset inside `text`, in bytes.
    cursor: usize,
}

impl Composition {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }
}

/// Squared distance from `point` to the segment `from`–`to`.
pub fn segment_distance_squared(from: (f32, f32), to: (f32, f32), point: (f32, f32)) -> f32 {
    let dx = to.0 - from.0;
    let dy = to.1 - from.1;
    let length_squared = dx * dx + dy * dy;
    if length_squared <= 0.0001 {
        let px = point.0 - from.0;
        let py = point.1 - from.1;
        return px * px + py * py;
    }
    let t = (((point.0 - from.0) * dx + (point.1 - from.1) * dy) / length_squared).clamp(0.0, 1.0);
    let nearest_x = from.0 + dx * t;
    let nearest_y = from.1 + dy * t;
    let px = point.0 - nearest_x;
    let py = point.1 - nearest_y;
    px * px + py * py
}

/// Whether an eraser disc at `point` with `radius` touches `mark`.
///
/// Marks are removed whole, so the test is generous: the shaft's stroke joins
/// the arrow's test, and text is covered by its approximate layout box.
pub fn mark_hits_eraser(mark: &Mark, point: (f32, f32), radius: f32) -> bool {
    match mark {
        Mark::Arrow {
            from, to, width, ..
        } => {
            let reach = radius + width * 0.5;
            segment_distance_squared(*from, *to, point) <= reach * reach
        }
        Mark::Rectangle { rect, width, .. } => {
            let reach = radius + width * 0.5;
            point.0 >= rect.0 - reach
                && point.0 <= rect.0 + rect.2 + reach
                && point.1 >= rect.1 - reach
                && point.1 <= rect.1 + rect.3 + reach
        }
        Mark::Mosaic { rect, .. } => {
            point.0 >= rect.0 - radius
                && point.0 <= rect.0 + rect.2 + radius
                && point.1 >= rect.1 - radius
                && point.1 <= rect.1 + rect.3 + radius
        }
        Mark::Text {
            origin, text, size, ..
        } => {
            // A rough box: the renderer's exact layout is a little wider.
            let width = size * 0.62 * text.chars().count().max(1) as f32;
            point.0 >= origin.0 - radius
                && point.0 <= origin.0 + width + radius
                && point.1 >= origin.1 - radius
                && point.1 <= origin.1 + *size + radius
        }
    }
}

/// The editable document: the source picture plus the marks drawn over it.
pub struct Doc {
    source: Option<crate::types::ImageData>,
    marks: Vec<Mark>,
    /// Whole-document snapshots taken *before* each change. Small documents
    /// (a handful of marks) make snapshots cheaper than per-mark history and
    /// let the eraser remove several marks as one undo step.
    undo: Vec<Vec<Mark>>,
    redo: Vec<Vec<Mark>>,
    /// The text mark currently being typed, if any.
    typing: Option<usize>,
    /// The IME composition shown in that text mark, if any.
    composition: Option<Composition>,
}

/// Undo steps kept; older snapshots are dropped.
const HISTORY_LIMIT: usize = 100;

impl Doc {
    pub fn new(source: crate::types::ImageData) -> Self {
        Self {
            source: Some(source),
            marks: Vec::new(),
            undo: Vec::new(),
            redo: Vec::new(),
            typing: None,
            composition: None,
        }
    }

    pub fn source(&self) -> Option<&crate::types::ImageData> {
        self.source.as_ref()
    }

    pub fn marks(&self) -> &[Mark] {
        &self.marks
    }

    /// Remembers the current marks as an undo step and drops the redo stack.
    fn snapshot(&mut self) {
        self.undo.push(self.marks.clone());
        if self.undo.len() > HISTORY_LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    pub fn push(&mut self, mark: Mark) {
        self.snapshot();
        self.marks.push(mark);
    }

    /// Removes every mark matched by `hit` without recording a step. The
    /// caller takes a snapshot before a live eraser gesture and hands it to
    /// [`Doc::record_erase`] when the gesture ends, so one drag is one undo.
    pub fn erase_marks(&mut self, mut hit: impl FnMut(&Mark) -> bool) -> bool {
        let before = self.marks.len();
        self.marks.retain(|mark| !hit(mark));
        self.marks.len() != before
    }

    /// Records `before` as the undo step for an eraser gesture. Returns false
    /// when nothing was erased.
    pub fn record_erase(&mut self, before: Vec<Mark>) -> bool {
        if before == self.marks {
            return false;
        }
        self.undo.push(before);
        if self.undo.len() > HISTORY_LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
        true
    }

    /// Puts the marks back exactly, without touching the history (a cancelled
    /// eraser gesture).
    pub fn restore_marks(&mut self, marks: Vec<Mark>) {
        self.marks = marks;
    }

    /// Undoes the last change; returns whether anything changed.
    pub fn undo(&mut self) -> bool {
        self.typing = None;
        self.composition = None;
        match self.undo.pop() {
            Some(previous) => {
                let current = std::mem::replace(&mut self.marks, previous);
                self.redo.push(current);
                true
            }
            None => false,
        }
    }

    /// Re-applies the last undone change.
    pub fn redo(&mut self) -> bool {
        self.typing = None;
        self.composition = None;
        match self.redo.pop() {
            Some(next) => {
                let current = std::mem::replace(&mut self.marks, next);
                self.undo.push(current);
                true
            }
            None => false,
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Starts a text mark at `origin` (source pixels) and returns its index.
    pub fn begin_text(&mut self, origin: (f32, f32), color: u32, size: f32) -> usize {
        self.composition = None;
        self.snapshot();
        self.marks.push(Mark::Text {
            origin,
            text: String::new(),
            color,
            size,
        });
        self.typing = Some(self.marks.len() - 1);
        self.marks.len() - 1
    }

    /// Appends a character to the text mark being typed. Ignored while an IME
    /// composition is open: the input method owns the keyboard, and the result
    /// arrives through [`Doc::commit_composition`], so a stray `WM_CHAR` must
    /// not slip in between the pinyin and its result.
    pub fn type_char(&mut self, character: char) -> bool {
        if self.composition.is_some() {
            return false;
        }
        let Some(index) = self.typing else {
            return false;
        };
        match self.marks.get_mut(index) {
            Some(Mark::Text { text, .. }) => {
                text.push(character);
                true
            }
            _ => false,
        }
    }

    pub fn backspace(&mut self) -> bool {
        if self.composition.is_some() {
            return false;
        }
        let Some(index) = self.typing else {
            return false;
        };
        match self.marks.get_mut(index) {
            Some(Mark::Text { text, .. }) => {
                text.pop();
                true
            }
            _ => false,
        }
    }

    /// Ends text entry; an empty text mark is removed together with the undo
    /// step taken when it began, so an empty tap is not an undoable change.
    /// An open composition is dropped: uncommitted pinyin is never baked in.
    pub fn finish_text(&mut self) {
        self.composition = None;
        if let Some(index) = self.typing.take() {
            if let Some(Mark::Text { text, .. }) = self.marks.get(index) {
                if text.trim().is_empty() {
                    self.marks.remove(index);
                    self.undo.pop();
                }
            }
        }
    }

    pub fn is_typing(&self) -> bool {
        self.typing.is_some()
    }

    pub fn is_composing(&self) -> bool {
        self.composition.is_some()
    }

    pub fn composition(&self) -> Option<&Composition> {
        self.composition.as_ref()
    }

    /// Starts or updates the IME composition drawn in the text box. `cursor` is
    /// a byte offset inside `text`; an empty string closes the composition.
    /// Only a text mark being typed can compose.
    pub fn set_composition(&mut self, text: &str, cursor: usize) -> bool {
        if !self.is_typing() {
            return false;
        }
        if text.is_empty() {
            return self.clear_composition();
        }
        let mut cursor = cursor.min(text.len());
        while cursor > 0 && !text.is_char_boundary(cursor) {
            cursor -= 1;
        }
        let next = Composition {
            text: text.to_string(),
            cursor,
        };
        if self.composition.as_ref() == Some(&next) {
            return false;
        }
        self.composition = Some(next);
        true
    }

    pub fn clear_composition(&mut self) -> bool {
        self.composition.take().is_some()
    }

    /// Commits the input method's result string into the text mark and closes
    /// the composition. This is the only path IME text takes into the picture;
    /// `WM_IME_CHAR` is not also handled.
    pub fn commit_composition(&mut self, text: &str) -> bool {
        let closed = self.composition.take().is_some();
        let Some(index) = self.typing else {
            return closed;
        };
        if text.is_empty() {
            return closed;
        }
        match self.marks.get_mut(index) {
            Some(Mark::Text { text: mark, .. }) => {
                mark.push_str(text);
                true
            }
            _ => closed,
        }
    }

    /// The text mark currently being typed, for the on-screen text box.
    pub fn typing_mark(&self) -> Option<&Mark> {
        self.typing.and_then(|index| self.marks.get(index))
    }

    /// Index of the text mark being typed, so rendering can draw it above the
    /// text box while the older marks stay below it.
    pub fn typing_index(&self) -> Option<usize> {
        self.typing
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ImageData;

    fn doc() -> Doc {
        Doc::new(ImageData::new(4, 4, vec![255; 64]))
    }

    fn rect() -> Mark {
        Mark::Rectangle {
            rect: (1.0, 1.0, 10.0, 10.0),
            color: 0xFF0000,
            width: 8.0,
        }
    }

    #[test]
    fn undo_and_redo_walk_the_history() {
        let mut doc = doc();
        assert!(!doc.undo());
        doc.push(rect());
        doc.push(rect());
        assert!(doc.can_undo());
        assert!(doc.undo());
        assert_eq!(doc.marks().len(), 1);
        assert!(doc.can_redo());
        assert!(doc.redo());
        assert_eq!(doc.marks().len(), 2);
        assert!(!doc.redo());
    }

    #[test]
    fn new_marks_clear_the_redo_stack() {
        let mut doc = doc();
        doc.push(rect());
        assert!(doc.undo());
        doc.push(rect());
        assert!(!doc.can_redo());
    }

    #[test]
    fn typing_builds_and_finishes_a_text_mark() {
        let mut doc = doc();
        let index = doc.begin_text((5.0, 6.0), 0x00FF00, 32.0);
        assert!(doc.is_typing());
        assert!(matches!(doc.typing_mark(), Some(Mark::Text { .. })));
        assert!(doc.type_char('H'));
        assert!(doc.type_char('i'));
        assert!(doc.backspace());
        assert!(doc.type_char('i'));
        doc.finish_text();
        assert!(!doc.is_typing());
        assert!(doc.typing_mark().is_none());
        match &doc.marks()[index] {
            Mark::Text { text, .. } => assert_eq!(text, "Hi"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn empty_text_marks_are_dropped() {
        let mut doc = doc();
        doc.begin_text((5.0, 6.0), 0x00FF00, 32.0);
        doc.finish_text();
        assert!(doc.marks().is_empty());
        assert!(!doc.can_undo(), "an empty tap is not an undo step");
    }

    #[test]
    fn composition_stays_out_of_the_mark_until_the_ime_commits() {
        let mut doc = doc();
        doc.begin_text((5.0, 6.0), 0x00FF00, 32.0);
        assert!(doc.set_composition("ni'hao", 6));
        assert!(doc.is_composing());
        assert_eq!(doc.composition().map(Composition::text), Some("ni'hao"));
        // The input method owns the keyboard: a stray character or backspace
        // must not slip in between the pinyin and its result.
        assert!(!doc.type_char('X'));
        assert!(!doc.backspace());
        assert!(doc.commit_composition("你好"));
        assert!(!doc.is_composing());
        doc.finish_text();
        match &doc.marks()[0] {
            Mark::Text { text, .. } => assert_eq!(text, "你好"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn composition_needs_a_text_mark_and_never_survives_finish() {
        let mut doc = doc();
        assert!(!doc.set_composition("ni", 2), "nothing is being typed");
        doc.begin_text((5.0, 6.0), 0x00FF00, 32.0);
        assert!(doc.set_composition("ni", 2));
        assert!(doc.set_composition("", 0), "an empty string closes it");
        assert!(!doc.is_composing());
        assert!(doc.set_composition("hao", 3));
        doc.finish_text();
        assert!(!doc.is_composing(), "finishing drops the pinyin");
        assert!(doc.marks().is_empty());
    }

    #[test]
    fn composition_cursor_is_clamped_and_undo_drops_it() {
        let mut doc = doc();
        doc.push(rect());
        doc.begin_text((0.0, 0.0), 0, 32.0);
        // A cursor inside a multi-byte character falls back to its start.
        assert!(doc.set_composition("你好", 2));
        assert_eq!(doc.composition().map(Composition::cursor), Some(0));
        assert!(doc.set_composition("你好", 99));
        assert_eq!(doc.composition().map(Composition::cursor), Some(6));
        // Undo closes the text entry, so the composition cannot outlive it.
        assert!(doc.undo());
        assert!(!doc.is_composing());
        assert!(!doc.is_typing());
    }

    #[test]
    fn the_eraser_removes_whole_marks_and_one_undo_restores_them() {
        let mut doc = doc();
        doc.push(rect());
        doc.push(Mark::Arrow {
            from: (20.0, 20.0),
            to: (80.0, 20.0),
            color: 0x00FF00,
            width: 4.0,
        });
        let before = doc.marks().to_vec();
        // A disc on the rectangle removes it; a second pass near the arrow
        // removes the arrow as well.
        assert!(doc.erase_marks(|mark| mark_hits_eraser(mark, (5.0, 5.0), 2.0)));
        assert!(doc.erase_marks(|mark| mark_hits_eraser(mark, (50.0, 20.0), 2.0)));
        assert!(doc.marks().is_empty());
        assert!(doc.record_erase(before));
        assert!(doc.undo());
        assert_eq!(doc.marks().len(), 2, "one undo restores the whole gesture");
        assert!(doc.redo());
        assert!(doc.marks().is_empty());
    }

    #[test]
    fn record_erase_ignores_a_gesture_that_erased_nothing() {
        let mut doc = doc();
        doc.push(rect());
        let before = doc.marks().to_vec();
        assert!(!doc.record_erase(before));
        assert!(doc.undo());
        assert!(
            doc.marks().is_empty(),
            "an empty eraser pass must not add a step above the push"
        );
    }

    #[test]
    fn eraser_hits_the_marks_it_covers() {
        let arrow = Mark::Arrow {
            from: (10.0, 10.0),
            to: (50.0, 10.0),
            color: 0,
            width: 4.0,
        };
        assert!(mark_hits_eraser(&arrow, (30.0, 12.0), 2.0));
        assert!(!mark_hits_eraser(&arrow, (30.0, 30.0), 2.0));
        let frame = Mark::Rectangle {
            rect: (10.0, 10.0, 40.0, 20.0),
            color: 0,
            width: 4.0,
        };
        assert!(
            mark_hits_eraser(&frame, (51.0, 20.0), 2.0),
            "touch the border"
        );
        assert!(!mark_hits_eraser(&frame, (80.0, 80.0), 2.0));
        let text = Mark::Text {
            origin: (10.0, 10.0),
            text: "Hello".into(),
            color: 0,
            size: 32.0,
        };
        assert!(mark_hits_eraser(&text, (20.0, 20.0), 2.0));
        assert!(!mark_hits_eraser(&text, (300.0, 300.0), 2.0));
    }

    #[test]
    fn segment_distance_is_zero_on_the_line() {
        assert_eq!(
            segment_distance_squared((0.0, 0.0), (10.0, 0.0), (5.0, 0.0)),
            0.0
        );
        assert!((segment_distance_squared((0.0, 0.0), (10.0, 0.0), (5.0, 3.0)) - 9.0).abs() < 1e-4);
        assert!(
            (segment_distance_squared((0.0, 0.0), (10.0, 0.0), (-4.0, 0.0)) - 16.0).abs() < 1e-4
        );
    }
}
