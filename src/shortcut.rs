//! Shortcut text parsing and registration.
//!
//! The WPF app stores shortcuts as strings like `Ctrl+Alt+S` in `settings.ini`.
//! This module turns them into `RegisterHotKey` arguments and back, with the
//! same rules: F1–F24 and PrtScn may stand alone; everything else needs at
//! least one modifier.

use windows::Win32::UI::Input::KeyboardAndMouse::{
    MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN,
};

/// A parsed hotkey.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Hotkey {
    pub modifiers: u32,
    pub virtual_key: u32,
}

impl Hotkey {
    pub fn label(self) -> String {
        let mut text = String::new();
        if self.modifiers & MOD_CONTROL.0 != 0 {
            text.push_str("Ctrl+");
        }
        if self.modifiers & MOD_ALT.0 != 0 {
            text.push_str("Alt+");
        }
        if self.modifiers & MOD_SHIFT.0 != 0 {
            text.push_str("Shift+");
        }
        if self.modifiers & MOD_WIN.0 != 0 {
            text.push_str("Win+");
        }
        // The recorder shows the modifiers alone while the user holds them;
        // virtual_key 0 is not a real key and must not render "VK00" (or a
        // dangling "+" from the modifier list).
        if self.virtual_key == 0 {
            if text.ends_with('+') {
                text.pop();
            }
            return text;
        }
        text.push_str(&key_name(self.virtual_key));
        text
    }

    /// The value passed to `RegisterHotKey`; `MOD_NOREPEAT` avoids repeats
    /// while the key is held.
    pub fn register_modifiers(self) -> u32 {
        self.modifiers | MOD_NOREPEAT.0
    }
}

fn key_name(vk: u32) -> String {
    if (0x41..=0x5A).contains(&vk) {
        return char::from_u32(vk).map(String::from).unwrap_or_default();
    }
    if (0x30..=0x39).contains(&vk) {
        return char::from_u32(vk).map(String::from).unwrap_or_default();
    }
    if (0x70..=0x87).contains(&vk) {
        return format!("F{}", vk - 0x70 + 1);
    }
    match vk {
        0x2C => "PrtScn".to_string(),
        0x20 => "Space".to_string(),
        0x0D => "Enter".to_string(),
        0x09 => "Tab".to_string(),
        0x1B => "Esc".to_string(),
        0x21 => "PgUp".to_string(),
        0x22 => "PgDn".to_string(),
        0x23 => "End".to_string(),
        0x24 => "Home".to_string(),
        0x25 => "Left".to_string(),
        0x26 => "Up".to_string(),
        0x27 => "Right".to_string(),
        0x28 => "Down".to_string(),
        0x2D => "Insert".to_string(),
        0x2E => "Delete".to_string(),
        _ => format!("VK{vk:02X}"),
    }
}

/// Parses `Ctrl+Alt+S` / `Ctrl+Shift+F1` / `PrtScn`. `None` when the text is
/// empty, unknown, or lacks a modifier on a normal key.
pub fn parse(text: &str) -> Option<Hotkey> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut modifiers = 0u32;
    let mut vk = None;
    for part in text.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        let lowered = part.to_ascii_lowercase();
        match lowered.as_str() {
            "ctrl" | "control" => modifiers |= MOD_CONTROL.0,
            "alt" => modifiers |= MOD_ALT.0,
            "shift" => modifiers |= MOD_SHIFT.0,
            "win" | "windows" | "meta" => modifiers |= MOD_WIN.0,
            _ => {
                if vk.is_some() {
                    return None;
                }
                vk = Some(parse_key(part)?);
            }
        }
    }
    let vk = vk?;
    let hotkey = Hotkey {
        modifiers,
        virtual_key: vk,
    };
    if !usable(hotkey) {
        return None;
    }
    Some(hotkey)
}

/// Whether a combination is acceptable: F1–F24 and PrtScn may stand alone,
/// everything else needs at least one modifier (BEHAVIOR §12.2).
pub fn usable(hotkey: Hotkey) -> bool {
    hotkey.modifiers != 0 || matches!(hotkey.virtual_key, 0x70..=0x87 | 0x2C)
}

fn parse_key(text: &str) -> Option<u32> {
    let upper = text.to_ascii_uppercase();
    let bytes = upper.as_bytes();
    if bytes.len() == 1 {
        let byte = bytes[0];
        if byte.is_ascii_uppercase() || byte.is_ascii_digit() {
            return Some(byte as u32);
        }
    }
    if let Some(number) = upper.strip_prefix('F') {
        if let Ok(index) = number.parse::<u32>() {
            if (1..=24).contains(&index) {
                return Some(0x70 + index - 1);
            }
        }
    }
    match upper.as_str() {
        "PRTSCN" | "PRTSC" | "PRINTSCREEN" => Some(0x2C),
        "SPACE" => Some(0x20),
        "ENTER" | "RETURN" => Some(0x0D),
        "TAB" => Some(0x09),
        "ESC" | "ESCAPE" => Some(0x1B),
        "PGUP" => Some(0x21),
        "PGDN" => Some(0x22),
        "END" => Some(0x23),
        "HOME" => Some(0x24),
        "LEFT" => Some(0x25),
        "UP" => Some(0x26),
        "RIGHT" => Some(0x27),
        "DOWN" => Some(0x28),
        "INSERT" | "INS" => Some(0x2D),
        "DELETE" | "DEL" => Some(0x2E),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stored_shortcut_forms() {
        let stored = parse("Ctrl+Alt+S").expect("stored shortcut form");
        assert_eq!(stored.virtual_key, 'S' as u32);
        assert_eq!(stored.modifiers, MOD_CONTROL.0 | MOD_ALT.0);

        let toggle = parse("Ctrl+Alt+T").expect("toggle shortcut");
        assert_eq!(toggle.virtual_key, 'T' as u32);
    }

    #[test]
    fn f_keys_and_printscreen_stand_alone() {
        assert!(parse("F5").is_some());
        assert!(parse("PrtScn").is_some());
        assert!(parse("A").is_none());
        assert!(parse("Ctrl").is_none());
    }

    #[test]
    fn rejects_unknown_and_double_keys() {
        assert!(parse("Ctrl+Alt+Nope").is_none());
        assert!(parse("Ctrl+A+B").is_none());
        assert!(parse("").is_none());
    }

    #[test]
    fn round_trips_through_display() {
        let hotkey = parse("Ctrl+Shift+F12").unwrap();
        assert_eq!(hotkey.label(), "Ctrl+Shift+F12");
        assert_eq!(parse(&hotkey.label()), Some(hotkey));
    }

    #[test]
    fn partial_modifiers_hide_the_placeholder_key() {
        let modifiers = Hotkey {
            modifiers: MOD_CONTROL.0 | MOD_ALT.0,
            virtual_key: 0,
        };
        assert_eq!(modifiers.label(), "Ctrl+Alt");
        let shift = Hotkey {
            modifiers: MOD_SHIFT.0,
            virtual_key: 0,
        };
        assert_eq!(shift.label(), "Shift");
    }

    #[test]
    fn modifier_order_is_normalised() {
        let a = parse("Alt+Ctrl+S").unwrap();
        let b = parse("Ctrl+Alt+S").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.label(), "Ctrl+Alt+S");
    }

    #[test]
    fn usable_matches_the_recorder_rules() {
        let plain = Hotkey {
            modifiers: 0,
            virtual_key: 'S' as u32,
        };
        assert!(!usable(plain));
        let function = Hotkey {
            modifiers: 0,
            virtual_key: 0x70,
        };
        assert!(usable(function));
        let print_screen = Hotkey {
            modifiers: 0,
            virtual_key: 0x2C,
        };
        assert!(usable(print_screen));
        let modified = Hotkey {
            modifiers: MOD_SHIFT.0,
            virtual_key: 'S' as u32,
        };
        assert!(usable(modified));
    }
}
