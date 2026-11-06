//! The note editor window and the thread that renders it (NOTE-EDITOR.md §5).
//!
//! The UI thread owns the borderless, focusable window, the IME context and the
//! clipboard; the render thread owns the [`EditorState`], every D2D resource and
//! the text layouts. Input crosses as [`Command`]s, results come back through a
//! queue in `Shared` plus a private `WM_APP` message. Committing never destroys
//! the window by itself: the app saves on a worker first and closes the editor
//! only after the file is safely on disk.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ValidateRect;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::Ime::{
    CANDIDATEFORM, CFS_CANDIDATEPOS, CFS_POINT, COMPOSITIONFORM, GCS_COMPSTR, GCS_RESULTSTR, HIMC,
    ImmAssociateContext, ImmCreateContext, ImmDestroyContext, ImmGetCompositionStringW,
    ImmGetContext, ImmReleaseContext, ImmSetCandidateWindow, ImmSetCompositionWindow,
    ImmSetOpenStatus,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, SetFocus, VK_A, VK_B, VK_BACK, VK_CONTROL, VK_DELETE,
    VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_I, VK_LEFT, VK_OEM_3, VK_RIGHT, VK_S, VK_SHIFT, VK_UP,
    VK_V, VK_X, VK_Y, VK_Z,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetClientRect,
    GetWindowLongPtrW, PostMessageW, SW_HIDE, SW_SHOW, SWP_NOACTIVATE, SWP_NOZORDER,
    SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, ShowWindow, WA_INACTIVE, WM_ACTIVATE,
    WM_APP, WM_CAPTURECHANGED, WM_CHAR, WM_CLOSE, WM_DESTROY, WM_DPICHANGED, WM_ERASEBKGND,
    WM_GETDLGCODE, WM_IME_COMPOSITION, WM_IME_STARTCOMPOSITION, WM_KEYDOWN, WM_KILLFOCUS,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT, WM_SETFOCUS, WM_SIZE,
    WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::PCWSTR;

use crate::capture::clipboard;
use crate::logging;
use crate::note_editor::display::PAPER_WIDTH;
use crate::note_editor::render::{
    Command, EditorState, KeyCode, MARGIN_DIP, Step, WINDOW_HEIGHT_DIP, WINDOW_WIDTH_DIP,
};
use crate::types::ImageData;
use crate::win32::{self, Monitor};
use crate::window::{self, ComGuard};

const CLASS_NAME: PCWSTR = windows::core::w!("PeglineNoteEditor");
const WINDOW_TITLE: PCWSTR = windows::core::w!("Pegline note");
/// Private notification: results are waiting in `Shared::events`.
const WM_APP_NOTE_EVENT: u32 = WM_APP + 12;

/// What the app learns from an editor.
#[derive(Clone, Debug)]
pub enum NoteEditorEvent {
    /// Commit was requested; the app must save and then close the editor.
    /// The image is the paper as the editor painted it, cached at commit time
    /// for the flight back to the line.
    Commit {
        text: String,
        image: Option<ImageData>,
    },
    /// Ctrl+C/Ctrl+X produced text for the clipboard.
    Copy(String),
}

/// The callback the app hands in; it runs on the UI thread.
type Notify = Box<dyn Fn(NoteEditorEvent) + Send>;

struct Shared {
    tx: Mutex<Sender<Command>>,
    events: Mutex<VecDeque<NoteEditorEvent>>,
    notify: Mutex<Option<Notify>>,
    /// The first `WM_SETFOCUS` has arrived; focus changes before that are part
    /// of the hand-off and must not commit.
    focus_ready: AtomicBool,
    /// A commit is already in flight; every path shares this flag.
    commit_requested: AtomicBool,
    closed: AtomicBool,
    stopping: AtomicBool,
    ime: AtomicIsize,
    /// Caret in client pixels, published by the render thread for the IME.
    caret_x: AtomicIsize,
    caret_y: AtomicIsize,
    /// Last left-button press, for double-click word selection.
    last_click_ms: AtomicIsize,
    last_click_x: AtomicI32,
    last_click_y: AtomicI32,
}

impl Shared {
    fn send(&self, command: Command) {
        if let Ok(tx) = self.tx.lock() {
            let _ = tx.send(command);
        }
    }

    fn push(&self, event: NoteEditorEvent, hwnd: HWND) {
        if let Ok(mut events) = self.events.lock() {
            events.push_back(event);
        }
        // SAFETY: posting to a window this process owns.
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_APP_NOTE_EVENT, WPARAM(0), LPARAM(0));
        }
    }
}

/// HWND is a raw pointer; the render thread only posts to and draws into the
/// window, never destroys it.
#[derive(Clone, Copy)]
struct SendHwnd(HWND);

// SAFETY: the render thread never destroys or re-parents the window.
unsafe impl Send for SendHwnd {}

struct Start {
    hwnd: SendHwnd,
    width_px: u32,
    height_px: u32,
    dpi: u32,
    color: usize,
    text: String,
}

/// One open note editor, owned by the UI thread.
pub struct NoteEditor {
    hwnd: HWND,
    shared: Arc<Shared>,
    join: Option<JoinHandle<()>>,
    /// The paper bitmap the opening flight carries, rendered once at open.
    flight_image: Option<ImageData>,
}

impl NoteEditor {
    /// Opens the editor on `monitor`, centred in its work area.
    pub fn open(
        text: String,
        color: usize,
        dpi: u32,
        monitor: Monitor,
        notify: Box<dyn Fn(NoteEditorEvent) + Send>,
    ) -> Result<Self, String> {
        Self::open_with(text, color, dpi, monitor, true, notify)
    }

    /// Creates the editor hidden; the app shows it when the flight lands
    /// (NOTE-EDITOR.md §4.6).
    pub fn open_hidden(
        text: String,
        color: usize,
        dpi: u32,
        monitor: Monitor,
        notify: Box<dyn Fn(NoteEditorEvent) + Send>,
    ) -> Result<Self, String> {
        Self::open_with(text, color, dpi, monitor, false, notify)
    }

    fn open_with(
        text: String,
        color: usize,
        dpi: u32,
        monitor: Monitor,
        visible: bool,
        notify: Box<dyn Fn(NoteEditorEvent) + Send>,
    ) -> Result<Self, String> {
        window::register_class(CLASS_NAME, Some(note_wndproc))?;
        let dpi = dpi.max(96);
        let frame = centered_rect(monitor.work, dpi);
        let (width, height) = (frame.width(), frame.height());
        // SAFETY: the module handle is this process and the class is registered.
        let instance = unsafe { GetModuleHandleW(None) }
            .map_err(|error| format!("GetModuleHandleW: {error}"))?;
        // SAFETY: the class is registered, the title is shared, and no parent is
        // requested; the window is positioned before it is shown.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_NOREDIRECTIONBITMAP | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                CLASS_NAME,
                WINDOW_TITLE,
                WS_POPUP,
                frame.left,
                frame.top,
                width,
                height,
                None,
                None,
                Some(HINSTANCE(instance.0)),
                None,
            )
        }
        .map_err(|error| format!("CreateWindowExW(note editor): {error}"))?;

        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            tx: Mutex::new(tx),
            events: Mutex::new(VecDeque::new()),
            notify: Mutex::new(Some(notify)),
            focus_ready: AtomicBool::new(false),
            commit_requested: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            ime: AtomicIsize::new(0),
            caret_x: AtomicIsize::new(-1),
            caret_y: AtomicIsize::new(-1),
            last_click_ms: AtomicIsize::new(0),
            last_click_x: AtomicI32::new(0),
            last_click_y: AtomicI32::new(0),
        });
        // SAFETY: the pointer stays valid while the window exists: `NoteEditor`
        // holds the Arc and destroys the window before dropping it.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, Arc::as_ptr(&shared) as isize) };

        // A plain Win32 window needs an IME context on Windows 11 TSF before
        // Chinese/Japanese will compose in it.
        // SAFETY: our window and a fresh context owned by this thread.
        let ime = unsafe { ImmCreateContext() };
        if !ime.0.is_null() {
            // SAFETY: the HWND is ours; the previous context is not owned here.
            unsafe {
                let _ = ImmAssociateContext(hwnd, ime);
                let _ = ImmSetOpenStatus(ime, true);
            }
            shared.ime.store(ime.0 as isize, Ordering::Release);
        } else {
            logging::error("note editor: ImmCreateContext failed");
        }

        let (client_width, client_height) =
            client_size(hwnd).unwrap_or((width as u32, height as u32));
        let render_dpi = win32::window_dpi(hwnd).max(96);
        let start = Start {
            hwnd: SendHwnd(hwnd),
            width_px: client_width,
            height_px: client_height,
            dpi: render_dpi,
            color,
            text,
        };
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread_shared = Arc::clone(&shared);
        let thread_hwnd = SendHwnd(hwnd);
        let join = match thread::Builder::new()
            .name("pegline-note-editor".to_string())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    render_thread(start, rx, thread_shared, thread_hwnd, ready_tx);
                }));
                if outcome.is_err() {
                    logging::error("panic in the note editor render thread");
                }
            }) {
            Ok(join) => join,
            Err(error) => {
                // SAFETY: the window exists and no other thread uses it.
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                return Err(format!("spawn note editor thread: {error}"));
            }
        };
        match ready_rx.recv() {
            Ok(Ok(flight_image)) => {
                logging::trace(format!(
                    "note editor open: {}x{} px at {} dpi, flight image {}",
                    client_width,
                    client_height,
                    render_dpi,
                    flight_image
                        .as_ref()
                        .map(|image| format!("{}x{}", image.width, image.height))
                        .unwrap_or_else(|| "missing".to_string())
                ));
                let mut editor = Self {
                    hwnd,
                    shared,
                    join: Some(join),
                    flight_image,
                };
                if visible {
                    editor.show();
                }
                Ok(editor)
            }
            Ok(Err(error)) => {
                shared.stopping.store(true, Ordering::Release);
                let _ = join.join();
                // SAFETY: the render thread has stopped.
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                Err(error)
            }
            Err(_) => {
                shared.stopping.store(true, Ordering::Release);
                let _ = join.join();
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                Err("note editor render thread died during setup".to_string())
            }
        }
    }

    /// Shows the window taken focus. Used directly, or by the app when the
    /// flight from the hover note reaches the centre of the screen.
    pub fn show(&mut self) {
        self.shared.commit_requested.store(false, Ordering::Release);
        // SAFETY: the window was created on this thread and is ready to show;
        // taking focus is the point of this window.
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOW);
            let foreground = SetForegroundWindow(self.hwnd);
            let focus = SetFocus(Some(self.hwnd));
            logging::trace(format!(
                "note editor shown: foreground={foreground:?} focus={focus:?}"
            ));
        }
        self.shared.send(Command::Focus(true));
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// Takes the paper bitmap rendered at open for the flight to the centre.
    /// `None` when the render thread could not produce one; the caller falls
    /// back to the card thumbnail.
    pub fn take_flight_image(&mut self) -> Option<ImageData> {
        self.flight_image.take()
    }

    /// Hides the window without destroying it, so the discard animation can
    /// carry the paper away before the editor is released.
    pub fn hide(&mut self) {
        // SAFETY: our own window is alive while `self` owns it.
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    /// True once `WM_DESTROY` has run.
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Acquire)
    }

    /// Re-enables input after a failed save and gives the window focus back.
    pub fn resume(&mut self) {
        self.shared.commit_requested.store(false, Ordering::Release);
        self.shared.send(Command::Resume);
        // SAFETY: our own window is alive while `self` owns it.
        unsafe {
            let _ = SetForegroundWindow(self.hwnd);
            let _ = SetFocus(Some(self.hwnd));
        }
    }

    /// A save failed: resume editing and flash the border red.
    pub fn error(&mut self) {
        self.shared.commit_requested.store(false, Ordering::Release);
        self.shared.send(Command::ErrorFlash);
        // SAFETY: our own window is alive while `self` owns it.
        unsafe {
            let _ = SetForegroundWindow(self.hwnd);
            let _ = SetFocus(Some(self.hwnd));
        }
    }

    /// Asks the render thread to commit. Safe to call more than once.
    pub fn commit(&mut self) {
        if self.shared.commit_requested.swap(true, Ordering::AcqRel) {
            return;
        }
        self.shared.send(Command::Commit);
    }

    /// Stops the render thread and destroys the window. Idempotent.
    pub fn shutdown(&mut self) {
        self.shared.stopping.store(true, Ordering::Release);
        self.shared.send(Command::Stop);
        if let Some(join) = self.join.take() {
            if join.join().is_err() {
                logging::error("note editor render thread panicked");
            }
        }
        if !self.shared.closed.load(Ordering::Acquire) {
            // SAFETY: the render thread has been joined; nothing else uses it.
            unsafe {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }
}

impl Drop for NoteEditor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Runs on the render thread until `Command::Stop` or a disconnect.
fn render_thread(
    start: Start,
    rx: Receiver<Command>,
    shared: Arc<Shared>,
    hwnd: SendHwnd,
    ready: Sender<Result<Option<ImageData>, String>>,
) {
    let _com = match ComGuard::init(windows::Win32::System::Com::COINIT_MULTITHREADED) {
        Ok(guard) => guard,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let mut state = match EditorState::new(
        start.hwnd.0,
        start.width_px,
        start.height_px,
        start.dpi,
        start.color,
        start.text,
    ) {
        Ok(state) => state,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    // The opening flight carries the editor's own paper, not the card
    // thumbnail. Rendering it here keeps the cost off the UI thread and makes
    // it identical to the screen the flight is about to reveal.
    let flight_image = match state.render_flight_image() {
        Ok(image) => Some(image),
        Err(error) => {
            logging::error(format!("note editor flight image: {error}"));
            None
        }
    };
    if ready.send(Ok(flight_image)).is_err() {
        return;
    }
    let mut commit_reported = false;
    loop {
        let now = win32::now_seconds();
        if let Err(error) = state.draw(now) {
            logging::error(format!("note editor draw: {error}"));
        }
        publish_caret(&state, &shared);
        let timeout = state.tick_in(now).unwrap_or(Duration::from_secs(3600));
        let command = match rx.recv_timeout(timeout) {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let resume = matches!(command, Command::Resume | Command::ErrorFlash);
        match state.handle(command, win32::now_seconds()) {
            Step::Stop => break,
            Step::Continue => {
                if resume {
                    commit_reported = false;
                }
            }
            Step::Copy(text) => shared.push(NoteEditorEvent::Copy(text), hwnd.0),
            Step::Commit { text, image } => {
                if !commit_reported {
                    commit_reported = true;
                    shared.push(NoteEditorEvent::Commit { text, image }, hwnd.0);
                }
            }
        }
    }
    logging::trace("note editor render thread stopped");
}

fn publish_caret(state: &EditorState, shared: &Shared) {
    if let Some((x, y, _)) = state.caret_client_px() {
        shared.caret_x.store(x as isize, Ordering::Relaxed);
        shared.caret_y.store(y as isize, Ordering::Relaxed);
    }
}

fn request_commit(shared: &Shared, hwnd: HWND) {
    let _ = hwnd;
    if shared.commit_requested.swap(true, Ordering::AcqRel) {
        return;
    }
    shared.send(Command::Commit);
}

/// Window procedure.
///
/// SAFETY: every Win32 call below touches only the editor window created in
/// [`NoteEditor::open`] and the `Shared` stored in its user data; message
/// pointers are read as the structures the system documents for that message.
unsafe extern "system" fn note_wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_ERASEBKGND => return LRESULT(1),
        WM_PAINT => {
            // SAFETY: clearing our own update region.
            unsafe {
                let _ = ValidateRect(Some(hwnd), None);
            }
            return LRESULT(0);
        }
        WM_GETDLGCODE => return LRESULT(1),
        WM_MOUSEWHEEL => {
            let Some(shared) = shared(hwnd) else {
                return LRESULT(0);
            };
            let delta = ((wparam.0 as i64 >> 16) & 0xFFFF) as i16 as f32 / 120.0;
            shared.send(Command::Scroll(delta * 40.0));
            return LRESULT(0);
        }
        _ => {}
    }

    // SAFETY: `open` stores the live `Shared` pointer before the window is
    // shown; messages that arrive during creation fall through to the default.
    let Some(shared) = shared(hwnd) else {
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    };

    match message {
        WM_ACTIVATE => {
            let active = (wparam.0 as u32) & 0xFFFF;
            if active == WA_INACTIVE {
                logging::trace("note editor WM_ACTIVATE inactive");
                if shared.focus_ready.load(Ordering::Acquire)
                    && !shared.stopping.load(Ordering::Acquire)
                {
                    request_commit(shared, hwnd);
                }
            }
            return LRESULT(0);
        }
        WM_SETFOCUS => {
            logging::trace("note editor WM_SETFOCUS");
            shared.focus_ready.store(true, Ordering::Release);
            shared.send(Command::Focus(true));
            return LRESULT(0);
        }
        WM_KILLFOCUS => {
            logging::trace("note editor WM_KILLFOCUS");
            shared.send(Command::Focus(false));
            if shared.focus_ready.load(Ordering::Acquire)
                && !shared.stopping.load(Ordering::Acquire)
            {
                request_commit(shared, hwnd);
            }
            return LRESULT(0);
        }
        WM_CLOSE => {
            logging::trace("note editor WM_CLOSE");
            request_commit(shared, hwnd);
            return LRESULT(0);
        }
        WM_DESTROY => {
            let ime = shared.ime.swap(0, Ordering::AcqRel);
            if ime != 0 {
                let context = HIMC(ime as *mut c_void);
                // SAFETY: the HWND is still valid during WM_DESTROY and the
                // context was created by us in `open`.
                unsafe {
                    let _ = ImmAssociateContext(hwnd, HIMC(std::ptr::null_mut()));
                    let _ = ImmDestroyContext(context);
                }
            }
            shared.closed.store(true, Ordering::Release);
            return LRESULT(0);
        }
        WM_APP_NOTE_EVENT => {
            deliver_pending(shared);
            return LRESULT(0);
        }
        WM_SIZE => {
            let width = (lparam.0 as usize & 0xFFFF) as u32;
            let height = ((lparam.0 as usize >> 16) & 0xFFFF) as u32;
            // SAFETY: pure query on our own window.
            let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
            shared.send(Command::Resize {
                width_px: width.max(1),
                height_px: height.max(1),
                dpi,
            });
            return LRESULT(0);
        }
        WM_DPICHANGED => {
            // Re-centre on the monitor's work area at the new DPI instead of
            // trusting the suggested rectangle (NOTE-EDITOR.md §5.8).
            // SAFETY: pure query on our own window.
            let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
            let work = win32::window_rect(hwnd)
                .and_then(|rect| {
                    win32::monitor_at(rect.left + rect.width() / 2, rect.top + rect.height() / 2)
                })
                .map(|monitor| monitor.work);
            // SAFETY: moving and resizing our own window.
            unsafe {
                if let Some(work) = work {
                    let frame = centered_rect(work, dpi);
                    let _ = SetWindowPos(
                        hwnd,
                        None,
                        frame.left,
                        frame.top,
                        frame.width(),
                        frame.height(),
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                } else {
                    let suggested = &*(lparam.0 as *const RECT);
                    let _ = SetWindowPos(
                        hwnd,
                        None,
                        suggested.left,
                        suggested.top,
                        suggested.right - suggested.left,
                        suggested.bottom - suggested.top,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
            }
            return LRESULT(0);
        }

        WM_CAPTURECHANGED => {
            shared.send(Command::PointerUp);
            return LRESULT(0);
        }
        WM_LBUTTONDOWN => {
            let (x, y) = mouse_xy(lparam);
            // SAFETY: pure query on our own window.
            let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
            if !paper_contains(x, y, dpi) {
                request_commit(shared, hwnd);
                return LRESULT(0);
            }
            // SAFETY: capturing the mouse for our own window.
            unsafe { SetCapture(hwnd) };
            shared.send(Command::PointerDown { x, y });
            // Double-click selects the word under the caret.
            let now_ms = (win32::now_seconds() * 1000.0) as isize;
            let last = shared.last_click_ms.load(Ordering::Relaxed);
            let dx = (x - shared.last_click_x.load(Ordering::Relaxed)).abs();
            let dy = (y - shared.last_click_y.load(Ordering::Relaxed)).abs();
            if last > 0 && now_ms - last < 500 && dx < 4 && dy < 4 {
                shared.last_click_ms.store(0, Ordering::Relaxed);
                shared.send(Command::SelectWord);
            } else {
                shared.last_click_ms.store(now_ms, Ordering::Relaxed);
                shared.last_click_x.store(x, Ordering::Relaxed);
                shared.last_click_y.store(y, Ordering::Relaxed);
            }
            return LRESULT(0);
        }
        WM_MOUSEMOVE => {
            let (x, y) = mouse_xy(lparam);
            shared.send(Command::PointerMove { x, y });
            return LRESULT(0);
        }
        WM_LBUTTONUP => {
            // SAFETY: balances SetCapture; a no-op without capture.
            unsafe {
                let _ = ReleaseCapture();
            }
            shared.send(Command::PointerUp);
            return LRESULT(0);
        }
        WM_CHAR => {
            forward_char(shared, wparam);
            return LRESULT(0);
        }
        WM_KEYDOWN => {
            if let Some(key) = key_code(wparam.0 as u32) {
                let shift = unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
                let control = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
                if control && key == KeyCode::V {
                    if let Some(text) = clipboard::read_text() {
                        shared.send(Command::Paste(text));
                    }
                    return LRESULT(0);
                }
                if control && key == KeyCode::C {
                    shared.send(Command::Copy);
                    return LRESULT(0);
                }
                if control && key == KeyCode::X {
                    shared.send(Command::Cut);
                    return LRESULT(0);
                }
                shared.send(Command::Key {
                    code: key,
                    shift,
                    control,
                });
            }
            return LRESULT(0);
        }
        WM_IME_STARTCOMPOSITION => {
            position_ime(hwnd, shared);
            return LRESULT(0);
        }
        WM_IME_COMPOSITION => {
            let flags = lparam.0 as u32;
            if flags & GCS_RESULTSTR.0 != 0 {
                if let Some(text) = ime_string(hwnd, GCS_RESULTSTR) {
                    shared.send(Command::CommitComposition(text));
                }
            }
            if flags & GCS_COMPSTR.0 != 0 {
                let text = ime_string(hwnd, GCS_COMPSTR).unwrap_or_default();
                shared.send(Command::Composition(text));
            }
            position_ime(hwnd, shared);
            return LRESULT(0);
        }
        _ => {}
    }
    // SAFETY: default processing for messages the editor does not handle.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn shared(hwnd: HWND) -> Option<&'static Shared> {
    // SAFETY: the pointer was set by `open` and is valid for the window's life;
    // `NoteEditor` keeps the `Arc` alive until after `WM_DESTROY`.
    unsafe {
        let raw = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Shared;
        if raw.is_null() {
            return None;
        }
        Some(&*raw)
    }
}

fn deliver_pending(shared: &Shared) {
    let notify = match shared.notify.lock() {
        Ok(mut slot) => slot.take(),
        Err(_) => None,
    };
    let Some(notify) = notify else {
        return;
    };
    let events: Vec<NoteEditorEvent> = match shared.events.lock() {
        Ok(mut queue) => queue.drain(..).collect(),
        Err(_) => Vec::new(),
    };
    for event in events {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| notify(event)));
        if outcome.is_err() {
            logging::error("panic in the note editor notify callback");
        }
    }
    if let Ok(mut slot) = shared.notify.lock() {
        *slot = Some(notify);
    }
}

fn ime_string(
    hwnd: HWND,
    flag: windows::Win32::UI::Input::Ime::IME_COMPOSITION_STRING,
) -> Option<String> {
    // SAFETY: the context belongs to our window and is released below.
    unsafe {
        let context = ImmGetContext(hwnd);
        if context.0.is_null() {
            return None;
        }
        let length = ImmGetCompositionStringW(context, flag, None, 0);
        if length <= 0 {
            let _ = ImmReleaseContext(hwnd, context);
            return Some(String::new());
        }
        let mut buffer = vec![0u16; length as usize / 2];
        let read = ImmGetCompositionStringW(
            context,
            flag,
            Some(buffer.as_mut_ptr() as *mut c_void),
            length as u32,
        );
        let _ = ImmReleaseContext(hwnd, context);
        if read <= 0 {
            return Some(String::new());
        }
        buffer.truncate(read as usize / 2);
        Some(String::from_utf16_lossy(&buffer))
    }
}

fn position_ime(hwnd: HWND, shared: &Shared) {
    let x = shared.caret_x.load(Ordering::Relaxed) as i32;
    let y = shared.caret_y.load(Ordering::Relaxed) as i32;
    if x < 0 || y < 0 {
        return;
    }
    // SAFETY: our own window; the returned HIMC is released below.
    let context = unsafe { ImmGetContext(hwnd) };
    if context.0.is_null() {
        return;
    }
    let composition = COMPOSITIONFORM {
        dwStyle: CFS_POINT,
        ptCurrentPos: POINT { x, y },
        rcArea: RECT::default(),
    };
    let candidate = CANDIDATEFORM {
        dwIndex: 0,
        dwStyle: CFS_CANDIDATEPOS,
        ptCurrentPos: POINT { x, y },
        rcArea: RECT::default(),
    };
    // SAFETY: both forms are live; the context came from `ImmGetContext`.
    unsafe {
        let _ = ImmSetCompositionWindow(context, &composition);
        let _ = ImmSetCandidateWindow(context, &candidate);
        let _ = ImmReleaseContext(hwnd, context);
    }
}

fn forward_char(shared: &Shared, wparam: WPARAM) {
    let unit = (wparam.0 as u32) & 0xFFFF;
    match unit {
        0x08 => shared.send(Command::Key {
            code: KeyCode::Backspace,
            shift: false,
            control: false,
        }),
        0x09 => shared.send(Command::Key {
            code: KeyCode::Tab,
            shift: false,
            control: false,
        }),
        0x0A | 0x0D => shared.send(Command::Key {
            code: KeyCode::Enter,
            shift: false,
            control: false,
        }),
        _ => {
            if let Some(character) = char::from_u32(unit).filter(|c| !c.is_control()) {
                shared.send(Command::Char(character));
            }
        }
    }
}

fn key_code(key: u32) -> Option<KeyCode> {
    Some(match key {
        k if k == VK_LEFT.0 as u32 => KeyCode::Left,
        k if k == VK_RIGHT.0 as u32 => KeyCode::Right,
        k if k == VK_UP.0 as u32 => KeyCode::Up,
        k if k == VK_DOWN.0 as u32 => KeyCode::Down,
        k if k == VK_HOME.0 as u32 => KeyCode::Home,
        k if k == VK_END.0 as u32 => KeyCode::End,
        k if k == VK_BACK.0 as u32 => KeyCode::Backspace,
        k if k == VK_DELETE.0 as u32 => KeyCode::Delete,
        k if k == VK_ESCAPE.0 as u32 => KeyCode::Escape,
        k if k == VK_A.0 as u32 => KeyCode::A,
        k if k == VK_B.0 as u32 => KeyCode::B,
        k if k == VK_I.0 as u32 => KeyCode::I,
        k if k == VK_S.0 as u32 => KeyCode::S,
        k if k == VK_V.0 as u32 => KeyCode::V,
        k if k == VK_X.0 as u32 => KeyCode::X,
        k if k == VK_Y.0 as u32 => KeyCode::Y,
        k if k == VK_Z.0 as u32 => KeyCode::Z,
        k if k == VK_OEM_3.0 as u32 => KeyCode::Backtick,
        _ => return None,
    })
}

fn mouse_xy(lparam: LPARAM) -> (i32, i32) {
    let packed = lparam.0 as usize;
    (
        (packed & 0xFFFF) as u16 as i16 as i32,
        ((packed >> 16) & 0xFFFF) as u16 as i16 as i32,
    )
}

/// The editor window rectangle for a work area and DPI: 500×400 DIP centred in
/// the work area. Shared by `open` and `WM_DPICHANGED` and unit tested.
fn centered_rect(work: crate::types::RectI, dpi: u32) -> crate::types::RectI {
    let scale = dpi.max(96) as f32 / 96.0;
    let width = (WINDOW_WIDTH_DIP * scale).round().max(1.0) as i32;
    let height = (WINDOW_HEIGHT_DIP * scale).round().max(1.0) as i32;
    let left = work.left + (work.width() - width).max(0) / 2;
    let top = work.top + (work.height() - height).max(0) / 2;
    crate::types::RectI::new(left, top, left + width, top + height)
}

/// Whether a client-pixel point is inside the paper's rounded rectangle.
fn paper_contains(x: i32, y: i32, dpi: u32) -> bool {
    let scale = dpi.max(96) as f32 / 96.0;
    let left = MARGIN_DIP * scale;
    let top = MARGIN_DIP * scale;
    let right = (MARGIN_DIP + PAPER_WIDTH) * scale;
    let bottom = (MARGIN_DIP + crate::note_editor::display::PAPER_HEIGHT) * scale;
    let x = x as f32;
    let y = y as f32;
    if x < left || x >= right || y < top || y >= bottom {
        return false;
    }
    // The 22 DIP corner radius: outside the quarter circle means outside.
    let radius = 22.0 * scale;
    let cx = if x < left + radius {
        left + radius
    } else if x > right - radius {
        right - radius
    } else {
        x
    };
    let cy = if y < top + radius {
        top + radius
    } else if y > bottom - radius {
        bottom - radius
    } else {
        y
    };
    let dx = x - cx;
    let dy = y - cy;
    dx * dx + dy * dy <= radius * radius
}

fn client_size(hwnd: HWND) -> Option<(u32, u32)> {
    let mut rect = RECT::default();
    // SAFETY: `rect` is a live local filled by the call.
    unsafe { GetClientRect(hwnd, &mut rect) }.ok()?;
    Some((
        (rect.right - rect.left).max(1) as u32,
        (rect.bottom - rect.top).max(1) as u32,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_paper_hit_test_excludes_the_shadow_and_the_corners() {
        // 100 %: paper 20..480 x 20..380.
        assert!(paper_contains(250, 200, 96));
        assert!(!paper_contains(10, 200, 96));
        assert!(!paper_contains(21, 21, 96));
        assert!(paper_contains(45, 45, 96));
        // 200 %: everything doubles.
        assert!(paper_contains(500, 400, 192));
        assert!(!paper_contains(30, 400, 192));
    }

    #[test]
    fn keys_map_to_editor_commands() {
        assert_eq!(key_code(VK_LEFT.0 as u32), Some(KeyCode::Left));
        assert_eq!(key_code(VK_END.0 as u32), Some(KeyCode::End));
        assert_eq!(key_code(VK_OEM_3.0 as u32), Some(KeyCode::Backtick));
        assert_eq!(key_code(0x99), None);
    }

    #[test]
    fn the_editor_is_centred_at_each_dpi() {
        let work = crate::types::RectI::new(0, 0, 1920, 1080);
        let frame = centered_rect(work, 96);
        assert_eq!((frame.width(), frame.height()), (500, 400));
        assert_eq!((frame.left, frame.top), (710, 340));

        let frame = centered_rect(work, 192);
        assert_eq!((frame.width(), frame.height()), (1000, 800));
        assert_eq!((frame.left, frame.top), (460, 140));

        // A work area that is not at the origin keeps the same relative centre.
        let work = crate::types::RectI::new(200, 100, 2120, 1180);
        let frame = centered_rect(work, 96);
        assert_eq!((frame.left, frame.top), (910, 440));
    }
}
