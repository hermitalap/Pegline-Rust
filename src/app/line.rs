//! The cards currently hanging on the line, as the UI thread knows them.
//!
//! Pure data: paths, sizes, cached text and thumbnails. The GPU-side bitmaps
//! live in the render thread's `Scene`, reached only through [`super::Command`],
//! so this struct has no `windows`/D2D types in it.
//!
//! It is its own struct because almost every gesture needs to iterate the cards
//! (`find`, `position`, `iter_mut`) while also touching the pointer state or the
//! window pool; keeping `items` on `App` made those two borrows conflict for the
//! whole enclosing block.

use super::Item;
use crate::types::CardId;

#[derive(Default)]
pub(super) struct Line {
    pub(super) items: Vec<Item>,
    /// Monotonic id source. Ids are never reused, so a stale id in a worker
    /// result cannot match a newer card.
    pub(super) next_id: CardId,
}
