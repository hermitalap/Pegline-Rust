//! Capture pipeline: file watcher, clipboard, decoding, area inference, twins.
//!
//! Everything expensive happens on worker threads; results arrive on the UI
//! thread as `Event`s drained by the 30 Hz tick.

pub mod areas;
pub mod clipboard;
mod dib;
pub mod watcher;
pub mod wic;

/// Maximum side of a card thumbnail, in pixels (`PLAN.md` performance rules).
pub const THUMB_MAX: u32 = 480;
/// Maximum side of the image carried by the flight window, in pixels.
pub const FLIGHT_MAX: u32 = 1600;
/// Maximum side used when comparing two screenshots for "same picture".
pub const COMPARE_MAX: u32 = 2048;
