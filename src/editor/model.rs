//! Pure drawing model for the annotation editor.
//!
//! Everything in this module is arithmetic on device pixels: no Win32, no
//! rendering. The window owns a [`Doc`], applies gestures to it and renders it;
//! tests can drive the same operations without a desktop.

/// The four tools (BEHAVIOR §11).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tool {
    Arrow,
    Rectangle,
    Text,
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

/// The editable document: the source picture plus the marks drawn over it.
pub struct Doc {
    source: Option<crate::types::ImageData>,
    marks: Vec<Mark>,
    undone: Vec<Mark>,
    /// The text mark currently being typed, if any.
    typing: Option<usize>,
}

impl Doc {
    pub fn new(source: crate::types::ImageData) -> Self {
        Self {
            source: Some(source),
            marks: Vec::new(),
            undone: Vec::new(),
            typing: None,
        }
    }

    pub fn source(&self) -> Option<&crate::types::ImageData> {
        self.source.as_ref()
    }

    pub fn marks(&self) -> &[Mark] {
        &self.marks
    }

    pub fn push(&mut self, mark: Mark) {
        self.marks.push(mark);
        self.undone.clear();
    }

    /// Removes the newest mark; returns whether anything changed.
    pub fn undo(&mut self) -> bool {
        self.typing = None;
        match self.marks.pop() {
            Some(mark) => {
                self.undone.push(mark);
                true
            }
            None => false,
        }
    }

    /// Puts the last undone mark back.
    pub fn redo(&mut self) -> bool {
        match self.undone.pop() {
            Some(mark) => {
                self.marks.push(mark);
                true
            }
            None => false,
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.marks.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.undone.is_empty()
    }

    /// Starts a text mark at `origin` (source pixels) and returns its index.
    pub fn begin_text(&mut self, origin: (f32, f32), color: u32, size: f32) -> usize {
        self.marks.push(Mark::Text {
            origin,
            text: String::new(),
            color,
            size,
        });
        self.undone.clear();
        self.typing = Some(self.marks.len() - 1);
        self.marks.len() - 1
    }

    /// Appends a character to the text mark being typed.
    pub fn type_char(&mut self, character: char) -> bool {
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

    /// Ends text entry; an empty text mark is removed.
    pub fn finish_text(&mut self) {
        if let Some(index) = self.typing.take() {
            if let Some(Mark::Text { text, .. }) = self.marks.get(index) {
                if text.trim().is_empty() {
                    self.marks.remove(index);
                }
            }
        }
    }

    pub fn is_typing(&self) -> bool {
        self.typing.is_some()
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
    }
}
