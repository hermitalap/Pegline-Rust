//! Tray icon and its menu, drawn by `Shell_NotifyIcon` and `TrackPopupMenu`.
//!
//! The tray owns the notification icon and translates a right-click into a menu
//! command. The caller (the app) makes every decision: labels and check marks
//! come in as `MenuState`, and the returned `MenuCommand` is applied by the app.
//!
//! Everything here runs on the UI thread. `TrackPopupMenu` pumps messages while
//! it is open, so the menu is rebuilt for every click and no borrow is kept
//! across the call.

use std::mem::size_of;
use std::sync::OnceLock;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFY_ICON_DATA_FLAGS, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, HICON, HMENU, MENU_ITEM_FLAGS, MF_CHECKED,
    MF_GRAYED, MF_SEPARATOR, MF_STRING, MF_UNCHECKED, PostMessageW, RegisterWindowMessageW,
    SetForegroundWindow, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, WM_NULL,
};
use windows::core::{PCWSTR, w};

use crate::i18n;
use crate::logging;
use crate::win32;
use crate::window::WM_TRAY_CALLBACK;

/// The icon id reported back in the callback (`wParam`).
const TRAY_ID: u32 = 1;
/// Capacity of `NOTIFYICONDATAW::szTip` in UTF-16 units.
const TIP_CHARS: usize = 128;
/// Capacity of `NOTIFYICONDATAW::szInfo` in UTF-16 units.
const INFO_CHARS: usize = 256;
/// Capacity of `NOTIFYICONDATAW::szInfoTitle` in UTF-16 units.
const INFO_TITLE_CHARS: usize = 64;
/// The tooltip the WPF app showed.
const TOOLTIP: &str = "Pegline";

// One menu id per command. They are explicit instead of derived from the enum
// order because the enum is a stable API and the menu order is a product
// decision; keeping them apart means adding a command cannot move an id.
const ID_TOGGLE_REVEAL: u32 = 1;
const ID_SNIP: u32 = 2;
const ID_NOTE: u32 = 3;
const ID_SNIP_KEY: u32 = 4;
const ID_NOTE_KEY: u32 = 5;
const ID_CLEAR_LINE: u32 = 6;
const ID_CLEAR_FILES: u32 = 7;
const ID_CATCH_SCREENSHOTS: u32 = 8;
const ID_OPEN_FOLDER: u32 = 9;
const ID_SOUND: u32 = 10;
const ID_AUTOSTART: u32 = 11;
const ID_EXIT: u32 = 12;

/// The tray menu items, in the order the WPF app showed them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MenuCommand {
    ToggleReveal,
    Snip,
    Note,
    SnipKey,
    NoteKey,
    ClearLine,
    ClearFiles,
    CatchScreenshots,
    OpenFolder,
    Sound,
    Autostart,
    Exit,
}

/// Check marks and enabled states for the menu.
#[derive(Clone, Debug)]
pub struct MenuState {
    pub revealed: bool,
    pub catch_clipboard: bool,
    pub sound_on: bool,
    pub autostart: bool,
    pub caught_files: usize,
    pub live_cards: usize,
    pub empty: bool,
    pub hotkey_registered: bool,
    pub snip_key: String,
    pub note_key: String,
}

impl Default for MenuState {
    fn default() -> Self {
        Self {
            revealed: false,
            catch_clipboard: true,
            sound_on: true,
            autostart: false,
            caught_files: 0,
            live_cards: 1,
            empty: false,
            hotkey_registered: true,
            snip_key: "Ctrl+Alt+S".to_string(),
            note_key: "Ctrl+Alt+N".to_string(),
        }
    }
}

/// The live tray icon.
pub struct Tray {
    hwnd: HWND,
    icon: HICON,
    tooltip: [u16; TIP_CHARS],
    visible: bool,
}

impl Tray {
    /// Adds the icon with resource id 1 at the small-icon size for the dpi.
    pub fn install(hwnd: HWND, dpi: u32) -> Result<Self, String> {
        if hwnd.is_invalid() {
            return Err("tray: cannot install on an invalid window".to_string());
        }
        let icon = win32::load_app_icon(icon_edge(dpi)).ok_or_else(|| {
            win32::werr("LoadImageW(icon 1)", windows::core::Error::from_thread())
        })?;
        let mut tray = Self {
            hwnd,
            icon,
            tooltip: tip_field(TOOLTIP),
            visible: false,
        };
        tray.add()?;
        tray.visible = true;
        Ok(tray)
    }

    /// Removes the icon. Idempotent, and harmless after Explorer restarted.
    pub fn remove(&mut self) {
        if !self.visible {
            return;
        }
        self.visible = false;
        let data = self.data(NOTIFY_ICON_DATA_FLAGS::default());
        // SAFETY: `data` is fully initialised, its `cbSize` matches the struct
        // layout, and it stays alive for the synchronous call. NIM_DELETE only
        // reads `hWnd` and `uID`.
        let removed = unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
        if !removed.as_bool() {
            logging::error(win32::werr(
                "Shell_NotifyIconW(NIM_DELETE)",
                windows::core::Error::from_thread(),
            ));
        }
    }

    /// Re-adds the icon after Explorer restarted (`TaskbarCreated`).
    pub fn reinstall(&mut self) {
        match self.add() {
            Ok(()) => self.visible = true,
            Err(error) => logging::error(format!("tray: {error}")),
        }
    }

    pub fn set_tooltip(&mut self, text: &str) {
        self.tooltip = tip_field(text);
        let data = self.data(NIF_TIP);
        // SAFETY: as in `remove`; NIM_MODIFY reads the fields named by `uFlags`,
        // and `szTip` is a NUL-terminated UTF-16 buffer inside the struct.
        let modified = unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) };
        if !modified.as_bool() {
            logging::error(win32::werr(
                "Shell_NotifyIconW(NIM_MODIFY)",
                windows::core::Error::from_thread(),
            ));
        }
    }

    /// Shows a balloon tip (used when the clipboard has no text for a note).
    pub fn show_balloon(&mut self, title: &str, text: &str) {
        let mut data = self.data(NIF_INFO);
        data.szInfoTitle = info_field::<{ INFO_TITLE_CHARS }>(title);
        data.szInfo = info_field::<{ INFO_CHARS }>(text);
        data.dwInfoFlags = NIIF_INFO;
        // SAFETY: `data` is fully initialised with NUL-terminated UTF-16 fields
        // and stays alive for the synchronous call.
        let shown = unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) };
        if !shown.as_bool() {
            logging::error("Shell_NotifyIconW(NIF_INFO) failed");
        }
    }

    /// Shows the context menu at the cursor and returns the chosen command.
    /// Must be called on the UI thread, from the message handler.
    pub fn show_menu(&mut self, state: &MenuState) -> Option<MenuCommand> {
        // SAFETY: CreatePopupMenu allocates an empty menu owned by this thread.
        // It is destroyed below on every path.
        let menu = match unsafe { CreatePopupMenu() } {
            Ok(menu) => menu,
            Err(error) => {
                logging::error(win32::werr("CreatePopupMenu", error));
                return None;
            }
        };

        let command = self.track_menu(menu, state);

        // SAFETY: `menu` was created above and is not used again; DestroyMenu
        // frees the menu and every item appended to it.
        let _ = unsafe { DestroyMenu(menu) };

        command
    }

    /// Fills `menu` and tracks it; the caller owns and destroys the handle.
    fn track_menu(&mut self, menu: HMENU, state: &MenuState) -> Option<MenuCommand> {
        if let Err(error) = build_menu(menu, state) {
            logging::error(format!("tray menu: {error}"));
            return None;
        }

        let Some((x, y)) = win32::cursor_pos() else {
            logging::error("tray menu: cursor position unavailable");
            return None;
        };

        // A popup owned by a background window needs its owner in the
        // foreground, otherwise it does not close when the user clicks away.
        // SAFETY: `hwnd` is our message window; the BOOL only reports whether
        // the foreground window actually changed.
        let _ = unsafe { SetForegroundWindow(self.hwnd) };

        // SAFETY: `menu` is alive and owned by this thread, `hwnd` is alive for
        // the process, and the call only reads them. TPM_RETURNCMD makes the
        // return value the chosen menu id instead of a success flag.
        let chosen = unsafe {
            TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
                x,
                y,
                None,
                self.hwnd,
                None,
            )
        };

        // Classic menu-dismiss workaround: without a posted message the next
        // click can be swallowed after the foreground window changed.
        // SAFETY: posting to our own window; a dropped message is harmless.
        let _ = unsafe { PostMessageW(Some(self.hwnd), WM_NULL, WPARAM(0), LPARAM(0)) };

        // 0 means cancelled; unknown ids are ignored.
        command_for_id(chosen.0 as u32)
    }

    /// The shell struct for one call; `flags` selects which fields are read.
    fn data(&self, flags: NOTIFY_ICON_DATA_FLAGS) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: TRAY_ID,
            uFlags: flags,
            uCallbackMessage: WM_TRAY_CALLBACK,
            hIcon: self.icon,
            szTip: self.tooltip,
            ..Default::default()
        }
    }

    /// Adds the icon; shared by `install` and `reinstall`.
    fn add(&self) -> Result<(), String> {
        let data = self.data(NIF_MESSAGE | NIF_ICON | NIF_TIP);
        // SAFETY: `data` is fully initialised and alive for the synchronous
        // call; `cbSize` matches the layout the shell expects.
        unsafe { Shell_NotifyIconW(NIM_ADD, &data) }
            .ok()
            .map_err(|error| win32::werr("Shell_NotifyIconW(NIM_ADD)", error))
    }
}

/// The tray icon edge in device pixels: the system small-icon metric, but never
/// below the 16 px that `docs/BEHAVIOR.md` §12.1 scales by the screen dpi.
fn icon_edge(dpi: u32) -> i32 {
    let scaled = (dpi.saturating_mul(16).saturating_add(48) / 96) as i32;
    win32::small_icon_size().max(scaled)
}

/// `szTip` as NUL-terminated UTF-16, truncated to the field without overrun.
fn tip_field(text: &str) -> [u16; TIP_CHARS] {
    let mut tip = [0u16; TIP_CHARS];
    for (slot, unit) in tip.iter_mut().zip(text.encode_utf16().take(TIP_CHARS - 1)) {
        *slot = unit;
    }
    tip
}

/// One of the fixed-size UTF-16 fields of `NOTIFYICONDATAW`.
fn info_field<const N: usize>(text: &str) -> [u16; N] {
    let mut field = [0u16; N];
    for (slot, unit) in field.iter_mut().zip(text.encode_utf16().take(N - 1)) {
        *slot = unit;
    }
    field
}

/// Appends one string item. Check marks, disabled state and the selection id are
/// all supplied by the caller.
fn append_item(
    menu: HMENU,
    command: MenuCommand,
    label: &str,
    flags: MENU_ITEM_FLAGS,
) -> Result<(), String> {
    let text = win32::wide(label);
    // SAFETY: `menu` is the popup being built and `text` is a live
    // NUL-terminated UTF-16 buffer; AppendMenuW copies the label into the menu
    // before it returns.
    unsafe {
        AppendMenuW(
            menu,
            MF_STRING | flags,
            menu_id(command) as usize,
            PCWSTR(text.as_ptr()),
        )
    }
    .map_err(|error| win32::werr("AppendMenuW", error))
}

/// Appends a separator line.
fn append_separator(menu: HMENU) -> Result<(), String> {
    // SAFETY: `menu` is the popup being built; a separator takes no label and no
    // id, so the null pointer is the documented input.
    unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null()) }
        .map_err(|error| win32::werr("AppendMenuW(separator)", error))
}

/// Builds the whole menu in the order `docs/BEHAVIOR.md` §12.1 lists it.
fn build_menu(menu: HMENU, state: &MenuState) -> Result<(), String> {
    let strings = i18n::s();

    let toggle = if state.revealed {
        strings.hide_line
    } else {
        strings.show_line
    };
    append_item(menu, MenuCommand::ToggleReveal, toggle, MF_UNCHECKED)?;
    append_item(
        menu,
        MenuCommand::Snip,
        strings.take_screenshot,
        MF_UNCHECKED,
    )?;
    append_item(menu, MenuCommand::Note, strings.hang_note, MF_UNCHECKED)?;

    let snip = shortcut_label(
        strings.screenshot_shortcut,
        strings.key_separator,
        &state.snip_key,
    );
    append_item(menu, MenuCommand::SnipKey, &snip, MF_UNCHECKED)?;
    let note = shortcut_label(
        strings.note_shortcut,
        strings.key_separator,
        &state.note_key,
    );
    append_item(menu, MenuCommand::NoteKey, &note, MF_UNCHECKED)?;

    let clear_line = if state.live_cards == 0 {
        MF_GRAYED
    } else {
        MF_UNCHECKED
    };
    append_item(
        menu,
        MenuCommand::ClearLine,
        strings.take_everything_down,
        clear_line,
    )?;

    let delete_caught = delete_caught_label(strings.delete_caught, state.caught_files);
    let clear_files = if state.caught_files == 0 {
        MF_GRAYED
    } else {
        MF_UNCHECKED
    };
    append_item(menu, MenuCommand::ClearFiles, &delete_caught, clear_files)?;

    let catch = if state.catch_clipboard {
        MF_CHECKED
    } else {
        MF_UNCHECKED
    };
    append_item(
        menu,
        MenuCommand::CatchScreenshots,
        strings.handle_screenshots,
        catch,
    )?;
    append_item(
        menu,
        MenuCommand::OpenFolder,
        strings.open_screenshots_folder,
        MF_UNCHECKED,
    )?;

    append_separator(menu)?;

    let sound = if state.sound_on {
        MF_CHECKED
    } else {
        MF_UNCHECKED
    };
    append_item(menu, MenuCommand::Sound, strings.sounds, sound)?;
    let login = if state.autostart {
        MF_CHECKED
    } else {
        MF_UNCHECKED
    };
    append_item(menu, MenuCommand::Autostart, strings.open_at_login, login)?;

    append_separator(menu)?;

    append_item(menu, MenuCommand::Exit, strings.quit, MF_UNCHECKED)?;
    Ok(())
}

/// `"Screenshot shortcut: Ctrl+Alt+S…"`, exactly as the WPF app formatted it.
fn shortcut_label(title: &str, separator: &str, key: &str) -> String {
    format!("{title}{separator}{key}…")
}

/// Substitutes the caught-file count into the localized `delete_caught` text.
fn delete_caught_label(template: &str, count: usize) -> String {
    template.replace("{}", &count.to_string())
}

/// The menu id for a command; one id per command, stable across builds.
fn menu_id(command: MenuCommand) -> u32 {
    match command {
        MenuCommand::ToggleReveal => ID_TOGGLE_REVEAL,
        MenuCommand::Snip => ID_SNIP,
        MenuCommand::Note => ID_NOTE,
        MenuCommand::SnipKey => ID_SNIP_KEY,
        MenuCommand::NoteKey => ID_NOTE_KEY,
        MenuCommand::ClearLine => ID_CLEAR_LINE,
        MenuCommand::ClearFiles => ID_CLEAR_FILES,
        MenuCommand::CatchScreenshots => ID_CATCH_SCREENSHOTS,
        MenuCommand::OpenFolder => ID_OPEN_FOLDER,
        MenuCommand::Sound => ID_SOUND,
        MenuCommand::Autostart => ID_AUTOSTART,
        MenuCommand::Exit => ID_EXIT,
    }
}

/// The inverse of [`menu_id`]; `None` for "cancelled" (0) and unknown ids.
fn command_for_id(id: u32) -> Option<MenuCommand> {
    match id {
        ID_TOGGLE_REVEAL => Some(MenuCommand::ToggleReveal),
        ID_SNIP => Some(MenuCommand::Snip),
        ID_NOTE => Some(MenuCommand::Note),
        ID_SNIP_KEY => Some(MenuCommand::SnipKey),
        ID_NOTE_KEY => Some(MenuCommand::NoteKey),
        ID_CLEAR_LINE => Some(MenuCommand::ClearLine),
        ID_CLEAR_FILES => Some(MenuCommand::ClearFiles),
        ID_CATCH_SCREENSHOTS => Some(MenuCommand::CatchScreenshots),
        ID_OPEN_FOLDER => Some(MenuCommand::OpenFolder),
        ID_SOUND => Some(MenuCommand::Sound),
        ID_AUTOSTART => Some(MenuCommand::Autostart),
        ID_EXIT => Some(MenuCommand::Exit),
        _ => None,
    }
}

/// The message Explorer broadcasts after a restart; the tray icon must be
/// re-added when it arrives.
pub fn taskbar_created_message() -> u32 {
    static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();
    *TASKBAR_CREATED.get_or_init(|| {
        // SAFETY: user32 registers the message from a static NUL-terminated
        // name and returns a process-wide id; no memory is retained by us.
        unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [MenuCommand; 12] = [
        MenuCommand::ToggleReveal,
        MenuCommand::Snip,
        MenuCommand::Note,
        MenuCommand::SnipKey,
        MenuCommand::NoteKey,
        MenuCommand::ClearLine,
        MenuCommand::ClearFiles,
        MenuCommand::CatchScreenshots,
        MenuCommand::OpenFolder,
        MenuCommand::Sound,
        MenuCommand::Autostart,
        MenuCommand::Exit,
    ];

    #[test]
    fn every_command_round_trips_through_its_menu_id() {
        for command in ALL {
            assert_eq!(command_for_id(menu_id(command)), Some(command));
        }
    }

    #[test]
    fn menu_ids_are_unique_and_nonzero() {
        let mut ids: Vec<u32> = ALL.iter().map(|command| menu_id(*command)).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), ALL.len(), "two commands share a menu id");
        assert!(!ids.contains(&0), "menu id 0 means cancelled");
    }

    #[test]
    fn unknown_or_cancelled_ids_map_to_nothing() {
        assert_eq!(command_for_id(0), None);
        assert_eq!(command_for_id(u32::MAX), None);
    }

    #[test]
    fn shortcut_labels_match_the_wpf_format() {
        assert_eq!(
            shortcut_label("Screenshot shortcut", ": ", "Ctrl+Alt+S"),
            "Screenshot shortcut: Ctrl+Alt+S…"
        );
        assert_eq!(
            shortcut_label("截图快捷键", "：", "Ctrl+Alt+S"),
            "截图快捷键：Ctrl+Alt+S…"
        );
    }

    #[test]
    fn delete_caught_substitutes_the_count() {
        assert_eq!(
            delete_caught_label("Delete caught screenshots ({})…", 3),
            "Delete caught screenshots (3)…"
        );
        assert_eq!(
            delete_caught_label("清空截图文件（{} 个）…", 0),
            "清空截图文件（0 个）…"
        );
    }

    #[test]
    fn tooltip_is_truncated_and_nul_terminated() {
        let tip = tip_field("Pegline");
        assert_eq!(String::from_utf16_lossy(&tip[..7]), "Pegline");
        assert_eq!(tip[7], 0);

        let long = "x".repeat(TIP_CHARS * 2);
        let tip = tip_field(&long);
        assert_eq!(tip[TIP_CHARS - 1], 0);
        assert!(
            tip[..TIP_CHARS - 1]
                .iter()
                .all(|&unit| unit == u16::from(b'x'))
        );
    }
}
