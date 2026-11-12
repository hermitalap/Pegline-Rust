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
use windows::Win32::UI::Input::Ime::{
    CANDIDATEFORM, CFS_CANDIDATEPOS, CFS_POINT, COMPOSITIONFORM, GCS_COMPSTR, GCS_RESULTSTR, HIMC,
    ImmAssociateContext, ImmCreateContext, ImmDestroyContext, ImmGetCompositionStringW,
    ImmGetContext, ImmReleaseContext, ImmSetCandidateWindow, ImmSetCompositionWindow,
    ImmSetOpenStatus,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, SetFocus, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
    VK_A, VK_B, VK_BACK, VK_C, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_I,
    VK_LEFT, VK_OEM_3, VK_RETURN, VK_RIGHT, VK_S, VK_SHIFT, VK_TAB, VK_UP, VK_V, VK_X, VK_Y, VK_Z,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetClientRect, GetCursorPos,
    GetWindowLongPtrW, HCURSOR, HTCLIENT, IDC_ARROW, IDC_IBEAM, IDC_SIZENS, IDC_SIZENWSE,
    IDC_SIZEWE, LoadCursorW, MINMAXINFO, PostMessageW, SPI_SETWORKAREA, SW_HIDE, SW_SHOW,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SetCursor, SetForegroundWindow, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, WA_INACTIVE, WM_ACTIVATE, WM_APP, WM_CAPTURECHANGED, WM_CHAR,
    WM_CLOSE, WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_ERASEBKGND, WM_GETDLGCODE,
    WM_GETMINMAXINFO, WM_IME_COMPOSITION, WM_IME_STARTCOMPOSITION, WM_KEYDOWN, WM_KILLFOCUS,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT, WM_SETCURSOR, WM_SETFOCUS,
    WM_SETTINGCHANGE, WM_SIZE, WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP,
};
use windows::core::PCWSTR;

use crate::capture::clipboard;
use crate::logging;
use crate::note_editor::display::{
    DEFAULT_PAPER, Hit, MARGIN_DIP, MAX_PAPER, MIN_PAPER, PaperSize, ResizeZone, hit_test,
};
use crate::note_editor::render::{Command, EditorState, KeyCode, Step, WHEEL_STEP};
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
/// `WM_MOUSELEAVE` lives in `Win32_UI_Controls`, a feature this crate does not
/// enable; the message number is fixed.
const WM_MOUSELEAVE: u32 = 0x02A3;

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
    /// A resize drag ended on a new paper size (whole DIP). The app persists it
    /// against the note's file; nothing is written for a bare click or a drag
    /// that could not move the clamp.
    Resized(PaperSize),
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
    /// The resize band the pointer is over: 0 none, 1 right, 2 bottom,
    /// 3 corner. Deduplicates `Command::Hover`, so only an entry or an exit
    /// repaints.
    hover_kind: AtomicU8,
    /// The UI thread's paper size and live drag. The render thread never reads
    /// it; the window procedure does, on the thread that owns the window.
    resize: Mutex<Resize>,
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

/// One live resize drag.
#[derive(Clone, Copy, Debug)]
struct Drag {
    zone: ResizeZone,
    /// Screen position where the button went down, pixels.
    start_cursor: (i32, i32),
    /// The window rectangle at that moment, screen pixels.
    start_window: crate::types::RectI,
    /// The paper at that moment, DIP.
    start_paper: PaperSize,
    /// The last window size applied, pixels, so a drag that is clamped and
    /// cannot grow any further is not mistaken for one that did.
    last_px: (i32, i32),
    /// The paper actually changed during this drag; a bare click does not.
    moved: bool,
}

/// The UI thread's resize state.
struct Resize {
    /// The size the user chose, DIP. This is what goes to disk.
    explicit: PaperSize,
    /// The size on screen, DIP: `explicit` clamped to the work area.
    effective: PaperSize,
    drag: Option<Drag>,
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
    /// Opens the editor on `monitor` with `paper` (DIP), centred in its work
    /// area unless the growth envelope says otherwise.
    pub fn open(
        text: String,
        color: usize,
        paper: PaperSize,
        dpi: u32,
        monitor: Monitor,
        notify: Box<dyn Fn(NoteEditorEvent) + Send>,
    ) -> Result<Self, String> {
        Self::open_with(text, color, paper, dpi, monitor, true, notify)
    }

    /// Creates the editor hidden; the app shows it when the flight lands
    /// (NOTE-EDITOR.md §4.6).
    pub fn open_hidden(
        text: String,
        color: usize,
        paper: PaperSize,
        dpi: u32,
        monitor: Monitor,
        notify: Box<dyn Fn(NoteEditorEvent) + Send>,
    ) -> Result<Self, String> {
        Self::open_with(text, color, paper, dpi, monitor, false, notify)
    }

    fn open_with(
        text: String,
        color: usize,
        paper: PaperSize,
        dpi: u32,
        monitor: Monitor,
        visible: bool,
        notify: Box<dyn Fn(NoteEditorEvent) + Send>,
    ) -> Result<Self, String> {
        window::register_class(CLASS_NAME, Some(note_wndproc))?;
        let frame = frame_for(monitor.work, paper, dpi);
        if frame.paper != paper {
            logging::trace(format!(
                "note resize: small work area {}x{} at {} dpi, paper {:?} instead of {:?}",
                monitor.work.width(),
                monitor.work.height(),
                dpi.max(96),
                frame.paper,
                paper
            ));
        }
        let hwnd = create_window(frame.rect)?;

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
            hover_kind: AtomicU8::new(0),
            resize: Mutex::new(Resize {
                explicit: paper,
                effective: frame.paper,
                drag: None,
            }),
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
            client_size(hwnd).unwrap_or((frame.rect.width() as u32, frame.rect.height() as u32));
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

    /// The paper size on screen, in DIP: `explicit` clamped to the work area.
    pub fn paper(&self) -> PaperSize {
        resize_state(&self.shared).0
    }

    /// The size the user chose, in DIP, before clamping. This is what a save
    /// writes back to settings.
    pub fn explicit_paper(&self) -> PaperSize {
        match self.shared.resize.lock() {
            Ok(ui) => ui.explicit,
            Err(_) => DEFAULT_PAPER,
        }
    }

    /// True when a screen-pixel point is outside the paper and its resize
    /// bands: the click that commits the note.
    pub fn is_commit_point(&self, x: i32, y: i32) -> bool {
        let dpi = win32::window_dpi(self.hwnd);
        matches!(
            hit_test(x, y, self.paper(), dpi),
            Hit::Margin | Hit::Outside
        )
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

    /// Highlights one resize edge, or clears it: the scripted acceptance run
    /// photographs the highlight without moving a real pointer.
    pub fn hover(&self, zone: Option<ResizeZone>) {
        self.shared.send(Command::Hover(zone));
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
        WM_MOUSELEAVE => return handle_mouse(hwnd, message, LPARAM(-1), shared),
        WM_SIZE | WM_DPICHANGED | WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
            return handle_resize(hwnd, message, wparam, lparam, shared);
        }
        WM_GETMINMAXINFO => return handle_minmax(hwnd, lparam),
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

/// `WM_SETCURSOR`: the I-beam says the paper is editable text, a double arrow
/// says the margin resizes, the arrow says the shadow or the outside commits.
/// A selection drag wins even after it leaves the paper: the capture still
/// belongs to the editor, and the caret is still following the pointer.
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
    let dpi = win32::window_dpi(hwnd);
    let dragging = shared.pointer_down.load(Ordering::Relaxed);
    let (paper, drag) = resize_state(shared);
    let hit = hit_test(point.x, point.y, paper, dpi);
    let cursor = match drag {
        Some(zone) => cursor_for(false, Hit::Resize(zone)),
        None => cursor_for(dragging, hit),
    };
    let previous = set_cursor(cursor);
    // Trace the decision, not the message: `WM_SETCURSOR` arrives at input
    // rate, so only a changed choice is worth a line. 0 unknown, 1 I-beam,
    // 2 arrow, 3 resize.
    let kind = match (cursor.0, hit) {
        _ if cursor.0 == IDC_IBEAM.0 => 1,
        _ if cursor.0 == IDC_ARROW.0 => 2,
        _ => 3,
    };
    if shared.cursor_kind.swap(kind, Ordering::Relaxed) != kind {
        logging::trace(format!(
            "note editor cursor: kind={kind} dragging={dragging} hit={hit:?} client=({}, {}) previous={:?}",
            point.x, point.y, previous.0
        ));
    }
    // Handled: the class cursor is NULL, so nothing else may set one here.
    LRESULT(1)
}

/// The cursor for a hit test, or for a selection drag that owns the capture.
fn cursor_for(dragging: bool, hit: Hit) -> PCWSTR {
    if dragging {
        // A selection drag selects wherever the pointer goes.
        return IDC_IBEAM;
    }
    match hit {
        Hit::Resize(ResizeZone::Right) => IDC_SIZEWE,
        Hit::Resize(ResizeZone::Bottom) => IDC_SIZENS,
        Hit::Resize(ResizeZone::Corner) => IDC_SIZENWSE,
        Hit::Paper => IDC_IBEAM,
        Hit::Margin | Hit::Outside => IDC_ARROW,
    }
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
/// A press on the right or bottom margin starts a resize; a press on the paper
/// starts a selection; anywhere else asks to commit.
fn handle_mouse(hwnd: HWND, message: u32, lparam: LPARAM, shared: &Shared) -> LRESULT {
    match message {
        WM_CAPTURECHANGED => {
            // A finished resize already took its drag out before releasing the
            // capture, so this is either the selection drag or another window
            // stealing the capture from an active resize. Either way the
            // pointer state must not stay stuck down.
            if take_drag(shared).is_some() {
                return LRESULT(0);
            }
            shared.pointer_down.store(false, Ordering::Release);
            shared.send(Command::PointerUp);
        }
        WM_LBUTTONDOWN => {
            let (x, y) = mouse_xy(lparam);
            let dpi = win32::window_dpi(hwnd);
            let (paper, _) = resize_state(shared);
            match hit_test(x, y, paper, dpi) {
                Hit::Resize(zone) => begin_resize(hwnd, zone, paper, shared),
                Hit::Paper => begin_selection(hwnd, x, y, shared),
                Hit::Margin | Hit::Outside => request_commit(shared, hwnd),
            }
            return LRESULT(0);
        }
        WM_MOUSEMOVE => {
            let (x, y) = mouse_xy(lparam);
            let dpi = win32::window_dpi(hwnd);
            let (paper, zone) = resize_state(shared);
            if let Some(zone) = zone {
                if let Some(cursor) = win32::cursor_pos() {
                    drag_resize(hwnd, cursor, dpi, shared);
                }
                // No WM_SETCURSOR arrives while the capture is held.
                set_cursor(cursor_for(false, Hit::Resize(zone)));
                return LRESULT(0);
            }
            if shared.pointer_down.load(Ordering::Relaxed) {
                // No WM_SETCURSOR reaches the editor while the capture is held,
                // so the drag has to keep the I-beam up itself; otherwise the
                // cursor turns into an arrow the moment the pointer leaves the
                // paper even though the selection is still following it.
                set_cursor(IDC_IBEAM);
            } else {
                update_hover(x, y, paper, dpi, shared);
                track_mouse_leave(hwnd);
            }
            shared.send_move(x, y);
        }
        WM_LBUTTONUP => {
            let (x, y) = mouse_xy(lparam);
            let dpi = win32::window_dpi(hwnd);
            let (paper, _) = resize_state(shared);
            if let Some(paper) = finish_resize(hwnd, shared) {
                shared.push(NoteEditorEvent::Resized(paper), hwnd);
                set_cursor(cursor_for(false, hit_test(x, y, paper, dpi)));
                return LRESULT(0);
            }
            if shared.pointer_down.load(Ordering::Relaxed) {
                // The selection drag ends wherever the pointer is, even over a
                // resize band: the band only matters for a fresh press.
                shared.pointer_down.store(false, Ordering::Release);
                // SAFETY: balances SetCapture; a no-op without capture.
                unsafe {
                    let _ = ReleaseCapture();
                }
                shared.send(Command::PointerUp);
                update_hover(x, y, paper, dpi, shared);
                return LRESULT(0);
            }
            // A press on a band that never moved: just put the cursor back.
            set_cursor(cursor_for(false, hit_test(x, y, paper, dpi)));
        }
        WM_MOUSELEAVE => update_hover(-1, -1, DEFAULT_PAPER, 96, shared),
        _ => {}
    }
    LRESULT(0)
}

/// Starts a text selection drag. The double-click test runs here so a second
/// press on the same word selects it instead of placing the caret.
fn begin_selection(hwnd: HWND, x: i32, y: i32, shared: &Shared) {
    // A highlight under the pointer would be misleading while selecting.
    clear_hover(shared);
    // SAFETY: capturing the mouse for our own window.
    unsafe { SetCapture(hwnd) };
    // The drag owns the pointer now: WM_SETCURSOR is suppressed during
    // capture, so this flag is what keeps the I-beam up at the edges.
    shared.pointer_down.store(true, Ordering::Release);
    shared.send(Command::PointerDown { x, y });
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

/// The UI-thread resize state: the paper on screen and the zone being dragged.
/// A poisoned lock degrades to the default paper rather than panicking.
fn resize_state(shared: &Shared) -> (PaperSize, Option<ResizeZone>) {
    match shared.resize.lock() {
        Ok(ui) => (ui.effective, ui.drag.as_ref().map(|drag| drag.zone)),
        Err(_) => (DEFAULT_PAPER, None),
    }
}

fn take_drag(shared: &Shared) -> Option<Drag> {
    shared.resize.lock().ok()?.drag.take()
}

/// Starts a resize drag from the margin. `SetCapture` is enough: the editor has
/// no non-client frame, so there is no modal sizing loop to re-enter.
fn begin_resize(hwnd: HWND, zone: ResizeZone, paper: PaperSize, shared: &Shared) {
    clear_hover(shared);
    let (Some(window), Some(cursor)) = (win32::window_rect(hwnd), win32::cursor_pos()) else {
        return;
    };
    let last_px = (window.width().max(1), window.height().max(1));
    if let Ok(mut ui) = shared.resize.lock() {
        ui.drag = Some(Drag {
            zone,
            start_cursor: cursor,
            start_window: window,
            start_paper: paper,
            last_px,
            moved: false,
        });
    }
    // SAFETY: capturing the mouse for our own window.
    unsafe { SetCapture(hwnd) };
    set_cursor(cursor_for(false, Hit::Resize(zone)));
    logging::trace(format!(
        "note editor resize: zone={} start=({}, {}) paper={}x{}",
        zone_kind(zone),
        cursor.0,
        cursor.1,
        paper.width.round(),
        paper.height.round()
    ));
}

/// One live resize step: track the pointer in screen pixels, clamp to the work
/// area, and resize the window without moving it. The render thread follows
/// through `WM_SIZE`; the paper stays fractional in DIP until the release.
fn drag_resize(hwnd: HWND, cursor: (i32, i32), dpi: u32, shared: &Shared) {
    let Some(work) = win32::monitor_at(cursor.0, cursor.1).map(|monitor| monitor.work) else {
        return;
    };
    let scale = dpi as f32 / 96.0;
    let mut applied = None;
    if let Ok(mut ui) = shared.resize.lock() {
        let Some(drag) = ui.drag.as_mut() else {
            return;
        };
        let paper = resized_paper(drag, cursor, work, dpi);
        let px = window_pixels(paper, scale);
        if px != drag.last_px {
            drag.last_px = px;
            drag.moved = true;
            ui.effective = paper;
            applied = Some(px);
        }
    }
    if let Some((width, height)) = applied {
        // SAFETY: resizing our own top-level window, which is hidden from the
        // taskbar and never activated by this call.
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                None,
                0,
                0,
                width,
                height,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
}

/// Ends a resize drag and returns the whole-DIP paper to persist, or `None`
/// when the pointer never actually changed the size (a click on the margin).
fn finish_resize(hwnd: HWND, shared: &Shared) -> Option<PaperSize> {
    // Take the drag out before releasing the capture: the resulting
    // WM_CAPTURECHANGED must not be mistaken for a stolen selection drag.
    let drag = take_drag(shared)?;
    // SAFETY: balances SetCapture; a no-op without capture.
    unsafe {
        let _ = ReleaseCapture();
    }
    if !drag.moved {
        return None;
    }
    let dpi = win32::window_dpi(hwnd);
    let scale = dpi as f32 / 96.0;
    let paper = match shared.resize.lock() {
        Ok(mut ui) => {
            let paper = ui.effective.rounded();
            ui.explicit = paper;
            ui.effective = paper;
            paper
        }
        Err(_) => return None,
    };
    let (width, height) = window_pixels(paper, scale);
    // SAFETY: the final size of our own window; the move flags keep it in place.
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            width,
            height,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
    logging::trace(format!(
        "note editor resize: paper={}x{}",
        paper.width.round(),
        paper.height.round()
    ));
    Some(paper)
}

/// Sends a hover change only when the zone under the pointer changed; the
/// render thread repaints the 2 DIP strip without touching the paper bitmap.
fn update_hover(x: i32, y: i32, paper: PaperSize, dpi: u32, shared: &Shared) {
    let zone = match hit_test(x, y, paper, dpi) {
        Hit::Resize(zone) => Some(zone),
        _ => None,
    };
    let kind = zone.map(zone_kind).unwrap_or(0);
    if shared.hover_kind.swap(kind, Ordering::Relaxed) != kind {
        shared.send(Command::Hover(zone));
    }
}

fn clear_hover(shared: &Shared) {
    if shared.hover_kind.swap(0, Ordering::Relaxed) != 0 {
        shared.send(Command::Hover(None));
    }
}

/// Asks for `WM_MOUSELEAVE` once, which clears the hover highlight when the
/// pointer leaves the window entirely.
fn track_mouse_leave(hwnd: HWND) {
    let event = TRACKMOUSEEVENT {
        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
        dwFlags: TME_LEAVE,
        hwndTrack: hwnd,
        dwHoverTime: 0,
    };
    // SAFETY: `event` is a live local with the documented size.
    let mut event = event;
    unsafe {
        let _ = TrackMouseEvent(&mut event);
    }
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

/// The client size change, and the DPI or work-area change that re-places the
/// window. `WM_SIZE` is the only path that tells the render thread about a new
/// size; the re-place handler keeps the user's explicit paper and clamps it.
fn handle_resize(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    shared: &Shared,
) -> LRESULT {
    match message {
        WM_SIZE => {
            let width = (lparam.0 as usize & 0xFFFF) as u32;
            let height = ((lparam.0 as usize >> 16) & 0xFFFF) as u32;
            let dpi = win32::window_dpi(hwnd);
            if let Ok(mut ui) = shared.resize.lock() {
                ui.effective = crate::note_editor::render::paper_of_window(width, height, dpi);
            }
            shared.send(Command::Resize {
                width_px: width.max(1),
                height_px: height.max(1),
                dpi,
            });
        }
        WM_DPICHANGED => reflow(hwnd, shared),
        WM_DISPLAYCHANGE => reflow(hwnd, shared),
        // Explorer bumps the work area, the taskbar or the theme; only the
        // work area changes where the editor may live.
        WM_SETTINGCHANGE if wparam.0 as u32 == SPI_SETWORKAREA.0 => reflow(hwnd, shared),
        _ => {}
    }
    LRESULT(0)
}

/// Re-places the window after a DPI or work-area change: the paper the user
/// chose is kept, clamped to the new work area, and the window is placed with
/// the growth envelope so a later resize still stays on screen. The value on
/// disk is not touched until the user drags again.
fn reflow(hwnd: HWND, shared: &Shared) {
    let (paper, dragging) = match shared.resize.lock() {
        Ok(ui) => (ui.explicit, ui.drag.is_some()),
        Err(_) => (DEFAULT_PAPER, false),
    };
    if dragging {
        return;
    }
    let dpi = win32::window_dpi(hwnd);
    let Some(rect) = win32::window_rect(hwnd) else {
        return;
    };
    let Some(monitor) =
        win32::monitor_at(rect.left + rect.width() / 2, rect.top + rect.height() / 2)
    else {
        return;
    };
    let frame = frame_for(monitor.work, paper, dpi);
    if let Ok(mut ui) = shared.resize.lock() {
        ui.effective = frame.paper;
    }
    // SAFETY: re-placing our own window; it stays hidden from the taskbar.
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            None,
            frame.rect.left,
            frame.rect.top,
            frame.rect.width(),
            frame.rect.height(),
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// `WM_GETMINMAXINFO` bounds the window: the smallest paper, the largest paper
/// and the work area. On a work area that cannot hold the minimum window the
/// two bounds meet at its size (BEHAVIOR §10.1, small-work exception).
fn handle_minmax(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    // SAFETY: the system passes a live `MINMAXINFO` for this message.
    let info = unsafe { &mut *(lparam.0 as *mut MINMAXINFO) };
    let dpi = win32::window_dpi(hwnd);
    let scale = dpi as f32 / 96.0;
    let Some(rect) = win32::window_rect(hwnd) else {
        return LRESULT(0);
    };
    let Some(monitor) =
        win32::monitor_at(rect.left + rect.width() / 2, rect.top + rect.height() / 2)
    else {
        return LRESULT(0);
    };
    let (min_width, min_height) = MIN_PAPER.window_size();
    let (max_width, max_height) = MAX_PAPER.window_size();
    let min_px = window_pixels(MIN_PAPER, scale);
    let max_px = window_pixels(MAX_PAPER, scale);
    let _ = (min_width, min_height, max_width, max_height);
    if monitor.work.width() < min_px.0 || monitor.work.height() < min_px.1 {
        info.ptMinTrackSize = POINT {
            x: monitor.work.width(),
            y: monitor.work.height(),
        };
        info.ptMaxTrackSize = info.ptMinTrackSize;
    } else {
        info.ptMinTrackSize = POINT {
            x: min_px.0,
            y: min_px.1,
        };
        info.ptMaxTrackSize = POINT {
            x: max_px.0.min(monitor.work.width()),
            y: max_px.1.min(monitor.work.height()),
        };
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

/// Where the editor window sits and how large the paper inside it really is.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Frame {
    rect: crate::types::RectI,
    /// The requested size clamped so the window fits `work`; smaller than the
    /// requested one only on a work area that cannot hold it.
    paper: PaperSize,
}

/// The editor window for `paper` on `work` at `dpi`.
///
/// The window is the paper plus two shadow margins, centred in the work area
/// unless that would leave no room to grow: a resize drag only moves the right
/// and bottom edges, so the window is placed so the largest paper still fits
/// from its top-left corner (BEHAVIOR §10.1).
fn frame_for(work: crate::types::RectI, paper: PaperSize, dpi: u32) -> Frame {
    let scale = dpi.max(96) as f32 / 96.0;
    let paper = fit_paper(work, paper, scale);
    let (window_width, window_height) = paper.window_size();
    let width = ((window_width * scale).round().max(1.0) as i32).min(work.width().max(1));
    let height = ((window_height * scale).round().max(1.0) as i32).min(work.height().max(1));
    // How far the window may grow before the largest paper leaves the work
    // area; both the paper maximum and the work area bound it.
    let (envelope_width, envelope_height) = MAX_PAPER.window_size();
    let growth_width = ((envelope_width * scale).round() as i32)
        .min(work.width())
        .max(1);
    let growth_height = ((envelope_height * scale).round() as i32)
        .min(work.height())
        .max(1);
    let centered_left = work.left + (work.width() - width).max(0) / 2;
    let centered_top = work.top + (work.height() - height).max(0) / 2;
    // Clamp the centre so the growth envelope stays on screen. When the work
    // area is smaller than the envelope the window sits at its top-left corner.
    let left = centered_left.min(work.right - growth_width).max(work.left);
    let top = centered_top.min(work.bottom - growth_height).max(work.top);
    Frame {
        rect: crate::types::RectI::new(left, top, left + width, top + height),
        paper,
    }
}

/// `paper` clamped so its window fits the work area. On a work area smaller
/// than the smallest allowed window the paper shrinks below `MIN_PAPER`
/// (BEHAVIOR §10.1, the small-work-area exception).
fn fit_paper(work: crate::types::RectI, paper: PaperSize, scale: f32) -> PaperSize {
    let available_width = (work.width() as f32 / scale - MARGIN_DIP * 2.0).max(1.0);
    let available_height = (work.height() as f32 / scale - MARGIN_DIP * 2.0).max(1.0);
    // The user's size first: it can be anything from an old settings file.
    let requested = paper.clamp();
    let width = if available_width <= MIN_PAPER.width {
        available_width
    } else {
        requested.width.min(available_width)
    };
    let height = if available_height <= MIN_PAPER.height {
        available_height
    } else {
        requested.height.min(available_height)
    };
    PaperSize::new(width, height)
}

/// One axis of a drag: the new size, never below the smallest the work area
/// allows and never above the largest the editor allows.
fn clamp_edge(value: f32, available: f32, min: f32, max: f32) -> f32 {
    let upper = available.min(max).max(1.0);
    let lower = min.min(upper);
    value.max(lower).min(upper)
}

/// The paper a drag of `zone` to `cursor` produces. Only the right and bottom
/// edges move; the window's top-left corner is fixed.
fn resized_paper(
    drag: &Drag,
    cursor: (i32, i32),
    work: crate::types::RectI,
    dpi: u32,
) -> PaperSize {
    let scale = dpi.max(96) as f32 / 96.0;
    let dx = (cursor.0 - drag.start_cursor.0) as f32 / scale;
    let dy = (cursor.1 - drag.start_cursor.1) as f32 / scale;
    let mut paper = drag.start_paper;
    if matches!(drag.zone, ResizeZone::Right | ResizeZone::Corner) {
        let available = (work.right - drag.start_window.left) as f32 / scale - MARGIN_DIP * 2.0;
        paper.width = clamp_edge(
            drag.start_paper.width + dx,
            available,
            MIN_PAPER.width,
            MAX_PAPER.width,
        );
    }
    if matches!(drag.zone, ResizeZone::Bottom | ResizeZone::Corner) {
        let available = (work.bottom - drag.start_window.top) as f32 / scale - MARGIN_DIP * 2.0;
        paper.height = clamp_edge(
            drag.start_paper.height + dy,
            available,
            MIN_PAPER.height,
            MAX_PAPER.height,
        );
    }
    paper
}

/// The window size in device pixels for a paper (rounding lives here, so the
/// live drag stays fractional in DIP and only the final `SetWindowPos` snaps).
fn window_pixels(paper: PaperSize, scale: f32) -> (i32, i32) {
    let (width, height) = paper.window_size();
    (
        (width * scale).round().max(1.0) as i32,
        (height * scale).round().max(1.0) as i32,
    )
}

/// The byte the hover trace and `Command::Hover` use for a zone.
fn zone_kind(zone: ResizeZone) -> u8 {
    match zone {
        ResizeZone::Right => 1,
        ResizeZone::Bottom => 2,
        ResizeZone::Corner => 3,
    }
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
    fn the_hit_test_names_every_region_of_the_window() {
        // 100 %: paper 20..480 x 20..380, window 500x400, corner grip 2 DIP.
        let paper = DEFAULT_PAPER;
        assert_eq!(hit_test(250, 200, paper, 96), Hit::Paper);
        assert_eq!(hit_test(10, 200, paper, 96), Hit::Margin);
        // Inside the paper rectangle but outside the 22 DIP rounded corner.
        assert_eq!(hit_test(21, 21, paper, 96), Hit::Margin);
        assert_eq!(hit_test(45, 45, paper, 96), Hit::Paper);
        // The right and bottom bands reach into the paper by the grip.
        assert_eq!(
            hit_test(490, 200, paper, 96),
            Hit::Resize(ResizeZone::Right)
        );
        assert_eq!(
            hit_test(479, 200, paper, 96),
            Hit::Resize(ResizeZone::Right)
        );
        assert_eq!(
            hit_test(250, 390, paper, 96),
            Hit::Resize(ResizeZone::Bottom)
        );
        assert_eq!(
            hit_test(480, 380, paper, 96),
            Hit::Resize(ResizeZone::Corner)
        );
        // The far corner of the shadow is past both bands: a commit area.
        assert_eq!(hit_test(495, 395, paper, 96), Hit::Margin);
        // The top-right and bottom-left shadow corners stay commit areas.
        assert_eq!(hit_test(490, 10, paper, 96), Hit::Margin);
        assert_eq!(hit_test(10, 390, paper, 96), Hit::Margin);
        assert_eq!(hit_test(250, 410, paper, 96), Hit::Outside);
        assert_eq!(hit_test(-1, 200, paper, 96), Hit::Outside);
        // 200 %: the client point and the bands double.
        assert_eq!(hit_test(500, 400, paper, 192), Hit::Paper);
        assert_eq!(hit_test(30, 400, paper, 192), Hit::Margin);
        assert_eq!(
            hit_test(970, 400, paper, 192),
            Hit::Resize(ResizeZone::Right)
        );
    }

    #[test]
    fn the_cursor_marks_the_paper_sticks_through_a_drag_and_shows_the_resize() {
        assert_eq!(cursor_for(false, Hit::Paper).0, IDC_IBEAM.0);
        assert_eq!(cursor_for(false, Hit::Margin).0, IDC_ARROW.0);
        assert_eq!(cursor_for(false, Hit::Outside).0, IDC_ARROW.0);
        assert_eq!(
            cursor_for(false, Hit::Resize(ResizeZone::Right)).0,
            IDC_SIZEWE.0
        );
        assert_eq!(
            cursor_for(false, Hit::Resize(ResizeZone::Bottom)).0,
            IDC_SIZENS.0
        );
        assert_eq!(
            cursor_for(false, Hit::Resize(ResizeZone::Corner)).0,
            IDC_SIZENWSE.0
        );
        // A drag that leaves the paper still selects: the I-beam must stay.
        assert_eq!(cursor_for(true, Hit::Margin).0, IDC_IBEAM.0);
        assert_eq!(cursor_for(true, Hit::Outside).0, IDC_IBEAM.0);
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
    fn the_editor_is_placed_with_the_growth_envelope() {
        let work = crate::types::RectI::new(0, 0, 1920, 1080);
        let frame = frame_for(work, DEFAULT_PAPER, 96);
        assert_eq!((frame.rect.width(), frame.rect.height()), (500, 400));
        assert_eq!((frame.rect.left, frame.rect.top), (710, 340));

        // At 200 % the 860x680 growth envelope is 1720x1360 px, taller than
        // the work area: the window moves to the top-left of that envelope so
        // a resize always has room on the right and below.
        let frame = frame_for(work, DEFAULT_PAPER, 192);
        assert_eq!((frame.rect.width(), frame.rect.height()), (1000, 800));
        assert_eq!((frame.rect.left, frame.rect.top), (200, 0));

        // A work area that is not at the origin keeps the same relative centre.
        let work = crate::types::RectI::new(200, 100, 2120, 1180);
        let frame = frame_for(work, DEFAULT_PAPER, 96);
        assert_eq!((frame.rect.left, frame.rect.top), (910, 440));

        // 1024x600 cannot hold the 860x680 growth envelope, so the window sits
        // at the left edge of the envelope: a resize has room on the right.
        let work = crate::types::RectI::new(0, 0, 1024, 600);
        let frame = frame_for(work, DEFAULT_PAPER, 96);
        assert_eq!((frame.rect.left, frame.rect.top), (164, 0));
        assert_eq!((frame.rect.width(), frame.rect.height()), (500, 400));

        // The largest paper is clamped to what the work area can hold.
        let frame = frame_for(work, MAX_PAPER, 96);
        assert_eq!((frame.paper.width, frame.paper.height), (820.0, 560.0));
        assert_eq!(frame.rect.bottom, 600);
    }

    #[test]
    fn a_tiny_work_area_shrinks_the_paper_below_its_minimum() {
        let work = crate::types::RectI::new(0, 0, 400, 300);
        let frame = frame_for(work, DEFAULT_PAPER, 96);
        assert_eq!((frame.paper.width, frame.paper.height), (360.0, 260.0));
        assert_eq!((frame.rect.width(), frame.rect.height()), (400, 300));
        assert_eq!((frame.rect.left, frame.rect.top), (0, 0));
    }

    #[test]
    fn a_resize_drag_moves_only_the_bottom_right() {
        let work = crate::types::RectI::new(0, 0, 1920, 1080);
        let start_window = crate::types::RectI::new(710, 340, 1210, 740);
        let drag = Drag {
            zone: ResizeZone::Corner,
            start_cursor: (1210, 740),
            start_window,
            start_paper: DEFAULT_PAPER,
            last_px: (500, 400),
            moved: false,
        };
        let paper = resized_paper(&drag, (1310, 790), work, 96);
        assert_eq!((paper.width, paper.height), (560.0, 410.0));

        // The work area bounds the growth, not only MAX_PAPER.
        let paper = resized_paper(&drag, (3000, 3000), work, 96);
        assert_eq!(paper.width, MAX_PAPER.width);
        assert_eq!(paper.height, MAX_PAPER.height);

        // Dragging back past the minimum holds the minimum.
        let paper = resized_paper(&drag, (0, 0), work, 96);
        assert_eq!(
            (paper.width, paper.height),
            (MIN_PAPER.width, MIN_PAPER.height)
        );

        // A right-edge drag leaves the height alone.
        let drag = Drag {
            zone: ResizeZone::Right,
            ..drag
        };
        let paper = resized_paper(&drag, (1310, 790), work, 96);
        assert_eq!((paper.width, paper.height), (560.0, DEFAULT_PAPER.height));
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
