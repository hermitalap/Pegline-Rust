//! Pegline library: every module the app and `probe` share.
//!
//! The binary crates are `pegline` (the tray app) and `probe` (the measurement
//! tool); both build on this library.
//!
//! Phase A is still landing: several helpers are used by the flight window and
//! the probe, which are the next milestone. Remove this allowance before the
//! v0.1 tag.
#![allow(dead_code)]

pub mod app;
pub mod capture;
pub mod dragdrop;
pub mod editor;
pub mod font;
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
