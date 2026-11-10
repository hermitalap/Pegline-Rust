//! The WYSIWYG sticky-note editor (docs/NOTE-EDITOR.md).
//!
//! The source `.txt` is the single truth; everything else is a projection:
//! [`model`] owns the text, caret, selection and undo, [`display`] turns source
//! lines into Markdown blocks and display runs, and the window/render modules
//! (added alongside the integration) paint that projection.

pub mod display;
pub mod model;
pub mod render;
pub mod window;
