//! Watching for new screenshots, and folding duplicates into one card.
//!
//! The folder watcher, the clipboard path and the twin-window heuristic all
//! exist to turn "a file appeared" into "one card on the line". Their state is
//! kept together because the three of them share the same bookkeeping: what was
//! seen recently, what we wrote ourselves, and how many restored cards are still
//! decoding.

use std::path::PathBuf;

use super::Recent;
use crate::capture::watcher;

#[derive(Default)]
pub(super) struct Capture {
    pub(super) watcher: Option<watcher::Watcher>,
    /// Captures from the twin window (4 s), used to fold the inbox copy and the
    /// Windows-saved copy of the same screenshot into one card.
    pub(super) recent: Vec<Recent>,
    /// Content hash of the last clipboard capture and when it arrived, so the
    /// same copied image is not hung twice (10 s window).
    pub(super) last_clip_hash: u64,
    pub(super) last_clip_at: f64,
    /// When the pending clipboard capture should be read.
    pub(super) clipboard_at: Option<f64>,
    /// Files our own editor just wrote, ignored by the watcher for 2 s.
    pub(super) recently_saved: Vec<(PathBuf, f64)>,
    pub(super) last_reload: f64,
    /// How many restored cards are still decoding; `pegged.txt` is not rewritten
    /// until they are all back.
    pub(super) restore_pending: usize,
}
