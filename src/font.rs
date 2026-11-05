//! Font families used by the app.
//!
//! Every non-symbol string uses the Windows UI variant of Microsoft YaHei so
//! the line, the editor and the shortcut dialog share one typeface. The two
//! undo/redo glyphs are symbols and keep a symbol face; the tool icons are
//! drawn as geometry, so they need no font at all.
//!
//! These are system fonts: no font file is shipped, so they add nothing to the
//! executable size. The cost is that the exact face depends on the Windows
//! installation; DirectWrite/GDI fall back when a family is unavailable.

use windows::core::{PCWSTR, w};

/// The family name as a `PCWSTR` for DirectWrite.
pub const UI_FAMILY: PCWSTR = w!("Microsoft YaHei UI");
/// The same family as a Rust string for GDI's `CreateFontW` calls.
pub const UI_FAMILY_STR: &str = "Microsoft YaHei UI";
/// Symbol face for the undo/redo arrows (U+21B6/U+21B7).
pub const SYMBOL_FAMILY: PCWSTR = w!("Segoe UI Symbol");
