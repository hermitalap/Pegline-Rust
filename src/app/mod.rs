//! The controller: one message loop, one state machine.
//!
//! The UI thread owns every decision — show/hide, reveal, hover, gestures,
//! capture orchestration, tray, hotkeys — and sends `Command`s to the render
//! thread. It never decodes, encodes, compares or deletes: those run on workers
//! and come back as [`Event`]s on the 30 Hz tick.
//!
//! Win32 objects (`HWND`, `HHOOK`, hotkeys, tray) are created and destroyed on
//! this thread only.
//!
//! The `--*-demo` acceptance scaffolding lives in the [`dev`] submodule: it can
//! reach the private state here, but the shipping paths never read it.

mod book;
mod capture_flow;
mod capture_state;
mod cards;
mod dev;
mod line;
mod pointer;
mod pool;
mod reveal;
mod sessions;
mod state;
mod tray_menu;

use book::FlightBook;
use capture_state::Capture;
use dev::Dev;
use line::Line;
use pool::WindowPool;
use sessions::Sessions;
use state::LineState;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey, VK_RETURN};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CallNextHookEx, CreatePopupMenu, DestroyMenu, DestroyWindow, IDYES, KillTimer,
    MB_ICONQUESTION, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO, MF_SEPARATOR, MF_STRING,
    MSLLHOOKSTRUCT, MessageBoxW, PostMessageW, RegisterWindowMessageW, SetForegroundWindow,
    SetTimer, SetWindowsHookExW, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu,
    UnhookWindowsHookEx, WH_MOUSE_LL, WM_CHAR, WM_CLIPBOARDUPDATE, WM_CLOSE, WM_DESTROY, WM_HOTKEY,
    WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_NULL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_TIMER,
};
use windows::core::{PCWSTR, w};

use crate::capture::{areas, clipboard, watcher, wic};
use crate::editor::{Editor, EditorEvent};
use crate::i18n;
use crate::layout;
use crate::logging;
use crate::note_editor::display::PaperSize;
use crate::note_editor::model;
use crate::note_editor::window::{NoteEditor, NoteEditorEvent};
use crate::notes;
use crate::preview;
use crate::scene::Command;
use crate::settings::Settings;
use crate::shortcut;
use crate::sound::{self, Cue};
use crate::spring::ease;
use crate::tray::Tray;
use crate::types::{CardId, CardSpec, ImageData, RectF, RectI};
use crate::win32::{self, Monitor};
use crate::window::flight::{FlightEvent, FlightImage, PinMode};
use crate::window::ghost::Ghost;
use crate::window::overlay::Overlay;
use crate::window::preview::{NOTE_PADDING_DIP, Preview, note_preview_card_dip};
use crate::window::{self, WM_APP_QUIT, WM_APP_WAKE, WM_TRAY_CALLBACK};

const CLASS_NAME: PCWSTR = w!("PeglineMessage");
const WINDOW_TITLE: PCWSTR = w!("Pegline");
const TIMER_TICK: usize = 1;
const TICK_MS: u32 = 33;
/// Follows the hover note's physics; the drag ghost follows itself on its own
/// render thread.
const TIMER_NOTE: usize = 3;
const NOTE_FOLLOW_MS: u32 = 15;

/// How long a quit waits for an in-flight note commit before dropping the
/// editor. Polled from the frame loop, never slept on.
const QUIT_GRACE_SECONDS: f64 = 2.0;

/// Largest file whose encoded bytes are also offered under the registered
/// `PNG` clipboard format during a drag. Bigger cards drag as a file only:
/// a 6K screenshot can be tens of megabytes, and reading plus copying that on
/// the UI thread would stall the pointer. Explorer, the usual target, reads the
/// file path anyway.
const DRAG_PNG_MAX: u64 = 8 * 1024 * 1024;

const HOTKEY_TOGGLE: i32 = 1;
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
/// How long the line stays down on every launch so a start is visible.
const STARTUP_SECONDS: f64 = 2.5;
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
///
/// The Windows file can appear several seconds after the capture (the watcher
/// then debounces for another quarter second before the decode and compare),
/// so the WPF's 4 s is too tight for this pipeline.
const TWIN_SECONDS: f64 = 8.0;
/// The same copied image is ignored for this long.
const CLIPBOARD_DUPLICATE_SECONDS: f64 = 10.0;
/// A pooled flight window idle for this long is destroyed to release its device
/// memory; the next flight builds a new one (~20 ms).
const FLIGHT_IDLE_SECONDS: f64 = 30.0;
/// The preview and drag-ghost windows are cheap to rebuild, so they are
/// destroyed sooner than the flight window. Their D3D devices are what cost
/// private memory when idle, not the window.
const HELPER_IDLE_SECONDS: f64 = 10.0;
/// Flight token for the note editor's paper, so note flights never touch the
/// card bookkeeping (`flying_card`).
const NOTE_FLIGHT_TOKEN: u64 = u64::MAX;
/// Flight token for the throw-away tumble of an empty new note.
const TUMBLE_FLIGHT_TOKEN: u64 = u64::MAX - 1;
/// Flight token for a picture growing out of its card into the annotation
/// editor. Distinct from the closing flight so the two hand-offs can each show
/// or hide their window at the right moment.
const ANNOTATE_OPEN_TOKEN: u64 = u64::MAX - 2;
/// Flight token for the annotated picture shrinking back onto the line.
const ANNOTATE_CLOSE_TOKEN: u64 = u64::MAX - 3;
/// Longest side of the flight image decoded for an annotation opening. The
/// card-sized thumbnail is far too small for the enlarged editor card.
const ANNOTATE_FLIGHT_MAX: u32 = 2048;
/// Hover-note physics: a damped spring home with a pointer pull whose
/// equilibrium offset is a fraction of the pointer distance.
///
/// The spring is only lightly under-damped (0.6) so the note still leans into
/// the pointer with a little inertia but does not swing; the pull tops out at
/// 30 DIP, which keeps the whole note near its slot instead of letting the
/// pointer drag it around the strip.
const NOTE_HOME_RESPONSE: f32 = 0.56;
const NOTE_HOME_DAMPING: f32 = 0.6;
const NOTE_PULL_RATIO: f32 = 0.22;
const NOTE_PULL_RANGE_DIP: f32 = 260.0;
const NOTE_PULL_MAX_DIP: f32 = 30.0;
const NOTE_BOB_DIP: f32 = 3.0;
const NOTE_BOB_HZ: f64 = 0.22;
/// Hover-note lean, degrees. A crosshair runs through the paper's centre: the
/// pointer leans the paper clockwise in the top-left and bottom-right
/// quadrants, counter-clockwise in the other two, by nothing on either axis and
/// by at most 5° on the diagonals once the pointer is 120 DIP from the centre.
/// The angular spring copies the travel spring so the lean lags and settles
/// with it.
const NOTE_TILT_MAX_DEG: f32 = 5.0;
const NOTE_TILT_RANGE_DIP: f32 = 120.0;
const NOTE_TILT_RESPONSE: f32 = 0.56;
const NOTE_TILT_DAMPING: f32 = 0.6;
/// Hover note fly-in/out. The entry is advertised by a corner above the screen,
/// so moving onto it drops the rest of the note down quickly instead of the old
/// slow fall from far above the screen.
const NOTE_FLY_IN_SECONDS: f64 = 0.22;
const NOTE_FLY_OUT_SECONDS: f64 = 0.42;

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
    /// When this card was caught, for the twin window. `0.0` for cards restored
    /// at startup: an old card is never the twin of a new capture.
    captured_at: f64,
}

/// A picture that is being annotated. `card` is set when the editor was opened
/// from the card's Annotate command: the saved PNG then replaces that card's
/// file and the card reloads in place. Without a card (the `--editor-demo`
/// check) the saved picture hangs as a new capture.
struct EditingCapture {
    path: PathBuf,
    card: Option<CardId>,
    own: bool,
    area: Option<RectI>,
    /// The card's bitmap when the editor opened; the cancel flight carries it
    /// home. `None` when there was no card.
    image: Option<ImageData>,
    tilt: f32,
    /// The enlarged card in screen pixels: the flight's target on the way in
    /// and its origin on the way back.
    card_rect: Option<RectI>,
}

/// A note open in the WYSIWYG editor (NOTE-EDITOR.md §5–§8).
struct EditingNote {
    /// `None` for a brand-new note that has no file yet.
    path: Option<PathBuf>,
    /// The card being edited, `None` for a new note.
    card: Option<CardId>,
    /// The text the editor was opened with, normalised to LF.
    original: String,
    color: usize,
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
    /// The note editor asked to commit or produced clipboard text.
    NoteEditor(NoteEditorEvent),
    /// A committed note was written and re-rendered (or failed to write).
    NoteSaved {
        card: Option<CardId>,
        path: PathBuf,
        text: String,
        image: Option<ImageData>,
        error: Option<String>,
    },
    /// A taken-down note finished moving into `Notes\Archive`.
    NoteArchived {
        id: CardId,
        result: Result<PathBuf, String>,
    },
    TrashDone {
        paths: Vec<PathBuf>,
    },
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

/// Where the blank entry note is in its little animation (NOTE-EDITOR.md §4.5).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HoverPhase {
    In,
    Hover,
    Out,
}

/// The blank note that appears beside the last card.
///
/// It is not dragged rigidly behind the pointer. It hangs at the slot where it
/// would be clipped to the line (`home`), bobs up and down there, and the
/// pointer pulls it with a force proportional to the distance and direction
/// from the note centre. A damped spring pulls it home, so the note lags,
/// overshoots and settles instead of following the mouse one to one. The same
/// pointer also leans the paper about its centre, with a second spring of its
/// own, so the twist trails the travel instead of snapping to it.
struct HoverNote {
    phase: HoverPhase,
    started: f64,
    from: (f32, f32),
    to: (f32, f32),
    /// Card centre in screen pixels; the preview window is centred here.
    center: (f32, f32),
    /// The theoretical hanging position, card centre in screen pixels.
    home: (f32, f32),
    /// Physical position/velocity in screen pixels (per second).
    pos: (f32, f32),
    vel: (f32, f32),
    /// Lean in degrees, positive clockwise, and its velocity per second.
    angle: f32,
    spin: f32,
    /// Start of the slow ±3 DIP bob.
    float_start: f64,
    dpi: u32,
    /// Last physics step, seconds.
    last: f64,
    /// Next animation trace line, seconds.
    trace_at: f64,
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
    events_tx: Sender<Event>,
    events_rx: Receiver<Event>,

    /// The pooled windows and the Win32 resources that belong to them.
    windows: WindowPool,
    /// The card currently in the air, and its hand-off deadlines.
    book: FlightBook,
    /// The cards on the line.
    line: Line,
    /// Visibility, hover and press state.
    state: LineState,
    /// The watcher, the twin window and the clipboard bookkeeping.
    capture: Capture,
    /// The two in-place editors and their hand-offs.
    sessions: Sessions,

    /// The monitor the line is on.
    screen: Option<Monitor>,
    /// Whether RegisterHotKey succeeded; the tray menu says when it did not.
    note_hotkey_ok: bool,
    /// Scripted acceptance demos (`--*-demo`); see [`dev`].
    dev: Dev,
    start_time: f64,
    quit: bool,
    /// Set while a quit is waiting for an in-flight note commit; the deadline
    /// after which the editor is dropped even if the write never reported back.
    quit_at: Option<f64>,
    destroyed: bool,
}

impl App {
    fn new() -> Self {
        let (events_tx, events_rx) = mpsc::channel();
        let start = win32::now_seconds();
        let mut app = Self {
            hwnd: HWND::default(),
            settings: Settings::load(),
            events_tx,
            events_rx,
            windows: WindowPool::default(),
            book: FlightBook::default(),
            line: Line::default(),
            state: LineState::default(),
            capture: Capture::default(),
            sessions: Sessions::default(),
            screen: None,
            note_hotkey_ok: false,
            dev: Dev::from_env(),
            start_time: start,
            quit: false,
            quit_at: None,
            destroyed: false,
        };
        // Three values are not `Default`: the first card id is 1, not 0, and the
        // two throttle clocks start at launch rather than at the epoch.
        app.line.next_id = 1;
        app.state.last_prune = start;
        app.capture.last_reload = start;
        app
    }

    fn start(&mut self) -> Result<(), String> {
        self.hwnd = window::create_message_window(
            CLASS_NAME,
            WINDOW_TITLE,
            Some(hidden_wndproc),
            self as *mut App as *mut (),
        )?;
        // Workers post `WM_APP_WAKE` here; it must be a real handle, not null.
        window::set_wake_target(self.hwnd);
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
            Ok(tray) => self.windows.tray = Some(tray),
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
                    window::wake();
                }
            }),
        ) {
            Ok(watcher) => {
                self.capture.watcher = Some(watcher);
                logging::trace(format!("watching {}", folder.display()));
            }
            Err(error) => logging::error(format!("watcher: {error}")),
        }

        // `--note-preview-demo` deliberately starts from an empty line: the
        // user's pegged cards would sit around the demo note and make the
        // captured preview depend on their number.
        if !self.dev.note_preview_demo {
            self.restore_pegged();
        }

        // Drop the line on every launch so a start is visible instead of just
        // a tray icon appearing. The first run keeps the longer welcome with
        // its empty hint; `startup_until` holds the line down even when the
        // pointer is nowhere near the top edge (update_pointer treats it as
        // busy), then it retracts normally unless the pointer is inside.
        let first_run = !self.settings.welcomed();
        let hold = if first_run {
            WELCOME_SECONDS
        } else {
            STARTUP_SECONDS
        };
        self.state.keep_open = true;
        self.state.startup_until = Some(self.start_time + hold);
        if first_run {
            self.settings.mark_welcomed();
        }
        self.present();
        self.reveal(false, 0.0);
        // The WPF app asked once, 1.2 s after the first start, whether it may
        // take over screenshots (BEHAVIOR §3.2). Reading the clipboard is the
        // user's call, never ours.
        if !self.settings.catch_offered()
            && !self.dev.demo
            && !self.dev.note_preview_demo
            && !self.dev.editor_demo
        {
            self.state.catch_offer_at = Some(self.start_time + 1.2);
        }

        if self.dev.demo {
            self.install_demo_cards();
        }
        if self.dev.note_preview_demo {
            self.start_note_preview_demo();
        }
        if self.dev.editor_demo {
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

    /// Opens the note shortcut recorder and applies the choice (BEHAVIOR §12.2).
    fn change_note_hotkey(&mut self) {
        let current = self.configured_hotkey("noteKey", "Ctrl+Alt+N");
        // Release every app hotkey while recording. Otherwise pressing one of
        // our own combinations triggers it instead of reaching the dialog, and
        // the recorder's availability probe would fail on our registrations.
        // SAFETY: the hotkey ids belong to this window.
        unsafe {
            let _ = UnregisterHotKey(Some(self.hwnd), HOTKEY_TOGGLE);
            let _ = UnregisterHotKey(Some(self.hwnd), HOTKEY_NOTE);
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
            None
        };
        let choice = {
            // The recorder runs its own message loop on this thread while `&mut
            // self` is live below.
            let _modal = window::ModalGuard::enter();
            window::shortcuts::ask(
                self.hwnd,
                i18n::s().note_shortcut,
                current,
                Box::new(validate),
            )
        };
        let mut registered = false;
        match choice {
            window::shortcuts::Choice::Cancelled => {
                if let Some(hotkey) = current {
                    registered = Self::register_hotkey(self.hwnd, HOTKEY_NOTE, hotkey);
                }
            }
            window::shortcuts::Choice::Chosen(None) => {
                self.settings.set_string("noteKey", "none");
                self.settings.save();
                registered = true;
            }
            window::shortcuts::Choice::Chosen(Some(hotkey)) => {
                if Self::register_hotkey(self.hwnd, HOTKEY_NOTE, hotkey) {
                    self.settings.set_string("noteKey", &hotkey.label());
                    self.settings.save();
                    registered = true;
                } else {
                    // Somebody took it between the probe and now.
                    logging::error("hotkey: registration failed after the dialog");
                    if let Some(previous) = current {
                        registered = Self::register_hotkey(self.hwnd, HOTKEY_NOTE, previous);
                    }
                    let body = i18n::s().conflict_external.to_string();
                    if let Some(tray) = self.windows.tray.as_mut() {
                        tray.show_balloon("Pegline", &body);
                    }
                }
            }
        }
        self.note_hotkey_ok = registered;
        // SAFETY: the toggle id belongs to this window; the combination is the
        // fixed Ctrl+Alt+T.
        let toggle = unsafe {
            RegisterHotKey(
                Some(self.hwnd),
                HOTKEY_TOGGLE,
                windows::Win32::UI::Input::KeyboardAndMouse::HOT_KEY_MODIFIERS(
                    windows::Win32::UI::Input::KeyboardAndMouse::MOD_CONTROL.0
                        | windows::Win32::UI::Input::KeyboardAndMouse::MOD_ALT.0,
                ),
                'T' as u32,
            )
        };
        if let Err(error) = toggle {
            logging::error(format!("RegisterHotKey(toggle): {error}"));
        }
    }

    /// Registers one app hotkey; returns whether Windows accepted it.
    fn register_hotkey(hwnd: HWND, id: i32, hotkey: shortcut::Hotkey) -> bool {
        // SAFETY: the window is ours and the id is one of our action ids.
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
            Ok(hook) => self.windows.hook = Some(hook),
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
                self.windows.overlay = Some(overlay);
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
                if wparam.0 == TIMER_NOTE {
                    self.update_note_entry(win32::now_seconds());
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
                    if let Some(tray) = self.windows.tray.as_mut() {
                        tray.reinstall();
                    }
                    return Some(LRESULT(0));
                }
                None
            }
        }
    }

    fn tick(&mut self) {
        if self.quit {
            return;
        }
        if self.dev.panic_test && !self.dev.panicked {
            self.dev.panicked = true;
            panic!("panic test requested");
        }
        if self.state.dragging {
            // The OLE drag loop is pumping messages on this thread; only the
            // 15 ms ghost timer should touch the UI until it returns.
            return;
        }
        let now = win32::now_seconds();
        self.drain_events(now);
        if self.quit_at.is_some() {
            // Shutting down: drain the commit result, then stop doing any other
            // work. `poll_quit` finishes the quit on its deadline at the latest.
            self.poll_quit(now);
            return;
        }
        if let Some(at) = self.dev.editor_demo_at {
            if now >= at {
                self.dev.editor_demo_at = None;
                if let Some(editor) = self.windows.editor.as_ref() {
                    // SAFETY: our own editor window, still alive while `Editor`
                    // is held; WM_CLOSE is the documented close request.
                    unsafe {
                        let _ = PostMessageW(Some(editor.hwnd()), WM_CLOSE, WPARAM(0), LPARAM(0));
                    }
                }
            }
        }
        if let Some(at) = self.book.note_show_at {
            if now >= at {
                self.book.note_show_at = None;
                if let Some(editor) = self.windows.note_editor.as_mut() {
                    editor.show();
                }
                // The flight was interrupted before its first frame (a new
                // capture can take the flight window over); the card must not
                // stay hanging next to the open editor.
                self.hide_note_card();
            }
        }
        if let Some(at) = self.book.annotate_show_at {
            if now >= at {
                self.book.annotate_show_at = None;
                if let Some(editor) = self.windows.editor.as_mut() {
                    editor.show();
                }
                self.hide_annotated_card();
            }
        }
        if let Some(at) = self.book.annotate_close_at {
            if now >= at {
                // The return flight never reported back; release the hidden
                // editor and put the card back rather than leaking both.
                logging::trace("annotation return deadline reached");
                self.close_editor();
            }
        }
        self.run_dev(now);
        if let Some(at) = self.state.catch_offer_at {
            if now >= at {
                self.state.catch_offer_at = None;
                self.offer_catching();
            }
        }
        if self.sessions.note_tumbling && now >= self.sessions.note_tumble_until {
            // The flight never reported back (interrupted by a capture, or its
            // thread died): release the hidden editor anyway.
            logging::trace("note tumble deadline reached");
            self.close_note_editor();
        }
        if let Some(at) = self.capture.clipboard_at {
            if now >= at {
                self.capture.clipboard_at = None;
                self.handle_clipboard();
            }
        }
        self.drain_mouse(now);
        self.update_pointer(now);
        self.update_resident();
        self.update_note_entry(now);
        self.update_timers(now);
        self.update_preview(now);
        self.prune_missing(now);
        self.reload_edited(now);
        self.release_idle_windows(now);
        self.update_click_through();
        if self
            .windows
            .editor
            .as_ref()
            .is_some_and(|editor| editor.is_closed())
        {
            if let Some(mut editor) = self.windows.editor.take() {
                editor.shutdown();
            }
        }
        if self
            .windows
            .note_editor
            .as_ref()
            .is_some_and(|editor| editor.is_closed())
        {
            if let Some(mut editor) = self.windows.note_editor.take() {
                editor.shutdown();
            }
            self.sessions.note_editing = None;
            self.sessions.note_committing = false;
            self.book.note_show_at = None;
            self.state.pinned = false;
        }
    }

    fn drain_events(&mut self, now: f64) {
        while let Ok(event) = self.events_rx.try_recv() {
            match event {
                Event::CaptureFile(path) => self.on_capture_file(path),
                Event::ClipboardChanged => {
                    if self.capture.clipboard_at.is_none() {
                        self.capture.clipboard_at = Some(now + CLIPBOARD_SETTLE);
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
                    // The text exists on disk but is not on the line (it was
                    // taken down earlier). "Show the note already there" means
                    // hang that file again instead of just opening an empty
                    // line or writing a duplicate.
                    logging::trace(format!("note exists off-line: {}", path.display()));
                    let color = self.settings.note_color(&file_name(&path));
                    let tx = self.events_tx.clone();
                    let thread_path = path.clone();
                    let _ = std::thread::Builder::new()
                        .name("pegline-note".to_string())
                        .spawn(move || {
                            let Some(text) = notes::read(&thread_path) else {
                                return;
                            };
                            match notes::render(&text, color) {
                                Some(image) => {
                                    let _ = tx.send(Event::NoteReady {
                                        path: thread_path,
                                        text,
                                        image,
                                    });
                                    window::wake();
                                }
                                None => logging::error(format!(
                                    "note render: {}",
                                    thread_path.display()
                                )),
                            }
                        });
                }
                Event::NoteImage { id, image } => self.on_thumbnail(id, image),
                Event::ThumbnailReady { id, image, text } => {
                    self.on_thumbnail(id, image);
                    if let Some(text) = text {
                        if let Some(item) = self.line.items.iter_mut().find(|item| item.id == id) {
                            item.text = Some(text);
                        }
                    }
                }
                Event::Editor(EditorEvent::Saved {
                    png,
                    preview,
                    flight,
                }) => self.on_editor_saved(png, preview, flight, now),
                Event::Editor(EditorEvent::Cancelled { flight }) => {
                    self.on_editor_cancelled(flight)
                }
                Event::NoteEditor(event) => self.on_note_editor_event(event),
                Event::NoteSaved {
                    card,
                    path,
                    text,
                    image,
                    error,
                } => self.on_note_saved(card, path, text, image, error),
                Event::NoteArchived { id, result } => self.on_note_archived(id, result),
                Event::TrashDone { paths } => {
                    logging::trace(format!("recycled {} files", paths.len()));
                }
                Event::Flight(FlightEvent::Started(token)) => self.on_flight_started(token),
                Event::Flight(FlightEvent::Landed { token, .. }) => self.on_flight_landed(token),
                Event::Flight(FlightEvent::Finished(id)) => self.on_flight_finished(id),
            }
        }
    }

    /// The flight window's first frame is on screen: whatever it took over
    /// from may disappear now without leaving a gap behind it.
    fn on_flight_started(&mut self, token: u64) {
        if token == TUMBLE_FLIGHT_TOKEN {
            // The thrown-away paper is drawn: the editor behind it
            // can disappear without a flash.
            if let Some(editor) = self.windows.note_editor.as_mut() {
                editor.hide();
            }
        } else if token == NOTE_FLIGHT_TOKEN {
            // The flight's first frame is on screen, so the note's
            // card can vanish from the line without a gap. It is
            // hidden only now: hiding it before the first frame
            // would leave the slot empty for a frame.
            self.hide_note_card();
        } else if token == ANNOTATE_OPEN_TOKEN {
            // Same hand-off for a picture growing into the editor.
            self.hide_annotated_card();
        } else if token == ANNOTATE_CLOSE_TOKEN {
            // The return flight is on screen: the editor behind it
            // can go without a frame of empty desktop.
            if let Some(editor) = self.windows.editor.as_mut() {
                editor.hide();
            }
        }
    }

    /// The flight reached its destination: hand the card over to the real one
    /// underneath, or show the editor the flight was carrying it into.
    fn on_flight_landed(&mut self, token: u64) {
        if token == NOTE_FLIGHT_TOKEN {
            self.book.note_show_at = None;
            if let Some(editor) = self.windows.note_editor.as_mut() {
                editor.show();
            }
            if let Some(flight) = self.windows.flight.as_ref() {
                win32::set_topmost(flight.hwnd());
            }
            return;
        }
        if token == ANNOTATE_OPEN_TOKEN {
            // The enlarged card is drawn by the flight; now the
            // real editor can appear underneath it.
            self.book.annotate_show_at = None;
            if let Some(editor) = self.windows.editor.as_mut() {
                editor.show();
            }
            if let Some(flight) = self.windows.flight.as_ref() {
                win32::set_topmost(flight.hwnd());
            }
            return;
        }
        if token == ANNOTATE_CLOSE_TOKEN {
            // The picture is home: hand it to the card underneath.
            let id = self.book.flying_card.take();
            self.book.flying_rehang = false;
            if let Some(id) = id {
                self.send_scene(Command::SetRehang { id, flying: false });
            }
            if let Some(flight) = self.windows.flight.as_ref() {
                win32::set_topmost(flight.hwnd());
            }
            return;
        }
        let rehang = self.book.flying_rehang && self.book.flying_card == Some(token);
        if self.book.flying_card == Some(token) {
            self.book.flying_card = None;
        }
        // The real card is already opaque; keep the fading flight
        // image above it so the hand-off stays a clean dissolve.
        if let Some(flight) = self.windows.flight.as_ref() {
            win32::set_topmost(flight.hwnd());
        }
        let command = if rehang {
            Command::SetRehang {
                id: token,
                flying: false,
            }
        } else {
            Command::SetFlying {
                id: token,
                flying: false,
            }
        };
        self.send_scene(command);
        self.book.flying_rehang = false;
    }

    /// The flight is over and its window can go back to the pool.
    fn on_flight_finished(&mut self, id: u64) {
        if id == NOTE_FLIGHT_TOKEN {
            if let Some(flight) = self.windows.flight.as_mut() {
                flight.hide();
            }
            self.windows.flight_idle_at = Some(win32::now_seconds());
            return;
        }
        if id == ANNOTATE_OPEN_TOKEN {
            // The editor is fully drawn underneath; the window can
            // give its surface back until the return flight.
            if let Some(flight) = self.windows.flight.as_mut() {
                flight.hide();
            }
            self.windows.flight_idle_at = Some(win32::now_seconds());
            return;
        }
        if id == ANNOTATE_CLOSE_TOKEN {
            if let Some(flight) = self.windows.flight.as_mut() {
                flight.hide();
            }
            self.windows.flight_idle_at = Some(win32::now_seconds());
            self.book.flying_card = None;
            self.book.flying_rehang = false;
            self.book.annotate_close_at = None;
            self.close_editor();
            return;
        }
        if id == TUMBLE_FLIGHT_TOKEN {
            // The thrown-away note is off the screen: release the
            // hidden editor now.
            if let Some(flight) = self.windows.flight.as_mut() {
                flight.hide();
            }
            self.windows.flight_idle_at = Some(win32::now_seconds());
            self.close_note_editor();
            return;
        }
        self.book.flying_card = None;
        self.book.flying_rehang = false;
        if let Some(flight) = self.windows.flight.as_mut() {
            flight.hide();
        }
        self.windows.flight_idle_at = Some(win32::now_seconds());
    }

    fn update_timers(&mut self, now: f64) {
        // Keep the editor-save suppression list from growing for the life of
        // the process; the watcher only needs the last few seconds.
        self.capture
            .recently_saved
            .retain(|(_, at)| now - *at < 10.0);
        if let Some(hide_at) = self.state.hide_at {
            if now >= hide_at {
                self.state.hide_at = None;
                if let Some(overlay) = self.windows.overlay.as_mut() {
                    overlay.hide();
                }
                self.state.is_present = false;
            }
        }
        if let Some(until) = self.state.startup_until {
            if now >= until {
                self.state.startup_until = None;
                self.state.keep_open = false;
                if self.line.items.is_empty() && !self.resident_active() {
                    self.dismiss(now);
                }
            }
        }
        if self.line.items.is_empty()
            && !self.state.keep_open
            && !self.resident_active()
            && self.state.is_present
        {
            let since = *self.state.empty_since.get_or_insert(now);
            if now - since >= EMPTY_DELAY {
                self.state.empty_since = None;
                self.dismiss(now);
            }
        } else {
            self.state.empty_since = None;
        }
    }

    fn prune_missing(&mut self, now: f64) {
        if now - self.state.last_prune < 1.0 {
            return;
        }
        self.state.last_prune = now;
        if self.dev.demo {
            return;
        }
        let mut gone = Vec::new();
        for item in &mut self.line.items {
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
        if let Some(at) = self.windows.flight_idle_at {
            if now - at >= FLIGHT_IDLE_SECONDS {
                self.windows.flight_idle_at = None;
                if let Some(mut flight) = self.windows.flight.take() {
                    flight.shutdown();
                    logging::trace("flight window released after idle");
                }
            }
        }
        if let Some(at) = self.windows.preview_idle_at {
            if now - at >= HELPER_IDLE_SECONDS && self.state.note_hover.is_none() {
                self.windows.preview_idle_at = None;
                if let Some(mut preview) = self.windows.preview.take() {
                    preview.shutdown();
                    logging::trace("preview window released after idle");
                }
            }
        }
        if let Some(at) = self.windows.ghost_idle_at {
            if now - at >= HELPER_IDLE_SECONDS {
                self.windows.ghost_idle_at = None;
                if let Some(mut ghost) = self.windows.ghost.take() {
                    ghost.shutdown();
                    logging::trace("drag ghost released after idle");
                }
            }
        }
    }

    // ---- card actions -----------------------------------------------------

    fn copy_item(&mut self, id: CardId) {
        let Some(item) = self.line.items.iter().find(|item| item.id == id) else {
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
                        window::wake();
                    }
                    Err(error) => logging::error(format!("copy decode: {error}")),
                },
            );
    }

    fn on_copy_ready(&mut self, id: CardId, dib: ImageData, png: Vec<u8>) {
        let Some(item) = self.line.items.iter().find(|item| item.id == id) else {
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

    /// The invisible note entry to the right of the last card, in window DIP
    /// (NOTE-EDITOR.md §4.1). The slot, top and height use the target layout;
    /// the horizontal position comes from the scene's row spring so the
    /// hotspot glides with the drawn corner while a removal re-centres the row.
    fn note_entry_hotspot(&self) -> Option<RectF> {
        let mut hotspot = self.note_entry_hotspot_layout()?;
        let overlay = self.windows.overlay.as_ref()?;
        let snapshot = overlay.snapshot();
        let snapshot = snapshot.lock().ok()?;
        if let Some(center_x) = snapshot.note_entry_x {
            let width = hotspot.width();
            hotspot.left = center_x - width * 0.5;
            hotspot.right = hotspot.left + width;
        }
        hotspot.top += snapshot.slide;
        Some(hotspot)
    }

    /// The hotspot at the line's target layout (slide 0). It supplies the slot
    /// and size, and is the hover note's home so the note lands on the
    /// theoretical slot instead of chasing the row spring.
    fn note_entry_hotspot_layout(&self) -> Option<RectF> {
        let overlay = self.windows.overlay.as_ref()?;
        let width_dip = overlay.width_px() as f32 / overlay.scale().max(0.1);
        let (height, note) = self
            .line
            .items
            .last()
            .map(|item| (item.card_size.1, item.note))
            .unwrap_or((layout::NOTE_CARD_HEIGHT, true));
        layout::note_hotspot(self.line.items.len(), width_dip, height, note)
    }

    /// The hover note's paper rectangle in screen pixels.
    ///
    /// The flight to the editor starts from the paper, not from the glass card:
    /// the card's frame dissolves when the flight begins, and keeping the paper
    /// the same size means only the thin frame goes away.
    fn note_hover_paper_px(&self) -> Option<RectI> {
        let hover = self.state.note_hover.as_ref()?;
        let scale = hover.dpi.max(96) as f32 / 96.0;
        let width = notes::CARD_WIDTH * scale;
        let height = notes::CARD_HEIGHT * scale;
        Some(RectI::new(
            (hover.center.0 - width * 0.5).round() as i32,
            (hover.center.1 - height * 0.5).round() as i32,
            (hover.center.0 + width * 0.5).round() as i32,
            (hover.center.1 + height * 0.5).round() as i32,
        ))
    }

    /// Where the hovering blank note's whole glass card is, in screen pixels.
    fn note_hover_card_px(&self) -> Option<RectI> {
        let paper = self.note_hover_paper_px()?;
        let scale = self.state.note_hover.as_ref()?.dpi.max(96) as f32 / 96.0;
        let inset = (layout::PHOTO_INSET * scale).round() as i32;
        Some(paper.inflate(inset))
    }

    /// True when a screen pixel is on the leaning hover note's card.
    ///
    /// The preview window is axis-aligned and only its content rotates, so the
    /// hit test undoes the lean about the card's centre instead of trusting
    /// the upright rectangle (NOTE-EDITOR.md §4.5).
    fn note_hover_hits(&self, x: i32, y: i32) -> bool {
        let Some(hover) = self.state.note_hover.as_ref() else {
            return false;
        };
        let scale = hover.dpi.max(96) as f32 / 96.0;
        let (card_w, card_h) = layout::note_card_size();
        tilted_card_hits(
            hover.center,
            card_w * 0.5 * scale,
            card_h * 0.5 * scale,
            hover.angle,
            x as f32,
            y as f32,
        )
    }

    fn note_preview_bounds(&self, center: (f32, f32), dpi: u32) -> RectI {
        let scale = dpi.max(96) as f32 / 96.0;
        let (card_w, card_h) = layout::note_card_size();
        let width = (card_w + NOTE_PADDING_DIP * 2.0) * scale;
        let height = (card_h + NOTE_PADDING_DIP * 2.0) * scale;
        RectI::new(
            (center.0 - width * 0.5).round() as i32,
            (center.1 - height * 0.5).round() as i32,
            (center.0 + width * 0.5).round() as i32,
            (center.1 + height * 0.5).round() as i32,
        )
    }

    /// The peeking corner, fast entry drop, follow/float and fly-out
    /// (NOTE-EDITOR.md §3.1, §4.3).
    fn update_note_entry(&mut self, now: f64) {
        if !self.state.is_revealed
            || self.windows.note_editor.is_some()
            || self.windows.menu_open
            || self.state.dragging
        {
            if self.state.note_hover.is_some() || self.state.note_peek_on {
                logging::trace(format!(
                    "note entry reset: revealed={} editor={} menu={} dragging={}",
                    self.state.is_revealed,
                    self.windows.note_editor.is_some(),
                    self.windows.menu_open,
                    self.state.dragging
                ));
            }
            self.hide_note_peek_now();
            self.state.note_hotspot_hovered = false;
            self.cancel_note_hover();
            return;
        }
        let Some(pointer) = self.note_entry_pointer() else {
            return;
        };
        let (pointer_x, pointer_y) = pointer;
        let Some(screen) = self
            .screen
            .or_else(|| win32::monitor_at(pointer_x, pointer_y))
        else {
            return;
        };
        let scale = self
            .windows
            .overlay
            .as_ref()
            .map(Overlay::scale)
            .unwrap_or(1.0);
        let (hotspot, in_hotspot, keep_near_entry, in_line_range) =
            self.note_entry_zones(pointer_x, pointer_y);
        self.state.note_hotspot_hovered = in_hotspot;
        self.set_note_peek(in_line_range && self.state.note_hover.is_none() && hotspot.is_some());

        let (card_w, card_h) = layout::note_card_size();
        let window_height = (card_h + NOTE_PADDING_DIP * 2.0) * scale;
        let work = screen.work;
        let mut bounds = None;
        let mut finished = false;
        let slow = self.dev.note_slow as f64;

        if let Some(hover) = self.state.note_hover.as_mut() {
            match hover.phase {
                HoverPhase::In => {
                    let t = ((now - hover.started) / (NOTE_FLY_IN_SECONDS * slow)).clamp(0.0, 1.0)
                        as f32;
                    let k = ease::in_out_cubic(t);
                    // No work-area clamp here: the fall starts above the screen
                    // and must be allowed to enter; clamping would pin it to
                    // the top edge. The card keeps its peek tilt for the first
                    // part of the fall and straightens as it lands.
                    let center = (
                        hover.from.0 + (hover.to.0 - hover.from.0) * k,
                        hover.from.1 + (hover.to.1 - hover.from.1) * k,
                    );
                    hover.center = center;
                    hover.angle = layout::NOTE_PEEK_ANGLE_DEG * (1.0 - k);
                    if now >= hover.trace_at {
                        logging::trace(format!("hover fly-in t={t:.2} center={:?}", hover.center));
                        hover.trace_at = now + 0.1;
                    }
                    if t >= 1.0 {
                        hover.phase = HoverPhase::Hover;
                        hover.last = now;
                        hover.float_start = now;
                        hover.pos = center;
                        hover.vel = (0.0, 0.0);
                        hover.angle = 0.0;
                        hover.spin = 0.0;
                        logging::trace(format!(
                            "note hover at home {:?} (hotspot={in_hotspot})",
                            hover.home
                        ));
                    } else if !keep_near_entry && t > 0.15 {
                        // The pointer left during the fly-in: do not finish the
                        // entry and then leave; reverse from the current point.
                        logging::trace("note hover reverses during fly-in");
                        hover.phase = HoverPhase::Out;
                        hover.started = now;
                        hover.last = now;
                        hover.from = center;
                        hover.to = (center.0, work.top as f32 - window_height * 0.7);
                    }
                }
                HoverPhase::Hover => {
                    if !keep_near_entry {
                        logging::trace("note hover starts flying out");
                        hover.phase = HoverPhase::Out;
                        hover.started = now;
                        hover.from = hover.center;
                        hover.to = (hover.center.0, work.top as f32 - window_height * 0.7);
                    } else {
                        step_hover_physics(hover, pointer_x, pointer_y, now, scale, work);
                    }
                }
                HoverPhase::Out => {
                    let t = ((now - hover.started) / (NOTE_FLY_OUT_SECONDS * slow)).clamp(0.0, 1.0)
                        as f32;
                    let k = ease::in_out_cubic(t);
                    hover.center = (
                        hover.from.0 + (hover.to.0 - hover.from.0) * k,
                        hover.from.1 + (hover.to.1 - hover.from.1) * k,
                    );
                    // The pointer no longer pulls: the paper straightens out
                    // while it climbs away, on the same angular spring.
                    let dt = ((now - hover.last) as f32).clamp(0.0, 0.05);
                    hover.last = now;
                    if dt > 0.0 {
                        step_hover_spin(hover, 0.0, dt);
                    }
                    if now >= hover.trace_at {
                        logging::trace(format!("hover fly-out t={t:.2} center={:?}", hover.center));
                        hover.trace_at = now + 0.1;
                    }
                    if t >= 1.0 {
                        finished = true;
                    }
                }
            }
            if !finished {
                let hover_scale = hover.dpi.max(96) as f32 / 96.0;
                let width = (card_w + NOTE_PADDING_DIP * 2.0) * hover_scale;
                let height = (card_h + NOTE_PADDING_DIP * 2.0) * hover_scale;
                bounds = Some(RectI::new(
                    (hover.center.0 - width * 0.5).round() as i32,
                    (hover.center.1 - height * 0.5).round() as i32,
                    (hover.center.0 + width * 0.5).round() as i32,
                    (hover.center.1 + height * 0.5).round() as i32,
                ));
            }
        } else if in_hotspot {
            // The pointer reached the advertised entry: show the rest of the
            // note at once (a short drop), with no dwell timer.
            self.begin_note_hover(now, screen, scale);
            return;
        }

        if let Some(bounds) = bounds {
            if let (Some(hover), Some(preview)) = (
                self.state.note_hover.as_ref(),
                self.windows.preview.as_ref(),
            ) {
                preview.move_to(bounds);
                // A window cannot rotate, so the paper is redrawn whenever the
                // lean changes; moving stays the cheap path in between.
                preview.tilt_note(hover.angle);
            }
        }
        if finished {
            self.cancel_note_hover();
        }
    }

    /// The pointer the entry follows: the real cursor, or the scripted one the
    /// visual acceptance runs park on the hotspot.
    ///
    /// The scripted pointer is placed *inside* the hotspot with the offset the
    /// run asked for (`PEGLINE_NOTE_DEMO_POINTER`), because that offset is what
    /// shows the pull and the lean of the paper.
    fn note_entry_pointer(&self) -> Option<(i32, i32)> {
        if !self.dev.note_hover_demo {
            return win32::cursor_pos();
        }
        if let Some(pointer) = self.dev.note_peek_pointer {
            return Some(pointer);
        }
        let hotspot = self.note_entry_hotspot()?;
        let overlay = self.windows.overlay.as_ref()?;
        let rect = win32::window_rect(overlay.hwnd())?;
        let scale = overlay.scale().max(0.1);
        let (cx, cy) = hotspot.center();
        // A narrow hotspot shrinks the parking zone.
        let limit_x = (hotspot.width() * 0.5 - 3.0).max(0.0);
        let limit_y = (hotspot.height() * 0.5 - 3.0).max(0.0);
        let dx = self.dev.note_demo_pointer.0.clamp(-limit_x, limit_x);
        let dy = self.dev.note_demo_pointer.1.clamp(-limit_y, limit_y);
        Some((
            rect.left + ((cx + dx) * scale).round() as i32,
            rect.top + ((cy + dy) * scale).round() as i32,
        ))
    }

    /// Where the pointer sits relative to the entry, in window DIP:
    /// `(hotspot, on the hotspot, near the entry, anywhere over the strip)`.
    ///
    /// The visible corner sits at the top edge, above the slot itself, so the
    /// entry owns the whole column from the screen edge down to the slot.
    /// `near` is a fixed 24 DIP margin that keeps the pulled note clickable at
    /// its edge — the note itself must never be the exit test, or the pull would
    /// keep bringing it back under the cursor and it would never fly out.
    fn note_entry_zones(
        &self,
        pointer_x: i32,
        pointer_y: i32,
    ) -> (Option<RectF>, bool, bool, bool) {
        let (dip_x, dip_y) = self.to_window_dip(pointer_x, pointer_y);
        let hotspot = self.note_entry_hotspot();
        let entry_hit =
            hotspot.map(|rect| RectF::from_xywh(rect.left, 0.0, rect.width(), rect.bottom));
        let in_hotspot = entry_hit
            .as_ref()
            .is_some_and(|rect| rect.contains(dip_x, dip_y));
        let keep_near = entry_hit
            .as_ref()
            .is_some_and(|rect| rect.inflated(24.0).contains(dip_x, dip_y));
        // The whole strip, plus a little air, is the tool's range: as soon as the
        // pointer is there the entry advertises itself with a peeking corner.
        // Only stepping onto the hotspot completes it.
        let in_line_range = self.windows.overlay.as_ref().is_some_and(|overlay| {
            win32::window_rect(overlay.hwnd()).is_some_and(|rect| {
                let margin = (12.0 * overlay.scale()).round() as i32;
                pointer_x >= rect.left - margin
                    && pointer_x < rect.right + margin
                    && pointer_y >= rect.top
                    && pointer_y < rect.bottom + margin
            })
        });
        (hotspot, in_hotspot, keep_near, in_line_range)
    }

    fn begin_note_hover(&mut self, now: f64, screen: Monitor, scale: f32) {
        self.hide_note_peek_now();
        self.hide_preview();
        self.state.preview_state.reset();
        if self.windows.preview.is_none() {
            match Preview::create() {
                Ok(preview) => {
                    self.windows.preview = Some(preview);
                    logging::trace("note preview window created");
                }
                Err(error) => {
                    logging::error(format!("note preview window: {error}"));
                    return;
                }
            }
        }
        let Some(home) = self.note_entry_home_px(scale) else {
            return;
        };
        let (card_w, card_h) = layout::note_card_size();
        let window_height = (card_h + NOTE_PADDING_DIP * 2.0) * scale;
        let window_width = (card_w + NOTE_PADDING_DIP * 2.0) * scale;
        let home = clamp_center(home, window_width * 0.5, window_height * 0.5, screen.work);
        // The entry advertises only its bottom-right corner above the screen;
        // the full note falls from that same off-screen position and rotates
        // upright as it lands.
        let Some(from) = self.note_entry_peek_center_px(scale) else {
            return;
        };
        let color = 0;
        let dpi = screen.dpi;
        let bounds = self.note_preview_bounds(from, dpi);
        if let Some(preview) = self.windows.preview.as_mut() {
            preview.show_note(color, bounds, dpi, layout::NOTE_PEEK_ANGLE_DEG);
        }
        self.windows.preview_idle_at = None;
        self.state.note_hover = Some(HoverNote {
            phase: HoverPhase::In,
            started: now,
            from,
            to: home,
            center: from,
            home,
            pos: from,
            vel: (0.0, 0.0),
            angle: layout::NOTE_PEEK_ANGLE_DEG,
            spin: 0.0,
            float_start: now,
            dpi,
            last: now,
            trace_at: now,
        });
        self.state.note_pin = true;
        self.present();
        // SAFETY: our own message window; the timer id is ours.
        let timer = unsafe { SetTimer(Some(self.hwnd), TIMER_NOTE, NOTE_FOLLOW_MS, None) };
        if timer == 0 {
            logging::error("note hover SetTimer failed");
        }
        logging::trace(format!("note hover fly-in to {home:?}"));
    }

    /// The card centre of the theoretical slot where a new note would hang,
    /// in screen pixels. The hover note floats here, not under the pointer.
    fn note_entry_home_px(&self, scale: f32) -> Option<(f32, f32)> {
        let hotspot = self.note_entry_hotspot_layout()?;
        let overlay = self.windows.overlay.as_ref()?;
        let rect = win32::window_rect(overlay.hwnd())?;
        let (_, card_h) = layout::note_card_size();
        let (center_x, _) = hotspot.center();
        // The card's top is the slot's top, so its glass frame lines up with
        // the frames of the cards already hanging.
        let center_y = hotspot.top + card_h * 0.5;
        Some((
            rect.left as f32 + center_x * scale,
            rect.top as f32 + center_y * scale,
        ))
    }

    /// Where the peeking corner starts, in screen pixels: the tilted card's
    /// centre, almost entirely above the line strip. The visible corner is at
    /// the entry's x, [`layout::NOTE_PEEK_VISIBLE_DIP`] below the top edge.
    /// Uses the same animated x as the drawn corner, so entering the entry
    /// during a reflow starts the full note where the corner already is.
    fn note_entry_peek_center_px(&self, scale: f32) -> Option<(f32, f32)> {
        let hotspot = self.note_entry_hotspot()?;
        let overlay = self.windows.overlay.as_ref()?;
        let rect = win32::window_rect(overlay.hwnd())?;
        let (center_x, center_y) = layout::note_peek_center(hotspot.center().0);
        Some((
            rect.left as f32 + center_x * scale,
            rect.top as f32 + center_y * scale,
        ))
    }

    fn cancel_note_hover(&mut self) {
        if self.state.note_hover.take().is_some() {
            // SAFETY: killing a timer we set on our own message window.
            unsafe {
                let _ = KillTimer(Some(self.hwnd), TIMER_NOTE);
            }
            logging::trace("note hover cancelled");
            self.hide_preview();
        }
        self.state.note_pin = false;
        self.state.note_hotspot_hovered = false;
    }

    /// Shows or hides the entry's peeking corner. The scene animates it, so the
    /// command is only sent when the state actually changes.
    fn set_note_peek(&mut self, on: bool) {
        let changed = self.state.note_peek_on != on;
        if changed {
            self.state.note_peek_on = on;
            logging::trace(format!("note peek {on}"));
        }
        // Keep asserting `true`: the scene snaps the peek away when the strip
        // retracts, and the next tick must restore it even if the app's flag
        // never observed the retract. The scene ignores a repeated target.
        if on {
            self.send_scene(Command::NotePeek(true));
        } else if changed {
            self.send_scene(Command::NotePeek(false));
        }
    }

    /// Takes the peeking corner away without its 0.22 s retract, so the full
    /// note replacing it never has a yellow copy shrinking behind it.
    fn hide_note_peek_now(&mut self) {
        if self.state.note_peek_on {
            self.state.note_peek_on = false;
            self.send_scene(Command::HideNotePeek);
        }
    }

    /// A click on the blank note opens a brand-new note in the editor.
    fn open_new_note_editor(&mut self) {
        if self.windows.note_editor.is_some() || self.windows.editor.is_some() {
            return;
        }
        logging::trace("new note editor requested");
        let Some(monitor) = self.screen.or_else(|| self.pointer_screen()) else {
            return;
        };
        let from = self.note_hover_card_px();
        let angle = self
            .state
            .note_hover
            .as_ref()
            .map(|hover| hover.angle)
            .unwrap_or(0.0);
        // The card's own blank paper dissolves into the editor view before the
        // flight starts; the editor image itself is rendered by its render
        // thread and taken below.
        let start_image = notes::render("", 0);
        self.cancel_note_hover();
        // The flight window is about to be taken over for the trip to the
        // editor; any card it was carrying must become visible in the scene.
        self.release_flying_card();
        let tx = self.events_tx.clone();
        let notify = Box::new(move |event| {
            let _ = tx.send(Event::NoteEditor(event));
        });
        match NoteEditor::open_hidden(
            String::new(),
            0,
            PaperSize::default(),
            monitor.dpi,
            monitor,
            notify,
        ) {
            Ok(editor) => {
                self.windows.note_editor = Some(editor);
                self.sessions.note_editing = Some(EditingNote {
                    path: None,
                    card: None,
                    original: String::new(),
                    color: 0,
                });
                self.sessions.note_committing = false;
                self.present();
                self.reveal(true, 0.0);
                let image = self
                    .windows
                    .note_editor
                    .as_mut()
                    .and_then(NoteEditor::take_flight_image)
                    .or_else(|| notes::render("", 0));
                let to = self.note_editor_card_px();
                let dpi = monitor.dpi;
                let launched = match (from, to, image) {
                    (Some(from), Some(to), Some(image)) => {
                        // A brand-new note is not hanging yet, so its flight has
                        // no clothespin.
                        self.windows
                            .ensure_flight(&self.events_tx)
                            .is_some_and(|flight| {
                                flight.fly_entry(
                                    NOTE_FLIGHT_TOKEN,
                                    FlightImage::Ready(image),
                                    start_image,
                                    from,
                                    to,
                                    angle,
                                    PinMode::Hidden,
                                    dpi,
                                    true,
                                )
                            })
                    }
                    _ => false,
                };
                if launched {
                    self.windows.flight_idle_at = None;
                    self.book.note_show_at = Some(win32::now_seconds() + 1.5);
                } else if let Some(editor) = self.windows.note_editor.as_mut() {
                    editor.show();
                }
                logging::trace("new note editor opened from the entry hotspot");
            }
            Err(error) => logging::error(format!("note editor: {error}")),
        }
    }

    /// Dwells on the hovered card and shows its large preview: the decoded
    /// photo for a picture, or a much larger copy of the note's paper for a
    /// sticky note (BEHAVIOR §7).
    fn update_preview(&mut self, now: f64) {
        // The entry hover owns the pooled preview window while it is visible.
        if self.state.note_hover.is_some() {
            return;
        }
        let wanted = self.state.hover.filter(|id| {
            self.state.is_present
                && self.state.is_revealed
                && self.state.pressed.is_none()
                && !self.windows.menu_open
                && self.line.items.iter().any(|item| item.id == *id)
        });
        let Some(id) = wanted else {
            self.hide_preview();
            self.state.preview_state.reset();
            return;
        };
        let Some(item) = self.line.items.iter().find(|item| item.id == id) else {
            self.hide_preview();
            self.state.preview_state.reset();
            return;
        };
        let stamp = item_stamp(item);
        let revision = item.revision;
        if !self
            .state
            .preview_state
            .observe(Some(id), revision, stamp, now)
        {
            return;
        }
        let version = self.state.preview_state.version();
        let Some(card) = self.current_card_px(id) else {
            return;
        };
        let Some(screen) = self.screen else {
            return;
        };
        let scale = self
            .windows
            .overlay
            .as_ref()
            .map(Overlay::scale)
            .unwrap_or(1.0);
        // The preview hangs below the card *and its hover controls*, so the
        // Copy/Open row stays visible and clickable (the WPF app's HoverFrame
        // did the same). Without this the big image covers the buttons.
        let below = (layout::below(item.note) * scale).round() as i32;
        let anchor = RectI::new(card.left, card.top, card.right, card.bottom + below);
        let note = item.note;
        let bounds = if note {
            // A note is not scaled to a source size: its preview is always the
            // same large sheet, so long notes stay readable.
            preview::fixed_bounds(note_preview_card_dip(), anchor, screen.work, scale)
        } else {
            preview::bounds(item.size_px, anchor, screen.work, scale)
        };
        if bounds.is_empty() {
            return;
        }
        let dpi = screen.dpi;
        let tx = self.events_tx.clone();
        if note {
            let text = item.text.clone();
            let path = item.path.clone();
            let color = item.color;
            // At least two pixels per DIP, so a 100 % monitor still gets a
            // crisp sheet when it is drawn at its DIP size.
            let render_scale = scale.max(2.0);
            let _ = std::thread::Builder::new()
                .name("pegline-note-preview".to_string())
                .spawn(move || {
                    let text = text.or_else(|| notes::read(&path)).unwrap_or_default();
                    match notes::render_preview(&text, color, render_scale) {
                        Some(image) => {
                            let _ = tx.send(Event::PreviewReady {
                                id,
                                version,
                                image,
                                bounds,
                                dpi,
                            });
                            window::wake();
                        }
                        None => logging::error("note preview render failed"),
                    }
                });
            return;
        }
        let decode = preview::decode_max(bounds, scale);
        let path = item.path.clone();
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
                    window::wake();
                }
                Err(error) => logging::error(format!("preview decode: {error}")),
            });
    }

    /// Hides the preview and releases its texture. Idempotent.
    fn hide_preview(&mut self) {
        if let Some(preview) = self.windows.preview.as_mut() {
            if preview.is_visible() {
                preview.hide();
                self.windows.preview_idle_at = Some(win32::now_seconds());
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
        if !self.state.preview_state.is_current(version) || self.state.hover != Some(id) {
            return;
        }
        let Some(item) = self.line.items.iter().find(|item| item.id == id) else {
            return;
        };
        if !item.path.exists() || !self.state.is_present || !self.state.is_revealed {
            return;
        }
        let note = item.note;
        if self.windows.preview.is_none() {
            match Preview::create() {
                Ok(preview) => {
                    self.windows.preview = Some(preview);
                    logging::trace("preview window created");
                }
                Err(error) => {
                    logging::error(format!("preview window: {error}"));
                    return;
                }
            }
        }
        if let Some(preview) = self.windows.preview.as_mut() {
            if note {
                preview.show_note_content(image, bounds, dpi);
            } else {
                preview.show(image, bounds, dpi);
            }
            self.windows.preview_idle_at = None;
        }
    }

    // ---- notes ------------------------------------------------------------

    /// `Ctrl+Alt+N`: the clipboard text hangs as a sticky note.
    fn note_shortcut(&mut self) {
        if self.windows.note_editor.is_some() {
            logging::trace("note shortcut ignored while the note editor is open");
            return;
        }
        let Some(text) = clipboard::read_text() else {
            self.note_is_empty();
            return;
        };
        // Clipboard text may use CRLF; notes are compared and stored as LF in
        // memory (the file writer adds CRLF back).
        let text = model::normalize(&text);
        if text.trim().is_empty() {
            self.note_is_empty();
            return;
        }
        // The same text again brings the note already hanging back into view.
        if self
            .line
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
                            window::wake();
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
                window::wake();
            });
    }

    fn note_is_empty(&mut self) {
        let shortcut = self
            .settings
            .string("noteKey")
            .unwrap_or("Ctrl+Alt+N")
            .to_string();
        let body = i18n::s().clipboard_empty.replace("{}", &shortcut);
        if let Some(tray) = self.windows.tray.as_mut() {
            tray.show_balloon("Pegline", &body);
        }
        logging::trace("note shortcut: clipboard has no text");
    }

    fn on_note_ready(&mut self, path: PathBuf, text: String, image: ImageData, now: f64) {
        if self.line.items.iter().any(|item| item.path == path) {
            return;
        }
        if self.line.items.len() >= layout::capacity(100_000.0) {
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
        self.capture.recent.retain(|recent| recent.path != path);
        self.capture.recent.push(Recent {
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
        let Some(item) = self.line.items.iter().find(|item| item.id == id) else {
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
                    window::wake();
                }
            });
    }

    fn set_note_color(&mut self, id: CardId, color: usize) {
        let Some(item) = self.line.items.iter_mut().find(|item| item.id == id) else {
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

    // ---- note editor ------------------------------------------------------

    /// Opens a hanging note in the WYSIWYG editor. The card stays on the line
    /// (pinned) so the committed note has a slot to land in.
    fn open_note_editor(&mut self, id: CardId) {
        if self.windows.note_editor.is_some() || self.windows.editor.is_some() {
            logging::trace("note editor ignored: another editor is open");
            return;
        }
        logging::trace(format!("note editor requested for card {id}"));
        self.cancel_note_hover();
        // Opening the editor interrupts whatever the flight window was doing.
        self.release_flying_card();
        let Some(item) = self.line.items.iter().find(|item| item.id == id) else {
            return;
        };
        let Some(text) = item.text.clone().or_else(|| notes::read(&item.path)) else {
            logging::error(format!("note read: {}", item.path.display()));
            return;
        };
        let original = model::normalize(&text);
        let color = item.color;
        let path = item.path.clone();
        let card_image = item.thumb.clone();
        let from = self.current_card_px(id);
        let angle = self.current_card_angle(id).unwrap_or(item.tilt);
        let Some(monitor) = self.screen.or_else(|| self.pointer_screen()) else {
            return;
        };
        let tx = self.events_tx.clone();
        let notify = Box::new(move |event| {
            let _ = tx.send(Event::NoteEditor(event));
        });
        let paper = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| self.settings.note_size(name))
            .unwrap_or_default();
        match NoteEditor::open_hidden(original.clone(), color, paper, monitor.dpi, monitor, notify)
        {
            Ok(editor) => {
                self.windows.note_editor = Some(editor);
                self.sessions.note_editing = Some(EditingNote {
                    path: Some(path),
                    card: Some(id),
                    original,
                    color,
                });
                self.sessions.note_committing = false;
                self.present();
                self.reveal(true, 0.0);
                let image = self
                    .windows
                    .note_editor
                    .as_mut()
                    .and_then(NoteEditor::take_flight_image)
                    .unwrap_or_else(|| card_image.clone());
                let to = self.note_editor_card_px();
                let dpi = monitor.dpi;
                let launched = match (from, to) {
                    (Some(from), Some(to)) => self
                        .windows
                        .ensure_flight(&self.events_tx)
                        .is_some_and(|flight| {
                            flight.fly_entry(
                                NOTE_FLIGHT_TOKEN,
                                FlightImage::Ready(image),
                                Some(card_image),
                                from,
                                to,
                                angle,
                                PinMode::Leave,
                                dpi,
                                true,
                            )
                        }),
                    _ => false,
                };
                if launched {
                    self.windows.flight_idle_at = None;
                    self.book.note_show_at = Some(win32::now_seconds() + 1.5);
                } else {
                    if let Some(editor) = self.windows.note_editor.as_mut() {
                        editor.show();
                    }
                    // No flight (the strip is gone or the flight window could
                    // not be built): hide the card at once.
                    self.hide_note_card();
                }
                logging::trace(format!("note editor for card {id}"));
            }
            Err(error) => logging::error(format!("note editor: {error}")),
        }
    }

    fn on_note_editor_event(&mut self, event: NoteEditorEvent) {
        match event {
            NoteEditorEvent::Resized(paper) => {
                // One write per drag, on the UI thread, after the release: the
                // settings file is small and the drag is over.
                let name = self
                    .sessions
                    .note_editing
                    .as_ref()
                    .and_then(|editing| editing.path.as_ref())
                    .and_then(|path| path.file_name())
                    .and_then(|name| name.to_str())
                    .map(str::to_owned);
                if let Some(name) = name {
                    self.settings.set_note_size(&name, paper);
                }
            }
            NoteEditorEvent::Copy(text) => {
                if let Err(error) = clipboard::write_text(self.hwnd, &text) {
                    logging::error(format!("note clipboard: {error}"));
                }
            }
            NoteEditorEvent::Commit { text, image } => {
                if self.dev.note_demo {
                    // The visual acceptance run needs the window to stay up
                    // while the desktop is captured; a stray click must not
                    // close it.
                    if let Some(editor) = self.windows.note_editor.as_mut() {
                        editor.resume();
                    }
                    return;
                }
                self.sessions.note_return_image = image;
                self.commit_note(text);
            }
        }
    }

    /// Saves the note the editor asked to commit. The window stays open and
    /// disabled until the worker reports.
    fn commit_note(&mut self, text: String) {
        if self.sessions.note_committing || self.sessions.note_tumbling {
            return;
        }
        let Some(editing) = self.sessions.note_editing.as_ref() else {
            return;
        };
        logging::trace(format!(
            "note commit requested: {} bytes, card {:?}",
            text.len(),
            editing.card
        ));
        let text = model::normalize(&text);
        let card = editing.card;
        let path = editing.path.clone();
        let color = editing.color;
        let original = editing.original.clone();
        if text.trim().is_empty() {
            if let Some(card) = card {
                self.archive_note(card);
            } else {
                // An empty new note was never written: throw the paper away
                // instead of making the editor vanish like a crash.
                self.discard_new_note();
            }
            return;
        }
        if text == original {
            // Opening the editor and clicking away without touching the text is
            // not a save, but it must look like one: the note flies back to its
            // card instead of the editor vanishing in place.
            self.fly_note_back_unchanged();
            return;
        }
        self.sessions.note_committing = true;
        let tx = self.events_tx.clone();
        let _ = std::thread::Builder::new()
            .name("pegline-note-save".to_string())
            .spawn(move || {
                let result = match &path {
                    Some(path) => notes::write_atomic(path, &text).map(|()| path.clone()),
                    None => notes::create(&text),
                };
                match result {
                    Ok(saved) => {
                        let image = notes::render(&text, color);
                        if image.is_none() {
                            logging::error(format!("note render: {}", saved.display()));
                        }
                        let _ = tx.send(Event::NoteSaved {
                            card,
                            path: saved,
                            text,
                            image,
                            error: None,
                        });
                    }
                    Err(error) => {
                        let _ = tx.send(Event::NoteSaved {
                            card,
                            path: path.unwrap_or_default(),
                            text,
                            image: None,
                            error: Some(error),
                        });
                    }
                }
                window::wake();
            });
    }

    /// An existing note was opened for editing but the text did not change:
    /// fly the card's own bitmap back to its slot, exactly like a save, then
    /// close the editor. A brand-new note never reaches this path (empty text
    /// is discarded, typed text is saved).
    fn fly_note_back_unchanged(&mut self) {
        let Some(editing) = self.sessions.note_editing.as_ref() else {
            self.close_note_editor();
            return;
        };
        let Some(id) = editing.card else {
            self.close_note_editor();
            return;
        };
        let Some(index) = self.line.items.iter().position(|item| item.id == id) else {
            self.close_note_editor();
            return;
        };
        let (card_image, tilt, card_size) = {
            let item = &self.line.items[index];
            (item.thumb.clone(), item.tilt, item.card_size)
        };
        let image = self.sessions.note_return_image.take().unwrap_or(card_image);
        let from = self.note_editor_card_px();
        let target = self.card_slot_px(index, self.line.items.len(), card_size);
        let dpi = self.screen_dpi();
        if let (Some(from), Some(target), Some(dpi)) = (from, target, dpi) {
            logging::trace(format!("unchanged note {id} flies back to its slot"));
            self.fly_note_back(id, from, image, tilt, target, dpi);
        }
        self.close_note_editor();
    }

    fn on_note_saved(
        &mut self,
        card: Option<CardId>,
        path: PathBuf,
        text: String,
        image: Option<ImageData>,
        error: Option<String>,
    ) {
        self.sessions.note_committing = false;
        let return_image = self.sessions.note_return_image.take();
        if let Some(error) = error {
            logging::error(format!("note save: {error}"));
            // The window keeps the text, flashes its border red and gets its
            // input back (NOTE-EDITOR.md §3.3 step 4, §5.6). No message box:
            // that would lose focus and commit again.
            if let Some(editor) = self.windows.note_editor.as_mut() {
                editor.error();
            }
            return;
        }
        let Some(image) = image else {
            logging::error("note saved but the card bitmap failed to render");
            if let Some(editor) = self.windows.note_editor.as_mut() {
                editor.error();
            }
            return;
        };
        let from = self.note_editor_card_px();
        let dpi = self.screen_dpi();
        let is_new = card.is_none();
        let saved_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned);
        match card {
            Some(id) => {
                let write = std::fs::metadata(&path)
                    .and_then(|meta| meta.modified())
                    .unwrap_or(UNIX_EPOCH);
                let tilt = self
                    .line
                    .items
                    .iter()
                    .find(|item| item.id == id)
                    .map(|item| item.tilt)
                    .unwrap_or(0.0);
                let target = self
                    .line
                    .items
                    .iter()
                    .position(|item| item.id == id)
                    .and_then(|index| {
                        let card_size = self.line.items[index].card_size;
                        self.card_slot_px(index, self.line.items.len(), card_size)
                    });
                if let Some(item) = self.line.items.iter_mut().find(|item| item.id == id) {
                    item.text = Some(text);
                    item.path = path;
                    item.last_write = write;
                }
                self.on_thumbnail(id, image.clone());
                // Fly the committed paper back to its slot; the refreshed card
                // is hidden under the flight until the hand-off.
                let flight_image = return_image.unwrap_or(image);
                if let (Some(from), Some(target), Some(dpi)) = (from, target, dpi) {
                    self.fly_note_back(id, from, flight_image, tilt, target, dpi);
                }
                self.sound(Cue::Tink);
            }
            None => {
                let id = self.alloc_id();
                let color = self
                    .sessions
                    .note_editing
                    .as_ref()
                    .map(|editing| editing.color)
                    .unwrap_or(0);
                let card_size = layout::card_size(notes::CARD_WIDTH, notes::CARD_HEIGHT);
                let index = self.line.items.len();
                let tilt = pseudo_random(id) * 5.0 - 2.5;
                let target = self.flight_target_px(index, index + 1, card_size);
                let mut flying = false;
                let flight_image = return_image.unwrap_or_else(|| image.clone());
                if let (Some(from), Some(target), Some(dpi)) = (from, target, dpi) {
                    self.release_flying_card();
                    if let Some(flight) = self.windows.ensure_flight(&self.events_tx) {
                        let slide_dip = if index == 0 {
                            0.0
                        } else {
                            -layout::SPACING * 0.5
                        };
                        flying = flight.fly_decorated(
                            id,
                            flight_image,
                            from,
                            target,
                            tilt,
                            slide_dip,
                            PinMode::Arrive,
                            dpi,
                        );
                        if flying {
                            self.windows.flight_idle_at = None;
                            self.book.flying_card = Some(id);
                            self.book.flying_rehang = false;
                        }
                    }
                }
                self.add_item(
                    id,
                    path,
                    true,
                    (notes::CARD_WIDTH as u32, notes::CARD_HEIGHT as u32),
                    image,
                    None,
                    true,
                    flying,
                    true,
                    color,
                    Some(text),
                );
                self.sound(Cue::Tink);
            }
        }
        // A brand-new note has no settings entry yet: remember the paper it
        // was written at under its fresh file name (BEHAVIOR §10.1).
        if is_new {
            if let (Some(name), Some(editor)) =
                (saved_name.as_deref(), self.windows.note_editor.as_ref())
            {
                self.settings.set_note_size(name, editor.explicit_paper());
            }
        }
        self.save_pegged();
        self.close_note_editor();
    }

    /// Starts the editor-to-card flight for an existing note. The strip is
    /// brought down first (if it had retracted) and the card waits invisible in
    /// its own slot, so the row never reflows around a note coming home.
    fn fly_note_back(
        &mut self,
        id: CardId,
        from: RectI,
        image: ImageData,
        tilt: f32,
        target: RectI,
        dpi: u32,
    ) -> bool {
        self.release_flying_card();
        self.present();
        self.reveal(false, PEEK_SECONDS);
        self.send_scene(Command::SetRehang { id, flying: true });
        if let Some(flight) = self.windows.ensure_flight(&self.events_tx) {
            if flight.fly_decorated(id, image, from, target, tilt, 0.0, PinMode::Arrive, dpi) {
                self.windows.flight_idle_at = None;
                self.book.flying_card = Some(id);
                self.book.flying_rehang = true;
                if let Some(flight) = self.windows.flight.as_ref() {
                    // `present`/`reveal` may have put the strip above the
                    // flight window; the landing note must stay on top.
                    win32::set_topmost(flight.hwnd());
                }
                return true;
            }
        }
        self.send_scene(Command::SetRehang { id, flying: false });
        false
    }

    /// Stops tracking a card the flight window is carrying and makes that card
    /// visible in the scene again. The command differs for a new arrival (the
    /// row is waiting to glide) and a returning note (the row never moved).
    fn release_flying_card(&mut self) {
        if let Some(previous) = self.book.flying_card.take() {
            let command = if self.book.flying_rehang {
                Command::SetRehang {
                    id: previous,
                    flying: false,
                }
            } else {
                Command::SetFlying {
                    id: previous,
                    flying: false,
                }
            };
            self.send_scene(command);
        }
        self.book.flying_rehang = false;
    }

    /// Empty text on an existing note: move its file into `Notes\Archive` and
    /// take the card down (NOTE-EDITOR.md §3.3, §8.3).
    fn archive_note(&mut self, id: CardId) {
        let Some(item) = self.line.items.iter().find(|item| item.id == id) else {
            return;
        };
        let path = item.path.clone();
        let tx = self.events_tx.clone();
        let _ = std::thread::Builder::new()
            .name("pegline-note-archive".to_string())
            .spawn(move || {
                let result = notes::archive(&path);
                let _ = tx.send(Event::NoteArchived { id, result });
                window::wake();
            });
    }

    fn on_note_archived(&mut self, id: CardId, result: Result<PathBuf, String>) {
        if self.dev.note_storage_id == Some(id) {
            if let Ok(archived) = &result {
                self.dev.note_storage_archived = Some(archived.clone());
            }
        }
        match result {
            Ok(archived) => {
                logging::trace(format!("archived note to {}", archived.display()));
                // The note left the line: its remembered paper size goes with
                // it, so a later note with the same name starts at the default.
                if let Some(name) = archived.file_name().and_then(|name| name.to_str()) {
                    self.settings.remove_note_size(name);
                    self.settings.save();
                }
                self.start_fall(id);
                self.remove_item(id);
                self.close_note_editor();
            }
            Err(error) => {
                logging::error(format!("archive note: {error}"));
                if let Some(tray) = self.windows.tray.as_mut() {
                    tray.show_balloon("Pegline", i18n::s().archive_failed);
                }
                // The file is untouched and the card stays; close the empty
                // editor instead of trapping the user in it.
                self.close_note_editor();
            }
        }
    }

    /// Hides the card of the note being edited while its paper is in the
    /// editor. `SetRehang` hides it in place, so the row keeps the slot the
    /// note will fly back to.
    fn hide_note_card(&mut self) {
        if let Some(id) = self
            .sessions
            .note_editing
            .as_ref()
            .and_then(|editing| editing.card)
        {
            self.send_scene(Command::SetRehang { id, flying: true });
        }
    }

    fn close_note_editor(&mut self) {
        logging::trace("note editor closed");
        self.sessions.note_committing = false;
        self.sessions.note_tumbling = false;
        self.book.note_show_at = None;
        self.sessions.note_return_image = None;
        // The card was hidden for the duration of the edit. A return flight
        // that is already carrying it will reveal it on landing; any other
        // close path (a failed save, a failed archive) must put it back here.
        let card = self
            .sessions
            .note_editing
            .as_ref()
            .and_then(|editing| editing.card);
        if let Some(id) = card {
            let carried = self.book.flying_rehang && self.book.flying_card == Some(id);
            if !carried {
                self.send_scene(Command::SetRehang { id, flying: false });
            }
        }
        self.sessions.note_editing = None;
        if let Some(mut editor) = self.windows.note_editor.take() {
            editor.shutdown();
        }
        self.state.pinned = false;
    }

    /// Throws an empty new note away: the paper shrinks, spins in a random
    /// direction and falls off the screen (NOTE-EDITOR.md §5.5).
    ///
    /// The editor hides at once so the flight is the only thing on screen; the
    /// editor is released when the flight reports it is done.
    fn discard_new_note(&mut self) {
        let Some(editor) = self.windows.note_editor.as_ref() else {
            return;
        };
        let Some(paper) = self.note_editor_paper_px() else {
            self.close_note_editor();
            return;
        };
        let dpi = win32::window_dpi(editor.hwnd()).max(96);
        // The render thread already produced the paper at the editor's live
        // size for the commit; the discard tumble reuses it instead of
        // rasterising 460x360 on the UI thread (BEHAVIOR §10.1).
        let image = self.sessions.note_return_image.take();
        // Random throw: the spin direction and the sideways push are independent
        // coin flips, the upward impulse is always small.
        let seed = (win32::now_seconds() * 1000.0) as u64;
        let spin = if pseudo_random(seed) < 0.5 { -1.0 } else { 1.0 }
            * (140.0 + pseudo_random(seed ^ 0xA5A5_5A5A) * 120.0);
        let sideways = if pseudo_random(seed ^ 0x1234_5678) < 0.5 {
            -1.0
        } else {
            1.0
        } * (60.0 + pseudo_random(seed ^ 0x9E37_79B9) * 120.0);
        let lift = -(60.0 + pseudo_random(seed ^ 0x51ED_2701) * 140.0);
        let scale = dpi as f32 / 96.0;
        let inset = (layout::PHOTO_INSET * scale).round() as i32;
        // The flight draws the paper inside its card frame, so the card starts
        // one inset outside the editor's paper.
        let from = paper.inflate(inset);
        logging::trace(format!(
            "discard new note: paper={paper:?} spin={spin:.0} velocity=({sideways:.0},{lift:.0})"
        ));
        // The editor stays visible until the flight reports its first frame, so
        // the paper never blinks out between the two windows.
        self.release_flying_card();
        let launched = match image {
            Some(image) => self
                .windows
                .ensure_flight(&self.events_tx)
                .is_some_and(|flight| {
                    flight.tumble(
                        TUMBLE_FLIGHT_TOKEN,
                        image,
                        from,
                        spin,
                        (sideways, lift),
                        dpi,
                    )
                }),
            None => false,
        };
        if launched {
            self.windows.flight_idle_at = None;
            self.sessions.note_tumbling = true;
            self.sessions.note_tumble_until = win32::now_seconds() + 2.5;
            self.state.pinned = true;
        } else {
            self.close_note_editor();
        }
    }

    fn on_thumbnail(&mut self, id: CardId, image: ImageData) {
        let Some(item) = self.line.items.iter_mut().find(|item| item.id == id) else {
            return;
        };
        item.thumb = image.clone();
        item.revision = item.revision.wrapping_add(1);
        self.send_scene(Command::SetImage { id, image });
    }

    /// Once a second: a picture or note changed on disk (Paint, Notepad) is
    /// re-decoded so the card and its preview follow (BEHAVIOR §4.5, §7.6).
    fn reload_edited(&mut self, now: f64) {
        if now - self.capture.last_reload < 1.0 {
            return;
        }
        self.capture.last_reload = now;
        if self.dev.demo {
            return;
        }
        let mut jobs: Vec<(CardId, PathBuf, bool, usize, u32)> = Vec::new();
        let thumb_max = self.thumb_max();
        for item in &mut self.line.items {
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
                            window::wake();
                        }
                        None => logging::error(format!("reload {}", path.display())),
                    }
                });
        }
    }

    // ---- annotation editor ------------------------------------------------

    /// Opens the annotation window for a picture that is already hanging. The
    /// saved PNG replaces that card's file and the card reloads in place; a
    /// cancel leaves the file untouched (BEHAVIOR §9.5, §11).
    fn annotate_item(&mut self, id: CardId) {
        if self.windows.editor.is_some() {
            logging::trace("annotate ignored: an editor is already open");
            return;
        }
        let Some(item) = self.line.items.iter().find(|item| item.id == id) else {
            return;
        };
        // Notes keep the old "Edit" verb, which opens the WYSIWYG note editor;
        // only pictures belong in the annotation window.
        if item.note {
            self.edit_item(id);
            return;
        }
        let path = item.path.clone();
        let size = item.size_px;
        self.open_editor(path, Some(id), false, None, size);
    }

    /// `--editor-demo`: opens the editor on a generated picture. Without a card
    /// the saved picture hangs as a new capture.
    fn open_editor_for(
        &mut self,
        path: PathBuf,
        own: bool,
        area: Option<RectI>,
        source_size: (u32, u32),
    ) {
        self.open_editor(path, None, own, area, source_size);
    }

    /// Opens the annotation window for `path`. With a card, the picture grows
    /// out of its slot into the editor's enlarged card; without one it simply
    /// opens in place.
    fn open_editor(
        &mut self,
        path: PathBuf,
        card: Option<CardId>,
        own: bool,
        area: Option<RectI>,
        source_size: (u32, u32),
    ) {
        if self.windows.editor.is_some() {
            return;
        }
        let Some(monitor) = self.screen.or_else(|| self.pointer_screen()) else {
            return;
        };
        let from = card.and_then(|id| self.current_card_px(id));
        let item = card.and_then(|id| self.line.items.iter().find(|item| item.id == id));
        let tilt = item.map(|item| item.tilt).unwrap_or(0.0);
        let angle = card
            .and_then(|id| self.current_card_angle(id))
            .unwrap_or(tilt);
        let fallback = item.map(|item| item.thumb.clone());
        let flight = from.is_some() && fallback.is_some();
        let flight_path = path.clone();
        let tx = self.events_tx.clone();
        let notify = Box::new(move |event: EditorEvent| {
            let _ = tx.send(Event::Editor(event));
            window::wake();
        });
        match Editor::open(&path, source_size, monitor.dpi, monitor, !flight, notify) {
            Ok(editor) => {
                let card_rect = editor.card_px();
                logging::trace(format!("editor opened for {}", path.display()));
                self.windows.editor = Some(editor);
                self.sessions.editing = Some(EditingCapture {
                    path,
                    card,
                    own,
                    area,
                    image: fallback.clone(),
                    tilt,
                    card_rect,
                });
                self.hide_preview();
                if flight {
                    self.present();
                    self.reveal(true, 0.0);
                    let launched = match (from, card_rect, fallback) {
                        (Some(from), Some(to), Some(fallback)) => self
                            .windows
                            .ensure_flight(&self.events_tx)
                            .is_some_and(|flight| {
                                flight.fly_entry(
                                    ANNOTATE_OPEN_TOKEN,
                                    FlightImage::File {
                                        path: flight_path,
                                        max_edge: ANNOTATE_FLIGHT_MAX,
                                        fallback,
                                    },
                                    None,
                                    from,
                                    to,
                                    angle,
                                    PinMode::Full,
                                    monitor.dpi,
                                    false,
                                )
                            }),
                        _ => false,
                    };
                    if launched {
                        self.windows.flight_idle_at = None;
                        self.book.annotate_show_at = Some(win32::now_seconds() + 1.5);
                    } else {
                        if let Some(editor) = self.windows.editor.as_mut() {
                            editor.show();
                        }
                        self.hide_annotated_card();
                    }
                } else if let Some(id) = card {
                    self.send_scene(Command::SetRehang { id, flying: true });
                }
            }
            Err(error) => logging::error(format!("editor: {error}")),
        }
    }

    /// Hides the picture being annotated while the editor is open; the row
    /// keeps its slot (`SetRehang`), so the flight has somewhere to land.
    fn hide_annotated_card(&mut self) {
        if let Some(id) = self
            .sessions
            .editing
            .as_ref()
            .and_then(|editing| editing.card)
        {
            self.send_scene(Command::SetRehang { id, flying: true });
        }
    }

    fn on_editor_saved(
        &mut self,
        png: Vec<u8>,
        preview: ImageData,
        flight_image: ImageData,
        now: f64,
    ) {
        let Some(editing) = self.sessions.editing.take() else {
            logging::error("editor saved without an open picture");
            return;
        };
        let target = editing.path;
        if let Err(error) = wic::write_png_bytes(&target, &png) {
            logging::error(format!("editor save {}: {error}", target.display()));
            self.close_editor();
            return;
        }
        // The watcher may see the same file if the card lives in the screenshots
        // folder; the edit must refresh the existing card, never hang a twin.
        self.capture.recently_saved.push((target.clone(), now));
        if let Some(id) = editing.card {
            // The landing card must already wear the marks: hand the scene the
            // preview, then let the worker upload the card-sized decode.
            self.send_scene(Command::SetImage {
                id,
                image: preview.clone(),
            });
            if let Some(item) = self.line.items.iter_mut().find(|item| item.id == id) {
                item.thumb = preview.clone();
                item.revision = item.revision.wrapping_add(1);
            }
            self.reload_item_picture(id, &target);
            self.fly_picture_back(id, editing.card_rect, editing.tilt, flight_image);
            return;
        }
        self.capture.recent.retain(|recent| recent.path != target);
        let own = editing.own || target.starts_with(win32::inbox_dir());
        self.spawn_capture(target, own, editing.area);
        self.close_editor();
    }

    /// Re-decodes one card's picture after the annotation editor wrote it.
    fn reload_item_picture(&mut self, id: CardId, path: &Path) {
        let thumb_max = self.thumb_max();
        if let Some(item) = self.line.items.iter_mut().find(|item| item.id == id) {
            // The one-second disk poll must not decode the same file again.
            item.last_write = std::fs::metadata(path)
                .and_then(|meta| meta.modified())
                .unwrap_or(UNIX_EPOCH);
        }
        let tx = self.events_tx.clone();
        let path = path.to_path_buf();
        let _ = std::thread::Builder::new()
            .name("pegline-editor-reload".to_string())
            .spawn(move || {
                let _com =
                    match window::ComGuard::init(windows::Win32::System::Com::COINIT_MULTITHREADED)
                    {
                        Ok(guard) => guard,
                        Err(error) => {
                            logging::error(format!("editor reload COM: {error}"));
                            return;
                        }
                    };
                match wic::decode_scaled(&path, thumb_max) {
                    Ok(image) => {
                        let _ = tx.send(Event::ThumbnailReady {
                            id,
                            image,
                            text: None,
                        });
                        window::wake();
                    }
                    Err(error) => logging::error(format!("reload {}: {error}", path.display())),
                }
            });
    }

    fn on_editor_cancelled(&mut self, flight_image: Option<ImageData>) {
        let Some(editing) = self.sessions.editing.take() else {
            return;
        };
        if let Some(id) = editing.card {
            // The file is untouched: the card's own bitmap flies home, at the
            // sharpest size the editor could render.
            let image = flight_image.or(editing.image);
            if let Some(image) = image {
                self.fly_picture_back(id, editing.card_rect, editing.tilt, image);
                return;
            }
            self.send_scene(Command::SetRehang { id, flying: false });
            self.close_editor();
            return;
        }
        // A demo capture may own a file to clean up.
        if editing.own {
            self.capture
                .recent
                .retain(|recent| recent.path != editing.path);
            if let Err(error) = std::fs::remove_file(&editing.path) {
                logging::error(format!(
                    "editor cancel remove {}: {error}",
                    editing.path.display()
                ));
            }
        }
        self.close_editor();
    }

    /// Flies the annotated picture back into `id`'s slot. The card is hidden
    /// under the flight and shown on landing; the editor is hidden as soon as
    /// the flight's first frame is up and released when it finishes.
    fn fly_picture_back(&mut self, id: CardId, from: Option<RectI>, tilt: f32, image: ImageData) {
        let Some(index) = self.line.items.iter().position(|item| item.id == id) else {
            self.close_editor();
            return;
        };
        let card_size = self.line.items[index].card_size;
        let target = self.card_slot_px(index, self.line.items.len(), card_size);
        let from = from.or_else(|| self.current_card_px(id));
        let dpi = self.screen_dpi();
        let (Some(from), Some(target), Some(dpi)) = (from, target, dpi) else {
            // No line or no geometry: just put the card back.
            self.send_scene(Command::SetRehang { id, flying: false });
            self.close_editor();
            return;
        };
        self.release_flying_card();
        self.present();
        self.reveal(false, PEEK_SECONDS);
        self.send_scene(Command::SetRehang { id, flying: true });
        if let Some(flight) = self.windows.ensure_flight(&self.events_tx) {
            if flight.fly_decorated(
                ANNOTATE_CLOSE_TOKEN,
                image,
                from,
                target,
                tilt,
                0.0,
                PinMode::Full,
                dpi,
            ) {
                self.windows.flight_idle_at = None;
                self.book.flying_card = Some(id);
                self.book.flying_rehang = true;
                self.book.annotate_close_at = Some(win32::now_seconds() + 2.0);
                if let Some(flight) = self.windows.flight.as_ref() {
                    // `present`/`reveal` may have put the strip above the
                    // flight window; the landing picture must stay on top.
                    win32::set_topmost(flight.hwnd());
                }
                return;
            }
        }
        self.send_scene(Command::SetRehang { id, flying: false });
        self.close_editor();
    }

    /// Releases the annotation window (hidden or not) and its bookkeeping.
    fn close_editor(&mut self) {
        self.book.annotate_show_at = None;
        self.book.annotate_close_at = None;
        self.sessions.editing = None;
        if let Some(mut editor) = self.windows.editor.take() {
            editor.shutdown();
        }
        // A return flight that is still running owns the card; any other path
        // must make it visible again. A capture flight started by a saved
        // demo picture is not ours: leave its bookkeeping alone.
        if self.book.flying_rehang {
            if let Some(id) = self.book.flying_card.take() {
                self.send_scene(Command::SetRehang { id, flying: false });
            }
            self.book.flying_rehang = false;
        }
        self.state.pinned = false;
    }

    /// The one-time first-run question from the WPF app: may Pegline take over
    /// screenshots? Yes switches clipboard catching on; No leaves it alone.
    fn offer_catching(&mut self) {
        self.settings.mark_catch_offered();
        let title = win32::wide(i18n::s().catch_title);
        let body = win32::wide(i18n::s().catch_body);
        // SAFETY: our own hidden window is the owner; both strings are alive
        // for the call. The box is modal to this thread only.
        let _modal = window::ModalGuard::enter();
        let answer = unsafe {
            MessageBoxW(
                // No owner: the line's hidden window is no-activate, and a box
                // owned by it would not become foreground (keyboard input then
                // never reaches Yes/No). A top-level box activates normally.
                None,
                PCWSTR(body.as_ptr()),
                PCWSTR(title.as_ptr()),
                MB_YESNO | MB_ICONQUESTION | MB_TOPMOST | MB_SETFOREGROUND,
            )
        };
        if answer == IDYES {
            self.settings.set_catch_clipboard(true);
        }
    }

    fn sound(&self, cue: Cue) {
        if !self.settings.sound_off() {
            sound::play(cue);
        }
    }

    fn open_item(&mut self, id: CardId) {
        let Some(item) = self.line.items.iter().find(|item| item.id == id) else {
            return;
        };
        if let Err(error) = win32::shell_open(&item.path) {
            logging::error(format!("open: {error}"));
        }
    }

    /// Long press: Paint for pictures, the WYSIWYG editor for notes. The
    /// right-click "Edit" command uses the same path (NOTE-EDITOR.md §3.2).
    fn edit_item(&mut self, id: CardId) {
        let Some(item) = self.line.items.iter().find(|item| item.id == id) else {
            return;
        };
        if item.note {
            self.open_note_editor(id);
            return;
        }
        let path = item.path.clone();
        let program = "mspaint.exe";
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
        let Some(item) = self.line.items.iter().find(|item| item.id == id) else {
            return;
        };
        // Follow OneDrive/known-folder redirection: USERPROFILE\Desktop is not
        // necessarily the desktop the user sees (this machine redirects it).
        let desktop = win32::desktop_dir().or_else(|| {
            std::env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join("Desktop"))
        });
        let Some(desktop) = desktop else {
            logging::error("save to desktop: no desktop folder");
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
        let Some(item) = self.line.items.iter().find(|item| item.id == id) else {
            return;
        };
        // Notes are never recycled: taking one down moves it to Notes\Archive
        // (NOTE-EDITOR.md §8.3). Pictures keep the WPF discard behaviour.
        if item.note {
            self.archive_note(id);
            return;
        }
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
        let Some(item) = self.line.items.iter().find(|item| item.id == id) else {
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
        let Some(item) = self.line.items.iter().find(|item| item.id == id) else {
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
        self.release_flying_card();
        if let Some(flight) = self.windows.ensure_flight(&self.events_tx) {
            if flight.drop_card(id, thumb, rect, tilt, dpi) {
                self.windows.flight_idle_at = None;
            }
        }
    }

    fn clear_line(&mut self) {
        let ids: Vec<(CardId, bool)> = self
            .line
            .items
            .iter()
            .map(|item| (item.id, item.note))
            .collect();
        for (id, note) in ids {
            if note {
                // "Take everything down" archives notes; pictures just leave
                // the line (NOTE-EDITOR.md §8.3).
                self.archive_note(id);
            } else {
                self.remove_item(id);
            }
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
            .line
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

    // ---- drag ghost -------------------------------------------------------

    fn start_drag(&mut self, id: CardId) {
        logging::trace(format!("drag start card {id}"));
        self.state.pressed = None;
        self.hide_preview();
        self.state.preview_state.reset();
        let Some(item) = self.line.items.iter().find(|item| item.id == id) else {
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
        if let Some(overlay) = self.windows.overlay.as_mut() {
            overlay.set_click_through(true);
        }
        self.send_scene(Command::Dragging(Some(id)));

        // The ghost is a 15 ms follower: `DoDragDrop` pumps messages, so the
        // timer below runs during the modal drag and moves the window.
        if let Some(cursor) = win32::cursor_pos() {
            let dpi = self.screen_dpi().unwrap_or(96);
            if self.windows.ghost.is_none() {
                match Ghost::create() {
                    Ok(ghost) => {
                        self.windows.ghost = Some(ghost);
                        logging::trace("drag ghost created");
                    }
                    Err(error) => logging::error(format!("drag ghost: {error}")),
                }
            }
            if let Some(ghost) = self.windows.ghost.as_mut() {
                ghost.show(&thumb, card_size, cursor, dpi);
                self.windows.ghost_idle_at = None;
            }
        }

        // A note drags as its text file; only pictures have a PNG payload, and
        // only when the file is small enough that reading it here is not a
        // stall. `drag_file` takes ownership so the bytes are not copied again.
        let png = if note { Vec::new() } else { drag_png(&path) };
        let image = ImageData::new(1, 1, vec![0, 0, 0, 0]);
        self.state.dragging = true;
        // `DoDragDrop` pumps this thread's messages while `&mut self` is live;
        // the ghost follows the pointer from its own render thread, so nothing
        // else has to run in here.
        let _modal = window::ModalGuard::enter();
        let effect = match crate::dragdrop::drag_file(self.hwnd, &path, &image, png) {
            Ok(effect) => effect,
            Err(error) => {
                logging::error(format!("drag: {error}"));
                0
            }
        };
        self.state.dragging = false;
        if let Some(ghost) = self.windows.ghost.as_mut() {
            ghost.hide();
            self.windows.ghost_idle_at = Some(win32::now_seconds());
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
        let item = self.line.items.iter().find(|item| item.id == id)?;
        let (own, note) = (item.own, item.note);
        let strings = i18n::s();
        // Notes keep "Edit" (the note editor); pictures get "Annotate", which
        // opens the same window as the old Ctrl+Alt+S capture flow.
        let middle = if note { strings.edit } else { strings.annotate };
        // SAFETY: menus are created and destroyed on this thread.
        unsafe {
            let menu = CreatePopupMenu().ok()?;
            let add = |text: &str, command: usize| {
                let wide = crate::win32::wide(text);
                let _ = AppendMenuW(menu, MF_STRING, command, PCWSTR(wide.as_ptr()));
            };
            add(strings.copy, 1);
            add(strings.open, 2);
            add(middle, 3);
            add(strings.show_in_explorer, 4);
            if own {
                add(strings.save_to_desktop, 5);
            }
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            if own {
                add(
                    if note {
                        strings.archive
                    } else {
                        strings.discard
                    },
                    6,
                );
            } else {
                add(strings.take_down, 7);
                add(strings.move_to_recycle, 8);
            }
            self.windows.menu_open = true;
            let _ = SetForegroundWindow(self.hwnd);
            // The menu runs a nested message loop on this thread while `&mut
            // self` is live; keep every other message away from `App`.
            let _modal = window::ModalGuard::enter();
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
            // Discard the button events produced while the menu was modal; the
            // chosen command already encodes the user's intent.
            if let Ok(mut queue) = mouse_queue().lock() {
                queue.clear();
            }
            self.windows.menu_open = false;
            match chosen.0 as usize {
                1 => Some(CardAction::Copy),
                2 => Some(CardAction::Open),
                3 => Some(if note {
                    CardAction::Edit
                } else {
                    CardAction::Annotate
                }),
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
                // Win+Shift+S and PrtScn deliver the capture through the
                // clipboard, so this is the only place the take-off can start
                // from: decode the big texture here, on the worker, exactly as
                // a file capture does (BEHAVIOR §6.1).
                let flight_image = if area.is_some() {
                    match wic::decode_scaled(&path, crate::capture::FLIGHT_MAX) {
                        Ok(image) => Some(image),
                        Err(error) => {
                            logging::error(format!("clipboard flight image: {error}"));
                            None
                        }
                    }
                } else {
                    None
                };
                let hash = App::image_hash(&image);
                let _ = tx.send(Event::CaptureReady {
                    path,
                    own: true,
                    size_px: size,
                    thumb,
                    area,
                    flight_image,
                    twin: None,
                    clip_hash: Some(hash),
                });
                window::wake();
                let _ = now;
            });
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
        if let Some(overlay) = self.windows.overlay.as_ref() {
            overlay.send(command);
        }
    }

    fn request_quit(&mut self) {
        if self.quit {
            return;
        }
        // A note being edited gets up to two seconds to commit; losing the
        // window without a write would throw the text away (NOTE-EDITOR.md
        // §8.5). The wait is polled from the frame loop rather than slept on
        // here: the UI thread must never block, and the commit result arrives
        // as a normal `Event` on the next tick.
        if self.windows.note_editor.is_some() {
            if let Some(editor) = self.windows.note_editor.as_mut() {
                editor.commit();
            }
            self.state.keep_open = false;
            self.quit_at = Some(win32::now_seconds() + QUIT_GRACE_SECONDS);
            return;
        }
        self.finish_quit();
    }

    /// Advances a pending quit: waits, without blocking, for the note editor's
    /// commit result or for its deadline.
    fn poll_quit(&mut self, now: f64) {
        let expired = self.quit_at.is_some_and(|deadline| now >= deadline);
        if self.windows.note_editor.is_some() && !expired {
            return;
        }
        if self.windows.note_editor.is_some() {
            logging::error("note editor still open at quit; its text was not saved");
        }
        self.finish_quit();
    }

    /// Closes every window and ends the message loop.
    fn finish_quit(&mut self) {
        self.quit = true;
        self.state.keep_open = false;
        if let Some(mut editor) = self.windows.note_editor.take() {
            editor.shutdown();
        }
        if !self.destroyed {
            // SAFETY: our own window.
            unsafe {
                let _ = DestroyWindow(self.hwnd);
            }
        } else {
            window::quit();
        }
    }

    fn shutdown(&mut self) {
        if let Some(hook) = self.windows.hook.take() {
            // SAFETY: the hook was installed by this thread.
            unsafe {
                let _ = UnhookWindowsHookEx(hook);
            }
        }
        // SAFETY: hotkeys are per-window and ours.
        unsafe {
            let _ = UnregisterHotKey(Some(self.hwnd), HOTKEY_TOGGLE);
            let _ = UnregisterHotKey(Some(self.hwnd), HOTKEY_NOTE);
        }
        if let Some(tray) = self.windows.tray.as_mut() {
            tray.remove();
        }
        if let Some(overlay) = self.windows.overlay.as_mut() {
            overlay.shutdown();
        }
        if let Some(flight) = self.windows.flight.as_mut() {
            flight.shutdown();
        }
        if let Some(preview) = self.windows.preview.as_mut() {
            preview.shutdown();
        }
        if let Some(ghost) = self.windows.ghost.as_mut() {
            ghost.shutdown();
        }
        if let Some(mut editor) = self.windows.editor.take() {
            editor.shutdown();
        }
        if let Some(watcher) = self.capture.watcher.as_mut() {
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
    Annotate,
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

    if !pointer.is_null() && window::in_modal_loop() {
        // A menu, message box, OLE drag or the shortcut recorder is pumping
        // messages on this thread, and the frame that entered it still holds
        // `&mut App`. Dereferencing the pointer here would create a second
        // mutable reference to the same object: undefined behaviour.
        //
        // Timers re-arm themselves, so dropping them costs at most one tick.
        // The rest is queued and re-posted when the outermost loop exits.
        if matches!(
            message,
            WM_CLIPBOARDUPDATE
                | WM_HOTKEY
                | WM_APP_QUIT
                | WM_TRAY_CALLBACK
                | WM_APP_WAKE
                | WM_CLOSE
        ) {
            window::defer_message(hwnd, message, wparam, lparam);
            return LRESULT(0);
        }
        // Everything else still has to reach the default procedure, which needs
        // no `App`. A menu's owner must answer `WM_ENTERIDLE`, `WM_ACTIVATEAPP`
        // and `WM_SETCURSOR` while the menu runs; returning 0 for those made
        // USER32 paint the popup's frame, separators and hover pill but no item
        // text, so the card and tray menus came up as blank white boxes.
        return unsafe {
            windows::Win32::UI::WindowsAndMessaging::DefWindowProcW(hwnd, message, wparam, lparam)
        };
    }

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

/// Types `text` into the note editor the way the keyboard does: printable
/// characters as `WM_CHAR`, a newline as a Return key-down. The scripted note
/// demos drive the real input path this way.
fn post_note_text(hwnd: HWND, text: &str) {
    for character in text.chars() {
        // SAFETY: our own editor window; both messages carry their payload in
        // wParam and are handled by its window procedure.
        unsafe {
            if character == '\n' {
                let _ = PostMessageW(
                    Some(hwnd),
                    WM_KEYDOWN,
                    WPARAM(VK_RETURN.0 as usize),
                    LPARAM(0),
                );
            } else {
                let _ = PostMessageW(Some(hwnd), WM_CHAR, WPARAM(character as usize), LPARAM(0));
            }
        }
    }
}

/// The encoded PNG a drag offers under the registered `PNG` clipboard format.
///
/// Empty for anything the UI thread should not read: a note (which drags as its
/// text file) or a picture too large to read without stalling the pointer.
fn drag_png(path: &Path) -> Vec<u8> {
    let Ok(metadata) = std::fs::metadata(path) else {
        return Vec::new();
    };
    if metadata.len() > DRAG_PNG_MAX {
        logging::trace(format!(
            "drag: {} is {} bytes; offering the file only",
            path.display(),
            metadata.len()
        ));
        return Vec::new();
    }
    std::fs::read(path).unwrap_or_default()
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

/// One step of the hover note's physics: a damped spring to the hanging slot,
/// plus a force from the note centre towards the pointer. The pointer force has
/// a static equilibrium offset of `distance * NOTE_PULL_RATIO` (capped), so the
/// note leans towards the mouse while the spring gives it inertia and overshoot.
fn step_hover_physics(
    hover: &mut HoverNote,
    pointer_x: i32,
    pointer_y: i32,
    now: f64,
    scale: f32,
    work: RectI,
) {
    let dt = ((now - hover.last) as f32).clamp(0.0, 0.05);
    hover.last = now;
    if dt <= 0.0 {
        return;
    }
    let omega = std::f32::consts::TAU / NOTE_HOME_RESPONSE;
    let stiffness = omega * omega;
    let damping = 2.0 * NOTE_HOME_DAMPING * omega;
    let bob = ((now - hover.float_start) * std::f64::consts::TAU * NOTE_BOB_HZ).sin() as f32
        * NOTE_BOB_DIP
        * scale;
    let home = (hover.home.0, hover.home.1 + bob);
    let pointer = (pointer_x as f32, pointer_y as f32);
    let range = NOTE_PULL_RANGE_DIP * scale;
    let max_pull = NOTE_PULL_MAX_DIP * scale;

    let steps = ((dt * 240.0).ceil() as usize).max(1);
    let h = dt / steps as f32;
    for _ in 0..steps {
        let mut ax = -stiffness * (hover.pos.0 - home.0) - damping * hover.vel.0;
        let mut ay = -stiffness * (hover.pos.1 - home.1) - damping * hover.vel.1;
        let dx = pointer.0 - hover.pos.0;
        let dy = pointer.1 - hover.pos.1;
        let distance = (dx * dx + dy * dy).sqrt();
        if distance > 0.5 && distance < range {
            // Static equilibrium: stiffness * offset = stiffness * desired.
            let desired = (distance * NOTE_PULL_RATIO).min(max_pull);
            let force = stiffness * desired / distance;
            ax += dx * force;
            ay += dy * force;
        }
        hover.vel.0 += ax * h;
        hover.vel.1 += ay * h;
        hover.pos.0 += hover.vel.0 * h;
        hover.pos.1 += hover.vel.1 * h;
        // The lean is driven by where the pointer is relative to the paper
        // right now, so it relaxes as the paper drifts under the pointer.
        let lean = hover_tilt_target(pointer, hover.pos, scale);
        step_hover_spin(hover, lean, h);
    }

    // Keep the whole preview window (card + shadow padding) inside the work
    // area; stop outward velocity so it does not buzz against the edge.
    let (card_w, card_h) = layout::note_card_size();
    let half_width = (card_w * 0.5 + NOTE_PADDING_DIP) * scale;
    let half_height = (card_h * 0.5 + NOTE_PADDING_DIP) * scale;
    let min_x = work.left as f32 + half_width;
    let max_x = work.right as f32 - half_width;
    let min_y = work.top as f32 + half_height;
    let max_y = work.bottom as f32 - half_height;
    if hover.pos.0 < min_x {
        hover.pos.0 = min_x;
        hover.vel.0 = hover.vel.0.max(0.0);
    } else if hover.pos.0 > max_x {
        hover.pos.0 = max_x;
        hover.vel.0 = hover.vel.0.min(0.0);
    }
    if hover.pos.1 < min_y {
        hover.pos.1 = min_y;
        hover.vel.1 = hover.vel.1.max(0.0);
    } else if hover.pos.1 > max_y {
        hover.pos.1 = max_y;
        hover.vel.1 = hover.vel.1.min(0.0);
    }
    hover.center = hover.pos;
    if now >= hover.trace_at {
        logging::trace(format!(
            "hover physics center={:?} lean={:.2}",
            hover.center, hover.angle
        ));
        hover.trace_at = now + 0.1;
    }
}

/// The lean the pointer asks for, in degrees (positive = clockwise).
///
/// The paper behaves as if a crosshair ran through its centre: no lean at the
/// crossing point or on either axis, clockwise in the top-left and bottom-right
/// quadrants, counter-clockwise in the other two. The direction term is
/// `sin(2φ)` so the quadrants blend instead of snapping on the axes, and the
/// distance ramp keeps the paper upright near the centre and saturates on the
/// diagonals at `NOTE_TILT_RANGE_DIP`.
fn hover_tilt_target(pointer: (f32, f32), center: (f32, f32), scale: f32) -> f32 {
    let dx = pointer.0 - center.0;
    let dy = pointer.1 - center.1;
    let distance_sq = dx * dx + dy * dy;
    if distance_sq < 0.25 {
        return 0.0;
    }
    let diagonal = 2.0 * dx * dy / distance_sq;
    let reach = (distance_sq.sqrt() / (NOTE_TILT_RANGE_DIP * scale)).min(1.0);
    NOTE_TILT_MAX_DEG * diagonal * reach
}

/// One step of the lean's angular spring, degrees.
///
/// It deliberately uses the travel spring's response and damping: the paper
/// lags behind the pointer and overshoots by the same amount as it travels, so
/// the twist and the pull read as one object instead of two effects.
fn step_hover_spin(hover: &mut HoverNote, target: f32, h: f32) {
    let omega = std::f32::consts::TAU / NOTE_TILT_RESPONSE;
    let accel =
        -omega * omega * (hover.angle - target) - 2.0 * NOTE_TILT_DAMPING * omega * hover.spin;
    hover.spin += accel * h;
    hover.angle += hover.spin * h;
}

/// True when `point` is inside a `half_w` x `half_h` card centred on `center`
/// and leaning by `degrees` (positive = clockwise).
///
/// The point is turned back by the lean instead of turning the card, so the
/// test costs two multiplies and needs no geometry.
fn tilted_card_hits(
    center: (f32, f32),
    half_w: f32,
    half_h: f32,
    degrees: f32,
    x: f32,
    y: f32,
) -> bool {
    let (sin, cos) = degrees.to_radians().sin_cos();
    let dx = x - center.0;
    let dy = y - center.1;
    let local_x = dx * cos + dy * sin;
    let local_y = -dx * sin + dy * cos;
    local_x.abs() <= half_w && local_y.abs() <= half_h
}

/// Keeps a hover-note centre far enough inside the work area that the whole
/// preview window (paper plus shadow padding) stays on screen.
fn clamp_center(center: (f32, f32), half_width: f32, half_height: f32, work: RectI) -> (f32, f32) {
    let min_x = work.left as f32 + half_width;
    let max_x = work.right as f32 - half_width;
    let min_y = work.top as f32 + half_height;
    let max_y = work.bottom as f32 - half_height;
    let x = if max_x >= min_x {
        center.0.clamp(min_x, max_x)
    } else {
        (min_x + max_x) * 0.5
    };
    let y = if max_y >= min_y {
        center.1.clamp(min_y, max_y)
    } else {
        (min_y + max_y) * 0.5
    };
    (x, y)
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

    #[test]
    fn annotating_a_card_writes_in_place_and_reloads_it() {
        let _com = window::ComGuard::init(windows::Win32::System::Com::COINIT_MULTITHREADED)
            .expect("COM initialization for the WIC test");
        let dir = std::env::temp_dir().join(format!(
            "pegline-annotate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("card.png");
        let before = ImageData::new(2, 2, vec![255u8; 2 * 2 * 4]);
        let encoded = wic::encode_png_bytes(&before).expect("encode before");
        wic::write_png_bytes(&path, &encoded).expect("write before");

        let mut app = App::new();
        let mut card = twin_card(false, 0.0, before);
        card.path = path.clone();
        app.line.items.push(card);
        app.sessions.editing = Some(EditingCapture {
            path: path.clone(),
            card: Some(1),
            own: false,
            area: None,
            image: None,
            tilt: 0.0,
            card_rect: None,
        });

        let after = ImageData::new(3, 2, vec![255u8; 3 * 2 * 4]);
        let marked = wic::encode_png_bytes(&after).expect("encode after");
        app.on_editor_saved(marked, after.clone(), after, 1.0);

        assert!(app.sessions.editing.is_none(), "the edit must finish");
        assert_eq!(
            app.line.items.len(),
            1,
            "an annotation must not hang a twin"
        );
        assert_eq!(
            wic::pixel_size(&path).expect("saved file"),
            (3, 2),
            "the marked PNG must replace the file in place"
        );
        let event = app
            .events_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("reload event");
        match event {
            Event::ThumbnailReady { id, image, text } => {
                assert_eq!(id, 1);
                assert_eq!((image.width, image.height), (3, 2));
                assert!(text.is_none());
            }
            _ => panic!("the editor save must reload the card thumbnail"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn twin_card(own: bool, captured_at: f64, thumb: ImageData) -> Item {
        Item {
            id: 1,
            path: PathBuf::from("C:\\shots\\a.png"),
            own,
            note: false,
            color: 0,
            text: None,
            size_px: (2, 2),
            card_size: (144.0, 112.0),
            tilt: 0.0,
            thumb,
            missing: 0,
            last_write: UNIX_EPOCH,
            revision: 1,
            captured_at,
        }
    }

    #[test]
    fn a_twin_needs_the_other_owner_a_recent_capture_and_the_same_picture() {
        // 2x2 premultiplied BGRA.
        let pixels = vec![
            10, 20, 30, 255, 40, 50, 60, 255, 70, 80, 90, 255, 100, 110, 120, 255,
        ];
        let thumb = ImageData::new(2, 2, pixels);
        let hash = App::image_hash(&thumb);
        let now = 100.0;

        let card = twin_card(true, now - 1.0, thumb.clone());
        assert!(App::is_twin_candidate(&card, false, (2, 2), hash, now));
        // Same owner is a duplicate, not a twin; the clipboard hash handles it.
        assert!(!App::is_twin_candidate(&card, true, (2, 2), hash, now));
        // A different picture size or picture is another capture.
        assert!(!App::is_twin_candidate(&card, false, (3, 2), hash, now));
        assert!(!App::is_twin_candidate(&card, false, (2, 2), hash ^ 1, now));
        // Cards restored at startup (`0.0`) and cards older than the window
        // are never twins of a fresh capture.
        let restored = twin_card(true, 0.0, thumb.clone());
        assert!(!App::is_twin_candidate(&restored, false, (2, 2), hash, now));
        let old = twin_card(true, now - TWIN_SECONDS - 0.1, thumb);
        assert!(!App::is_twin_candidate(&old, false, (2, 2), hash, now));
    }

    fn hover_at(home: (f32, f32)) -> HoverNote {
        HoverNote {
            phase: HoverPhase::Hover,
            started: 0.0,
            from: home,
            to: home,
            center: home,
            home,
            pos: home,
            vel: (0.0, 0.0),
            angle: 0.0,
            spin: 0.0,
            float_start: 0.0,
            dpi: 96,
            last: 0.0,
            trace_at: 0.0,
        }
    }

    #[test]
    fn hover_note_is_pulled_towards_the_pointer() {
        let work = RectI::new(0, 0, 2000, 1000);
        let mut note = hover_at((500.0, 300.0));
        for step in 1..=120 {
            step_hover_physics(&mut note, 700, 300, step as f64 / 60.0, 1.0, work);
        }
        // 200 px away, capped at 42 px of equilibrium offset; the spring needs
        // to have moved visibly towards the pointer.
        assert!(note.center.0 > 520.0, "center={:?}", note.center);
        assert!(
            (note.center.1 - 300.0).abs() < 12.0,
            "center={:?}",
            note.center
        );
    }

    #[test]
    fn hover_note_returns_home_and_stays_in_its_window() {
        let work = RectI::new(0, 0, 2000, 1000);
        let mut note = hover_at((500.0, 300.0));
        note.pos = (700.0, 500.0);
        note.vel = (500.0, -300.0);
        for step in 1..=300 {
            step_hover_physics(&mut note, 500, 300, step as f64 / 60.0, 1.0, work);
        }
        assert!(
            (note.center.0 - 500.0).abs() < 30.0,
            "center={:?}",
            note.center
        );
        assert!(
            (note.center.1 - 300.0).abs() < 12.0,
            "center={:?}",
            note.center
        );
        // A pointer beyond the pull radius does not drag the note away.
        let mut far = hover_at((500.0, 300.0));
        for step in 1..=120 {
            step_hover_physics(&mut far, 1500, 900, step as f64 / 60.0, 1.0, work);
        }
        assert!(
            (far.center.0 - 500.0).abs() < 12.0,
            "center={:?}",
            far.center
        );
        assert!(
            (far.center.1 - 300.0).abs() < 12.0,
            "center={:?}",
            far.center
        );
        assert!(far.center.0 > 0.0 && far.center.1 > 0.0);
        assert!(far.center.0 < 2000.0 && far.center.1 < 1000.0);
    }

    #[test]
    fn the_lean_follows_the_pointer_quadrants() {
        let center = (500.0, 300.0);
        // 150 px along each diagonal is past the 120 DIP saturation point.
        let tl = hover_tilt_target((350.0, 150.0), center, 1.0);
        let br = hover_tilt_target((650.0, 450.0), center, 1.0);
        let tr = hover_tilt_target((650.0, 150.0), center, 1.0);
        let bl = hover_tilt_target((350.0, 450.0), center, 1.0);
        // Clockwise (positive) in the top-left and bottom-right quadrants, the
        // other way round in the other two, at the full 5° on the diagonals.
        assert!((tl - 5.0).abs() < 0.01, "tl={tl}");
        assert!((br - 5.0).abs() < 0.01, "br={br}");
        assert!((tr + 5.0).abs() < 0.01, "tr={tr}");
        assert!((bl + 5.0).abs() < 0.01, "bl={bl}");
        // The crosshair is a dead line: no lean at the crossing point or on
        // either axis, so the quadrants blend instead of snapping.
        assert_eq!(hover_tilt_target(center, center, 1.0), 0.0);
        assert!(hover_tilt_target((800.0, 300.0), center, 1.0).abs() < 0.001);
        assert!(hover_tilt_target((500.0, 0.0), center, 1.0).abs() < 0.001);
        // The lean grows with the distance from the crossing point.
        let near = hover_tilt_target((520.0, 320.0), center, 1.0);
        let far = hover_tilt_target((560.0, 360.0), center, 1.0);
        assert!(
            near > 0.0 && near < far && far < tl,
            "near={near} far={far}"
        );
        // The range is in DIP, so the same pointer on a 200% screen leans the
        // same paper by the same amount.
        let scaled = hover_tilt_target((800.0, 600.0), center, 2.0);
        assert!((scaled - 5.0).abs() < 0.01, "scaled={scaled}");
    }

    #[test]
    fn the_lean_lags_overshoots_and_settles() {
        let mut note = hover_at((500.0, 300.0));
        let h = 1.0 / 240.0;
        let mut peak: f32 = 0.0;
        for step in 0..480 {
            step_hover_spin(&mut note, 4.0, h);
            peak = peak.max(note.angle);
            if step == 0 {
                // The paper does not snap to the pointer in one tick.
                assert!(note.angle > 0.0 && note.angle < 0.1, "angle={}", note.angle);
            }
        }
        assert!(peak > 4.2, "peak={peak}");
        assert!((note.angle - 4.0).abs() < 0.05, "angle={}", note.angle);
    }

    #[test]
    fn the_physics_leans_the_paper_towards_the_pointer_diagonal() {
        let work = RectI::new(0, 0, 2000, 1000);
        let mut down = hover_at((500.0, 300.0));
        for step in 1..=120 {
            step_hover_physics(&mut down, 800, 600, step as f64 / 60.0, 1.0, work);
        }
        assert!(down.angle > 0.5, "angle={}", down.angle);
        let mut up = hover_at((500.0, 300.0));
        for step in 1..=120 {
            step_hover_physics(&mut up, 800, 0, step as f64 / 60.0, 1.0, work);
        }
        assert!(up.angle < -0.5, "angle={}", up.angle);
    }

    #[test]
    fn a_leaning_paper_is_hit_where_it_is_drawn() {
        // A 136x88 DIP note, upright: the corners are the edges.
        assert!(tilted_card_hits((0.0, 0.0), 68.0, 44.0, 0.0, 67.0, 43.0));
        assert!(!tilted_card_hits((0.0, 0.0), 68.0, 44.0, 0.0, 69.0, 0.0));
        // Leaning clockwise by 5° lifts the left end of the top edge, so the
        // upright rectangle's top-left corner is no longer paper...
        assert!(!tilted_card_hits((0.0, 0.0), 68.0, 44.0, 5.0, -67.0, -43.0));
        // ...while the paper's own rotated corner is.
        assert!(tilted_card_hits((0.0, 0.0), 68.0, 44.0, 5.0, -60.0, -46.0));
    }
}
