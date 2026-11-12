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
use std::sync::atomic::{
    AtomicBool, AtomicI32, AtomicI64, AtomicIsize, AtomicU8, AtomicU64, Ordering,
};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{ScreenToClient, ValidateRect};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::Ime::{
    CANDIDATEFORM, CFS_CANDIDATEPOS, CFS_POINT, COMPOSITIONFORM, GCS_COMPSTR, GCS_RESULTSTR, HIMC,
    ImmAssociateContext, ImmCreateContext, ImmDestroyContext, ImmGetCompositionStringW,
    ImmGetContext, ImmReleaseContext, ImmSetCandidateWindow, ImmSetCompositionWindow,
    ImmSetOpenStatus,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, SetFocus, VK_A, VK_B, VK_BACK, VK_C, VK_CONTROL,
    VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_I, VK_LEFT, VK_OEM_3, VK_RETURN, VK_RIGHT,
    VK_S, VK_SHIFT, VK_TAB, VK_UP, VK_V, VK_X, VK_Y, VK_Z,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetClientRect, GetCursorPos,
    GetWindowLongPtrW, HCURSOR, HTCLIENT, IDC_ARROW, IDC_IBEAM, LoadCursorW, PostMessageW, SW_HIDE,
    SW_SHOW, SWP_NOACTIVATE, SWP_NOZORDER, SetCursor, SetForegroundWindow, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, WA_INACTIVE, WM_ACTIVATE, WM_APP, WM_CAPTURECHANGED, WM_CHAR,
    WM_CLOSE, WM_DESTROY, WM_DPICHANGED, WM_ERASEBKGND, WM_GETDLGCODE, WM_IME_COMPOSITION,
    WM_IME_STARTCOMPOSITION, WM_KEYDOWN, WM_KILLFOCUS, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_PAINT, WM_SETCURSOR, WM_SETFOCUS, WM_SIZE, WS_EX_NOREDIRECTIONBITMAP,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::PCWSTR;

use crate::capture::clipboard;
use crate::logging;
use crate::note_editor::display::PAPER_WIDTH;
use crate::note_editor::render::{
    Command, EditorState, KeyCode, MARGIN_DIP, Step, WHEEL_STEP, WINDOW_HEIGHT_DIP,
    WINDOW_WIDTH_DIP,
};
use crate::types::ImageData;
use crate::win32::{self, Monitor};
use crate::window::{self, ComGuard};

const CLASS_NAME: PCWSTR = windows::core::w!("PeglineNoteEditor");
const WINDOW_TITLE: PCWSTR = windows::core::w!("Pegline note");
/// Private notification: results are waiting in `Shared::events`.
const WM_APP_NOTE_EVENT: u32 = WM_APP + 12;
/// Most commands one render-loop iteration may consume. Pointer moves are
/// coalesced before they are handled; the cap only bounds how long one frame
/// can spend draining a backlog.
const BATCH_LIMIT: usize = 512;
/// Instrumentation: how many posted `PointerMove` timestamps are remembered for
/// the apply-lag measurement. A power of two, so the slot is a mask.
const MOVE_RING: usize = 16_384;
/// Instrumentation: samples a single drag contributes to its p50/p95 report.
const STAT_SAMPLES: usize = 2048;
/// Instrumentation: the progress line is written at most this often, so a drag
/// cannot bury `trace.log` in lines.
const TRACE_INTERVAL_US: i64 = 250_000;

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
    /// A left-button press started on the paper and still owns the mouse
    /// capture; the I-beam has to stay up when that drag leaves the paper.
    pointer_down: AtomicBool,
    /// The cursor choice the last `WM_SETCURSOR` installed: 0 unknown, 1
    /// I-beam, 2 arrow. Only a change is traced, so the log stays quiet while
    /// the pointer moves inside one region.
    cursor_kind: AtomicU8,
    /// Tracing is on for this process. Read once, so the hot loop never formats
    /// a measurement for a log that is not being written.
    trace_on: bool,
    /// Instrumentation: how many `PointerMove`s have been posted so far.
    moves_posted: AtomicU64,
    /// Instrumentation: post time (monotonic microseconds) of each recent
    /// `PointerMove`, in post order; the slot is the sequence number masked
    /// with `MOVE_RING - 1`.
    move_ring: Box<[AtomicI64]>,
}

impl Shared {
    fn send(&self, command: Command) {
        if let Ok(tx) = self.tx.lock() {
            let _ = tx.send(command);
        }
    }

    /// Sends one pointer move and remembers when it was posted. The render
    /// thread maps the same slot back when it applies the move, which is how
    /// the apply-lag trace is measured; nothing is recorded when tracing is
    /// off, and nothing allocates either way.
    fn send_move(&self, x: i32, y: i32) {
        if self.trace_on {
            let index = self.moves_posted.fetch_add(1, Ordering::Relaxed);
            let slot = (index as usize) & (MOVE_RING - 1);
            self.move_ring[slot].store(now_micros(), Ordering::Relaxed);
        }
        self.send(Command::PointerMove { x, y });
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
        let frame = frame_rect(monitor, dpi);
        let hwnd = create_window(frame)?;

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
            pointer_down: AtomicBool::new(false),
            cursor_kind: AtomicU8::new(0),
            trace_on: logging_trace_on(),
            moves_posted: AtomicU64::new(0),
            move_ring: (0..MOVE_RING)
                .map(|_| AtomicI64::new(0))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        });
        // SAFETY: the pointer stays valid while the window exists: `NoteEditor`
        // holds the Arc and destroys the window before dropping it.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, Arc::as_ptr(&shared) as isize) };

        attach_ime(hwnd, &shared);

        let (client_width, client_height) =
            client_size(hwnd).unwrap_or((frame.width() as u32, frame.height() as u32));
        let render_dpi = win32::window_dpi(hwnd).max(96);
        let start = Start {
            hwnd: SendHwnd(hwnd),
            width_px: client_width,
            height_px: client_height,
            dpi: render_dpi,
            color,
            text,
        };
        let (join, flight_image) = start_render_thread(hwnd, start, rx, &shared)?;
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

/// The editor window rectangle for `monitor` at `dpi`, centred in its work area.
fn frame_rect(monitor: Monitor, dpi: u32) -> crate::types::RectI {
    centered_rect(monitor.work, dpi.max(96))
}

/// Creates the editor window at `frame`, still hidden. The window class must
/// already be registered.
fn create_window(frame: crate::types::RectI) -> Result<HWND, String> {
    let (width, height) = (frame.width(), frame.height());
    // SAFETY: the module handle is this process and the class is registered.
    let instance =
        unsafe { GetModuleHandleW(None) }.map_err(|error| format!("GetModuleHandleW: {error}"))?;
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
    Ok(hwnd)
}

/// A plain Win32 window needs an IME context on Windows 11 TSF before
/// Chinese/Japanese will compose in it.
fn attach_ime(hwnd: HWND, shared: &Shared) {
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
}

/// Starts the render thread and waits for its readiness handshake, which both
/// proves the surface is ready and carries the paper for the opening flight.
/// On failure the window is destroyed so `open` can return the error.
fn start_render_thread(
    hwnd: HWND,
    start: Start,
    rx: Receiver<Command>,
    shared: &Arc<Shared>,
) -> Result<(JoinHandle<()>, Option<ImageData>), String> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let thread_shared = Arc::clone(shared);
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
        Ok(Ok(flight_image)) => Ok((join, flight_image)),
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

/// Mirrors `logging::init`: tracing is on when `PEGLINE_TRACE=1` or a
/// `trace.on` file sits next to the settings. Duplicated only to decide whether
/// the loop formats its measurements at all.
fn logging_trace_on() -> bool {
    std::env::var("PEGLINE_TRACE").as_deref() == Ok("1")
        || win32::app_dir().join("trace.on").exists()
}

/// Monotonic microseconds from the same clock as `win32::now_seconds`. Only the
/// instrumentation uses it.
fn now_micros() -> i64 {
    (win32::now_seconds() * 1_000_000.0) as i64
}

/// The timestamp-ring slot of the posted move with that sequence number.
fn move_slot(sequence: u64) -> usize {
    (sequence as usize) & (MOVE_RING - 1)
}

/// p50, p95 and max of one sample set. Sorts in place, so it only runs when a
/// drag ends, never per frame.
fn summarise(samples: &mut [u32]) -> (u32, u32, u32) {
    if samples.is_empty() {
        return (0, 0, 0);
    }
    samples.sort_unstable();
    let last = samples.len() - 1;
    (samples[last / 2], samples[last * 95 / 100], samples[last])
}

/// Takes the next command that has to be applied from a drained batch. Runs of
/// consecutive `PointerMove`s keep only their last position, because the
/// positions are absolute and the intermediate ones are redundant; every other
/// command is returned in order. Nothing is allocated.
fn next_applied<I: Iterator<Item = Command>>(
    commands: &mut std::iter::Peekable<I>,
) -> Option<Command> {
    loop {
        let command = commands.next()?;
        if matches!(command, Command::PointerMove { .. })
            && matches!(commands.peek(), Some(Command::PointerMove { .. }))
        {
            continue;
        }
        return Some(command);
    }
}

/// Per-drag measurements for the trace. Fixed arrays, so recording a frame
/// never allocates; only the report lines do, and only when tracing is on.
struct DragStats {
    batch: [u32; STAT_SAMPLES],
    lag_us: [u32; STAT_SAMPLES],
    handle_us: [u32; STAT_SAMPLES],
    draw_us: [u32; STAT_SAMPLES],
    len: usize,
    moves: u64,
    coalesced: u64,
    queued_max: u32,
    start_us: i64,
    last_progress_us: i64,
}

impl DragStats {
    fn new(now_us: i64) -> Self {
        Self {
            batch: [0; STAT_SAMPLES],
            lag_us: [0; STAT_SAMPLES],
            handle_us: [0; STAT_SAMPLES],
            draw_us: [0; STAT_SAMPLES],
            len: 0,
            moves: 0,
            coalesced: 0,
            queued_max: 0,
            start_us: now_us,
            last_progress_us: now_us,
        }
    }

    fn record(&mut self, batch: u32, lag_us: u32, handle_us: u32, draw_us: u32) {
        if self.len < STAT_SAMPLES {
            self.batch[self.len] = batch;
            self.lag_us[self.len] = lag_us;
            self.handle_us[self.len] = handle_us;
            self.draw_us[self.len] = draw_us;
            self.len += 1;
        }
    }

    fn note_queue(&mut self, depth: u32) {
        self.queued_max = self.queued_max.max(depth);
    }

    /// True at most every `TRACE_INTERVAL_US`, so the progress line stays
    /// rate-limited while a drag is running.
    fn progress_due(&mut self, now_us: i64) -> bool {
        if now_us - self.last_progress_us < TRACE_INTERVAL_US {
            return false;
        }
        self.last_progress_us = now_us;
        true
    }

    fn finish(&mut self) -> String {
        let (batch_p50, batch_p95, batch_max) = summarise(&mut self.batch[..self.len]);
        let (lag_p50, lag_p95, lag_max) = summarise(&mut self.lag_us[..self.len]);
        let (handle_p50, handle_p95, handle_max) = summarise(&mut self.handle_us[..self.len]);
        let (draw_p50, draw_p95, draw_max) = summarise(&mut self.draw_us[..self.len]);
        let elapsed_ms = ((now_micros() - self.start_us) as f64) / 1000.0;
        format!(
            "note editor drag: moves={} coalesced={} queued_max={} batch_p50={batch_p50} batch_p95={batch_p95} batch_max={batch_max} lag_ms_p50={:.2} lag_ms_p95={:.2} lag_ms_max={:.2} handle_us_p50={handle_p50} handle_us_p95={handle_p95} handle_us_max={handle_max} draw_us_p50={draw_p50} draw_us_p95={draw_p95} draw_us_max={draw_max} drag_ms={elapsed_ms:.1}",
            self.moves,
            self.coalesced,
            self.queued_max,
            lag_p50 as f64 / 1000.0,
            lag_p95 as f64 / 1000.0,
            lag_max as f64 / 1000.0,
        )
    }
}

/// Draws one frame. Returns false when the render loop has to stop: the device
/// is gone for good, and the text has been handed to the app through the normal
/// commit path so the note is still written.
fn draw_frame(state: &mut EditorState, shared: &Shared, hwnd: HWND, lost: &mut u32) -> bool {
    let now = win32::now_seconds();
    if let Err(error) = state.draw(now) {
        logging::error(format!("note editor draw: {error}"));
        // A lost device used to be logged and ignored forever: the window
        // froze on its last frame and the text could never be committed.
        *lost += 1;
        if *lost > 3 {
            // Give up on the GPU, never on the text.
            shared.push(
                NoteEditorEvent::Commit {
                    text: state.text().to_string(),
                    image: None,
                },
                hwnd,
            );
            return false;
        }
        if let Err(error) = state.rebuild_surface(hwnd) {
            logging::error(format!("note editor rebuild: {error}"));
        }
    }
    true
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
    let mut lost = 0u32;
    // Instrumentation, and the batch buffer reused by every drain: once warm,
    // the loop below allocates nothing.
    let trace_on = shared.trace_on;
    let mut applied_moves: u64 = 0;
    let mut stats = if trace_on {
        Some(DragStats::new(now_micros()))
    } else {
        None
    };
    let mut batch: Vec<Command> = Vec::with_capacity(BATCH_LIMIT);
    loop {
        let draw_started = Instant::now();
        if !draw_frame(&mut state, &shared, hwnd.0, &mut lost) {
            break;
        }
        let draw_us = draw_started.elapsed().as_micros().min(u128::from(u32::MAX)) as u32;
        publish_caret(&state, &shared);
        // Wait only when nothing is queued: while the input queue has work, keep
        // draining and drawing instead of sleeping between single commands.
        if batch.is_empty() {
            let now = win32::now_seconds();
            let timeout = state.tick_in(now).unwrap_or(Duration::from_secs(3600));
            match rx.recv_timeout(timeout) {
                Ok(command) => batch.push(command),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        // Drain everything that is ready, up to a bounded batch, so one frame
        // can never spend unbounded time replaying input.
        while batch.len() < BATCH_LIMIT {
            match rx.try_recv() {
                Ok(command) => batch.push(command),
                Err(_) => break,
            }
        }
        let drained = batch.len();
        let moves_in_batch = batch
            .iter()
            .filter(|command| matches!(command, Command::PointerMove { .. }))
            .count() as u32;
        let last_move_stamp = if trace_on && moves_in_batch > 0 {
            shared.move_ring[move_slot(applied_moves + u64::from(moves_in_batch) - 1)]
                .load(Ordering::Relaxed)
        } else {
            0
        };
        let queue_depth = if trace_on {
            let posted = shared.moves_posted.load(Ordering::Relaxed);
            posted
                .saturating_sub(applied_moves)
                .min(u64::from(u32::MAX)) as u32
        } else {
            0
        };
        applied_moves += u64::from(moves_in_batch);
        let handle_started = Instant::now();
        let mut stop = false;
        let mut down_seen = false;
        let mut up_seen = false;
        let mut applied_in_batch = 0u32;
        {
            let mut commands = batch.drain(..).peekable();
            while let Some(command) = next_applied(&mut commands) {
                let resume = matches!(command, Command::Resume | Command::ErrorFlash);
                if matches!(&command, Command::PointerDown { .. }) {
                    down_seen = true;
                }
                if matches!(&command, Command::PointerUp) {
                    up_seen = true;
                }
                if matches!(&command, Command::PointerMove { .. }) {
                    applied_in_batch += 1;
                }
                match state.handle(command, win32::now_seconds()) {
                    Step::Stop => {
                        stop = true;
                        break;
                    }
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
        }
        let handle_us = handle_started
            .elapsed()
            .as_micros()
            .min(u128::from(u32::MAX)) as u32;
        if trace_on {
            let lag_us = if last_move_stamp > 0 {
                (now_micros() - last_move_stamp).clamp(0, i64::from(u32::MAX)) as u32
            } else {
                0
            };
            if down_seen {
                if let Some(entry) = stats.as_mut() {
                    *entry = DragStats::new(now_micros());
                }
            }
            if let Some(entry) = stats.as_mut() {
                entry.note_queue(queue_depth);
                entry.moves += u64::from(moves_in_batch);
                entry.coalesced += u64::from(moves_in_batch.saturating_sub(applied_in_batch));
                entry.record(
                    drained.min(u32::MAX as usize) as u32,
                    lag_us,
                    handle_us,
                    draw_us,
                );
                if entry.progress_due(now_micros()) {
                    logging::trace(format!(
                        "note editor loop: batch={drained} moves={} coalesced={} queue={queue_depth} lag_ms={:.2} handle_us={handle_us} draw_us={draw_us}",
                        entry.moves,
                        entry.coalesced,
                        lag_us as f64 / 1000.0
                    ));
                }
            }
            if up_seen {
                if let Some(mut entry) = stats.take() {
                    let line = entry.finish();
                    logging::trace(line);
                    stats = Some(DragStats::new(now_micros()));
                }
            }
        }
        if stop {
            break;
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
    // A panic may not unwind out of an `extern "system"` function: it aborts the
    // process. Contain it here.
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        note_wndproc_inner(hwnd, message, wparam, lparam)
    })) {
        Ok(result) => result,
        Err(_) => {
            crate::logging::error("panic in note_wndproc");
            LRESULT(0)
        }
    }
}

/// The real handler; plain `unsafe fn` so a panic unwinds into the guard above.
unsafe fn note_wndproc_inner(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Answered while the window is being created, before `open` has stored the
    // `Shared` pointer: these need no editor state.
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
        _ => {}
    }

    // SAFETY: `open` stores the live `Shared` pointer before the window is
    // shown; messages that arrive during creation fall through to the default.
    let Some(shared) = shared(hwnd) else {
        // A wheel message was answered here before the pointer existed, and it
        // still is: it is the one message this window takes without state.
        if message == WM_MOUSEWHEEL {
            return LRESULT(0);
        }
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    };

    match message {
        WM_MOUSEWHEEL => return handle_wheel(wparam, shared),
        WM_SETCURSOR => return handle_cursor(hwnd, wparam, lparam, shared),
        WM_MOUSEMOVE | WM_LBUTTONDOWN | WM_LBUTTONUP | WM_CAPTURECHANGED => {
            return handle_mouse(hwnd, message, lparam, shared);
        }
        WM_KEYDOWN | WM_CHAR => return handle_key(message, wparam, shared),
        WM_SETFOCUS | WM_KILLFOCUS | WM_ACTIVATE => {
            return handle_focus(hwnd, message, wparam, shared);
        }
        WM_SIZE | WM_DPICHANGED => return handle_resize(hwnd, message, lparam, shared),
        WM_IME_STARTCOMPOSITION | WM_IME_COMPOSITION => {
            return handle_ime(hwnd, message, lparam, shared);
        }
        WM_CLOSE | WM_DESTROY => return handle_close(hwnd, message, shared),
        WM_APP_NOTE_EVENT => {
            deliver_pending(shared);
            return LRESULT(0);
        }
        _ => {}
    }
    // SAFETY: default processing for messages the editor does not handle.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

/// `WM_MOUSEWHEEL`: the high word of `wparam` is a signed multiple of
/// `WHEEL_DELTA`; the render thread accumulates the steps.
fn handle_wheel(wparam: WPARAM, shared: &Shared) -> LRESULT {
    let delta = ((wparam.0 as i64 >> 16) & 0xFFFF) as i16 as f32 / 120.0;
    shared.send(Command::Scroll(delta * WHEEL_STEP));
    LRESULT(0)
}

/// `WM_SETCURSOR`: the I-beam says the paper is editable text, the arrow says
/// the shadow around it is not. A selection drag wins even after it leaves the
/// paper: the capture still belongs to the editor, and the caret is still
/// following the pointer.
fn handle_cursor(hwnd: HWND, wparam: WPARAM, lparam: LPARAM, shared: &Shared) -> LRESULT {
    // The low word of `lparam` is the hit-test code the system computed. The
    // class cursor is NULL, so a non-client hit test must reach the default
    // processing that owns the frame.
    if (lparam.0 as u32 & 0xFFFF) != HTCLIENT {
        // SAFETY: default non-client processing for our own window.
        return unsafe { DefWindowProcW(hwnd, WM_SETCURSOR, wparam, lparam) };
    }
    let mut point = POINT::default();
    // SAFETY: `point` is a live local filled by the two queries.
    unsafe {
        if GetCursorPos(&mut point).is_err() || !ScreenToClient(hwnd, &mut point).as_bool() {
            return LRESULT(1);
        }
    }
    // SAFETY: pure query on our own window.
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    let dragging = shared.pointer_down.load(Ordering::Relaxed);
    let over_paper = paper_contains(point.x, point.y, dpi);
    let ibeam = cursor_is_ibeam(dragging, over_paper);
    let previous = set_cursor(if ibeam { IDC_IBEAM } else { IDC_ARROW });
    // Trace the decision, not the message: `WM_SETCURSOR` arrives at input
    // rate, so only a changed choice is worth a line.
    let kind = u8::from(ibeam);
    if shared.cursor_kind.swap(kind, Ordering::Relaxed) != kind {
        logging::trace(format!(
            "note editor cursor: ibeam={ibeam} dragging={dragging} over_paper={over_paper} client=({}, {}) previous={:?}",
            point.x, point.y, previous.0
        ));
    }
    // Handled: the class cursor is NULL, so nothing else may set one here.
    LRESULT(1)
}

/// The cursor the editor shows: the I-beam over the paper, and for the whole
/// time a selection drag owns the capture, wherever the pointer goes.
fn cursor_is_ibeam(dragging: bool, over_paper: bool) -> bool {
    dragging || over_paper
}

/// Installs one of the system cursors on the calling thread. The handles are
/// shared, so `SetCursor` costs nothing and never has to be cached. Returns the
/// handle that was installed before.
fn set_cursor(cursor: PCWSTR) -> HCURSOR {
    // SAFETY: standard cursor identifiers are process-wide shared handles; a
    // failed load leaves the current cursor untouched.
    if let Ok(loaded) = unsafe { LoadCursorW(None, cursor) } {
        unsafe { SetCursor(Some(loaded)) }
    } else {
        HCURSOR::default()
    }
}

/// The pointer: movement, press, release and the capture loss that ends a drag.
/// A press outside the paper asks to commit instead of starting a drag.
fn handle_mouse(hwnd: HWND, message: u32, lparam: LPARAM, shared: &Shared) -> LRESULT {
    match message {
        WM_CAPTURECHANGED => {
            shared.pointer_down.store(false, Ordering::Release);
            shared.send(Command::PointerUp);
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
            // The drag owns the pointer now: WM_SETCURSOR is suppressed during
            // capture, so this flag is what keeps the I-beam up at the edges.
            shared.pointer_down.store(true, Ordering::Release);
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
        }
        WM_MOUSEMOVE => {
            let (x, y) = mouse_xy(lparam);
            if shared.pointer_down.load(Ordering::Relaxed) {
                // No WM_SETCURSOR reaches the editor while the capture is held,
                // so the drag has to keep the I-beam up itself; otherwise the
                // cursor turns into an arrow the moment the pointer leaves the
                // paper even though the selection is still following it.
                set_cursor(IDC_IBEAM);
            }
            shared.send_move(x, y);
        }
        WM_LBUTTONUP => {
            shared.pointer_down.store(false, Ordering::Release);
            // SAFETY: balances SetCapture; a no-op without capture.
            unsafe {
                let _ = ReleaseCapture();
            }
            shared.send(Command::PointerUp);
        }
        _ => {}
    }
    LRESULT(0)
}

/// The keyboard: `WM_KEYDOWN` carries commands and the Ctrl shortcuts for the
/// clipboard, `WM_CHAR` carries printable text.
fn handle_key(message: u32, wparam: WPARAM, shared: &Shared) -> LRESULT {
    match message {
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
        }
        WM_CHAR => forward_char(shared, wparam),
        _ => {}
    }
    LRESULT(0)
}

/// Focus and activation. The first `WM_SETFOCUS` arms committing; losing focus
/// or activation afterwards asks for a commit so the text is never lost.
fn handle_focus(hwnd: HWND, message: u32, wparam: WPARAM, shared: &Shared) -> LRESULT {
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
        }
        WM_SETFOCUS => {
            logging::trace("note editor WM_SETFOCUS");
            shared.focus_ready.store(true, Ordering::Release);
            shared.send(Command::Focus(true));
        }
        WM_KILLFOCUS => {
            logging::trace("note editor WM_KILLFOCUS");
            shared.send(Command::Focus(false));
            if shared.focus_ready.load(Ordering::Acquire)
                && !shared.stopping.load(Ordering::Acquire)
            {
                request_commit(shared, hwnd);
            }
        }
        _ => {}
    }
    LRESULT(0)
}

/// The client size, and a DPI change that re-centres the window.
fn handle_resize(hwnd: HWND, message: u32, lparam: LPARAM, shared: &Shared) -> LRESULT {
    match message {
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
        }
        _ => {}
    }
    LRESULT(0)
}

/// The IME: a composition start re-anchors the candidate window at the caret,
/// and each composition reports its committed and in-progress text.
fn handle_ime(hwnd: HWND, message: u32, lparam: LPARAM, shared: &Shared) -> LRESULT {
    match message {
        WM_IME_STARTCOMPOSITION => position_ime(hwnd, shared),
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
        }
        _ => {}
    }
    LRESULT(0)
}

/// Closing. Both messages ask for a commit first; `WM_DESTROY` also releases
/// the IME context and marks the editor closed.
fn handle_close(hwnd: HWND, message: u32, shared: &Shared) -> LRESULT {
    match message {
        WM_CLOSE => {
            logging::trace("note editor WM_CLOSE");
            request_commit(shared, hwnd);
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
        }
        _ => {}
    }
    LRESULT(0)
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
    if let Some(character) = char::from_u32(unit).filter(|c| !c.is_control()) {
        shared.send(Command::Char(character));
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
        // Tab and Enter must be handled here, not through `WM_CHAR`: the
        // system also turns their key-down into a character message, and
        // handling both deleted two characters per Backspace and would
        // insert two lines per Enter.
        k if k == VK_TAB.0 as u32 => KeyCode::Tab,
        k if k == VK_RETURN.0 as u32 => KeyCode::Enter,
        k if k == VK_ESCAPE.0 as u32 => KeyCode::Escape,
        k if k == VK_A.0 as u32 => KeyCode::A,
        k if k == VK_B.0 as u32 => KeyCode::B,
        k if k == VK_C.0 as u32 => KeyCode::C,
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
    fn the_cursor_marks_the_paper_and_sticks_through_a_drag() {
        // The paper is editable text: I-beam. The shadow is not: arrow.
        assert!(cursor_is_ibeam(false, true));
        assert!(!cursor_is_ibeam(false, false));
        // A drag that leaves the paper still selects: the I-beam must stay.
        assert!(cursor_is_ibeam(true, false));
        assert!(cursor_is_ibeam(true, true));
    }

    #[test]
    fn keys_map_to_editor_commands() {
        assert_eq!(key_code(VK_LEFT.0 as u32), Some(KeyCode::Left));
        assert_eq!(key_code(VK_END.0 as u32), Some(KeyCode::End));
        assert_eq!(key_code(VK_OEM_3.0 as u32), Some(KeyCode::Backtick));
        // Backspace, Tab and Enter are key-down commands only: `WM_CHAR`
        // carries printable text, never these, or every edit would run twice.
        assert_eq!(key_code(VK_BACK.0 as u32), Some(KeyCode::Backspace));
        assert_eq!(key_code(VK_TAB.0 as u32), Some(KeyCode::Tab));
        assert_eq!(key_code(VK_RETURN.0 as u32), Some(KeyCode::Enter));
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

    /// Applies `next_applied` the way the render loop does, including the stop.
    fn take_applied(commands: Vec<Command>) -> Vec<Command> {
        let mut applied = Vec::new();
        let mut commands = commands.into_iter().peekable();
        while let Some(command) = next_applied(&mut commands) {
            let stop = matches!(command, Command::Stop);
            applied.push(command);
            if stop {
                break;
            }
        }
        applied
    }

    #[test]
    fn a_run_of_pointer_moves_keeps_only_its_last_position() {
        let applied = take_applied(vec![
            Command::PointerMove { x: 1, y: 1 },
            Command::PointerMove { x: 2, y: 2 },
            Command::PointerMove { x: 3, y: 3 },
        ]);
        assert_eq!(applied.len(), 1);
        assert!(matches!(applied[0], Command::PointerMove { x: 3, y: 3 }));
    }

    #[test]
    fn commands_around_move_runs_keep_their_order() {
        let applied = take_applied(vec![
            Command::PointerDown { x: 1, y: 1 },
            Command::PointerMove { x: 2, y: 2 },
            Command::PointerMove { x: 3, y: 3 },
            Command::Copy,
            Command::PointerMove { x: 4, y: 4 },
            Command::PointerUp,
            Command::Commit,
        ]);
        assert_eq!(applied.len(), 6);
        assert!(matches!(applied[0], Command::PointerDown { x: 1, y: 1 }));
        assert!(matches!(applied[1], Command::PointerMove { x: 3, y: 3 }));
        assert!(matches!(applied[2], Command::Copy));
        assert!(matches!(applied[3], Command::PointerMove { x: 4, y: 4 }));
        assert!(matches!(applied[4], Command::PointerUp));
        assert!(matches!(applied[5], Command::Commit));
    }

    #[test]
    fn a_batch_without_moves_is_applied_in_order() {
        let applied = take_applied(vec![
            Command::Char('a'),
            Command::Char('b'),
            Command::Commit,
        ]);
        assert_eq!(applied.len(), 3);
        assert!(matches!(applied[0], Command::Char('a')));
        assert!(matches!(applied[1], Command::Char('b')));
        assert!(matches!(applied[2], Command::Commit));
    }

    #[test]
    fn stop_ends_the_batch_and_keeps_everything_before_it() {
        let applied = take_applied(vec![
            Command::PointerMove { x: 1, y: 1 },
            Command::PointerMove { x: 2, y: 2 },
            Command::Stop,
            Command::Char('x'),
            Command::Commit,
        ]);
        assert_eq!(applied.len(), 2);
        assert!(matches!(applied[0], Command::PointerMove { x: 2, y: 2 }));
        assert!(matches!(applied[1], Command::Stop));
    }

    #[test]
    fn an_empty_batch_applies_nothing() {
        assert!(take_applied(Vec::new()).is_empty());
    }
}
