//! The tray icon's menu: what it offers, and what each command does.
//!
//! `TrackPopupMenu` pumps this thread's messages while it is open, so the
//! caller wraps the call in a `ModalGuard` and every command here runs after
//! the menu has closed and the borrow of `App` is once again the only one.

use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::WM_LBUTTONUP;
use windows::Win32::UI::WindowsAndMessaging::WM_RBUTTONUP;

use super::{count_inbox_files, mouse_queue, shortcut_label};
use crate::i18n;
use crate::logging;
use crate::tray::{MenuCommand, MenuState};
use crate::win32;
use crate::window;

impl super::App {
    pub(super) fn on_tray(&mut self, wparam: WPARAM, lparam: LPARAM) {
        if wparam.0 != 1 {
            return;
        }
        let mouse = lparam.0 as u32;
        if mouse == WM_LBUTTONUP {
            self.toggle();
        } else if mouse == WM_RBUTTONUP {
            let state = self.menu_state();
            self.windows.menu_open = true;
            let command = {
                // The tray menu pumps messages on this thread while `&mut self`
                // is live.
                let _modal = window::ModalGuard::enter();
                self.windows
                    .tray
                    .as_mut()
                    .and_then(|tray| tray.show_menu(&state))
            };
            // As with the card menu, a tray-menu click must not be replayed as
            // a click on the line afterwards.
            if let Ok(mut queue) = mouse_queue().lock() {
                queue.clear();
            }
            self.windows.menu_open = false;
            if let Some(command) = command {
                self.apply(command);
            }
        }
    }

    pub(super) fn menu_state(&self) -> MenuState {
        MenuState {
            revealed: self.state.is_revealed,
            line_always: self.settings.line_always(),
            catch_clipboard: self.settings.catch_clipboard(),
            sound_on: !self.settings.sound_off(),
            autostart: win32::autostart_enabled(),
            caught_files: count_inbox_files(),
            live_cards: self.line.items.len(),
            note_key: self.hotkey_menu_label("noteKey", "Ctrl+Alt+N", self.note_hotkey_ok),
        }
    }

    /// The shortcut text shown in the tray menu, including "none" and the
    /// "(in use by another app)" suffix.
    pub(super) fn hotkey_menu_label(
        &self,
        setting: &str,
        default: &str,
        registered: bool,
    ) -> String {
        let key = self.settings.string(setting).unwrap_or(default);
        if key == "none" {
            i18n::s().none.to_string()
        } else {
            shortcut_label(key, registered)
        }
    }

    pub(super) fn apply(&mut self, command: MenuCommand) {
        match command {
            MenuCommand::ToggleReveal => self.toggle(),
            MenuCommand::LineAlways => {
                let on = !self.settings.line_always();
                self.set_line_always(on);
            }
            MenuCommand::CatchScreenshots => {
                let on = !self.settings.catch_clipboard();
                self.settings.set_catch_clipboard(on);
            }
            MenuCommand::Sound => {
                let off = !self.settings.sound_off();
                self.settings.set_sound_off(off);
            }
            MenuCommand::Autostart => {
                let on = !win32::autostart_enabled();
                if let Err(error) = win32::set_autostart(on) {
                    logging::error(format!("autostart: {error}"));
                }
            }
            MenuCommand::OpenFolder => {
                let folder = if self.settings.catch_clipboard() {
                    win32::inbox_dir()
                } else {
                    win32::screenshots_folder().unwrap_or_else(win32::inbox_dir)
                };
                let _ = std::fs::create_dir_all(&folder);
                if let Err(error) = win32::shell_open(&folder) {
                    logging::error(format!("open folder: {error}"));
                }
            }
            MenuCommand::ClearLine => self.clear_line(),
            MenuCommand::ClearFiles => self.clear_inbox(),
            MenuCommand::Exit => self.request_quit(),
            MenuCommand::Note => self.note_shortcut(),
            MenuCommand::NoteKey => self.change_note_hotkey(),
        }
    }
}
