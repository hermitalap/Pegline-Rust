//! `settings.ini` and `pegged.txt`, in the exact formats the WPF version uses so
//! a user can switch between the two builds without losing the line.
//!
//! Both files live in `%APPDATA%\Pegline`. Unknown keys are preserved: they may
//! belong to a newer (or older) build, and dropping them would be user-visible.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::logging;
use crate::win32;

/// Settings keys used by the MVP. Kept as constants so a typo is a compile
/// error rather than a silently ignored setting.
pub mod keys {
    pub const WELCOMED: &str = "welcomed";
    pub const CATCH_OFFERED: &str = "catchOffered";
    pub const CATCH_CLIPBOARD: &str = "catchClipboard";
    pub const SOUND_OFF: &str = "soundOff";
    pub const SNIP_KEY: &str = "snipKey";
    pub const NOTE_KEY: &str = "noteKey";
    pub const FPS: &str = "fps";
    /// Prefix of the per-note colour keys (`noteColor:<file name>`).
    pub const NOTE_COLOR_PREFIX: &str = "noteColor:";
}

/// How the frame loop paces itself (`fps=auto|60|120`, BEHAVIOR §12.3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FpsCap {
    /// Follow the display refresh rate: `Present(1, 0)` on every frame.
    Auto,
    /// Never present faster than 60 Hz, even on a 120 Hz display.
    Hz60,
    /// Never present faster than 120 Hz.
    Hz120,
}

impl FpsCap {
    pub fn parse(text: &str) -> Self {
        match text.trim() {
            "60" => Self::Hz60,
            "120" => Self::Hz120,
            _ => Self::Auto,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Hz60 => "60",
            Self::Hz120 => "120",
        }
    }

    /// Minimum frame period in seconds, or 0 for "whatever the display does".
    pub fn period(self) -> f64 {
        match self {
            Self::Auto => 0.0,
            Self::Hz60 => 1.0 / 60.0,
            Self::Hz120 => 1.0 / 120.0,
        }
    }
}

/// One parsed `key=value` pair.
#[derive(Clone, Debug, Default)]
pub struct Settings {
    values: BTreeMap<String, String>,
    path: PathBuf,
}

impl Settings {
    /// Loads from the default location, creating the directory if needed.
    pub fn load() -> Self {
        let dir = win32::app_dir();
        if let Err(error) = fs::create_dir_all(&dir) {
            logging::error(format!("create config dir {}: {error}", dir.display()));
        }
        let path = dir.join("settings.ini");
        let mut settings = Self {
            values: BTreeMap::new(),
            path,
        };
        settings.read();
        settings
    }

    fn read(&mut self) {
        let Ok(bytes) = fs::read(&self.path) else {
            return;
        };
        let text = String::from_utf8_lossy(&bytes);
        for line in text.lines() {
            let line = line.trim_end_matches('\r');
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                self.values
                    .insert(key.trim().to_string(), value.to_string());
            }
        }
    }

    /// Writes the map back atomically (temp file + rename).
    pub fn save(&self) -> bool {
        let mut body = String::new();
        for (key, value) in &self.values {
            body.push_str(key);
            body.push('=');
            body.push_str(value);
            body.push_str("\r\n");
        }
        let temporary = self.path.with_extension("ini.tmp");
        let written = fs::File::create(&temporary).and_then(|mut file| {
            file.write_all(body.as_bytes())?;
            file.sync_all()
        });
        if let Err(error) = written {
            logging::error(format!("write {}: {error}", temporary.display()));
            return false;
        }
        if let Err(error) = fs::rename(&temporary, &self.path) {
            logging::error(format!("rename settings: {error}"));
            return false;
        }
        true
    }

    pub fn bool(&self, key: &str) -> bool {
        self.values
            .get(key)
            .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    }

    pub fn set_bool(&mut self, key: &str, value: bool) {
        self.values
            .insert(key.to_string(), if value { "1" } else { "0" }.to_string());
    }

    pub fn string(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    pub fn set_string(&mut self, key: &str, value: &str) {
        self.values.insert(key.to_string(), value.to_string());
    }

    pub fn remove(&mut self, key: &str) {
        self.values.remove(key);
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// First run: show the welcome line for five seconds.
    pub fn welcomed(&self) -> bool {
        self.bool(keys::WELCOMED)
    }

    pub fn mark_welcomed(&mut self) {
        self.set_bool(keys::WELCOMED, true);
        self.save();
    }

    pub fn catch_offered(&self) -> bool {
        self.bool(keys::CATCH_OFFERED)
    }

    pub fn mark_catch_offered(&mut self) {
        self.set_bool(keys::CATCH_OFFERED, true);
        self.save();
    }

    pub fn catch_clipboard(&self) -> bool {
        self.bool(keys::CATCH_CLIPBOARD)
    }

    pub fn set_catch_clipboard(&mut self, on: bool) {
        self.set_bool(keys::CATCH_CLIPBOARD, on);
        self.save();
    }

    pub fn sound_off(&self) -> bool {
        self.bool(keys::SOUND_OFF)
    }

    pub fn set_sound_off(&mut self, off: bool) {
        self.set_bool(keys::SOUND_OFF, off);
        self.save();
    }

    pub fn fps_cap(&self) -> FpsCap {
        FpsCap::parse(self.string(keys::FPS).unwrap_or("auto"))
    }

    /// The note colour index (0–3) stored for a note file name.
    pub fn note_color(&self, file_name: &str) -> usize {
        self.string(&format!("{}{}", keys::NOTE_COLOR_PREFIX, file_name))
            .and_then(|value| value.parse::<usize>().ok())
            .map(|color| color.min(3))
            .unwrap_or(0)
    }

    pub fn set_note_color(&mut self, file_name: &str, color: usize) {
        let key = format!("{}{}", keys::NOTE_COLOR_PREFIX, file_name);
        self.set_string(&key, &color.min(3).to_string());
        self.save();
    }
}

/// Reads `pegged.txt`: one absolute path per line, CRLF, no quoting.
pub fn read_pegged() -> Vec<PathBuf> {
    let path = win32::app_dir().join("pegged.txt");
    let Ok(bytes) = fs::read(&path) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&bytes);
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Rewrites `pegged.txt` with the currently hanging files. Called on every
/// add/remove; a failed write is logged and otherwise ignored.
pub fn write_pegged(paths: &[PathBuf]) {
    let path = win32::app_dir().join("pegged.txt");
    let mut body = String::new();
    for item in paths {
        body.push_str(&item.to_string_lossy());
        body.push_str("\r\n");
    }
    if let Err(error) = fs::write(&path, body) {
        logging::error(format!("write {}: {error}", path.display()));
    }
}

/// Creates the inbox directory, returning whether it is usable.
pub fn ensure_inbox() -> bool {
    let dir = win32::inbox_dir();
    match fs::create_dir_all(&dir) {
        Ok(()) => true,
        Err(error) => {
            logging::error(format!("create inbox {}: {error}", dir.display()));
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> BTreeMap<String, String> {
        let mut map = BTreeMap::new();
        for line in text.lines() {
            if let Some((key, value)) = line.split_once('=') {
                map.insert(key.trim().to_string(), value.to_string());
            }
        }
        map
    }

    #[test]
    fn preserves_unknown_keys_when_edited() {
        let mut map = parse("welcomed=1\r\nsomethingNew=a=b\r\n");
        map.insert("soundOff".into(), "0".into());
        assert_eq!(map.get("somethingNew").map(String::as_str), Some("a=b"));
        assert_eq!(map.len(), 3);
    }

    #[test]
    fn boolean_accepts_one_and_true() {
        let settings = Settings {
            values: parse("a=1\r\nb=true\r\nc=0\r\nd=TRUE\r\n"),
            path: PathBuf::new(),
        };
        assert!(settings.bool("a"));
        assert!(settings.bool("b"));
        assert!(!settings.bool("c"));
        assert!(settings.bool("d"));
        assert!(!settings.bool("missing"));
    }

    #[test]
    fn frame_cap_parses_and_defaults_to_auto() {
        let mut settings = Settings {
            values: parse(""),
            path: PathBuf::new(),
        };
        assert_eq!(settings.fps_cap(), FpsCap::Auto);
        settings.set_string(keys::FPS, "60");
        assert_eq!(settings.fps_cap(), FpsCap::Hz60);
        settings.set_string(keys::FPS, "120");
        assert_eq!(settings.fps_cap(), FpsCap::Hz120);
        settings.set_string(keys::FPS, "bogus");
        assert_eq!(settings.fps_cap(), FpsCap::Auto);
        assert_eq!(FpsCap::Hz60.period(), 1.0 / 60.0);
        assert_eq!(FpsCap::Auto.period(), 0.0);
    }

    #[test]
    fn note_colors_are_keyed_by_file_name_and_clamped() {
        let mut settings = Settings {
            values: parse(""),
            path: PathBuf::new(),
        };
        assert_eq!(settings.note_color("便签 2026-11-02 120000.txt"), 0);
        settings.set_string("noteColor:便签 2026-11-02 120000.txt", "2");
        assert_eq!(settings.note_color("便签 2026-11-02 120000.txt"), 2);
        settings.set_string("noteColor:x.txt", "99");
        assert_eq!(settings.note_color("x.txt"), 3);
    }
}
