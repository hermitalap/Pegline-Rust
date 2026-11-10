//! The two in-place editors: what is open, and what is in flight around it.
//!
//! Both editors own a window in [`super::pool::WindowPool`]; this is the rest of
//! the hand-off — the capture or note being edited, whether a commit is in
//! flight, and the paper bitmap the return flight carries. The two share the
//! struct because they share the deadlines: an interrupted flight can leave
//! either one hidden, and both need releasing anyway.

use super::{EditingCapture, EditingNote};
use crate::types::ImageData;

#[derive(Default)]
pub(super) struct Sessions {
    /// The picture being annotated, while the annotation editor is open.
    pub(super) editing: Option<EditingCapture>,
    /// The note open in the WYSIWYG editor.
    pub(super) note_editing: Option<EditingNote>,
    /// True between a commit request and its worker result; the window stays
    /// open and disabled meanwhile.
    pub(super) note_committing: bool,
    /// The editor's paper bitmap captured when the user committed, carried by
    /// the flight back to the line.
    pub(super) note_return_image: Option<ImageData>,
    /// True while an empty new note is tumbling off the screen; the editor is
    /// hidden and no click may commit it again.
    pub(super) note_tumbling: bool,
    /// Deadline for the tumble: the editor is released even if the flight was
    /// interrupted and never reported back.
    pub(super) note_tumble_until: f64,
}
