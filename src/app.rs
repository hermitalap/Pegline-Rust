//! The controller: one message loop, one state machine.
//!
//! The UI thread owns every decision — show/hide, reveal, hover, gestures,
//! capture orchestration, tray, hotkeys — and sends `Command`s to the render
//! thread. It never decodes, encodes, compares or deletes: those run on workers
//! and come back as [`Event`]s on the 30 Hz tick.
//!
//! Win32 objects (`HWND`, `HHOOK`, hotkeys, tray) are created and destroyed on
//! this thread only.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, RegisterHotKey, UnregisterHotKey, VK_LBUTTON,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CallNextHookEx, CreatePopupMenu, DestroyMenu, DestroyWindow, HHOOK, KillTimer,
    MF_SEPARATOR, MF_STRING, MSLLHOOKSTRUCT, PostMessageW, RegisterWindowMessageW,
    SetForegroundWindow, SetTimer, SetWindowsHookExW, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    TrackPopupMenu, UnhookWindowsHookEx, WH_MOUSE_LL, WM_CLIPBOARDUPDATE, WM_CLOSE, WM_DESTROY,
    WM_HOTKEY, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_NULL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_TIMER,
};
use windows::core::{PCWSTR, w};

use crate::capture::{areas, clipboard, watcher, wic};
use crate::i18n;
use crate::layout;
use crate::logging;
use crate::scene::Command;
use crate::settings::{self, Settings};
use crate::shortcut;
use crate::tray::{MenuCommand, MenuState, Tray};
use crate::types::{CardId, CardSpec, ImageData, RectI};
use crate::win32::{self, Monitor};
use crate::window::overlay::Overlay;
use crate::window::{self, WM_APP_QUIT, WM_APP_WAKE, WM_TRAY_CALLBACK};

const CLASS_NAME: PCWSTR = w!("PeglineMessage");
const WINDOW_TITLE: PCWSTR = w!("Pegline");
const TIMER_TICK: usize = 1;
const TICK_MS: u32 = 33;

const HOTKEY_TOGGLE: i32 = 1;
const HOTKEY_SNIP: i32 = 2;
const HOTKEY_NOTE: i32 = 3;

/// How long the pointer must rest on the top edge before the line drops.
const REVEAL_DELAY: f64 = 0.3;
/// How long after the pointer leaves before the line retracts.
const RETRACT_DELAY: f64 = 0.5;
/// Delay before the window is hidden after a dismiss.
const HIDE_DELAY: f64 = 0.4;
/// How long the line stays open after a capture.
const PEEK_SECONDS: f64 = 2.5;
/// How long an empty line stays open after the last card leaves.
const EMPTY_DELAY: f64 = 0.7;
/// First-run welcome duration.
const WELCOME_SECONDS: f64 = 5.0;
/// Clipboard settle delay.
const CLIPBOARD_SETTLE: f64 = 0.15;
/// Long press threshold.
const LONG_PRESS: f64 = 0.45;
/// Drag threshold, DIP (converted with the screen scale).
const DRAG_DIP: f32 = 4.0;
/// Double-click window.
const DOUBLE_CLICK: f64 = 0.4;
/// Missing file ticks before a card is dropped (one per second).
const MISSING_LIMIT: u32 = 2;

/// One pegged item, as the UI thread knows it.
struct Item {
    id: CardId,
    path: PathBuf,
    /// True when the file lives in Pegline's own inbox (we may recycle it).
    own: bool,
    size_px: (u32, u32),
    card_size: (f32, f32),
    tilt: f32,
    missing: u32,
    last_write: SystemTime,
}

/// Worker results.
enum Event {
    CaptureFile(PathBuf),
    ClipboardChanged,
    CaptureReady {
        path: PathBuf,
        own: bool,
        size_px: (u32, u32),
        thumb: ImageData,
        area: Option<RectI>,
    },
    RestoreReady {
        id: CardId,
        size_px: (u32, u32),
        thumb: ImageData,
    },
    CopyReady {
        id: CardId,
        dib: ImageData,
        png: Vec<u8>,
    },
    TrashDone {
        paths: Vec<PathBuf>,
    },
    Quit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MouseKind {
    LeftDown,
    LeftUp,
    RightDown,
    RightUp,
}

struct MouseEvent {
    kind: MouseKind,
    x: i32,
    y: i32,
    time: f64,
}

fn mouse_queue() -> &'static Mutex<VecDeque<MouseEvent>> {
    static QUEUE: OnceLock<Mutex<VecDeque<MouseEvent>>> = OnceLock::new();
    QUEUE.get_or_init(|| Mutex::new(VecDeque::with_capacity(32)))
}

/// The low-level mouse hook: records the buttons and returns immediately.
///
/// SAFETY: the hook runs on the UI thread while it pumps messages; the queue is
/// locked briefly and never held across OLE/COM calls.
unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let message = wparam.0 as u32;
        let kind = match message {
            WM_LBUTTONDOWN => Some(MouseKind::LeftDown),
            WM_LBUTTONUP => Some(MouseKind::LeftUp),
            WM_RBUTTONDOWN => Some(MouseKind::RightDown),
            WM_RBUTTONUP => Some(MouseKind::RightUp),
            _ => None,
        };
        if let Some(kind) = kind {
            // SAFETY: WH_MOUSE_LL passes an MSLLHOOKSTRUCT for mouse messages.
            let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
            if let Ok(mut queue) = mouse_queue().try_lock() {
                if queue.len() < 256 {
                    queue.push_back(MouseEvent {
                        kind,
                        x: info.pt.x,
                        y: info.pt.y,
                        time: win32::now_seconds(),
                    });
                }
            }
        }
    }
    // SAFETY: the documented chaining call for any hook procedure.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// A card being pressed, plus what has happened to it so far.
struct Press {
    id: CardId,
    start: (i32, i32),
    since: f64,
    moved: bool,
    handled: bool,
}

/// Process entry point; returns the process exit code.
pub fn run() -> i32 {
    let com = match window::ComGuard::ole().or_else(|_| window::ComGuard::apartment()) {
        Ok(guard) => Some(guard),
        Err(error) => {
            logging::error(error);
            None
        }
    };

    let app = Box::new(App::new());
    let raw = Box::into_raw(app);
    // SAFETY: `raw` is a live boxed App for as long as the message loop runs;
    // the window procedure only touches it from this thread.
    let started = unsafe { (*raw).start() };
    if let Err(error) = started {
        logging::error(format!("startup failed: {error}"));
        unsafe { drop(Box::from_raw(raw)) };
        return 1;
    }

    let code = window::message_loop();

    // SAFETY: the loop has ended; no window procedure can run after the windows
    // are destroyed inside shutdown().
    unsafe {
        (*raw).shutdown();
        drop(Box::from_raw(raw));
    }
    drop(com);
    code
}

struct App {
    hwnd: HWND,
    settings: Settings,
    tray: Option<Tray>,
    overlay: Option<Overlay>,
    events_tx: Sender<Event>,
    events_rx: Receiver<Event>,

    items: Vec<Item>,
    next_id: CardId,
    screen: Option<Monitor>,
    demo: bool,

    is_present: bool,
    is_revealed: bool,
    pinned: bool,
    peek_until: f64,
    hot_zone_since: Option<f64>,
    away_since: Option<f64>,
    band_suppressed: bool,
    keep_open: bool,
    welcome_until: Option<f64>,
    empty_since: Option<f64>,
    hide_at: Option<f64>,

    hover: Option<CardId>,
    pressed: Option<Press>,
    last_click: Option<(f64, CardId)>,
    menu_open: bool,

    hook: Option<HHOOK>,
    watcher: Option<watcher::Watcher>,
    clipboard_at: Option<f64>,
    start_time: f64,
    last_prune: f64,
    quit: bool,
    destroyed: bool,
    hung_files: Vec<PathBuf>,
}

impl App {
    fn new() -> Self {
        let (events_tx, events_rx) = mpsc::channel();
        let start = win32::now_seconds();
        Self {
            hwnd: HWND::default(),
            settings: Settings::load(),
            tray: None,
            overlay: None,
            events_tx,
            events_rx,
            items: Vec::new(),
            next_id: 1,
            screen: None,
            demo: std::env::args().any(|arg| arg == "--demo"),
            is_present: false,
            is_revealed: false,
            pinned: false,
            peek_until: 0.0,
            hot_zone_since: None,
            away_since: None,
            band_suppressed: false,
            keep_open: false,
            welcome_until: None,
            empty_since: None,
            hide_at: None,
            hover: None,
            pressed: None,
            last_click: None,
            menu_open: false,
            hook: None,
            watcher: None,
            clipboard_at: None,
            start_time: start,
            last_prune: start,
            quit: false,
            destroyed: false,
            hung_files: Vec::new(),
        }
    }

    fn start(&mut self) -> Result<(), String> {
        self.hwnd = window::create_message_window(
            CLASS_NAME,
            WINDOW_TITLE,
            Some(hidden_wndproc),
            self as *mut App as *mut (),
        )?;
        // SAFETY: the window exists; the timer id is ours.
        let timer = unsafe { SetTimer(Some(self.hwnd), TIMER_TICK, TICK_MS, None) };
        if timer == 0 {
            logging::error("SetTimer failed");
        }

        self.register_hotkeys();
        self.install_mouse_hook();

        if let Err(error) = clipboard::install_listener(self.hwnd) {
            logging::error(format!("clipboard listener: {error}"));
        }

        let dpi = win32::window_dpi(self.hwnd);
        match Tray::install(self.hwnd, dpi) {
            Ok(tray) => self.tray = Some(tray),
            Err(error) => logging::error(format!("tray: {error}")),
        }

        self.open_overlay();

        // Watch the real Windows screenshot folder. The watcher treats existing
        // files as known, so only new captures are reported.
        let folder = win32::screenshots_folder().unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("USERPROFILE").unwrap_or_default())
                .join("Pictures")
                .join("Screenshots")
        });
        let tx = self.events_tx.clone();
        match watcher::start(
            folder.clone(),
            Box::new(move |paths| {
                for path in paths {
                    let _ = tx.send(Event::CaptureFile(path));
                    window::wake(HWND::default());
                }
            }),
        ) {
            Ok(watcher) => {
                self.watcher = Some(watcher);
                logging::trace(format!("watching {}", folder.display()));
            }
            Err(error) => logging::error(format!("watcher: {error}")),
        }

        self.restore_pegged();

        if !self.settings.welcomed() {
            self.keep_open = true;
            self.welcome_until = Some(self.start_time + WELCOME_SECONDS);
            self.settings.mark_welcomed();
        }

        if self.demo {
            self.install_demo_cards();
        }

        logging::trace("app started");
        Ok(())
    }

    fn register_hotkeys(&mut self) {
        // SAFETY: the window is ours; the ids are ours; the modifiers/keys come
        // from the parser.
        unsafe {
            let _ = RegisterHotKey(
                Some(self.hwnd),
                HOTKEY_TOGGLE,
                windows::Win32::UI::Input::KeyboardAndMouse::HOT_KEY_MODIFIERS(
                    windows::Win32::UI::Input::KeyboardAndMouse::MOD_CONTROL.0
                        | windows::Win32::UI::Input::KeyboardAndMouse::MOD_ALT.0,
                ),
                'T' as u32,
            );
        }
        let snip = self
            .settings
            .string("snipKey")
            .and_then(shortcut::parse)
            .or_else(|| shortcut::parse("Ctrl+Alt+S"));
        if let Some(hotkey) = snip {
            // SAFETY: as above.
            let result = unsafe {
                RegisterHotKey(
                    Some(self.hwnd),
                    HOTKEY_SNIP,
                    windows::Win32::UI::Input::KeyboardAndMouse::HOT_KEY_MODIFIERS(
                        hotkey.register_modifiers(),
                    ),
                    hotkey.virtual_key,
                )
            };
            if let Err(error) = result {
                logging::error(format!("RegisterHotKey(snip): {error}"));
            }
        }
        let note = self
            .settings
            .string("noteKey")
            .and_then(shortcut::parse)
            .or_else(|| shortcut::parse("Ctrl+Alt+N"));
        if let Some(hotkey) = note {
            let result = unsafe {
                RegisterHotKey(
                    Some(self.hwnd),
                    HOTKEY_NOTE,
                    windows::Win32::UI::Input::KeyboardAndMouse::HOT_KEY_MODIFIERS(
                        hotkey.register_modifiers(),
                    ),
                    hotkey.virtual_key,
                )
            };
            if let Err(error) = result {
                logging::error(format!("RegisterHotKey(note): {error}"));
            }
        }
    }

    fn install_mouse_hook(&mut self) {
        // SAFETY: the hook procedure is a static function; `None` module is
        // this process and the thread id 0 means "the calling thread".
        match unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), None, 0) } {
            Ok(hook) => self.hook = Some(hook),
            Err(error) => logging::error(format!("SetWindowsHookExW: {error}")),
        }
    }

    fn open_overlay(&mut self) {
        let screen = self
            .pointer_screen()
            .or_else(|| win32::monitors().into_iter().next());
        let Some(screen) = screen else {
            logging::error("no monitors found");
            return;
        };
        logging::trace(format!(
            "overlay screen bounds={:?} work={:?} dpi={}",
            screen.bounds, screen.work, screen.dpi
        ));
        match Overlay::create(screen.work, screen.dpi) {
            Ok(overlay) => {
                self.overlay = Some(overlay);
                self.screen = Some(screen);
            }
            Err(error) => logging::error(format!("overlay: {error}")),
        }
    }

    fn pointer_screen(&self) -> Option<Monitor> {
        let (x, y) = win32::cursor_pos()?;
        win32::monitor_at(x, y)
    }

    /// Handles one window message. `None` means "let DefWindowProc do it".
    fn on_message(
        &mut self,
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<LRESULT> {
        match message {
            WM_TIMER | WM_APP_WAKE => {
                self.tick();
                Some(LRESULT(0))
            }
            WM_APP_QUIT => {
                self.request_quit();
                Some(LRESULT(0))
            }
            WM_TRAY_CALLBACK => {
                self.on_tray(wparam, lparam);
                Some(LRESULT(0))
            }
            WM_HOTKEY => {
                match wparam.0 as i32 {
                    HOTKEY_TOGGLE => self.toggle(),
                    HOTKEY_SNIP => self.snip(),
                    HOTKEY_NOTE => self.note_shortcut(),
                    _ => {}
                }
                Some(LRESULT(0))
            }
            WM_CLIPBOARDUPDATE => {
                self.on_clipboard_changed();
                Some(LRESULT(0))
            }
            WM_CLOSE => {
                // SAFETY: our own window.
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                Some(LRESULT(0))
            }
            WM_DESTROY => {
                self.destroyed = true;
                window::quit();
                Some(LRESULT(0))
            }
            _ => {
                if message == taskbar_created() {
                    if let Some(tray) = self.tray.as_mut() {
                        tray.reinstall();
                    }
                    return Some(LRESULT(0));
                }
                None
            }
        }
    }

    fn on_tray(&mut self, wparam: WPARAM, lparam: LPARAM) {
        if wparam.0 != 1 {
            return;
        }
        let mouse = lparam.0 as u32;
        if mouse == WM_LBUTTONUP {
            self.toggle();
        } else if mouse == WM_RBUTTONUP {
            let state = self.menu_state();
            let command = self.tray.as_mut().and_then(|tray| tray.show_menu(&state));
            if let Some(command) = command {
                self.apply(command);
            }
        }
    }

    fn menu_state(&self) -> MenuState {
        MenuState {
            revealed: self.is_revealed,
            catch_clipboard: self.settings.catch_clipboard(),
            sound_on: !self.settings.sound_off(),
            autostart: win32::autostart_enabled(),
            caught_files: count_inbox_files(),
            live_cards: self.items.len(),
            empty: self.items.is_empty(),
            hotkey_registered: true,
            snip_key: self
                .settings
                .string("snipKey")
                .unwrap_or("Ctrl+Alt+S")
                .to_string(),
            note_key: self
                .settings
                .string("noteKey")
                .unwrap_or("Ctrl+Alt+N")
                .to_string(),
        }
    }

    fn apply(&mut self, command: MenuCommand) {
        match command {
            MenuCommand::ToggleReveal => self.toggle(),
            MenuCommand::Snip => self.snip(),
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
            MenuCommand::Note | MenuCommand::SnipKey | MenuCommand::NoteKey => {
                logging::trace("menu command not implemented in the MVP");
            }
        }
    }

    fn tick(&mut self) {
        if self.quit {
            return;
        }
        let now = win32::now_seconds();
        self.drain_events(now);
        if let Some(at) = self.clipboard_at {
            if now >= at {
                self.clipboard_at = None;
                self.handle_clipboard();
            }
        }
        self.drain_mouse(now);
        self.update_pointer(now);
        self.update_timers(now);
        self.prune_missing(now);
        self.update_click_through();
    }

    fn drain_events(&mut self, now: f64) {
        while let Ok(event) = self.events_rx.try_recv() {
            match event {
                Event::Quit => self.request_quit(),
                Event::CaptureFile(path) => self.on_capture_file(path),
                Event::ClipboardChanged => {
                    if self.clipboard_at.is_none() {
                        self.clipboard_at = Some(now + CLIPBOARD_SETTLE);
                    }
                }
                Event::CaptureReady {
                    path,
                    own,
                    size_px,
                    thumb,
                    area,
                } => self.on_capture_ready(path, own, size_px, thumb, area, now),
                Event::RestoreReady { id, size_px, thumb } => {
                    self.on_restore_ready(id, size_px, thumb)
                }
                Event::CopyReady { id, dib, png } => self.on_copy_ready(id, dib, png),
                Event::TrashDone { paths } => {
                    logging::trace(format!("recycled {} files", paths.len()));
                }
            }
        }
    }

    fn drain_mouse(&mut self, _now: f64) {
        loop {
            let event = {
                let Ok(mut queue) = mouse_queue().try_lock() else {
                    return;
                };
                queue.pop_front()
            };
            let Some(event) = event else {
                return;
            };
            match event.kind {
                MouseKind::LeftDown => self.on_left_down(event.x, event.y, event.time),
                MouseKind::LeftUp => self.on_left_up(event.x, event.y, event.time),
                MouseKind::RightUp => self.on_right_up(event.x, event.y),
                MouseKind::RightDown => {}
            }
        }
    }

    fn on_left_down(&mut self, x: i32, y: i32, now: f64) {
        let Some((id, frame)) = self.hit_test(x, y) else {
            return;
        };
        // The discard cross is the top-left 26 DIP square of the card.
        let (dip_x, dip_y) = self.to_window_dip(x, y);
        if dip_x >= frame.left
            && dip_x < frame.left + layout::CROSS_HIT
            && dip_y >= frame.top
            && dip_y < frame.top + layout::CROSS_HIT
        {
            self.discard(id);
            return;
        }
        self.pressed = Some(Press {
            id,
            start: (x, y),
            since: now,
            moved: false,
            handled: false,
        });
        self.send_scene(Command::Press(Some(id)));
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.set_click_through(false);
        }
    }

    fn on_left_up(&mut self, x: i32, y: i32, now: f64) {
        let Some(press) = self.pressed.take() else {
            return;
        };
        self.send_scene(Command::Press(None));
        if press.moved || press.handled {
            return;
        }
        let (dx, dy) = (x - press.start.0, y - press.start.1);
        let moved = (dx.abs() + dy.abs()) as f32;
        let scale = self.overlay.as_ref().map(Overlay::scale).unwrap_or(1.0);
        if moved > DRAG_DIP * scale {
            return;
        }
        let double = self
            .last_click
            .map(|(when, id)| id == press.id && now - when < DOUBLE_CLICK)
            .unwrap_or(false);
        if double {
            self.last_click = None;
            self.open_item(press.id);
        } else {
            self.last_click = Some((now, press.id));
            self.copy_item(press.id);
        }
    }

    fn on_right_up(&mut self, x: i32, y: i32) {
        let Some((id, _)) = self.hit_test(x, y) else {
            return;
        };
        let Some(next) = self.show_card_menu(id, x, y) else {
            return;
        };
        match next {
            CardAction::Copy => self.copy_item(id),
            CardAction::Open => self.open_item(id),
            CardAction::Edit => self.edit_item(id),
            CardAction::Reveal => {
                if let Some(item) = self.items.iter().find(|item| item.id == id) {
                    if let Err(error) = win32::reveal_in_explorer(&item.path) {
                        logging::error(format!("reveal: {error}"));
                    }
                }
            }
            CardAction::SaveDesktop => self.save_to_desktop(id),
            CardAction::Discard | CardAction::TakeDown | CardAction::Trash => self.discard(id),
        }
    }

    fn update_pointer(&mut self, now: f64) {
        let Some((x, y)) = win32::cursor_pos() else {
            return;
        };
        let Some(screen) = win32::monitor_at(x, y) else {
            return;
        };

        let screen_changed = self
            .screen
            .map(|current| current.handle != screen.handle)
            .unwrap_or(true);
        if screen_changed && self.is_revealed {
            self.place_on_screen(screen);
        }

        let band_top = screen.bounds.top + (screen.work.top - screen.bounds.top).max(2);
        let in_band = x >= screen.bounds.left
            && x < screen.bounds.right
            && y >= screen.bounds.top
            && y < band_top;
        if !in_band {
            self.band_suppressed = false;
        }

        let blocked = win32::foreground_is_fullscreen_on(screen.bounds);
        let button_down = unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } < 0;

        if !self.is_revealed {
            let can_reveal =
                self.wanted() && !blocked && in_band && !self.band_suppressed && !button_down;
            if can_reveal {
                match self.hot_zone_since {
                    Some(since) if now - since >= REVEAL_DELAY => {
                        if screen_changed || self.screen.is_none() {
                            self.place_on_screen(screen);
                        }
                        logging::trace(format!("reveal: dwell elapsed on {x},{y}"));
                        self.present();
                        self.reveal(false, 0.0);
                    }
                    Some(_) => {}
                    None => self.hot_zone_since = Some(now),
                }
            } else {
                self.hot_zone_since = None;
            }
        } else {
            // The zone reaches from the screen's top edge to the window bottom,
            // so moving up never hides the line.
            let inside = self.overlay.as_ref().is_some_and(|overlay| {
                let Some(rect) = win32::window_rect(overlay.hwnd()) else {
                    return false;
                };
                x >= rect.left && x < rect.right && y >= screen.bounds.top && y < rect.bottom
            });
            if inside && self.pinned {
                self.pinned = false;
            }
            let busy =
                self.pinned || self.pressed.is_some() || self.menu_open || now < self.peek_until;
            if inside || busy {
                self.away_since = None;
            } else {
                let since = *self.away_since.get_or_insert(now);
                if now - since >= RETRACT_DELAY {
                    self.away_since = None;
                    self.set_revealed(false);
                }
            }
        }

        // Hover and click-through.
        let hover = if self.is_revealed {
            self.hit_test(x, y).map(|(id, _)| id)
        } else {
            None
        };
        if self.hover != hover {
            self.hover = hover;
            self.send_scene(Command::Hover(hover));
        }
        if let Some(press) = self.pressed.as_mut() {
            let scale = self.overlay.as_ref().map(Overlay::scale).unwrap_or(1.0);
            let distance = ((x - press.start.0).abs() + (y - press.start.1).abs()) as f32;
            if distance > DRAG_DIP * scale {
                press.moved = true;
            }
            if !press.moved
                && !press.handled
                && now - press.since >= LONG_PRESS
                && unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } < 0
            {
                let id = press.id;
                press.handled = true;
                self.edit_item(id);
            }
        }
        // Start a drag once the pointer has clearly moved.
        if let Some(press) = self.pressed.as_ref() {
            if press.moved && !press.handled && unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } < 0
            {
                let id = press.id;
                self.start_drag(id);
            }
        }
    }

    fn update_timers(&mut self, now: f64) {
        if let Some(hide_at) = self.hide_at {
            if now >= hide_at {
                self.hide_at = None;
                if let Some(overlay) = self.overlay.as_mut() {
                    overlay.hide();
                }
                self.is_present = false;
            }
        }
        if let Some(until) = self.welcome_until {
            if now >= until {
                self.welcome_until = None;
                self.keep_open = false;
                if self.items.is_empty() {
                    self.dismiss(now);
                }
            }
        }
        if self.items.is_empty() && !self.keep_open && self.is_present {
            let since = *self.empty_since.get_or_insert(now);
            if now - since >= EMPTY_DELAY {
                self.empty_since = None;
                self.dismiss(now);
            }
        } else {
            self.empty_since = None;
        }
    }

    fn prune_missing(&mut self, now: f64) {
        if now - self.last_prune < 1.0 {
            return;
        }
        self.last_prune = now;
        if self.demo {
            return;
        }
        let mut gone = Vec::new();
        for item in &mut self.items {
            if item.path.exists() {
                item.missing = 0;
            } else {
                item.missing += 1;
                if item.missing >= MISSING_LIMIT {
                    gone.push(item.id);
                }
            }
        }
        for id in gone {
            self.remove_item(id);
        }
    }

    fn update_click_through(&mut self) {
        let Some(overlay) = self.overlay.as_mut() else {
            return;
        };
        let pass = self.hover.is_none() && self.pressed.is_none();
        overlay.set_click_through(pass);
    }

    // ---- reveal state -----------------------------------------------------

    fn wanted(&self) -> bool {
        !self.items.is_empty() || self.keep_open || self.demo
    }

    fn present(&mut self) {
        if self.is_present {
            return;
        }
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.show(true);
            self.is_present = true;
            self.hide_at = None;
        }
    }

    fn dismiss(&mut self, now: f64) {
        if !self.is_present {
            return;
        }
        self.set_revealed(false);
        if self.hide_at.is_none() {
            self.hide_at = Some(now + HIDE_DELAY);
        }
    }

    fn reveal(&mut self, pin: bool, peek: f64) {
        if !self.is_present {
            return;
        }
        if pin {
            self.pinned = true;
        }
        if peek > 0.0 {
            self.peek_until = win32::now_seconds() + peek;
        }
        self.away_since = None;
        if self.is_revealed {
            return;
        }
        self.is_revealed = true;
        self.send_scene(Command::Reveal(true));
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.show(true);
        }
    }

    fn set_revealed(&mut self, revealed: bool) {
        if self.is_revealed == revealed {
            return;
        }
        self.is_revealed = revealed;
        self.pinned = false;
        self.peek_until = 0.0;
        if !revealed {
            self.hover = None;
            self.send_scene(Command::Hover(None));
        }
        self.send_scene(Command::Reveal(revealed));
    }

    fn toggle(&mut self) {
        if self.is_revealed {
            self.dismiss(win32::now_seconds());
        } else {
            let screen = self.pointer_screen();
            if let Some(screen) = screen {
                if self.screen.map(|current| current.handle) != Some(screen.handle) {
                    self.place_on_screen(screen);
                }
            }
            self.present();
            self.reveal(true, 0.0);
        }
    }

    fn place_on_screen(&mut self, screen: Monitor) {
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.place(screen.work, screen.dpi);
            overlay.send(Command::SetVisible(self.is_present));
        }
        self.screen = Some(screen);
        self.enforce_capacity();
    }

    // ---- items ------------------------------------------------------------

    fn restore_pegged(&mut self) {
        let paths = settings::read_pegged();
        for path in paths {
            if !path.exists() || !wic::is_image_path(&path) {
                continue;
            }
            let id = self.next_id;
            self.next_id += 1;
            let tx = self.events_tx.clone();
            let thread_path = path.clone();
            let _ = std::thread::Builder::new()
                .name("pegline-restore".to_string())
                .spawn(
                    move || match wic::decode_scaled(&thread_path, crate::capture::THUMB_MAX) {
                        Ok(thumb) => {
                            let _ = tx.send(Event::RestoreReady {
                                id,
                                size_px: (thumb.width, thumb.height),
                                thumb,
                            });
                            window::wake(HWND::default());
                        }
                        Err(error) => {
                            logging::error(format!("restore {}: {error}", thread_path.display()))
                        }
                    },
                );
        }
    }

    fn on_restore_ready(&mut self, id: CardId, size_px: (u32, u32), thumb: ImageData) {
        let Some(path) = settings::read_pegged().into_iter().nth(self.items.len()) else {
            return;
        };
        if self.items.len() >= layout::capacity(100_000.0) {
            return;
        }
        let own = path.starts_with(win32::inbox_dir());
        self.add_item(id, path, own, size_px, thumb, None, false);
    }

    fn on_capture_file(&mut self, path: PathBuf) {
        if !wic::is_image_path(&path) {
            return;
        }
        let Ok(metadata) = std::fs::metadata(&path) else {
            return;
        };
        if metadata.len() == 0 {
            return;
        }
        let fresh = metadata
            .modified()
            .ok()
            .and_then(|time| SystemTime::now().duration_since(time).ok())
            .map(|age| age.as_secs_f64() < 3.0)
            .unwrap_or(false);
        let own = path.starts_with(win32::inbox_dir());
        let area = self.guess_area(&path, fresh);
        self.spawn_capture(path, own, area);
    }

    fn spawn_capture(&mut self, path: PathBuf, own: bool, area: Option<RectI>) {
        let tx = self.events_tx.clone();
        let thread_path = path.clone();
        let _ = std::thread::Builder::new()
            .name("pegline-decode".to_string())
            .spawn(
                move || match wic::decode_scaled(&thread_path, crate::capture::THUMB_MAX) {
                    Ok(thumb) => {
                        let size_px =
                            wic::pixel_size(&thread_path).unwrap_or((thumb.width, thumb.height));
                        let _ = tx.send(Event::CaptureReady {
                            path: thread_path,
                            own,
                            size_px,
                            thumb,
                            area,
                        });
                        window::wake(HWND::default());
                    }
                    Err(error) => {
                        logging::error(format!("decode {}: {error}", thread_path.display()))
                    }
                },
            );
    }

    fn on_capture_ready(
        &mut self,
        path: PathBuf,
        own: bool,
        size_px: (u32, u32),
        thumb: ImageData,
        _area: Option<RectI>,
        now: f64,
    ) {
        if self.items.iter().any(|item| item.path == path) {
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.add_item(id, path, own, size_px, thumb, None, true);
        self.present();
        self.reveal(false, PEEK_SECONDS);
        let _ = now;
    }

    #[allow(clippy::too_many_arguments)]
    fn add_item(
        &mut self,
        id: CardId,
        path: PathBuf,
        own: bool,
        size_px: (u32, u32),
        thumb: ImageData,
        area: Option<RectI>,
        arriving: bool,
    ) {
        let card_size = layout::card_size(size_px.0 as f32, size_px.1 as f32);
        let tilt = pseudo_random(id) * 5.0 - 2.5;
        let item = Item {
            id,
            path: path.clone(),
            own,
            size_px,
            card_size,
            tilt,
            missing: 0,
            last_write: std::fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .unwrap_or(UNIX_EPOCH),
        };
        self.items.push(item);
        self.send_scene(Command::AddCard(CardSpec {
            id,
            card_size,
            tilt,
            arriving,
            image: Some(thumb),
        }));
        let _ = area;
        self.enforce_capacity();
        self.save_pegged();
        self.empty_since = None;
        if !self.is_present {
            self.present();
        }
    }

    fn alloc_id(&mut self) -> CardId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn enforce_capacity(&mut self) {
        let width_dip = self
            .overlay
            .as_ref()
            .map(|overlay| overlay.width_px() as f32 / overlay.scale().max(0.1))
            .unwrap_or(1920.0);
        let capacity = layout::capacity(width_dip);
        while self.items.len() > capacity {
            let id = self.items[0].id;
            self.remove_item(id);
        }
    }

    fn remove_item(&mut self, id: CardId) {
        if let Some(position) = self.items.iter().position(|item| item.id == id) {
            self.items.remove(position);
            self.send_scene(Command::RemoveCard { id });
            self.save_pegged();
        }
    }

    fn save_pegged(&mut self) {
        let paths: Vec<PathBuf> = self.items.iter().map(|item| item.path.clone()).collect();
        settings::write_pegged(&paths);
    }

    fn hit_test(&self, x: i32, y: i32) -> Option<(CardId, crate::types::RectF)> {
        let overlay = self.overlay.as_ref()?;
        let rect = win32::window_rect(overlay.hwnd())?;
        let snapshot = overlay.snapshot();
        let snapshot = snapshot.lock().ok()?;
        let scale = snapshot.scale.max(0.1);
        let dip_x = (x - rect.left) as f32 / scale;
        let dip_y = (y - rect.top) as f32 / scale;
        let id = snapshot.card_at(dip_x, dip_y)?;
        let card = snapshot.cards.iter().find(|card| card.id == id)?;
        Some((id, card.frame))
    }

    fn to_window_dip(&self, x: i32, y: i32) -> (f32, f32) {
        let Some(overlay) = self.overlay.as_ref() else {
            return (f32::NAN, f32::NAN);
        };
        let Some(rect) = win32::window_rect(overlay.hwnd()) else {
            return (f32::NAN, f32::NAN);
        };
        let scale = overlay.scale().max(0.1);
        (
            (x - rect.left) as f32 / scale,
            (y - rect.top) as f32 / scale,
        )
    }

    // ---- card actions -----------------------------------------------------

    fn copy_item(&mut self, id: CardId) {
        let Some(item) = self.items.iter().find(|item| item.id == id) else {
            return;
        };
        let path = item.path.clone();
        let tx = self.events_tx.clone();
        let _ = std::thread::Builder::new()
            .name("pegline-copy".to_string())
            .spawn(
                move || match wic::decode_scaled(&path, crate::capture::COMPARE_MAX) {
                    Ok(dib) => {
                        let png = match wic::encode_png_bytes(&dib) {
                            Ok(bytes) => bytes,
                            Err(error) => {
                                logging::error(format!("copy encode: {error}"));
                                return;
                            }
                        };
                        let _ = tx.send(Event::CopyReady { id, dib, png });
                        window::wake(HWND::default());
                    }
                    Err(error) => logging::error(format!("copy decode: {error}")),
                },
            );
    }

    fn on_copy_ready(&mut self, id: CardId, dib: ImageData, png: Vec<u8>) {
        let Some(item) = self.items.iter().find(|item| item.id == id) else {
            return;
        };
        let path = item.path.clone();
        match clipboard::write_image(self.hwnd, &dib, &png, Some(&path)) {
            Ok(()) => logging::trace(format!("copied card {id} ({} png bytes)", png.len())),
            Err(error) => {
                logging::error(format!("clipboard write: {error}"));
                return;
            }
        }
        clipboard::mark_self_write();
        self.send_scene(Command::Copied(id));
    }

    fn open_item(&mut self, id: CardId) {
        let Some(item) = self.items.iter().find(|item| item.id == id) else {
            return;
        };
        if let Err(error) = win32::shell_open(&item.path) {
            logging::error(format!("open: {error}"));
        }
    }

    fn edit_item(&mut self, id: CardId) {
        let Some(item) = self.items.iter().find(|item| item.id == id) else {
            return;
        };
        let path = item.path.clone();
        // The MVP uses the default editor; the markup sheet is phase B.
        if let Err(error) = win32::shell_open(&path) {
            logging::error(format!("edit: {error}"));
        }
    }

    fn save_to_desktop(&mut self, id: CardId) {
        let Some(item) = self.items.iter().find(|item| item.id == id) else {
            return;
        };
        let Some(desktop) =
            std::env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join("Desktop"))
        else {
            return;
        };
        let Some(name) = item.path.file_name() else {
            return;
        };
        let target = unique_path(&desktop, &PathBuf::from(name));
        if let Err(error) = std::fs::copy(&item.path, &target) {
            logging::error(format!("save to desktop: {error}"));
            return;
        }
        self.remove_item(id);
    }

    fn discard(&mut self, id: CardId) {
        let Some(item) = self.items.iter().find(|item| item.id == id) else {
            return;
        };
        let path = item.path.clone();
        let own = item.own;
        if own {
            let tx = self.events_tx.clone();
            let _ = std::thread::Builder::new()
                .name("pegline-trash".to_string())
                .spawn(move || {
                    if let Err(error) = win32::recycle_files(std::slice::from_ref(&path)) {
                        logging::error(format!("recycle {}: {error}", path.display()));
                    }
                    let _ = tx.send(Event::TrashDone { paths: vec![path] });
                });
        }
        self.remove_item(id);
    }

    fn clear_line(&mut self) {
        for id in self.items.iter().map(|item| item.id).collect::<Vec<_>>() {
            self.remove_item(id);
        }
    }

    fn clear_inbox(&mut self) {
        let files: Vec<PathBuf> = std::fs::read_dir(win32::inbox_dir())
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                    .filter(|path| wic::is_image_path(path))
                    .collect()
            })
            .unwrap_or_default();
        if files.is_empty() {
            return;
        }
        for id in self
            .items
            .iter()
            .filter(|item| item.own)
            .map(|item| item.id)
            .collect::<Vec<_>>()
        {
            self.remove_item(id);
        }
        let tx = self.events_tx.clone();
        let list = files.clone();
        let _ = std::thread::Builder::new()
            .name("pegline-trash".to_string())
            .spawn(move || {
                if let Err(error) = win32::recycle_files(&list) {
                    logging::error(format!("recycle inbox: {error}"));
                }
                let _ = tx.send(Event::TrashDone { paths: list });
            });
    }

    fn start_drag(&mut self, id: CardId) {
        self.pressed = None;
        let Some(item) = self.items.iter().find(|item| item.id == id) else {
            return;
        };
        let path = item.path.clone();
        if !path.exists() {
            self.remove_item(id);
            return;
        }
        // Click-through while dragging so the drop target under the pointer is
        // reachable; the strip stays visible.
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.set_click_through(true);
        }
        self.send_scene(Command::Dragging(Some(id)));
        let png = std::fs::read(&path).unwrap_or_default();
        let image = ImageData::new(1, 1, vec![0, 0, 0, 0]);
        let effect = crate::dragdrop::drag_file(self.hwnd, &path, &image, &png).unwrap_or(0);
        self.send_scene(Command::Dragging(None));
        // Clear the backlog of hook events produced during the modal drag loop.
        if let Ok(mut queue) = mouse_queue().lock() {
            queue.clear();
        }
        let moved = effect & 2 != 0; // DROPEFFECT_MOVE
        if moved && !path.exists() {
            self.remove_item(id);
        }
        self.update_click_through();
    }

    fn show_card_menu(&mut self, id: CardId, x: i32, y: i32) -> Option<CardAction> {
        let own = self.items.iter().find(|item| item.id == id)?.own;
        let strings = i18n::s();
        // SAFETY: menus are created and destroyed on this thread.
        unsafe {
            let menu = CreatePopupMenu().ok()?;
            let add = |text: &str, command: usize| {
                let wide = crate::win32::wide(text);
                let _ = AppendMenuW(menu, MF_STRING, command, PCWSTR(wide.as_ptr()));
            };
            add(strings.copy, 1);
            add(strings.open, 2);
            add(strings.edit, 3);
            add(strings.show_in_explorer, 4);
            if own {
                add(strings.save_to_desktop, 5);
            }
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            if own {
                add(strings.discard, 6);
            } else {
                add(strings.take_down, 7);
                add(strings.move_to_recycle, 8);
            }
            self.menu_open = true;
            let _ = SetForegroundWindow(self.hwnd);
            let chosen = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
                x,
                y,
                Some(0),
                self.hwnd,
                None,
            );
            let _ = DestroyMenu(menu);
            let _ = PostMessageW(Some(self.hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            self.menu_open = false;
            match chosen.0 as usize {
                1 => Some(CardAction::Copy),
                2 => Some(CardAction::Open),
                3 => Some(CardAction::Edit),
                4 => Some(CardAction::Reveal),
                5 => Some(CardAction::SaveDesktop),
                6 => Some(CardAction::Discard),
                7 => Some(CardAction::TakeDown),
                8 => Some(CardAction::Trash),
                _ => None,
            }
        }
    }

    // ---- capture ----------------------------------------------------------

    fn on_clipboard_changed(&mut self) {
        if !self.settings.catch_clipboard() || clipboard::self_write_recent() {
            return;
        }
        let _ = self.events_tx.send(Event::ClipboardChanged);
    }

    fn handle_clipboard(&mut self) {
        let now = win32::now_seconds();
        let tx = self.events_tx.clone();
        let foreground = win32::foreground_rect().map(|(_, rect)| rect);
        let pointer = win32::cursor_pos().unwrap_or((0, 0));
        let screens: Vec<RectI> = win32::monitors().into_iter().map(|m| m.bounds).collect();
        let virtual_screen = win32::virtual_screen();
        let _ = std::thread::Builder::new()
            .name("pegline-clipboard".to_string())
            .spawn(move || {
                // WPF waits 0.15 s before reading; the settle is applied by the
                // caller's timer, but a short sleep here avoids racing the app
                // that just wrote the clipboard.
                std::thread::sleep(Duration::from_millis(20));
                let image = match clipboard::read_screenshot() {
                    Ok(Some(image)) => image,
                    Ok(None) => return,
                    Err(error) => {
                        logging::error(format!("clipboard read: {error}"));
                        return;
                    }
                };
                let size = (image.width, image.height);
                let area = areas::guess(size, &screens, virtual_screen, foreground, pointer, true);
                let folder = win32::inbox_dir();
                let _ = std::fs::create_dir_all(&folder);
                let path = inbox_path(&folder);
                if let Err(error) = wic::encode_png(&path, &image) {
                    logging::error(format!("clipboard save: {error}"));
                    return;
                }
                let thumb = match wic::decode_scaled(&path, crate::capture::THUMB_MAX) {
                    Ok(thumb) => thumb,
                    Err(error) => {
                        logging::error(format!("clipboard thumb: {error}"));
                        return;
                    }
                };
                let _ = tx.send(Event::CaptureReady {
                    path,
                    own: true,
                    size_px: size,
                    thumb,
                    area,
                });
                window::wake(HWND::default());
                let _ = now;
            });
    }

    fn snip(&mut self) {
        if let Err(error) = win32::shell_open(std::path::Path::new("ms-screenclip:")) {
            logging::error(format!("ms-screenclip: {error}"));
        }
    }

    fn note_shortcut(&mut self) {
        // Notes arrive in phase B; the shortcut is registered so the user sees
        // it as taken rather than silently doing nothing.
        let text = clipboard::read_text().unwrap_or_default();
        if text.trim().is_empty() {
            logging::trace("note shortcut: clipboard has no text");
        } else {
            logging::trace("note shortcut: notes are phase B");
        }
    }

    fn guess_area(&self, path: &std::path::Path, fresh: bool) -> Option<RectI> {
        let size = wic::pixel_size(path).ok()?;
        let screens: Vec<RectI> = win32::monitors().into_iter().map(|m| m.bounds).collect();
        let pointer = win32::cursor_pos().unwrap_or((0, 0));
        areas::guess(
            size,
            &screens,
            win32::virtual_screen(),
            win32::foreground_rect().map(|(_, rect)| rect),
            pointer,
            fresh,
        )
    }

    fn send_scene(&self, command: Command) {
        if let Some(overlay) = self.overlay.as_ref() {
            overlay.send(command);
        }
    }

    fn request_quit(&mut self) {
        if self.quit {
            return;
        }
        self.quit = true;
        self.keep_open = false;
        if !self.destroyed {
            // SAFETY: our own window.
            unsafe {
                let _ = DestroyWindow(self.hwnd);
            }
        } else {
            window::quit();
        }
    }

    fn install_demo_cards(&mut self) {
        let now = win32::now_seconds();
        for index in 0..3u32 {
            let (w, h) = match index {
                0 => (480, 270),
                1 => (270, 480),
                _ => (300, 300),
            };
            let mut pixels = vec![0u8; (w * h * 4) as usize];
            for y in 0..h {
                for x in 0..w {
                    let i = ((y * w + x) * 4) as usize;
                    pixels[i] = (40 + (x * 180 / w)) as u8;
                    pixels[i + 1] = (80 + (y * 120 / h)) as u8;
                    pixels[i + 2] = 180 + (index * 20) as u8;
                    pixels[i + 3] = 255;
                }
            }
            let id = self.alloc_id();
            let size_px = (w, h);
            let card_size = layout::card_size(w as f32, h as f32);
            let tilt = pseudo_random(id) * 5.0 - 2.5;
            self.items.push(Item {
                id,
                path: PathBuf::from(format!("demo://card{index}")),
                own: false,
                size_px,
                card_size,
                tilt,
                missing: 0,
                last_write: UNIX_EPOCH,
            });
            logging::trace(format!(
                "demo card id={id} px={w}x{h} card={:.1}x{:.1} tilt={tilt:.2}",
                card_size.0, card_size.1
            ));
            self.send_scene(Command::AddCard(CardSpec {
                id,
                card_size,
                tilt,
                arriving: false,
                image: Some(ImageData::new(w, h, pixels)),
            }));
        }
        self.present();
        self.keep_open = true;
        self.reveal(true, 0.0);
        logging::trace(format!(
            "demo cards: {} present={}",
            self.items.len(),
            self.is_present
        ));
        let _ = now;
    }

    fn shutdown(&mut self) {
        if let Some(hook) = self.hook.take() {
            // SAFETY: the hook was installed by this thread.
            unsafe {
                let _ = UnhookWindowsHookEx(hook);
            }
        }
        // SAFETY: hotkeys are per-window and ours.
        unsafe {
            let _ = UnregisterHotKey(Some(self.hwnd), HOTKEY_TOGGLE);
            let _ = UnregisterHotKey(Some(self.hwnd), HOTKEY_SNIP);
            let _ = UnregisterHotKey(Some(self.hwnd), HOTKEY_NOTE);
        }
        if let Some(tray) = self.tray.as_mut() {
            tray.remove();
        }
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.shutdown();
        }
        if let Some(watcher) = self.watcher.as_mut() {
            watcher.stop();
        }
        if !self.destroyed && !self.hwnd.is_invalid() {
            // SAFETY: our own window and timer.
            unsafe {
                let _ = KillTimer(Some(self.hwnd), TIMER_TICK);
                let _ = DestroyWindow(self.hwnd);
            }
        }
        let _ = self.settings.save();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CardAction {
    Copy,
    Open,
    Edit,
    Reveal,
    SaveDesktop,
    Discard,
    TakeDown,
    Trash,
}

/// Window procedure for the hidden message window.
///
/// SAFETY: `GWLP_USERDATA` holds the boxed `App` pointer for the lifetime of the
/// window; all messages are delivered on the thread that created it.
unsafe extern "system" fn hidden_wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: reading our own window's user data.
    let pointer = unsafe {
        windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(
            hwnd,
            windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA,
        )
    } as *mut App;

    if !pointer.is_null() {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // SAFETY: pointer validity is guaranteed by `app::run`.
            unsafe { (*pointer).on_message(hwnd, message, wparam, lparam) }
        }));
        match outcome {
            Ok(Some(result)) => return result,
            Ok(None) => {}
            Err(_) => logging::error("panic in the hidden window procedure"),
        }
    }

    // SAFETY: the default handler for messages we do not process.
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::DefWindowProcW(hwnd, message, wparam, lparam)
    }
}

fn taskbar_created() -> u32 {
    static MESSAGE: OnceLock<u32> = OnceLock::new();
    *MESSAGE.get_or_init(|| {
        // SAFETY: a registered message name; no pointers retained.
        unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) }
    })
}

fn pseudo_random(seed: u64) -> f32 {
    let mut x = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51afd7ed558ccd);
    x ^= x >> 33;
    (x & 0xFFFF) as f32 / 65535.0
}

fn count_inbox_files() -> usize {
    std::fs::read_dir(win32::inbox_dir())
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| wic::is_image_path(&entry.path()))
                .count()
        })
        .unwrap_or(0)
}

fn inbox_path(folder: &std::path::Path) -> PathBuf {
    let stamp = local_stamp();
    let prefix = i18n::s().screenshot_file_prefix;
    let base = folder.join(format!("{prefix} {stamp}.png"));
    unique_path(folder, Path::new(base.file_name().unwrap_or_default()))
}

fn unique_path(folder: &std::path::Path, name: &std::path::Path) -> PathBuf {
    let candidate = folder.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let stem = name
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_else(|| "Screenshot".to_string());
    let extension = name
        .extension()
        .map(|ext| ext.to_string_lossy().to_string())
        .unwrap_or_else(|| "png".to_string());
    for index in 2..10_000 {
        let candidate = folder.join(format!("{stem} {index}.{extension}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    folder.join(format!("{stem} {}.{extension}", pseudo_random(1) as u32))
}

/// Local time as `yyyy-MM-dd HHmmss`, the format the WPF app used.
fn local_stamp() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // The logging module owns the civil-date conversion for its timestamps;
    // reuse the same algorithm with the local offset from the system.
    let offset = local_offset_seconds();
    let local = now as i64 + offset as i64;
    let days = local.div_euclid(86_400);
    let seconds = local.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}{:02}{:02}",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    )
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn local_offset_seconds() -> i32 {
    use windows::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    let mut info = TIME_ZONE_INFORMATION::default();
    // SAFETY: the API fills the struct; the return value picks the bias.
    let result = unsafe { GetTimeZoneInformation(&mut info) };
    let bias = match result {
        0 => info.Bias,
        1 => info.Bias + info.StandardBias,
        2 => info.Bias + info.DaylightBias,
        _ => info.Bias,
    };
    -(bias * 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_path_appends_numbers() {
        let dir = std::env::temp_dir().join(format!("pegline-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let first = dir.join("Shot 2026-01-01 120000.png");
        std::fs::write(&first, b"x").unwrap();
        let second = unique_path(&dir, Path::new("Shot 2026-01-01 120000.png"));
        assert_eq!(
            second.file_name().unwrap().to_string_lossy(),
            "Shot 2026-01-01 120000 2.png"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stamp_shape_is_stable() {
        let stamp = local_stamp();
        assert_eq!(stamp.len(), 17, "yyyy-MM-dd HHmmss");
        assert_eq!(&stamp[4..5], "-");
        assert_eq!(&stamp[10..11], " ");
    }

    #[test]
    fn pseudo_random_is_in_unit_range() {
        for id in 0..100 {
            let value = pseudo_random(id);
            assert!((0.0..=1.0).contains(&value));
        }
    }
}
