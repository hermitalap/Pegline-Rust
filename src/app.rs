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
use crate::editor::{Editor, EditorEvent};
use crate::i18n;
use crate::layout;
use crate::logging;
use crate::notes;
use crate::preview::{self, HoverState};
use crate::scene::Command;
use crate::settings::{self, Settings};
use crate::shortcut;
use crate::sound::{self, Cue};
use crate::tray::{MenuCommand, MenuState, Tray};
use crate::types::{CardId, CardSpec, ImageData, RectI};
use crate::win32::{self, Monitor};
use crate::window::flight::{Flight, FlightEvent};
use crate::window::ghost::Ghost;
use crate::window::overlay::Overlay;
use crate::window::preview::Preview;
use crate::window::{self, WM_APP_QUIT, WM_APP_WAKE, WM_TRAY_CALLBACK};

const CLASS_NAME: PCWSTR = w!("PeglineMessage");
const WINDOW_TITLE: PCWSTR = w!("Pegline");
const TIMER_TICK: usize = 1;
const TICK_MS: u32 = 33;
/// Follows the pointer during a drag (BEHAVIOR §3.2: the ghost follows every
/// 15 ms).
const TIMER_GHOST: usize = 2;
const GHOST_MS: u32 = 15;

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
/// Inbox and Windows-saved copies of one screenshot merge within this window.
const TWIN_SECONDS: f64 = 4.0;
/// The same copied image is ignored for this long.
const CLIPBOARD_DUPLICATE_SECONDS: f64 = 10.0;
/// A pooled flight window idle for this long is destroyed to release its device
/// memory; the next flight builds a new one (~20 ms).
const FLIGHT_IDLE_SECONDS: f64 = 30.0;
/// The preview and drag-ghost windows are cheap to rebuild, so they are
/// destroyed sooner than the flight window. Their D3D devices are what cost
/// private memory when idle, not the window.
const HELPER_IDLE_SECONDS: f64 = 10.0;

/// One pegged item, as the UI thread knows it.
struct Item {
    id: CardId,
    path: PathBuf,
    /// True when the file lives in Pegline's own inbox (we may recycle it).
    own: bool,
    /// True for a sticky note (a `.txt` in Pegline's Notes folder).
    note: bool,
    /// Note colour index, 0–3. Always 0 for pictures.
    color: usize,
    /// Note text, cached so a copy never touches the disk on the UI thread.
    text: Option<String>,
    size_px: (u32, u32),
    card_size: (f32, f32),
    tilt: f32,
    /// The 480 px thumbnail, kept so a drop can be drawn without re-decoding.
    thumb: ImageData,
    missing: u32,
    last_write: SystemTime,
    /// Bumped whenever `thumb` changes; the hover preview uses it to notice an
    /// edit under the pointer.
    revision: u64,
}

/// A capture that is being annotated before it hangs (`Ctrl+Alt+S`).
struct EditingCapture {
    path: PathBuf,
    own: bool,
    area: Option<RectI>,
    /// The Windows-saved copy of the same screenshot, when one appears while
    /// the editor is open: the edit is written there instead (BEHAVIOR §9.5).
    twin: Option<PathBuf>,
    /// The thumbnail as it was before the edit, used to recognise a copy that
    /// reached the line before the editor opened.
    original: ImageData,
}

/// A capture seen in the last few seconds, kept for twin merging.
struct Recent {
    at: f64,
    path: PathBuf,
    own: bool,
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
        flight_image: Option<ImageData>,
        /// Another capture of the same picture with a different owner, found
        /// within the twin window.
        twin: Option<PathBuf>,
        /// Content hash, set for clipboard captures so a repeated copy within
        /// 10 s is ignored before it reaches the line.
        clip_hash: Option<u64>,
    },
    RestoreReady {
        id: CardId,
        path: PathBuf,
        size_px: (u32, u32),
        thumb: Option<ImageData>,
        note: bool,
        text: Option<String>,
        color: usize,
    },
    CopyReady {
        id: CardId,
        dib: ImageData,
        png: Vec<u8>,
    },
    /// The flight window reported progress.
    Flight(FlightEvent),
    /// A worker finished decoding the hover preview.
    PreviewReady {
        id: CardId,
        version: u32,
        image: ImageData,
        bounds: RectI,
        dpi: u32,
    },
    /// A new note is written and rendered.
    NoteReady {
        path: PathBuf,
        text: String,
        image: ImageData,
    },
    /// The same text is already hanging; show that note instead.
    NoteExisting {
        path: PathBuf,
    },
    /// A note was recoloured and re-rendered.
    NoteImage {
        id: CardId,
        image: ImageData,
    },
    /// An edited picture (or note) was reloaded after its file changed.
    ThumbnailReady {
        id: CardId,
        image: ImageData,
        text: Option<String>,
    },
    /// The annotation editor finished.
    Editor(EditorEvent),
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
    flight: Option<Flight>,
    /// The hover preview window; created on first use and pooled afterwards.
    preview: Option<Preview>,
    preview_state: HoverState,
    /// The drag preview; created on the first drag and hidden between drags.
    ghost: Option<Ghost>,
    ghost_follow: bool,
    /// True while the OLE drag loop owns the UI thread.
    dragging: bool,
    /// The annotation editor, while one is open.
    editor: Option<Editor>,
    editing: Option<EditingCapture>,
    /// `Ctrl+Alt+S`: annotate the next clipboard capture before hanging it.
    /// Unix-free monotonic deadline; 0 means not armed (60 s window, as WPF).
    arm_editor_until: f64,
    /// Files our own editor just wrote, ignored by the watcher for 2 s.
    recently_saved: Vec<(PathBuf, f64)>,
    last_reload: f64,
    /// When the pooled flight window last went idle; after a while it is
    /// destroyed to give its ~34 MB of device memory back (M6).
    flight_idle_at: Option<f64>,
    /// The card the flight window is carrying, if any. A newer capture or a
    /// drop interrupts the old animation (BEHAVIOR §3.8), so this is cleared as
    /// soon as the flight window moves on.
    flying_card: Option<CardId>,
    /// The same for the preview and the drag ghost, which are cheaper to
    /// rebuild.
    preview_idle_at: Option<f64>,
    ghost_idle_at: Option<f64>,
    /// Captures from the twin window (4 s), used to fold the inbox copy and the
    /// Windows-saved copy of the same screenshot into one card.
    recent: Vec<Recent>,
    /// Content hash of the last clipboard capture and when it arrived, so the
    /// same copied image is not hung twice (10 s window).
    last_clip_hash: u64,
    last_clip_at: f64,
    /// Whether RegisterHotKey succeeded; the tray menu says when it did not.
    snip_hotkey_ok: bool,
    note_hotkey_ok: bool,
    /// `--panic-test`: panic once inside the message handler to prove recovery.
    panic_test: bool,
    panicked: bool,
    /// `--editor-demo`: open the annotation editor on a generated picture and
    /// close it after a few seconds. Development check, like `--demo`.
    editor_demo_at: Option<f64>,
    /// How many restored cards are still decoding; `pegged.txt` is not
    /// rewritten until they are all back.
    restore_pending: usize,
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
            flight: None,
            preview: None,
            preview_state: HoverState::default(),
            ghost: None,
            ghost_follow: false,
            dragging: false,
            editor: None,
            editing: None,
            arm_editor_until: 0.0,
            recently_saved: Vec::new(),
            last_reload: start,
            flight_idle_at: None,
            flying_card: None,
            preview_idle_at: None,
            ghost_idle_at: None,
            recent: Vec::new(),
            last_clip_hash: 0,
            last_clip_at: 0.0,
            snip_hotkey_ok: false,
            note_hotkey_ok: false,
            panic_test: std::env::args().any(|arg| arg == "--panic-test"),
            panicked: false,
            editor_demo_at: None,
            restore_pending: 0,
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
        if std::env::args().any(|arg| arg == "--editor-demo") {
            self.install_editor_demo();
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
        let snip = self.configured_hotkey("snipKey", "Ctrl+Alt+S");
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
            match result {
                Ok(()) => self.snip_hotkey_ok = true,
                Err(error) => logging::error(format!("RegisterHotKey(snip): {error}")),
            }
        } else {
            self.snip_hotkey_ok = true;
        }
        let note = self.configured_hotkey("noteKey", "Ctrl+Alt+N");
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
            match result {
                Ok(()) => self.note_hotkey_ok = true,
                Err(error) => logging::error(format!("RegisterHotKey(note): {error}")),
            }
        } else {
            self.note_hotkey_ok = true;
        }
    }

    /// The hotkey stored for an action, or the default when unset; `None` when
    /// the user chose "no shortcut" (stored as `none`).
    fn configured_hotkey(&self, setting: &str, default: &str) -> Option<shortcut::Hotkey> {
        match self.settings.string(setting) {
            Some("none") => None,
            Some(text) => shortcut::parse(text),
            None => shortcut::parse(default),
        }
    }

    /// Opens the recorder for one action and applies the choice (BEHAVIOR §12.2).
    fn change_hotkey(&mut self, target: HotkeyTarget) {
        let (id, setting, default, other_setting, other_default, title) = match target {
            HotkeyTarget::Snip => (
                HOTKEY_SNIP,
                "snipKey",
                "Ctrl+Alt+S",
                "noteKey",
                "Ctrl+Alt+N",
                i18n::s().screenshot_shortcut,
            ),
            HotkeyTarget::Note => (
                HOTKEY_NOTE,
                "noteKey",
                "Ctrl+Alt+N",
                "snipKey",
                "Ctrl+Alt+S",
                i18n::s().note_shortcut,
            ),
        };
        let current = self.configured_hotkey(setting, default);
        let other = self.configured_hotkey(other_setting, other_default);
        // Our own registration would make the recorder's availability probe
        // fail; release it and put it back when the dialog is cancelled.
        // SAFETY: the hotkey id belongs to this window.
        unsafe {
            let _ = UnregisterHotKey(Some(self.hwnd), id);
        }
        let validate = move |choice: Option<shortcut::Hotkey>| -> Option<String> {
            let hotkey = choice?;
            if hotkey.modifiers
                == (windows::Win32::UI::Input::KeyboardAndMouse::MOD_CONTROL.0
                    | windows::Win32::UI::Input::KeyboardAndMouse::MOD_ALT.0)
                && hotkey.virtual_key == 'T' as u32
            {
                return Some(i18n::s().conflict_toggle.to_string());
            }
            if other == Some(hotkey) {
                return Some(i18n::s().conflict_other.to_string());
            }
            None
        };
        let choice = window::shortcuts::ask(self.hwnd, title, current, Box::new(validate));
        let mut registered = false;
        match choice {
            window::shortcuts::Choice::Cancelled => {
                if let Some(hotkey) = current {
                    registered = Self::register_hotkey(self.hwnd, id, hotkey);
                }
            }
            window::shortcuts::Choice::Chosen(None) => {
                self.settings.set_string(setting, "none");
                self.settings.save();
                registered = true;
            }
            window::shortcuts::Choice::Chosen(Some(hotkey)) => {
                if Self::register_hotkey(self.hwnd, id, hotkey) {
                    self.settings.set_string(setting, &hotkey.label());
                    self.settings.save();
                    registered = true;
                } else {
                    // Somebody took it between the probe and now.
                    logging::error("hotkey: registration failed after the dialog");
                    if let Some(previous) = current {
                        registered = Self::register_hotkey(self.hwnd, id, previous);
                    }
                    let body = i18n::s().conflict_external.to_string();
                    if let Some(tray) = self.tray.as_mut() {
                        tray.show_balloon("Pegline", &body);
                    }
                }
            }
        }
        match target {
            HotkeyTarget::Snip => self.snip_hotkey_ok = registered,
            HotkeyTarget::Note => self.note_hotkey_ok = registered,
        }
    }

    /// Registers one app hotkey; returns whether Windows accepted it.
    fn register_hotkey(hwnd: HWND, id: i32, hotkey: shortcut::Hotkey) -> bool {
        // SAFETY: the window is ours and the id is one of our two action ids.
        let result = unsafe {
            RegisterHotKey(
                Some(hwnd),
                id,
                windows::Win32::UI::Input::KeyboardAndMouse::HOT_KEY_MODIFIERS(
                    hotkey.register_modifiers(),
                ),
                hotkey.virtual_key,
            )
        };
        match result {
            Ok(()) => true,
            Err(error) => {
                logging::error(format!("RegisterHotKey({id}): {error}"));
                false
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
                // The frame cap follows `fps=auto|60|120` from settings.ini.
                overlay.set_frame_cap(self.settings.fps_cap().period());
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
            WM_TIMER => {
                if wparam.0 == TIMER_GHOST {
                    self.follow_ghost();
                } else {
                    self.tick();
                }
                Some(LRESULT(0))
            }
            WM_APP_WAKE => {
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
            snip_key: self.hotkey_menu_label("snipKey", "Ctrl+Alt+S", self.snip_hotkey_ok),
            note_key: self.hotkey_menu_label("noteKey", "Ctrl+Alt+N", self.note_hotkey_ok),
        }
    }

    /// The shortcut text shown in the tray menu, including "none" and the
    /// "(in use by another app)" suffix.
    fn hotkey_menu_label(&self, setting: &str, default: &str, registered: bool) -> String {
        let key = self.settings.string(setting).unwrap_or(default);
        if key == "none" {
            i18n::s().none.to_string()
        } else {
            shortcut_label(key, registered)
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
            MenuCommand::Note => self.note_shortcut(),
            MenuCommand::SnipKey => self.change_hotkey(HotkeyTarget::Snip),
            MenuCommand::NoteKey => self.change_hotkey(HotkeyTarget::Note),
        }
    }

    fn tick(&mut self) {
        if self.quit {
            return;
        }
        if self.panic_test && !self.panicked {
            self.panicked = true;
            panic!("panic test requested");
        }
        if self.dragging {
            // The OLE drag loop is pumping messages on this thread; only the
            // 15 ms ghost timer should touch the UI until it returns.
            return;
        }
        let now = win32::now_seconds();
        self.drain_events(now);
        if let Some(at) = self.editor_demo_at {
            if now >= at {
                self.editor_demo_at = None;
                if let Some(editor) = self.editor.as_ref() {
                    // SAFETY: our own editor window, still alive while `Editor`
                    // is held; WM_CLOSE is the documented close request.
                    unsafe {
                        let _ = PostMessageW(Some(editor.hwnd()), WM_CLOSE, WPARAM(0), LPARAM(0));
                    }
                }
            }
        }
        if let Some(at) = self.clipboard_at {
            if now >= at {
                self.clipboard_at = None;
                self.handle_clipboard();
            }
        }
        self.drain_mouse(now);
        self.update_pointer(now);
        self.update_timers(now);
        self.update_preview(now);
        self.prune_missing(now);
        self.reload_edited(now);
        self.release_idle_windows(now);
        self.update_click_through();
        if self
            .editor
            .as_ref()
            .is_some_and(|editor| editor.is_closed())
        {
            if let Some(mut editor) = self.editor.take() {
                editor.shutdown();
            }
        }
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
                    flight_image,
                    twin,
                    clip_hash,
                } => self.on_capture_ready(
                    path,
                    own,
                    size_px,
                    thumb,
                    area,
                    flight_image,
                    twin,
                    clip_hash,
                    now,
                ),
                Event::RestoreReady {
                    id,
                    path,
                    size_px,
                    thumb,
                    note,
                    text,
                    color,
                } => self.on_restore_ready(id, path, size_px, thumb, note, text, color),
                Event::CopyReady { id, dib, png } => self.on_copy_ready(id, dib, png),
                Event::PreviewReady {
                    id,
                    version,
                    image,
                    bounds,
                    dpi,
                } => self.on_preview_ready(id, version, image, bounds, dpi),
                Event::NoteReady { path, text, image } => {
                    self.on_note_ready(path, text, image, now)
                }
                Event::NoteExisting { path } => {
                    logging::trace(format!("note already hanging: {}", path.display()));
                    self.present();
                    self.reveal(false, PEEK_SECONDS);
                }
                Event::NoteImage { id, image } => self.on_thumbnail(id, image),
                Event::ThumbnailReady { id, image, text } => {
                    self.on_thumbnail(id, image);
                    if let Some(text) = text {
                        if let Some(item) = self.items.iter_mut().find(|item| item.id == id) {
                            item.text = Some(text);
                        }
                    }
                }
                Event::Editor(EditorEvent::Saved(bytes)) => self.on_editor_saved(bytes, now),
                Event::Editor(EditorEvent::Cancelled) => self.on_editor_cancelled(),
                Event::TrashDone { paths } => {
                    logging::trace(format!("recycled {} files", paths.len()));
                }
                Event::Flight(FlightEvent::Landed { token, .. }) => {
                    if self.flying_card == Some(token) {
                        self.flying_card = None;
                    }
                    self.send_scene(Command::SetFlying {
                        id: token,
                        flying: false,
                    });
                }
                Event::Flight(FlightEvent::Finished(_id)) => {
                    self.flying_card = None;
                    if let Some(flight) = self.flight.as_mut() {
                        flight.hide();
                    }
                    self.flight_idle_at = Some(win32::now_seconds());
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
                MouseKind::RightDown => self.on_right_down(event.x, event.y),
            }
        }
    }

    fn on_left_down(&mut self, x: i32, y: i32, now: f64) {
        self.hide_preview();
        self.on_band_click(x, y);
        if let Some((id, color)) = self.swatch_at(x, y) {
            self.set_note_color(id, color);
            return;
        }
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

    /// A click on any screen top band suppresses the reveal until the pointer
    /// leaves the band, and puts a revealed line away unless the click landed
    /// on a photo (the same rule as the WPF low-level hook).
    fn on_band_click(&mut self, x: i32, y: i32) {
        let Some(screen) = win32::monitor_at(x, y) else {
            return;
        };
        let band_top = screen.bounds.top + (screen.work.top - screen.bounds.top).max(2);
        let in_band = x >= screen.bounds.left
            && x < screen.bounds.right
            && y >= screen.bounds.top
            && y < band_top;
        if !in_band {
            return;
        }
        self.band_suppressed = true;
        self.hot_zone_since = None;
        logging::trace(format!("band click at {x},{y}"));
        if self.is_revealed && self.hit_test(x, y).is_none() {
            self.pinned = false;
            self.set_revealed(false);
        }
    }

    fn on_right_down(&mut self, x: i32, y: i32) {
        self.hide_preview();
        self.on_band_click(x, y);
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
            CardAction::Discard | CardAction::TakeDown => self.discard(id),
            CardAction::Trash => self.trash_item(id),
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
        // Keep the editor-save suppression list from growing for the life of
        // the process; the watcher only needs the last few seconds.
        self.recently_saved.retain(|(_, at)| now - *at < 10.0);
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

    /// Destroys pooled helper windows after they have been idle long enough
    /// (M6). Their D3D devices are the expensive part, so releasing them is what
    /// makes the private memory fall back after a burst of captures or drags.
    fn release_idle_windows(&mut self, now: f64) {
        if let Some(at) = self.flight_idle_at {
            if now - at >= FLIGHT_IDLE_SECONDS {
                self.flight_idle_at = None;
                if let Some(mut flight) = self.flight.take() {
                    flight.shutdown();
                    logging::trace("flight window released after idle");
                }
            }
        }
        if let Some(at) = self.preview_idle_at {
            if now - at >= HELPER_IDLE_SECONDS {
                self.preview_idle_at = None;
                if let Some(mut preview) = self.preview.take() {
                    preview.shutdown();
                    logging::trace("preview window released after idle");
                }
            }
        }
        if let Some(at) = self.ghost_idle_at {
            if now - at >= HELPER_IDLE_SECONDS {
                self.ghost_idle_at = None;
                if let Some(mut ghost) = self.ghost.take() {
                    ghost.shutdown();
                    logging::trace("drag ghost released after idle");
                }
            }
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
        self.hide_preview();
        self.preview_state.reset();
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
            self.hide_preview();
            self.preview_state.reset();
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
        self.hide_preview();
        self.preview_state.reset();
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.place(screen.work, screen.dpi);
            overlay.send(Command::SetVisible(self.is_present));
            overlay.set_frame_cap(self.settings.fps_cap().period());
        }
        self.screen = Some(screen);
        self.enforce_capacity();
    }

    // ---- items ------------------------------------------------------------

    fn restore_pegged(&mut self) {
        let paths: Vec<PathBuf> = settings::read_pegged()
            .into_iter()
            .filter(|path| path.exists() && (wic::is_image_path(path) || notes::is_note(path)))
            .collect();
        self.restore_pending = paths.len();
        for path in paths {
            if !path.exists() || !(wic::is_image_path(&path) || notes::is_note(&path)) {
                continue;
            }
            let id = self.next_id;
            self.next_id += 1;
            let tx = self.events_tx.clone();
            let thread_path = path.clone();
            let thumb_max = self.thumb_max();
            let note = notes::is_note(&path);
            let color = self.settings.note_color(&file_name(&path));
            let _ = std::thread::Builder::new()
                .name("pegline-restore".to_string())
                .spawn(move || {
                    let (thumb, text) = if note {
                        let text = notes::read(&thread_path);
                        let image = text.as_deref().and_then(|text| notes::render(text, color));
                        (image, text)
                    } else {
                        (wic::decode_scaled(&thread_path, thumb_max).ok(), None)
                    };
                    if thumb.is_none() {
                        logging::error(format!("restore {}", thread_path.display()));
                    }
                    let size_px = if note {
                        (notes::CARD_WIDTH as u32, notes::CARD_HEIGHT as u32)
                    } else {
                        thumb
                            .as_ref()
                            .map(|thumb| (thumb.width, thumb.height))
                            .unwrap_or((0, 0))
                    };
                    let _ = tx.send(Event::RestoreReady {
                        id,
                        path: thread_path.clone(),
                        size_px,
                        thumb,
                        note,
                        text,
                        color,
                    });
                    window::wake(HWND::default());
                });
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn on_restore_ready(
        &mut self,
        id: CardId,
        path: PathBuf,
        size_px: (u32, u32),
        thumb: Option<ImageData>,
        note: bool,
        text: Option<String>,
        color: usize,
    ) {
        self.restore_pending = self.restore_pending.saturating_sub(1);
        let Some(thumb) = thumb else {
            if self.restore_pending == 0 {
                self.save_pegged();
            }
            return;
        };
        if self.items.len() >= layout::capacity(100_000.0) {
            return;
        }
        let own = path.starts_with(win32::inbox_dir()) || notes::is_note(&path);
        self.add_item(
            id, path, own, size_px, thumb, None, false, false, note, color, text,
        );
    }

    fn on_capture_file(&mut self, path: PathBuf) {
        if !wic::is_image_path(&path) {
            return;
        }
        let now = win32::now_seconds();
        // Our own annotation save lands in the watched folder; it is hung by
        // the editor callback, not by the watcher.
        if self
            .recently_saved
            .iter()
            .any(|(saved, at)| saved == &path && now - at < 2.0)
        {
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
        let now = win32::now_seconds();
        self.prune_recent(now);
        // Candidates must be recorded before the worker starts so two captures
        // that land together can still see each other.
        let candidates: Vec<(PathBuf, bool)> = self
            .recent
            .iter()
            .filter(|recent| recent.path != path)
            .map(|recent| (recent.path.clone(), recent.own))
            .collect();
        self.recent.retain(|recent| recent.path != path);
        self.recent.push(Recent {
            at: now,
            path: path.clone(),
            own,
        });

        let tx = self.events_tx.clone();
        let thread_path = path.clone();
        let want_flight = area.is_some();
        let thumb_max = self.thumb_max();
        let _ = std::thread::Builder::new()
            .name("pegline-decode".to_string())
            .spawn(move || {
                let thumb = match wic::decode_scaled(&thread_path, thumb_max) {
                    Ok(thumb) => thumb,
                    Err(error) => {
                        logging::error(format!("decode {}: {error}", thread_path.display()));
                        return;
                    }
                };
                let size_px = wic::pixel_size(&thread_path).unwrap_or((thumb.width, thumb.height));
                // Only a capture with a known origin flies, so only then is the
                // big texture worth decoding.
                let flight_image = if want_flight {
                    wic::decode_scaled(&thread_path, crate::capture::FLIGHT_MAX).ok()
                } else {
                    None
                };
                // Twin: the same picture saved by Windows and caught here, one
                // of them long-lived and one of them a fresh inbox copy. The
                // comparison decodes both at 2048 px, so it stays off the UI.
                let mut twin = None;
                for (candidate, candidate_own) in candidates {
                    if candidate_own != own
                        && candidate.exists()
                        && wic::same_picture(&thread_path, &candidate).unwrap_or(false)
                    {
                        twin = Some(candidate);
                        break;
                    }
                }
                let _ = tx.send(Event::CaptureReady {
                    path: thread_path,
                    own,
                    size_px,
                    thumb,
                    area,
                    flight_image,
                    twin,
                    clip_hash: None,
                });
                window::wake(HWND::default());
            });
    }

    /// Forgets captures older than the twin window.
    fn prune_recent(&mut self, now: f64) {
        self.recent.retain(|recent| now - recent.at < TWIN_SECONDS);
    }

    /// Folds the two halves of one screenshot into one card. The Windows-saved
    /// file always wins; the inbox copy is deleted, as in the WPF app.
    fn resolve_twin(&mut self, path: PathBuf, own: bool, twin: PathBuf) {
        logging::trace(format!(
            "twin: {} + {} (new own={own})",
            path.display(),
            twin.display()
        ));
        self.recent
            .retain(|recent| recent.path != path && recent.path != twin);
        if own {
            // New arrival is our inbox copy; the external file stays.
            if let Err(error) = std::fs::remove_file(&path) {
                logging::error(format!("twin remove {}: {error}", path.display()));
            }
            return;
        }
        // New arrival is the Windows-saved copy: repoint the existing card and
        // delete the inbox file it was showing.
        if let Some(item) = self.items.iter_mut().find(|item| item.path == twin) {
            item.path = path.clone();
            item.own = false;
            item.last_write = std::fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .unwrap_or(UNIX_EPOCH);
            if let Err(error) = std::fs::remove_file(&twin) {
                logging::error(format!("twin remove {}: {error}", twin.display()));
            }
            self.save_pegged();
            self.recent.push(Recent {
                at: win32::now_seconds(),
                path,
                own: false,
            });
            return;
        }
        // The old card is gone: hang the Windows-saved file normally.
        self.recent.push(Recent {
            at: win32::now_seconds(),
            path,
            own: false,
        });
    }

    /// FNV-1a over a sampled view of the pixels. The sampling matches the WPF
    /// catcher (every 4th row, 3-byte steps) so repeated clipboard copies of the
    /// same screenshot hash the same way.
    fn image_hash(image: &ImageData) -> u64 {
        let mut hash: u64 = 14_695_981_039_346_656_037;
        hash ^= (image.width as u64) * 31 + image.height as u64;
        let stride = image.width as usize * 4;
        for (row, y) in (0..image.height as usize).step_by(4).enumerate() {
            let _ = row;
            let start = y * stride;
            let end = (start + stride).min(image.bgra.len());
            let mut index = start;
            while index < end {
                hash ^= image.bgra[index] as u64;
                hash = hash.wrapping_mul(1_099_511_628_211);
                index += 3;
            }
        }
        hash
    }
    #[allow(clippy::too_many_arguments)]
    fn on_capture_ready(
        &mut self,
        path: PathBuf,
        own: bool,
        size_px: (u32, u32),
        thumb: ImageData,
        area: Option<RectI>,
        flight_image: Option<ImageData>,
        twin: Option<PathBuf>,
        clip_hash: Option<u64>,
        now: f64,
    ) {
        if let Some(hash) = clip_hash {
            if hash == self.last_clip_hash && now - self.last_clip_at < CLIPBOARD_DUPLICATE_SECONDS
            {
                logging::trace("clipboard duplicate ignored");
                let _ = std::fs::remove_file(&path);
                self.recent.retain(|recent| recent.path != path);
                return;
            }
            self.last_clip_hash = hash;
            self.last_clip_at = now;
        }
        // The capture the editor is already working on must not hang twice.
        if self
            .editing
            .as_ref()
            .is_some_and(|editing| editing.path == path)
        {
            return;
        }
        // `Ctrl+Alt+S`: the clipboard capture is annotated before it hangs.
        if now < self.arm_editor_until && own && clip_hash.is_some() {
            self.arm_editor_until = 0.0;
            self.open_editor_for(path, own, area, size_px, thumb);
            return;
        }
        // A Windows-saved copy of the picture being edited is the twin: the
        // edit is written there instead of leaving a duplicate (BEHAVIOR §9.5).
        if let Some(twin_path) = twin.as_ref() {
            if let Some(editing) = self.editing.as_mut() {
                if twin_path == &editing.path {
                    logging::trace(format!("editor twin: {}", path.display()));
                    editing.twin = Some(path);
                    return;
                }
            }
        }
        if let Some(twin) = twin {
            self.resolve_twin(path, own, twin);
            return;
        }
        if self.items.iter().any(|item| item.path == path) {
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        let mut flying = false;
        if let (Some(area), Some(image), Some(dpi)) = (area, flight_image, self.screen_dpi()) {
            let card_size = layout::card_size(size_px.0 as f32, size_px.1 as f32);
            let index = self.items.len();
            if let Some(end) = self.card_frame_px(index, index + 1, card_size) {
                // A burst of captures must not queue flights: show the old card
                // at its slot and let the flight window switch to the new one.
                if let Some(previous) = self.flying_card.take() {
                    self.send_scene(Command::SetFlying {
                        id: previous,
                        flying: false,
                    });
                }
                if let Some(flight) = self.ensure_flight() {
                    flying = flight.fly(id, image, area, end, dpi);
                    if flying {
                        self.flight_idle_at = None;
                        self.flying_card = Some(id);
                    }
                }
            }
        }
        self.add_item(
            id,
            path.clone(),
            own,
            size_px,
            thumb,
            area,
            true,
            flying,
            false,
            0,
            None,
        );
        self.recent.retain(|recent| recent.path != path);
        self.recent.push(Recent { at: now, path, own });
        self.sound(Cue::Tink);
        self.present();
        self.reveal(false, PEEK_SECONDS);
    }
    /// The DPI of the monitor the line is on.
    fn screen_dpi(&self) -> Option<u32> {
        self.screen.as_ref().map(|screen| screen.dpi)
    }

    /// Creates the flight window on first use. Keeping a second D3D device
    /// alive costs ~34 MB of private memory for an animation that may never
    /// run, so the window is built lazily and then pooled.
    fn ensure_flight(&mut self) -> Option<&mut Flight> {
        if self.flight.is_none() {
            let flight_tx = self.events_tx.clone();
            match Flight::create(Box::new(move |event| {
                let _ = flight_tx.send(Event::Flight(event));
                window::wake(HWND::default());
            })) {
                Ok(flight) => {
                    self.flight = Some(flight);
                    logging::trace("flight window created");
                }
                Err(error) => logging::error(format!("flight window: {error}")),
            }
        }
        self.flight.as_mut()
    }

    /// Decode size for a card thumbnail: exactly the pixels the card occupies
    /// (136 DIP wide at the current scale), capped by `THUMB_MAX`. Oversampling
    /// to 480 px costs ~11 MB of driver memory for six cards and buys nothing:
    /// the card never draws wider than 272 px on a 200% screen.
    fn thumb_max(&self) -> u32 {
        let scale = self
            .screen
            .as_ref()
            .map(|screen| screen.scale())
            .unwrap_or(1.0);
        ((layout::CARD_WIDTH - 14.0) * scale)
            .ceil()
            .clamp(96.0, crate::capture::THUMB_MAX as f32) as u32
    }

    /// Where card `index` of `count` will hang, in screen pixels.
    fn card_frame_px(&self, index: usize, count: usize, card_size: (f32, f32)) -> Option<RectI> {
        let overlay = self.overlay.as_ref()?;
        let window = win32::window_rect(overlay.hwnd())?;
        let scale = overlay.scale().max(0.1);
        let width_dip = overlay.width_px() as f32 / scale;
        let x = layout::slot_x(index, count, width_dip);
        let top = layout::rope_y(x, width_dip) - layout::PIN_ABOVE + layout::CARD_OFFSET_BELOW_TOP;
        let left = x - card_size.0 * 0.5;
        Some(RectI::new(
            window.left + (left * scale).round() as i32,
            window.top + (top * scale).round() as i32,
            window.left + ((left + card_size.0) * scale).round() as i32,
            window.top + ((top + card_size.1) * scale).round() as i32,
        ))
    }

    /// The card's current rectangle on screen, in pixels.
    fn current_card_px(&self, id: CardId) -> Option<RectI> {
        let overlay = self.overlay.as_ref()?;
        let window = win32::window_rect(overlay.hwnd())?;
        let snapshot = overlay.snapshot();
        let snapshot = snapshot.lock().ok()?;
        let card = snapshot.cards.iter().find(|card| card.id == id)?;
        let scale = snapshot.scale.max(0.1);
        Some(RectI::new(
            window.left + (card.frame.left * scale).round() as i32,
            window.top + (card.frame.top * scale).round() as i32,
            window.left + (card.frame.right * scale).round() as i32,
            window.top + (card.frame.bottom * scale).round() as i32,
        ))
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
        flying: bool,
        note: bool,
        color: usize,
        text: Option<String>,
    ) {
        let card_size = layout::card_size(size_px.0 as f32, size_px.1 as f32);
        let tilt = pseudo_random(id) * 5.0 - 2.5;
        let item = Item {
            id,
            path: path.clone(),
            own,
            note,
            color: color.min(notes::COLOR_COUNT - 1),
            text,
            size_px,
            card_size,
            tilt,
            thumb: thumb.clone(),
            missing: 0,
            last_write: std::fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .unwrap_or(UNIX_EPOCH),
            revision: 1,
        };
        self.items.push(item);
        self.send_scene(Command::AddCard(CardSpec {
            id,
            card_size,
            tilt,
            arriving,
            image: Some(thumb),
            note,
        }));
        if note {
            self.send_scene(Command::SetNoteColor { id, color });
        }
        if flying {
            self.send_scene(Command::SetFlying { id, flying: true });
        }
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
            self.start_fall(id);
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
        if self.restore_pending > 0 {
            return;
        }
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

    /// The note colour dot under the pointer, in window DIP.
    fn swatch_at(&self, x: i32, y: i32) -> Option<(CardId, usize)> {
        let overlay = self.overlay.as_ref()?;
        let rect = win32::window_rect(overlay.hwnd())?;
        let snapshot = overlay.snapshot();
        let snapshot = snapshot.lock().ok()?;
        let scale = snapshot.scale.max(0.1);
        snapshot.swatch_at(
            (x - rect.left) as f32 / scale,
            (y - rect.top) as f32 / scale,
        )
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
        if item.note {
            // A note copies as its text, ready to paste anywhere.
            let text = item
                .text
                .clone()
                .or_else(|| notes::read(&item.path))
                .unwrap_or_default();
            match clipboard::write_text(self.hwnd, &text) {
                Ok(()) => {
                    logging::trace(format!("copied note {id} ({} chars)", text.chars().count()));
                    self.send_scene(Command::Copied(id));
                }
                Err(error) => logging::error(format!("clipboard note: {error}")),
            }
            return;
        }
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

    // ---- hover preview ----------------------------------------------------

    /// Dwells on the hovered photo, decodes it at display size and shows the
    /// preview. Notes are excluded, exactly like the WPF line (BEHAVIOR §7).
    fn update_preview(&mut self, now: f64) {
        let wanted = self.hover.filter(|id| {
            self.is_present
                && self.is_revealed
                && self.pressed.is_none()
                && !self.menu_open
                && self.items.iter().any(|item| item.id == *id && !item.note)
        });
        let Some(id) = wanted else {
            self.hide_preview();
            self.preview_state.reset();
            return;
        };
        let Some(item) = self.items.iter().find(|item| item.id == id) else {
            self.hide_preview();
            self.preview_state.reset();
            return;
        };
        let stamp = item_stamp(item);
        let revision = item.revision;
        if !self.preview_state.observe(Some(id), revision, stamp, now) {
            return;
        }
        let version = self.preview_state.version();
        let Some(anchor) = self.current_card_px(id) else {
            return;
        };
        let Some(screen) = self.screen else {
            return;
        };
        let scale = self.overlay.as_ref().map(Overlay::scale).unwrap_or(1.0);
        let bounds = preview::bounds(item.size_px, anchor, screen.work, scale);
        if bounds.is_empty() {
            return;
        }
        let decode = preview::decode_max(bounds, scale);
        let path = item.path.clone();
        let dpi = screen.dpi;
        let tx = self.events_tx.clone();
        let _ = std::thread::Builder::new()
            .name("pegline-preview".to_string())
            .spawn(move || match wic::decode_scaled(&path, decode) {
                Ok(image) => {
                    let _ = tx.send(Event::PreviewReady {
                        id,
                        version,
                        image,
                        bounds,
                        dpi,
                    });
                    window::wake(HWND::default());
                }
                Err(error) => logging::error(format!("preview decode: {error}")),
            });
    }

    /// Hides the preview and releases its texture. Idempotent.
    fn hide_preview(&mut self) {
        if let Some(preview) = self.preview.as_mut() {
            if preview.is_visible() {
                preview.hide();
                self.preview_idle_at = Some(win32::now_seconds());
            }
        }
    }

    fn on_preview_ready(
        &mut self,
        id: CardId,
        version: u32,
        image: ImageData,
        bounds: RectI,
        dpi: u32,
    ) {
        if !self.preview_state.is_current(version) || self.hover != Some(id) {
            return;
        }
        let Some(item) = self.items.iter().find(|item| item.id == id) else {
            return;
        };
        if item.note || !item.path.exists() || !self.is_present || !self.is_revealed {
            return;
        }
        if self.preview.is_none() {
            match Preview::create() {
                Ok(preview) => {
                    self.preview = Some(preview);
                    logging::trace("preview window created");
                }
                Err(error) => {
                    logging::error(format!("preview window: {error}"));
                    return;
                }
            }
        }
        if let Some(preview) = self.preview.as_mut() {
            preview.show(image, bounds, dpi);
            self.preview_idle_at = None;
        }
    }

    // ---- notes ------------------------------------------------------------

    /// `Ctrl+Alt+N`: the clipboard text hangs as a sticky note.
    fn note_shortcut(&mut self) {
        let Some(text) = clipboard::read_text() else {
            self.note_is_empty();
            return;
        };
        if text.trim().is_empty() {
            self.note_is_empty();
            return;
        }
        // The same text again brings the note already hanging back into view.
        if self
            .items
            .iter()
            .any(|item| item.note && item.text.as_deref() == Some(text.as_str()))
        {
            self.present();
            self.reveal(false, PEEK_SECONDS);
            return;
        }
        let tx = self.events_tx.clone();
        let _ = std::thread::Builder::new()
            .name("pegline-note".to_string())
            .spawn(move || {
                // Notes that are no longer on the line still count: the folder
                // is the storage and the line is only a view of it.
                if let Ok(entries) = std::fs::read_dir(notes::folder()) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path
                            .extension()
                            .is_some_and(|extension| extension.eq_ignore_ascii_case("txt"))
                            && notes::read(&path).as_deref() == Some(text.as_str())
                        {
                            let _ = tx.send(Event::NoteExisting { path });
                            window::wake(HWND::default());
                            return;
                        }
                    }
                }
                let path = match notes::create(&text) {
                    Ok(path) => path,
                    Err(error) => {
                        logging::error(format!("note create: {error}"));
                        return;
                    }
                };
                let image = match notes::render(&text, 0) {
                    Some(image) => image,
                    None => {
                        logging::error(format!("note render: {}", path.display()));
                        return;
                    }
                };
                let _ = tx.send(Event::NoteReady { path, text, image });
                window::wake(HWND::default());
            });
    }

    fn note_is_empty(&mut self) {
        let shortcut = self
            .settings
            .string("noteKey")
            .unwrap_or("Ctrl+Alt+N")
            .to_string();
        let body = i18n::s().clipboard_empty.replace("{}", &shortcut);
        if let Some(tray) = self.tray.as_mut() {
            tray.show_balloon("Pegline", &body);
        }
        logging::trace("note shortcut: clipboard has no text");
    }

    fn on_note_ready(&mut self, path: PathBuf, text: String, image: ImageData, now: f64) {
        if self.items.iter().any(|item| item.path == path) {
            return;
        }
        if self.items.len() >= layout::capacity(100_000.0) {
            return;
        }
        let id = self.alloc_id();
        let color = self.settings.note_color(&file_name(&path));
        self.add_item(
            id,
            path.clone(),
            true,
            (notes::CARD_WIDTH as u32, notes::CARD_HEIGHT as u32),
            image,
            None,
            false,
            false,
            true,
            color,
            Some(text.clone()),
        );
        self.recent.retain(|recent| recent.path != path);
        self.recent.push(Recent {
            at: now,
            path,
            own: true,
        });
        if color != 0 {
            self.recolour_note(id, color);
        }
        self.sound(Cue::Tink);
        self.present();
        self.reveal(false, PEEK_SECONDS);
    }

    /// Re-renders a note in `color` on a worker and redraws the card.
    fn recolour_note(&mut self, id: CardId, color: usize) {
        let Some(item) = self.items.iter().find(|item| item.id == id) else {
            return;
        };
        let Some(text) = item.text.clone() else {
            return;
        };
        let tx = self.events_tx.clone();
        let _ = std::thread::Builder::new()
            .name("pegline-note".to_string())
            .spawn(move || {
                if let Some(image) = notes::render(&text, color) {
                    let _ = tx.send(Event::NoteImage { id, image });
                    window::wake(HWND::default());
                }
            });
    }

    fn set_note_color(&mut self, id: CardId, color: usize) {
        let Some(item) = self.items.iter_mut().find(|item| item.id == id) else {
            return;
        };
        if !item.note || item.color == color {
            return;
        }
        item.color = color;
        let name = file_name(&item.path);
        self.settings.set_note_color(&name, color);
        self.send_scene(Command::SetNoteColor { id, color });
        self.recolour_note(id, color);
    }

    fn on_thumbnail(&mut self, id: CardId, image: ImageData) {
        let Some(item) = self.items.iter_mut().find(|item| item.id == id) else {
            return;
        };
        item.thumb = image.clone();
        item.revision = item.revision.wrapping_add(1);
        self.send_scene(Command::SetImage { id, image });
    }

    /// Once a second: a picture or note changed on disk (Paint, Notepad) is
    /// re-decoded so the card and its preview follow (BEHAVIOR §4.5, §7.6).
    fn reload_edited(&mut self, now: f64) {
        if now - self.last_reload < 1.0 {
            return;
        }
        self.last_reload = now;
        if self.demo {
            return;
        }
        let mut jobs: Vec<(CardId, PathBuf, bool, usize, u32)> = Vec::new();
        let thumb_max = self.thumb_max();
        for item in &mut self.items {
            let Ok(metadata) = std::fs::metadata(&item.path) else {
                continue;
            };
            let Ok(modified) = metadata.modified() else {
                continue;
            };
            if modified <= item.last_write {
                continue;
            }
            item.last_write = modified;
            jobs.push((item.id, item.path.clone(), item.note, item.color, thumb_max));
        }
        for (id, path, note, color, thumb_max) in jobs {
            let tx = self.events_tx.clone();
            let _ = std::thread::Builder::new()
                .name("pegline-reload".to_string())
                .spawn(move || {
                    let (image, text) = if note {
                        match notes::read(&path) {
                            Some(text) => (notes::render(&text, color), Some(text)),
                            None => (None, None),
                        }
                    } else {
                        (wic::decode_scaled(&path, thumb_max).ok(), None)
                    };
                    match image {
                        Some(image) => {
                            let _ = tx.send(Event::ThumbnailReady { id, image, text });
                            window::wake(HWND::default());
                        }
                        None => logging::error(format!("reload {}", path.display())),
                    }
                });
        }
    }

    // ---- annotation editor ------------------------------------------------

    /// Opens the annotation editor for a capture that must be marked up before
    /// it hangs (`Ctrl+Alt+S`). Falls back to hanging the raw capture.
    #[allow(clippy::too_many_arguments)]
    fn open_editor_for(
        &mut self,
        path: PathBuf,
        own: bool,
        area: Option<RectI>,
        size_px: (u32, u32),
        thumb: ImageData,
    ) {
        let dpi = self.screen_dpi().unwrap_or(96);
        let tx = self.events_tx.clone();
        let notify = Box::new(move |event: EditorEvent| {
            let _ = tx.send(Event::Editor(event));
            window::wake(HWND::default());
        });
        match Editor::open(&path, dpi, notify) {
            Ok(editor) => {
                logging::trace(format!("editor opened for {}", path.display()));
                self.editor = Some(editor);
                self.editing = Some(EditingCapture {
                    path: path.clone(),
                    own,
                    area,
                    twin: None,
                    original: thumb,
                });
                self.recent.retain(|recent| recent.path != path);
                self.recent.push(Recent {
                    at: win32::now_seconds(),
                    path,
                    own,
                });
                self.hide_preview();
            }
            Err(error) => {
                logging::error(format!("editor: {error}"));
                // The capture must still hang: a broken editor may not eat it.
                let id = self.alloc_id();
                self.add_item(
                    id, path, own, size_px, thumb, area, true, false, false, 0, None,
                );
                self.present();
                self.reveal(false, PEEK_SECONDS);
            }
        }
    }

    fn on_editor_saved(&mut self, png: Vec<u8>, now: f64) {
        let Some(editing) = self.editing.take() else {
            logging::error("editor saved without an open capture");
            return;
        };
        let mut target = editing.twin.clone().unwrap_or_else(|| editing.path.clone());
        let mut replaced_card = false;
        if editing.twin.is_none() {
            // A copy may have reached the line before the editor opened; write
            // over that one instead of hanging a duplicate.
            let original_hash = Self::image_hash(&editing.original);
            if let Some(existing) = self.items.iter_mut().find(|item| {
                !item.note
                    && item.path != editing.path
                    && Self::image_hash(&item.thumb) == original_hash
            }) {
                target = existing.path.clone();
                // Force the one-second reload to pick the new pixels up.
                existing.last_write = UNIX_EPOCH;
                replaced_card = true;
            }
        }
        if let Err(error) = wic::write_png_bytes(&target, &png) {
            logging::error(format!("editor save {}: {error}", target.display()));
            return;
        }
        if target != editing.path {
            if let Err(error) = std::fs::remove_file(&editing.path) {
                logging::error(format!(
                    "editor twin remove {}: {error}",
                    editing.path.display()
                ));
            }
        }
        self.recently_saved.push((target.clone(), now));
        self.recent
            .retain(|recent| recent.path != editing.path && recent.path != target);
        if replaced_card {
            logging::trace(format!("editor wrote over {}", target.display()));
            return;
        }
        let own = target.starts_with(win32::inbox_dir());
        self.spawn_capture(target, own, editing.area);
    }

    fn on_editor_cancelled(&mut self) {
        let Some(editing) = self.editing.take() else {
            return;
        };
        self.recent.retain(|recent| recent.path != editing.path);
        if editing.own {
            if let Err(error) = std::fs::remove_file(&editing.path) {
                logging::error(format!(
                    "editor cancel remove {}: {error}",
                    editing.path.display()
                ));
            }
        }
    }

    // ---- drag ghost -------------------------------------------------------

    /// Moves the drag preview with the pointer; driven by a 15 ms timer while
    /// `DoDragDrop` pumps messages.
    fn follow_ghost(&mut self) {
        if !self.ghost_follow {
            return;
        }
        let (Some(cursor), Some(screen)) = (win32::cursor_pos(), self.pointer_screen()) else {
            return;
        };
        if let Some(ghost) = self.ghost.as_mut() {
            ghost.move_to(cursor, screen.dpi);
        }
    }

    fn sound(&self, cue: Cue) {
        if !self.settings.sound_off() {
            sound::play(cue);
        }
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
        // Long press: Paint for pictures, Notepad for notes (BEHAVIOR §8).
        let program = if item.note {
            "notepad.exe"
        } else {
            "mspaint.exe"
        };
        if win32::launch(program, &path).is_ok() {
            return;
        }
        if win32::shell_edit(&path).is_ok() {
            return;
        }
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
        self.start_fall(id);
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

    /// "Move to Recycle Bin": recycles the file even when it is not ours, then
    /// takes the card down (BEHAVIOR §8.2).
    fn trash_item(&mut self, id: CardId) {
        let Some(item) = self.items.iter().find(|item| item.id == id) else {
            return;
        };
        let path = item.path.clone();
        self.start_fall(id);
        let tx = self.events_tx.clone();
        let _ = std::thread::Builder::new()
            .name("pegline-trash".to_string())
            .spawn(move || {
                if let Err(error) = win32::recycle_files(std::slice::from_ref(&path)) {
                    logging::error(format!("recycle {}: {error}", path.display()));
                }
                let _ = tx.send(Event::TrashDone { paths: vec![path] });
            });
        self.remove_item(id);
    }

    /// Starts the falling animation for a card that is leaving the line.
    fn start_fall(&mut self, id: CardId) {
        let Some(item) = self.items.iter().find(|item| item.id == id) else {
            return;
        };
        let thumb = item.thumb.clone();
        let tilt = item.tilt;
        let Some(rect) = self.current_card_px(id) else {
            return;
        };
        let Some(dpi) = self.screen_dpi() else {
            return;
        };
        self.sound(Cue::Pop);
        // A drop interrupts a flight in progress; the card left behind must be
        // visible at its slot before it falls.
        if let Some(previous) = self.flying_card.take() {
            self.send_scene(Command::SetFlying {
                id: previous,
                flying: false,
            });
        }
        if let Some(flight) = self.ensure_flight() {
            if flight.drop_card(id, thumb, rect, tilt, dpi) {
                self.flight_idle_at = None;
            }
        }
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
        self.sound(Cue::Rustle);
        for id in self
            .items
            .iter()
            .filter(|item| item.own && !item.note)
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
        self.hide_preview();
        self.preview_state.reset();
        let Some(item) = self.items.iter().find(|item| item.id == id) else {
            return;
        };
        let path = item.path.clone();
        let note = item.note;
        let thumb = item.thumb.clone();
        let card_size = item.card_size;
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

        // The ghost is a 15 ms follower: `DoDragDrop` pumps messages, so the
        // timer below runs during the modal drag and moves the window.
        if let Some(cursor) = win32::cursor_pos() {
            let dpi = self.screen_dpi().unwrap_or(96);
            if self.ghost.is_none() {
                match Ghost::create() {
                    Ok(ghost) => {
                        self.ghost = Some(ghost);
                        logging::trace("drag ghost created");
                    }
                    Err(error) => logging::error(format!("drag ghost: {error}")),
                }
            }
            if let Some(ghost) = self.ghost.as_mut() {
                ghost.show(&thumb, card_size, cursor, dpi);
                self.ghost_idle_at = None;
            }
            self.ghost_follow = self.ghost.is_some();
            if self.ghost_follow {
                // SAFETY: our own window and timer id.
                unsafe {
                    let _ = SetTimer(Some(self.hwnd), TIMER_GHOST, GHOST_MS, None);
                }
            }
        }

        // A note drags as its text file; only pictures have a PNG payload.
        let png = if note {
            Vec::new()
        } else {
            std::fs::read(&path).unwrap_or_default()
        };
        let image = ImageData::new(1, 1, vec![0, 0, 0, 0]);
        self.dragging = true;
        let effect = match crate::dragdrop::drag_file(self.hwnd, &path, &image, &png) {
            Ok(effect) => effect,
            Err(error) => {
                logging::error(format!("drag: {error}"));
                0
            }
        };
        self.dragging = false;
        self.ghost_follow = false;
        // SAFETY: the timer was created above on this thread.
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_GHOST);
        }
        if let Some(ghost) = self.ghost.as_mut() {
            ghost.hide();
            self.ghost_idle_at = Some(win32::now_seconds());
        }
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
        // While the snip shortcut is armed, catching is on even if the user
        // turned ordinary clipboard catching off (as in the WPF catcher).
        let armed = win32::now_seconds() < self.arm_editor_until;
        if (!self.settings.catch_clipboard() && !armed) || clipboard::self_write_recent() {
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
        let thumb_max = self.thumb_max();
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
                let thumb = match wic::decode_scaled(&path, thumb_max) {
                    Ok(thumb) => thumb,
                    Err(error) => {
                        logging::error(format!("clipboard thumb: {error}"));
                        return;
                    }
                };
                let hash = App::image_hash(&image);
                let _ = tx.send(Event::CaptureReady {
                    path,
                    own: true,
                    size_px: size,
                    thumb,
                    area,
                    flight_image: None,
                    twin: None,
                    clip_hash: Some(hash),
                });
                window::wake(HWND::default());
                let _ = now;
            });
    }

    fn snip(&mut self) {
        // The next clipboard capture opens the annotation editor first
        // (BEHAVIOR §9.2: "转接编辑器"). The catcher stays armed for 60 s, which
        // is how long a snip selection can take.
        self.arm_editor_until = win32::now_seconds() + 60.0;
        if let Err(error) = win32::shell_open(std::path::Path::new("ms-screenclip:")) {
            logging::error(format!("ms-screenclip: {error}"));
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

    /// `--editor-demo`: a generated picture in the real annotation editor, so
    /// the window, the GPU stack and the toolbar can be checked without a snip.
    fn install_editor_demo(&mut self) {
        let (w, h) = (900u32, 600u32);
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let index = ((y * w + x) * 4) as usize;
                pixels[index] = (40 + x * 180 / w) as u8;
                pixels[index + 1] = (60 + y * 150 / h) as u8;
                pixels[index + 2] = 200u8.saturating_sub((x * 120 / w) as u8);
                pixels[index + 3] = 255;
            }
        }
        let image = ImageData::new(w, h, pixels);
        let encoded = match wic::encode_png_bytes(&image) {
            Ok(bytes) => bytes,
            Err(error) => {
                logging::error(format!("editor demo encode: {error}"));
                return;
            }
        };
        let directory = win32::local_dir();
        if let Err(error) = std::fs::create_dir_all(&directory) {
            logging::error(format!("editor demo dir: {error}"));
            return;
        }
        let path = directory.join("editor-demo.png");
        if let Err(error) = wic::write_png_bytes(&path, &encoded) {
            logging::error(format!("editor demo write: {error}"));
            return;
        }
        let thumb = image;
        self.open_editor_for(path, false, None, (w, h), thumb);
        self.editor_demo_at = Some(win32::now_seconds() + 4.0);
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
                note: false,
                color: 0,
                text: None,
                size_px,
                card_size,
                tilt,
                thumb: ImageData::new(w, h, pixels.clone()),
                missing: 0,
                last_write: UNIX_EPOCH,
                revision: 1,
            });
            self.send_scene(Command::AddCard(CardSpec {
                id,
                card_size,
                tilt,
                arriving: false,
                image: Some(ImageData::new(w, h, pixels)),
                note: false,
            }));
        }
        self.present();
        self.keep_open = true;
        self.reveal(true, 0.0);
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
        if let Some(flight) = self.flight.as_mut() {
            flight.shutdown();
        }
        if let Some(preview) = self.preview.as_mut() {
            preview.shutdown();
        }
        if let Some(ghost) = self.ghost.as_mut() {
            ghost.shutdown();
        }
        if let Some(mut editor) = self.editor.take() {
            editor.shutdown();
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

/// The two configurable actions in the recorder.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HotkeyTarget {
    Snip,
    Note,
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

fn shortcut_label(key: &str, registered: bool) -> String {
    if registered {
        key.to_string()
    } else {
        format!("{key}{}", i18n::s().in_use_by_another_app)
    }
}

/// A file's last write time as whole seconds since the epoch; 0 when unknown.
fn item_stamp(item: &Item) -> u64 {
    item.last_write
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// The file name as a `String`, for settings keys such as `noteColor:<name>`.
fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
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
