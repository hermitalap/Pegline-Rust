//! The annotation editor: mark up a capture before it hangs (BEHAVIOR §11).
//!
//! `model` holds the pure drawing model (marks, undo/redo, hit geometry) and
//! `window` owns the real window and its render thread. The app opens one with
//! [`window::Editor::open`] and receives [`window::EditorEvent`]s on its own
//! thread; the editor never touches the line.

pub mod layout;
pub mod model;
pub mod window;

pub use window::{Editor, EditorEvent};
