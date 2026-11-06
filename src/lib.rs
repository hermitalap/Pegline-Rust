//! Pegline library: every module the app and `probe` share.
//!
//! The binary crates are `pegline` (the tray app) and `probe` (the measurement
//! tool); both build on this library.
//!
//! `dead_code` is deliberately **not** allowed here. The crate used to carry a
//! blanket `#![allow(dead_code)]` left over from Phase A, which hid both the
//! private items and — because `pub` items are never dead-code checked — more
//! than twenty unreferenced public helpers. Keeping the default warning on is
//! what stops that from happening again.

pub mod app;
pub mod capture;
pub mod dragdrop;
pub mod editor;
pub mod font;
pub mod gfx;
pub mod i18n;
pub mod layout;
pub mod logging;
pub mod note_editor;
pub mod notes;
pub mod preview;
pub mod render;
pub mod scene;
pub mod settings;
pub mod shortcut;
pub mod sound;
pub mod spring;
pub mod tray;
pub mod types;
pub mod win32;
pub mod window;
