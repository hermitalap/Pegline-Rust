//! The annotation editor window and its render thread (BEHAVIOR §11).
//!
//! The UI thread owns the window, the frame and the input plumbing; a dedicated
//! render thread owns the Direct3D/Direct2D stack, the decoded source picture,
//! the [`Doc`] and every pixel. Input crosses as [`Command`]s; results cross
//! back through a `Mutex` plus a private `WM_APP` message, which the window
//! procedure turns into an [`EditorEvent`] on the UI thread before destroying
//! the window.
//!
//! The editor never touches the source file: the app writes the returned PNG
//! itself with a temporary file and a rename. An open editor sleeps in `recv`
//! whenever nothing is dirty, so it costs no CPU between gestures.

mod paint;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_SIZE_F, D2D_SIZE_U, D2D1_COLOR_F, D2D1_FIGURE_BEGIN_FILLED, D2D1_FIGURE_END_CLOSED,
    D2D1_FIGURE_END_OPEN,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ARC_SEGMENT, D2D1_ARC_SIZE_SMALL, D2D1_BITMAP_PROPERTIES1, D2D1_CAP_STYLE_ROUND,
    D2D1_DASH_STYLE, D2D1_DASH_STYLE_CUSTOM, D2D1_DASH_STYLE_SOLID, D2D1_LINE_JOIN_ROUND,
    D2D1_ROUNDED_RECT, D2D1_STROKE_STYLE_PROPERTIES, D2D1_SWEEP_DIRECTION_CLOCKWISE,
    D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, ID2D1Bitmap,
    ID2D1Bitmap1, ID2D1Device, ID2D1DeviceContext, ID2D1Factory, ID2D1Factory1, ID2D1Geometry,
    ID2D1RenderTarget, ID2D1SolidColorBrush, ID2D1StrokeStyle,
};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext};
use windows::Win32::Graphics::DirectComposition::{
    IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FONT_WEIGHT_MEDIUM, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_HIT_TEST_METRICS,
    DWRITE_TEXT_RANGE, IDWriteFactory, IDWriteTextFormat, IDWriteTextLayout,
};
use windows::Win32::Graphics::Dxgi::{IDXGIDevice, IDXGISwapChain1, IDXGISwapChain3};
use windows::Win32::Graphics::Gdi::{ScreenToClient, ValidateRect};
use windows::Win32::Graphics::Imaging::{CLSID_WICImagingFactory2, IWICImagingFactory};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance};
use windows::Win32::UI::Input::Ime::{
    CPS_CANCEL, GCS_COMPSTR, GCS_CURSORPOS, GCS_RESULTSTR, ISC_SHOWUICOMPOSITIONWINDOW,
    ImmGetCompositionStringW, ImmGetContext, ImmNotifyIME, ImmReleaseContext, NI_COMPOSITIONSTR,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, SetFocus, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
    VK_CONTROL, VK_ESCAPE, VK_S, VK_Y, VK_Z,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetClientRect, GetCursorPos, GetWindowLongPtrW,
    HTCLIENT, IDC_ARROW, IDC_CROSS, IDC_HAND, IDC_IBEAM, LoadCursorW, PostMessageW, SW_HIDE,
    SW_SHOW, SWP_NOACTIVATE, SWP_NOZORDER, SetCursor, SetForegroundWindow, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, WM_ACTIVATE, WM_APP, WM_CAPTURECHANGED, WM_CHAR, WM_CLOSE,
    WM_DESTROY, WM_DPICHANGED, WM_ERASEBKGND, WM_IME_CHAR, WM_IME_COMPOSITION,
    WM_IME_ENDCOMPOSITION, WM_IME_SETCONTEXT, WM_IME_STARTCOMPOSITION, WM_KEYDOWN, WM_KILLFOCUS,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_PAINT, WM_SETCURSOR, WM_SETFOCUS, WM_SIZE,
    WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::{Interface, PCWSTR, w};
use windows_numerics::{Matrix3x2, Vector2};

use crate::capture::wic;
use crate::editor::layout::{self, Hit, Layout};
use crate::editor::model::{COLORS, Doc, Mark, Tool, WIDTHS};
use crate::font;
use crate::gfx::{
    create_composition, create_d2d, create_d3d_device, create_dwrite, create_swap_chain,
};
use crate::i18n;
use crate::logging;
use crate::types::{ImageData, RectF, RectI};
use crate::win32;
use crate::window;
use paint::Chrome;

// The paint module draws with the shared D2D helpers through `super::`.
pub(crate) use crate::gfx::{
    create_text_format, create_text_layout, d2d_rect, pixel_format, rounded, solid, text_metrics,
};

/// The result of one edit, delivered on the UI thread.
#[derive(Clone, Debug)]
pub enum EditorEvent {
    /// The marked-up picture as PNG bytes, at the source resolution, plus a
    /// card-sized preview and a larger flight image so the way back carries the
    /// marks sharp even at the editor card's size.
    Saved {
        png: Vec<u8>,
        preview: ImageData,
        flight: ImageData,
    },
    /// The user cancelled; the file is untouched. `flight` is a sharp copy of
    /// the original for the return flight (absent when it could not be
    /// rendered).
    Cancelled { flight: Option<ImageData> },
}

const CLASS_NAME: PCWSTR = w!("PeglineEditor");
/// Private notification: a result is waiting in `Shared::event`.
const WM_APP_EDITOR_DONE: u32 = WM_APP + 10;
/// Private notification: the text caret moved, so the IME's composition and
/// candidate windows must be re-anchored. Sent by the render thread only while
/// a composition is open.
const WM_APP_IME_CARET: u32 = WM_APP + 11;
/// `WM_MOUSELEAVE` lives in `Win32_UI_Controls`, a feature this crate does not
/// otherwise need.
const WM_MOUSELEAVE: u32 = 0x02A3;

/// Side of one mosaic block in source pixels (BEHAVIOR §11).
const MOSAIC_BLOCK: u32 = 16;
/// Text marks are 32 source pixels tall, SemiBold, like the WPF text box.
const TEXT_SIZE: f32 = 32.0;
/// Radius of the eraser disc, DIP. A mark touched by the disc is removed
/// whole, so the eraser never leaves half a stroke behind.
const ERASER_RADIUS_DIP: f32 = 14.0;
/// Cached mosaic textures kept for undo/redo; the cache is cleared when full.
const MOSAIC_CACHE: usize = 16;

/// Button and icon geometry, DIP. The pills themselves are pure layout.
const ICON_STROKE: f32 = 2.0;
/// The Lucide icons are designed on a 24×24 grid with a 2 px stroke; they are
/// drawn in an 18 DIP box inside the 34 DIP tool button.
const ICON_BOX: f32 = 18.0;
const ICON_DESIGN: f32 = 24.0;

/// The floating chrome: white glass like the cards on the line, dark ink and
/// the one blue accent the buttons already used.
const CHROME_GLASS: u32 = 0xF7_F7_F7;
const CHROME_GLASS_ALPHA: f32 = 0xB8 as f32 / 255.0;
const CHROME_GLASS_LIT: u32 = 0xFF_FF_FF;
const CHROME_GLASS_LIT_ALPHA: f32 = 0xF2 as f32 / 255.0;
const CHROME_BORDER_ALPHA: f32 = 0x70 as f32 / 255.0;
const CHROME_INK: u32 = 0x1E_1E_1E;
const CHROME_INK_DIM: u32 = 0x9A_9A_9A;
/// Disabled action labels: dark enough to stay legible on the light Mica pill,
/// still clearly quieter than the enabled ink.
const CHROME_INK_MUTED: u32 = 0x4A_4A_4A;
const CHROME_ACCENT: u32 = 0x0E_63_9C;
const CHROME_ACCENT_HOVER: u32 = 0x11_77_BB;
const CHROME_ACCENT_BORDER: u32 = 0x82_C4_FF;

/// The five tools in toolbar order.
const TOOLS: [Tool; 5] = [
    Tool::Arrow,
    Tool::Rectangle,
    Tool::Text,
    Tool::Eraser,
    Tool::Mosaic,
];

/// One open editor.
pub struct Editor {
    hwnd: HWND,
    shared: Arc<Shared>,
    join: Option<JoinHandle<()>>,
    /// Source pixels; `card_px` needs them to rebuild the same layout.
    source_size: (u32, u32),
}

impl Editor {
    /// Opens the annotation window for `path` on `monitor`, centred on it.
    ///
    /// `source_size` is the picture's pixel size. The app knows it before the
    /// render thread decodes, so the window's card can be laid out — and the
    /// opening flight can aim at it — immediately.
    ///
    /// `visible` false creates the window hidden; the app shows it when the
    /// card-to-editor flight lands.
    pub fn open(
        path: &Path,
        source_size: (u32, u32),
        dpi: u32,
        monitor: win32::Monitor,
        visible: bool,
        notify: Box<dyn Fn(EditorEvent) + Send>,
    ) -> Result<Self, String> {
        let dpi = monitor.dpi.max(dpi).max(96);
        let scale = dpi as f32 / 96.0;
        let work = monitor.work;
        let (width_dip, height_dip) = layout::window_size(
            source_size,
            (work.width() as f32 / scale, work.height() as f32 / scale),
        );
        let width = (width_dip * scale).round().max(320.0) as i32;
        let height = (height_dip * scale).round().max(240.0) as i32;
        let left = work.left + (work.width() - width).max(0) / 2;
        let top = work.top + (work.height() - height).max(0) / 2;
        let title = win32::wide(i18n::s().editor_title);
        // A frameless, topmost popup: the editor is a floating hanging picture,
        // not a document window. It has no caption buttons and no border; the
        // card, the pin and the floating pills are the whole image.
        let hwnd = window::create_window(
            CLASS_NAME,
            PCWSTR(title.as_ptr()),
            WS_POPUP,
            WS_EX_NOREDIRECTIONBITMAP | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            RectI::new(left, top, left + width, top + height),
            Some(editor_wndproc),
            "editor",
        )?;

        let (tx, rx) = mpsc::channel();
        let initial = layout::fit(width as f32 / scale, height as f32 / scale, source_size);
        let shared = Arc::new(Shared {
            tx: Mutex::new(tx),
            notify: Mutex::new(Some(notify)),
            event: Mutex::new(None),
            layout: Mutex::new(initial),
            pending_high: AtomicU32::new(0),
            captured: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            reported: AtomicBool::new(false),
            delivered: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            focus_ready: AtomicBool::new(false),
            commit_requested: AtomicBool::new(false),
            text_tool: AtomicBool::new(false),
            ime: AtomicIsize::new(0),
            composing: AtomicBool::new(false),
            caret_x: AtomicI32::new(-1),
            caret_y: AtomicI32::new(-1),
            candidate_x: AtomicI32::new(-1),
            candidate_y: AtomicI32::new(-1),
        });
        // SAFETY: the pointer stays valid while the window exists: `Editor`
        // holds the `Arc` and `shutdown` destroys the window before that `Arc`
        // is dropped.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, Arc::as_ptr(&shared) as isize) };

        // A plain Win32 window does not get an IME context on Windows 11 TSF
        // unless it asks for one. Without this Chinese/Japanese input never
        // starts composing in the text tool.
        match win32::ime_attach(hwnd) {
            Some(ime) => {
                shared.ime.store(ime, Ordering::Release);
            }
            None => logging::error("editor: ImmCreateContext failed"),
        }

        let (client_width, client_height) = client_size(hwnd).unwrap_or((1, 1));
        let render_dpi = win32::window_dpi(hwnd);
        let start = Start {
            hwnd: SendHwnd(hwnd),
            path: path.to_path_buf(),
            width_px: client_width,
            height_px: client_height,
            dpi: render_dpi,
            source_size,
        };
        let thread_shared = Arc::clone(&shared);
        let fallback_hwnd = SendHwnd(hwnd);
        let spawn = window::spawn_render_thread("pegline-editor", "editor", move |ready| {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                render_thread(start, rx, Arc::clone(&thread_shared), ready);
            }));
            if outcome.is_err() {
                logging::error("panic in the editor render thread");
            }
            // A thread that ends without a result (a panic, a WIC failure) must
            // still close the window instead of leaving it stuck.
            if !thread_shared.stopping.load(Ordering::Acquire)
                && !thread_shared.reported.load(Ordering::Acquire)
            {
                report(
                    &thread_shared,
                    fallback_hwnd.handle(),
                    EditorEvent::Cancelled { flight: None },
                );
            }
        });
        let (join, ready_rx) = match spawn {
            Ok(pair) => pair,
            Err(error) => {
                // SAFETY: the window exists and no other thread uses it.
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                return Err(error);
            }
        };
        let join = window::await_render_ready(
            &ready_rx,
            join,
            || shared.stopping.store(true, Ordering::Release),
            hwnd,
            "editor",
        )?;

        logging::trace(format!(
            "editor open: {} {}x{} px dpi {} source {}x{}",
            path.display(),
            width,
            height,
            render_dpi,
            source_size.0,
            source_size.1
        ));
        let mut editor = Self {
            hwnd,
            shared,
            join: Some(join),
            source_size,
        };
        if visible {
            editor.show();
        }
        Ok(editor)
    }

    /// Shows the window and takes focus. Called when the card-to-editor flight
    /// lands, or directly when no flight can run.
    pub fn show(&mut self) {
        self.shared.commit_requested.store(false, Ordering::Release);
        // SAFETY: our own window; taking focus is the point of this window.
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOW);
            let _ = SetForegroundWindow(self.hwnd);
            let _ = SetFocus(Some(self.hwnd));
        }
        logging::trace("editor shown");
    }

    /// Hides the window so a flight can carry the picture away before the
    /// editor is released.
    pub fn hide(&mut self) {
        // SAFETY: our own window, alive while `self` owns it.
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    /// The glass card in screen pixels; the app flies the picture to and from
    /// exactly this rectangle.
    pub fn card_px(&self) -> Option<RectI> {
        let rect = win32::window_rect(self.hwnd)?;
        let (width_px, height_px) = client_size(self.hwnd)?;
        let dpi = win32::window_dpi(self.hwnd);
        let scale = dpi as f32 / 96.0;
        let layout = layout::fit(
            width_px as f32 / scale,
            height_px as f32 / scale,
            self.source_size,
        );
        Some(RectI::new(
            rect.left + (layout.card.left * scale).round() as i32,
            rect.top + (layout.card.top * scale).round() as i32,
            rect.left + (layout.card.right * scale).round() as i32,
            rect.top + (layout.card.bottom * scale).round() as i32,
        ))
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// True once `WM_DESTROY` has run; the app may drop the editor then.
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Acquire)
    }

    /// Stops the render thread and destroys the window. Idempotent.
    pub fn shutdown(&mut self) {
        let clean = window::stop_render_thread(self.join.take(), || {
            self.shared.stopping.store(true, Ordering::Release);
            let _ = self.shared.send(Command::Stop);
        });
        if !clean {
            logging::error("editor render thread panicked");
        }
        if !self.shared.closed.load(Ordering::Acquire) {
            // SAFETY: the render thread has been joined, so nothing else uses
            // the window; destroying it releases the DirectComposition target.
            unsafe {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }
}

impl Drop for Editor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The callback the app hands in; it runs on the UI thread at most once.
type Notify = Box<dyn Fn(EditorEvent) + Send>;

/// State shared by the UI thread and the render thread.
struct Shared {
    tx: Mutex<Sender<Command>>,
    notify: Mutex<Option<Notify>>,
    event: Mutex<Option<EditorEvent>>,
    /// Where the card and the pills are, for the cursor and the window proc.
    /// The render thread updates it on a resize or a DPI change.
    layout: Mutex<Layout>,
    /// Pending high surrogate of a `WM_CHAR` pair.
    pending_high: AtomicU32,
    captured: AtomicBool,
    closed: AtomicBool,
    reported: AtomicBool,
    delivered: AtomicBool,
    stopping: AtomicBool,
    /// The window has taken focus once. Only then does losing focus finish the
    /// edit; otherwise the initial activation churn would save immediately.
    focus_ready: AtomicBool,
    /// A save has already been requested, so focus loss and an outside click
    /// cannot ask twice.
    commit_requested: AtomicBool,
    /// The Text tool was the last tool button pressed; drives the I-beam
    /// cursor over the canvas. The render thread owns the real tool state.
    text_tool: AtomicBool,
    /// IME context associated with the editor window (0 when unavailable).
    ime: AtomicIsize,
    /// An IME composition is open. Set on `WM_IME_STARTCOMPOSITION`, cleared on
    /// `WM_IME_ENDCOMPOSITION` and whenever the app ends the text box: Esc and
    /// the save paths consult it before they touch the label.
    composing: AtomicBool,
    /// Insertion point in client pixels: where `COMPOSITIONFORM` puts the
    /// composition window of input methods that still draw their own.
    caret_x: AtomicI32,
    caret_y: AtomicI32,
    /// Where the candidate list should hang, in client pixels: under the text
    /// box rather than on top of the glyphs.
    candidate_x: AtomicI32,
    candidate_y: AtomicI32,
}

impl Shared {
    /// Queues a command. False means the render thread is gone.
    fn send(&self, command: Command) -> bool {
        match self.tx.lock() {
            Ok(tx) => tx.send(command).is_ok(),
            Err(_) => false,
        }
    }
}

/// HWND is a raw pointer; the render thread only posts to and draws into it.
#[derive(Clone, Copy)]
struct SendHwnd(HWND);

// SAFETY: the render thread never destroys or re-parents the window; it only
// posts messages and hands the handle to DirectComposition.
unsafe impl Send for SendHwnd {}

impl SendHwnd {
    /// Accessing the handle through a method keeps the closure capturing the
    /// `Send` wrapper rather than the raw (non-`Send`) `HWND` field.
    fn handle(&self) -> HWND {
        self.0
    }
}

/// Input and control messages from the UI thread.
enum Command {
    Resize {
        width_px: u32,
        height_px: u32,
        dpi: u32,
    },
    PointerDown {
        x: i32,
        y: i32,
    },
    PointerMove {
        x: i32,
        y: i32,
    },
    PointerUp {
        x: i32,
        y: i32,
    },
    PointerCancel,
    /// The pointer left the window; clear the toolbar hover state.
    HoverClear,
    Char(char),
    Backspace,
    /// The IME's in-progress string and its caret offset in bytes. Drawn inside
    /// the text box, underlined, and never part of the picture.
    Composition {
        text: String,
        cursor: usize,
    },
    /// The IME's result string; the only path composed text takes into a mark.
    CommitComposition(String),
    /// `WM_IME_ENDCOMPOSITION`: drop the composition without committing.
    EndComposition,
    FinishText,
    Escape,
    Undo,
    Redo,
    Save,
    Stop,
}

/// What the render thread should do after a command.
enum Step {
    Continue,
    Done(EditorEvent),
    Stop,
}

/// Everything the render thread needs at startup.
struct Start {
    hwnd: SendHwnd,
    path: PathBuf,
    width_px: u32,
    height_px: u32,
    dpi: u32,
    source_size: (u32, u32),
}

/// Window procedure: forwards input, delivers the result, closes the window.
///
/// SAFETY: the Win32 calls below only touch the editor window created in
/// [`Editor::open`] and the `Shared` pointer stored in its `GWLP_USERDATA`,
/// which outlives the window. Message pointers are only read as the structures
/// the system documents for that message.
unsafe extern "system" fn editor_wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // A panic may not unwind out of an `extern "system"` function: it aborts the
    // process. Contain it here.
    // SAFETY: USER32 passes `hwnd` and the message parameters for the editor window;
    // the body treats them as valid for this call (it writes through the
    // `WM_GETMINMAXINFO`/`WM_DPICHANGED` pointers USER32 supplied) and keeps
    // none of them past its return.
    crate::window::guard("editor_wndproc", || unsafe {
        editor_wndproc_inner(hwnd, message, wparam, lparam)
    })
}

/// The real handler; plain `unsafe fn` so a panic unwinds into the guard above.
///
/// A dispatcher only: it keeps the `GWLP_USERDATA` null check, the routing and
/// the default handling, and hands each message family to one `handle_*`
/// function below, so no single concern hides inside a 300-line match.
unsafe fn editor_wndproc_inner(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // Erasing and painting may arrive before `open` has stored a `Shared`, so
    // they are answered before the lookup, exactly as they were.
    match message {
        WM_ERASEBKGND | WM_PAINT => return handle_paint(hwnd, message),
        _ => {}
    }

    // SAFETY: `open` stores the live `Shared` pointer before the window is
    // shown; messages that arrive during creation fall through to the default.
    let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const Shared;
    if raw.is_null() {
        // SAFETY: default handling for messages that arrive during creation.
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    // SAFETY: the pointer was set by `open` and lives as long as the window.
    let shared = unsafe { &*raw };

    match message {
        WM_ACTIVATE | WM_SETFOCUS | WM_KILLFOCUS => {
            return handle_focus(hwnd, message, wparam, shared);
        }
        WM_CLOSE => return handle_close(hwnd, shared),
        WM_APP_EDITOR_DONE => return handle_deliver(hwnd, shared),
        WM_DESTROY => return handle_destroy(hwnd, shared),
        WM_SIZE | WM_DPICHANGED => return handle_resize(hwnd, message, lparam, shared),
        WM_LBUTTONDOWN | WM_MOUSEMOVE | WM_MOUSELEAVE | WM_LBUTTONUP | WM_CAPTURECHANGED => {
            return handle_mouse(hwnd, message, lparam, shared);
        }
        WM_SETCURSOR => return handle_cursor(hwnd, message, wparam, lparam, shared),
        WM_KEYDOWN | WM_CHAR => {
            // A `WM_KEYDOWN` the editor does not use belongs to the window: the
            // handler says so with `None` and the default call below runs.
            if let Some(result) = handle_key(hwnd, message, wparam, shared) {
                return result;
            }
        }
        WM_IME_SETCONTEXT
        | WM_IME_STARTCOMPOSITION
        | WM_IME_COMPOSITION
        | WM_IME_ENDCOMPOSITION
        | WM_IME_CHAR
        | WM_APP_IME_CARET => return handle_ime(hwnd, message, wparam, lparam, shared),
        _ => {}
    }
    // SAFETY: default processing for messages the editor does not handle.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

/// `WM_ERASEBKGND` and `WM_PAINT`: the render thread paints every pixel, so the
/// window only swallows the erase and clears its own update region.
fn handle_paint(hwnd: HWND, message: u32) -> LRESULT {
    if message == WM_ERASEBKGND {
        return LRESULT(1);
    }
    // SAFETY: clearing our own update region.
    unsafe {
        let _ = ValidateRect(Some(hwnd), None);
    }
    LRESULT(0)
}

/// `WM_ACTIVATE`, `WM_SETFOCUS` and `WM_KILLFOCUS`: the editor takes no focus
/// from the line, but losing focus ends the edit.
fn handle_focus(hwnd: HWND, message: u32, wparam: WPARAM, shared: &Shared) -> LRESULT {
    match message {
        WM_ACTIVATE => {
            // Clicking anywhere outside the editor — another app, the desktop,
            // the line — finishes the edit exactly like clicking the empty part
            // of this window. Not before the window has had focus once: the
            // activation churn while the opening flight lands would otherwise
            // save the picture before anyone could draw on it.
            let active = (wparam.0 as u32) & 0xFFFF;
            logging::trace(format!("editor activate {active}"));
            if active == 0 && shared.focus_ready.load(Ordering::Acquire) {
                // Uncommitted pinyin is never baked into the picture: finish
                // the composition before the save is requested.
                cancel_composition(hwnd, shared);
                request_save(shared);
            }
        }
        WM_SETFOCUS => {
            logging::trace("editor focus");
            shared.focus_ready.store(true, Ordering::Release);
        }
        WM_KILLFOCUS => {
            logging::trace("editor focus lost");
            if shared.focus_ready.load(Ordering::Acquire) {
                cancel_composition(hwnd, shared);
                request_save(shared);
            }
        }
        _ => {}
    }
    LRESULT(0)
}

/// `WM_CLOSE`: closing saves, like clicking outside the card, and uncommitted
/// pinyin is cancelled first so it never reaches the picture.
fn handle_close(hwnd: HWND, shared: &Shared) -> LRESULT {
    cancel_composition(hwnd, shared);
    request_save(shared);
    LRESULT(0)
}

/// `WM_APP_EDITOR_DONE`: the render thread has published a result; hand it to
/// the app here, on the UI thread.
fn handle_deliver(_hwnd: HWND, shared: &Shared) -> LRESULT {
    deliver_pending(shared);
    LRESULT(0)
}

/// `WM_DESTROY`: drop the input context and mark the window closed. The
/// `Shared` outlives the window, so the app can still read the flag.
fn handle_destroy(hwnd: HWND, shared: &Shared) -> LRESULT {
    let ime = shared.ime.swap(0, Ordering::AcqRel);
    win32::ime_detach(hwnd, ime);
    shared.closed.store(true, Ordering::Release);
    LRESULT(0)
}

/// `WM_SIZE` and `WM_DPICHANGED`: keep the render thread's pixel geometry and
/// DPI in step with the window.
fn handle_resize(hwnd: HWND, message: u32, lparam: LPARAM, shared: &Shared) -> LRESULT {
    match message {
        WM_SIZE => {
            let width = (lparam.0 as usize & 0xFFFF) as u32;
            let height = ((lparam.0 as usize >> 16) & 0xFFFF) as u32;
            // SAFETY: pure query on our own window.
            let dpi = win32::window_dpi(hwnd);
            shared.send(Command::Resize {
                width_px: width.max(1),
                height_px: height.max(1),
                dpi,
            });
        }
        WM_DPICHANGED => {
            // SAFETY: lparam is the suggested window rectangle for this message.
            let suggested = unsafe { &*(lparam.0 as *const RECT) };
            // SAFETY: moving and resizing our own window, without activating it.
            unsafe {
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
        _ => {}
    }
    LRESULT(0)
}

/// `WM_LBUTTONDOWN`, `WM_MOUSEMOVE`, `WM_MOUSELEAVE`, `WM_LBUTTONUP` and
/// `WM_CAPTURECHANGED`: pointer positions, capture and the hover highlight, all
/// forwarded to the render thread, which owns the hit math.
fn handle_mouse(hwnd: HWND, message: u32, lparam: LPARAM, shared: &Shared) -> LRESULT {
    match message {
        WM_LBUTTONDOWN => {
            let (x, y) = mouse_xy(lparam);
            // A click ends the text mark it lands on (or the whole edit, on
            // Save/Cancel), so the input method must stop composing first; a
            // composition left open would keep feeding a mark that is gone.
            cancel_composition(hwnd, shared);
            // The render thread owns the tool state and decides what the click
            // hit; it publishes the Text-tool flag that drives the I-beam and
            // the IME caret. Keeping the hit math in one place means the
            // responsive toolbar geometry cannot drift from input.
            shared.captured.store(true, Ordering::Release);
            // SAFETY: capturing the mouse for our own window.
            unsafe { SetCapture(hwnd) };
            shared.send(Command::PointerDown { x, y });
        }
        WM_MOUSEMOVE => {
            let (x, y) = mouse_xy(lparam);
            // Ask for WM_MOUSELEAVE too, so a hover highlight never sticks
            // after the pointer leaves the window.
            let mut track = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            // SAFETY: `track` is a valid, fully initialised TRACKMOUSEEVENT for
            // our own window.
            unsafe {
                let _ = TrackMouseEvent(&mut track as *mut _);
            }
            shared.send(Command::PointerMove { x, y });
        }
        WM_MOUSELEAVE => {
            shared.send(Command::HoverClear);
        }
        WM_LBUTTONUP => {
            let (x, y) = mouse_xy(lparam);
            shared.captured.store(false, Ordering::Release);
            // SAFETY: balances `SetCapture`; releasing without capture is a no-op.
            unsafe {
                let _ = ReleaseCapture();
            }
            shared.send(Command::PointerUp { x, y });
        }
        WM_CAPTURECHANGED => {
            // The flag is always cleared, but only a capture we owned turns
            // into a cancelled gesture.
            let was_captured = shared.captured.swap(false, Ordering::AcqRel);
            if was_captured {
                shared.send(Command::PointerCancel);
            }
        }
        _ => {}
    }
    LRESULT(0)
}

/// `WM_SETCURSOR`: the cursor shape over the pills, the photo and the gaps.
fn handle_cursor(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    shared: &Shared,
) -> LRESULT {
    // Hand over the floating pills, cross/I-beam over the photo and a plain
    // arrow in the transparent gaps around the card.
    if (lparam.0 as u32 & 0xFFFF) == HTCLIENT {
        let mut point = POINT::default();
        // SAFETY: pure queries on our own window and the thread cursor.
        unsafe {
            let _ = GetCursorPos(&mut point);
            let _ = ScreenToClient(hwnd, &mut point);
        }
        let dpi = win32::window_dpi(hwnd) as f32;
        let x = point.x as f32 / (dpi / 96.0);
        let y = point.y as f32 / (dpi / 96.0);
        let layout = shared.layout.lock().map(|layout| *layout).ok();
        let cursor = match layout {
            Some(layout) if layout.hit(x, y).is_some() => IDC_HAND,
            Some(layout) if layout.on_photo(x, y) => {
                if shared.text_tool.load(Ordering::Acquire) {
                    IDC_IBEAM
                } else {
                    IDC_CROSS
                }
            }
            _ => IDC_ARROW,
        };
        // SAFETY: system cursors are process-wide and never freed.
        unsafe {
            let loaded = LoadCursorW(None, cursor).unwrap_or_default();
            let _ = SetCursor(Some(loaded));
        }
        return LRESULT(1);
    }
    // SAFETY: default handling for the non-client area.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

/// `WM_KEYDOWN` and `WM_CHAR`: the shortcuts, Esc and text input. `None` means
/// the key is not the editor's, so the caller's default handling runs.
fn handle_key(hwnd: HWND, message: u32, wparam: WPARAM, shared: &Shared) -> Option<LRESULT> {
    if message == WM_CHAR {
        forward_char(shared, wparam);
        return Some(LRESULT(0));
    }
    // Everything else routed here is `WM_KEYDOWN`.
    // SAFETY: `GetKeyState` is a thread-local query with no preconditions.
    let control = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
    let key = (wparam.0 as u32) & 0xFFFF;
    if key == VK_ESCAPE.0 as u32 {
        // Esc cancels the pinyin, not the text box: the second Esc,
        // with no composition open, ends the text entry.
        if shared.composing.load(Ordering::Acquire) {
            cancel_composition(hwnd, shared);
            return Some(LRESULT(0));
        }
        shared.send(Command::Escape);
        return Some(LRESULT(0));
    }
    if shared.composing.load(Ordering::Acquire) {
        // The input method owns undo/redo while it is composing.
        if control && (key == VK_Z.0 as u32 || key == VK_Y.0 as u32) {
            return Some(LRESULT(0));
        }
    }
    if control && key == VK_Z.0 as u32 {
        shared.send(Command::Undo);
        return Some(LRESULT(0));
    }
    if control && key == VK_Y.0 as u32 {
        shared.send(Command::Redo);
        return Some(LRESULT(0));
    }
    if control && key == VK_S.0 as u32 {
        cancel_composition(hwnd, shared);
        request_save(shared);
        return Some(LRESULT(0));
    }
    None
}

/// The IME path: `WM_IME_SETCONTEXT`, `WM_IME_STARTCOMPOSITION`,
/// `WM_IME_COMPOSITION`, `WM_IME_ENDCOMPOSITION`, `WM_IME_CHAR` and the render
/// thread's `WM_APP_IME_CARET`.
fn handle_ime(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    shared: &Shared,
) -> LRESULT {
    match message {
        WM_IME_SETCONTEXT => {
            // The editor draws the composition itself (inside its white box),
            // so the input method must not also paint the default floating
            // composition window over the picture. The candidate list stays the
            // IME's own window; only the composition bit is cleared.
            let flags = lparam.0 & !(ISC_SHOWUICOMPOSITIONWINDOW as isize);
            // SAFETY: default handling for the rest of the flags.
            return unsafe { DefWindowProcW(hwnd, message, wparam, LPARAM(flags)) };
        }
        WM_IME_STARTCOMPOSITION => {
            shared.composing.store(true, Ordering::Release);
            position_ime(hwnd, shared);
            // Returning 0 claims the composition: the IME hands over the
            // string instead of drawing a window of its own.
        }
        WM_IME_COMPOSITION => {
            let flags = lparam.0 as u32;
            if flags & GCS_RESULTSTR.0 != 0 {
                if let Some(text) = win32::ime_string(hwnd, GCS_RESULTSTR) {
                    shared.send(Command::CommitComposition(text));
                }
                // The pinyin is done; an IME that skips ENDCOMPOSITION must not
                // leave the Esc guard armed.
                shared.composing.store(false, Ordering::Release);
            }
            if flags & GCS_COMPSTR.0 != 0 {
                let text = win32::ime_string(hwnd, GCS_COMPSTR).unwrap_or_default();
                let cursor = ime_cursor(hwnd, &text);
                if !text.is_empty() {
                    shared.composing.store(true, Ordering::Release);
                }
                shared.send(Command::Composition { text, cursor });
            }
            position_ime(hwnd, shared);
        }
        WM_IME_ENDCOMPOSITION => {
            shared.composing.store(false, Ordering::Release);
            shared.send(Command::EndComposition);
        }
        WM_IME_CHAR => {
            // With an IME context the result string is the single commit path
            // (see `Command::CommitComposition`); handling this too would
            // insert every committed character twice. It stays only as a
            // fallback for a window that never got a context, where no
            // composition can be read at all.
            if shared.ime.load(Ordering::Acquire) == 0 {
                let code = (wparam.0 as u32) & 0xFFFF;
                if let Some(character) = char::from_u32(code) {
                    shared.send(Command::Char(character));
                }
            }
        }
        WM_APP_IME_CARET => {
            // The render thread just published a new caret after painting the
            // composition; this is the only point where the caret and the IME
            // windows agree, because the caret is measured from the layout.
            position_ime(hwnd, shared);
        }
        _ => {}
    }
    LRESULT(0)
}

/// Asks the render thread for one save.
///
/// Losing focus, clicking outside the card and Ctrl+S all funnel through here,
/// so a single gesture cannot queue two saves; a failed save clears the flag at
/// the other end so the user may try again.
fn request_save(shared: &Shared) {
    if shared.commit_requested.swap(true, Ordering::AcqRel) {
        return;
    }
    if !shared.send(Command::Save) {
        shared.commit_requested.store(false, Ordering::Release);
    }
}

/// Stops an open IME composition without committing it.
///
/// Called whenever the app is about to end the text mark (Esc, a click, Save,
/// focus loss). Cancelling rather than committing is deliberate: uncommitted
/// pinyin never becomes part of the picture, and a composition left open would
/// keep feeding a mark that is already gone.
fn cancel_composition(hwnd: HWND, shared: &Shared) {
    if !shared.composing.swap(false, Ordering::AcqRel) {
        return;
    }
    logging::trace("IME composition cancelled");
    // SAFETY: our own window; the context is released below.
    unsafe {
        let context = ImmGetContext(hwnd);
        if context.0.is_null() {
            return;
        }
        let _ = ImmNotifyIME(context, NI_COMPOSITIONSTR, CPS_CANCEL, 0);
        let _ = ImmReleaseContext(hwnd, context);
    }
}

/// The composition caret, in bytes inside `text`. `GCS_CURSORPOS` returns the
/// position in UTF-16 units as the call's value, not as a buffer.
fn ime_cursor(hwnd: HWND, text: &str) -> usize {
    // SAFETY: our own window; the context is released below.
    unsafe {
        let context = ImmGetContext(hwnd);
        if context.0.is_null() {
            return text.len();
        }
        let units = ImmGetCompositionStringW(context, GCS_CURSORPOS, None, 0);
        let _ = ImmReleaseContext(hwnd, context);
        // Zero is a valid position (the caret at the start of the composition);
        // only a negative result means the query failed.
        if units < 0 {
            return text.len();
        }
        utf16_to_byte(text, units as usize)
    }
}

/// Byte offset in `text` for a UTF-16 code-unit offset.
fn utf16_to_byte(text: &str, units: usize) -> usize {
    let mut seen = 0;
    for (index, character) in text.char_indices() {
        if seen >= units {
            return index;
        }
        seen += character.len_utf16();
    }
    text.len()
}

/// Hands the queued result to the app, then destroys the window.
fn deliver_pending(shared: &Shared) {
    if shared.stopping.load(Ordering::Acquire) {
        return;
    }
    let event = match shared.event.lock() {
        Ok(mut slot) => slot.take(),
        Err(_) => None,
    };
    if let Some(event) = event {
        deliver(shared, event);
    }
}

/// Calls `notify` exactly once on the UI thread. The window is deliberately
/// left standing: the app starts the flight back to the line, hides this window
/// when the flight's first frame is up, and destroys it when the flight ends.
/// Destroying it here would leave one frame with neither the editor nor the
/// flight on screen.
fn deliver(shared: &Shared, event: EditorEvent) {
    if shared.delivered.swap(true, Ordering::AcqRel) {
        return;
    }
    let notify = match shared.notify.lock() {
        Ok(mut slot) => slot.take(),
        Err(_) => None,
    };
    if let Some(notify) = notify {
        // The app owns the callback; a panic in it must not leave the window up.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| notify(event)));
        if outcome.is_err() {
            logging::error("panic in the editor notify callback");
        }
    }
}

/// Stores the render thread's result and asks the UI thread to deliver it.
fn report(shared: &Shared, hwnd: HWND, event: EditorEvent) {
    if shared.stopping.load(Ordering::Acquire) || shared.reported.swap(true, Ordering::AcqRel) {
        return;
    }
    if let Ok(mut slot) = shared.event.lock() {
        *slot = Some(event);
    }
    // SAFETY: posting to a window this process owns. If it is already gone the
    // post fails and the app sees `is_closed` instead.
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_APP_EDITOR_DONE, WPARAM(0), LPARAM(0));
    }
}

/// Points the input method at the text caret.
///
/// `COMPOSITIONFORM` gets the insertion point, so input methods that still draw
/// their own composition window put it at the caret instead of at a box corner.
/// `CANDIDATEFORM` gets the bottom of the white box, so the candidate list
/// opens under the line instead of on top of the glyphs. Both take client-area
/// coordinates relative to `hwnd`, in physical pixels, never screen ones.
fn position_ime(hwnd: HWND, shared: &Shared) {
    let x = shared.caret_x.load(Ordering::Relaxed);
    let y = shared.caret_y.load(Ordering::Relaxed);
    let candidate_x = shared.candidate_x.load(Ordering::Relaxed);
    let candidate_y = shared.candidate_y.load(Ordering::Relaxed);
    logging::trace(format!(
        "IME position {x},{y} candidate {candidate_x},{candidate_y}"
    ));
    win32::ime_position(hwnd, (x, y), Some((candidate_x, candidate_y)));
}

/// Turns one `WM_CHAR` into text input, combining surrogate pairs.
fn forward_char(shared: &Shared, wparam: WPARAM) {
    let unit = (wparam.0 as u32) & 0xFFFF;
    match unit {
        0x08 => {
            shared.send(Command::Backspace);
        }
        0x0A | 0x0D => {
            shared.send(Command::FinishText);
        }
        0xD800..=0xDBFF => shared.pending_high.store(unit, Ordering::Release),
        0xDC00..=0xDFFF => {
            let high = shared.pending_high.swap(0, Ordering::AcqRel);
            if (0xD800..=0xDBFF).contains(&high) {
                let code = 0x1_0000 + ((high - 0xD800) << 10) + (unit - 0xDC00);
                if let Some(character) = char::from_u32(code) {
                    shared.send(Command::Char(character));
                }
            }
        }
        _ if unit >= 0x20 && unit != 0x7F => {
            if let Some(character) = char::from_u32(unit) {
                shared.send(Command::Char(character));
            }
        }
        _ => {}
    }
}

fn mouse_xy(lparam: LPARAM) -> (i32, i32) {
    let packed = lparam.0 as usize;
    (
        (packed & 0xFFFF) as u16 as i16 as i32,
        ((packed >> 16) & 0xFFFF) as u16 as i16 as i32,
    )
}

/// The client area in physical pixels.
fn client_size(hwnd: HWND) -> Option<(u32, u32)> {
    let mut rect = RECT::default();
    // SAFETY: `rect` is a live local filled by the call.
    unsafe { GetClientRect(hwnd, &mut rect) }.ok()?;
    Some((
        ((rect.right - rect.left).max(1)) as u32,
        ((rect.bottom - rect.top).max(1)) as u32,
    ))
}

// ---- the render thread ----------------------------------------------------

/// DIP to physical pixels, never zero. Only the IME anchor tests need it; the
/// production paths fold the same arithmetic into their own layout math.
#[cfg(test)]
fn pixels(dip: f32, dpi: u32) -> i32 {
    ((dip * dpi.max(96) as f32 / 96.0).round() as i32).max(1)
}

fn render_thread(
    start: Start,
    rx: Receiver<Command>,
    shared: Arc<Shared>,
    ready: Sender<Result<(), String>>,
) {
    let _com = match window::ComGuard::init(COINIT_MULTITHREADED) {
        Ok(guard) => guard,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let surface = match Surface::new(start.hwnd.0, start.width_px, start.height_px, start.dpi) {
        Ok(surface) => surface,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let mut state = match EditorState::new(
        start.width_px,
        start.height_px,
        start.dpi,
        start.source_size,
        surface,
        Arc::clone(&shared),
    ) {
        Ok(state) => state,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    if ready.send(Ok(())).is_err() {
        return;
    }
    // First frame: dark background and toolbar while the picture decodes.
    if let Err(error) = state.draw() {
        logging::error(format!("editor first frame: {error}"));
    }

    let mut done: Option<EditorEvent> = None;
    if let Err(error) = state.load(&start.path) {
        logging::error(format!("editor decode {}: {error}", start.path.display()));
        done = Some(EditorEvent::Cancelled { flight: None });
    }
    if done.is_none() {
        'session: loop {
            if !state.needs_draw() {
                match rx.recv() {
                    Ok(command) => match state.apply(command) {
                        Step::Continue => {}
                        Step::Done(event) => {
                            done = Some(event);
                            break 'session;
                        }
                        Step::Stop => break 'session,
                    },
                    Err(_) => break 'session,
                }
            }
            let mut stopping = false;
            while let Ok(command) = rx.try_recv() {
                match state.apply(command) {
                    Step::Continue => {}
                    Step::Done(event) => {
                        done = Some(event);
                        break 'session;
                    }
                    Step::Stop => {
                        stopping = true;
                        break;
                    }
                }
            }
            if stopping {
                break;
            }
            if !state.needs_draw() {
                continue;
            }
            if let Err(error) = state.draw() {
                logging::error(format!("editor draw: {error}"));
                state.lost += 1;
                if state.lost > 3 {
                    done = Some(EditorEvent::Cancelled { flight: None });
                    break;
                }
                if let Err(error) = state.rebuild() {
                    logging::error(format!("editor rebuild: {error}"));
                }
                // `draw` clears the flag of every pass it finished, so a lost
                // surface stops here instead of spinning; the rebuild's repaint
                // waits for the next command, and a rebuilt surface repaints the
                // card anyway because it has no card bitmap.
                state.content_dirty = false;
                state.chrome_dirty = false;
            }
        }
    }
    // Report while the surface is still composed. The window is deliberately
    // left showing its last frame: the app starts the return flight, hides the
    // editor only when the flight's first frame is up, and sends `Stop` when
    // the flight ends. Dropping the swap chain and composition target here
    // used to make the window blink empty for a frame or two before the flight
    // appeared.
    if let Some(event) = done {
        report(&shared, start.hwnd.0, event);
        // If the app already asked us to stop (a direct close, an app exit),
        // the `Stop` was consumed by the session loop above; there is no
        // second one, and waiting here would deadlock `shutdown`'s join.
        if !shared.stopping.load(Ordering::Acquire) {
            while let Ok(command) = rx.recv() {
                if matches!(command, Command::Stop) {
                    break;
                }
            }
        }
    }
    // Release the swap chain and the DirectComposition target before the UI
    // thread destroys the window they belong to.
    drop(state);
    logging::trace("editor render thread stopped");
}

// ---- the editing state ----------------------------------------------------

/// One shortcut/button label, with its measured size in DIP.
struct Label {
    layout: IDWriteTextLayout,
    width: f32,
    height: f32,
}

/// Every text layout the floating action pills draw. Created once per surface.
struct Labels {
    undo: Label,
    redo: Label,
    cancel: Label,
    save: Label,
}

impl Labels {
    fn build(dwrite: &IDWriteFactory, strings: &crate::i18n::Strings) -> Result<Self, String> {
        let ui = create_text_format(dwrite, font::UI_FAMILY, 13.0, DWRITE_FONT_WEIGHT_MEDIUM)?;
        Ok(Self {
            undo: label(dwrite, &ui, strings.undo)?,
            redo: label(dwrite, &ui, strings.redo)?,
            cancel: label(dwrite, &ui, strings.cancel)?,
            save: label(dwrite, &ui, strings.save)?,
        })
    }
}

/// Where the picture goes, in DIP, and the source-pixel to DIP scale.
#[derive(Clone, Copy, Debug)]
struct CanvasLayout {
    image: RectF,
    scale: f32,
}

/// Cached text layout for one text mark.
struct TextEntry {
    text: String,
    layout: IDWriteTextLayout,
}

/// The text mark being typed, laid out together with the IME composition, with
/// the composition run underlined. The box, its caret and the IME anchors all
/// come from this layout while a composition is open, so the pinyin sits inside
/// the white box and pushes it wider as it grows.
struct ComposedText {
    /// Committed text and composition, for the cache key.
    committed: String,
    composition: String,
    layout: IDWriteTextLayout,
    width: f32,
    height: f32,
}

/// A drag in progress, in source pixels.
#[derive(Clone, Copy)]
struct Drag {
    start: (f32, f32),
    current: (f32, f32),
}

/// Everything on the render thread's side of the editor.
struct EditorState {
    surface: Surface,
    chrome: Chrome,
    assets: Assets,
    labels: Labels,
    text_format: IDWriteTextFormat,
    texts: Vec<TextEntry>,
    /// Laid-out composition, present only while the IME is composing.
    composing: Option<ComposedText>,
    doc: Option<Doc>,
    tool: Tool,
    color: u32,
    width: f32,
    drag: Option<Drag>,
    /// Marks as they were when the current eraser gesture began; committed as
    /// one undo step when the gesture ends.
    erase_before: Option<Vec<Mark>>,
    /// Scratch hit flags for one eraser step, reused so a drag does not
    /// allocate a vector per step.
    erase_hits: Vec<bool>,
    /// The button under the pointer, for the hover state.
    hover: Option<Hit>,
    /// Card, photo, pin and every floating button, in DIP.
    layout: Layout,
    /// Rounded mask for the photo's corners, rebuilt with the layout.
    photo_mask: Option<ID2D1Geometry>,
    canvas: CanvasLayout,
    width_px: u32,
    height_px: u32,
    dpi: f32,
    source_size: (u32, u32),
    texts_dirty: bool,
    /// The card bitmap (photo and marks) must be painted again.
    content_dirty: bool,
    /// The window around the card (glass, pin and pills) must be composed
    /// again; the card bitmap is reused exactly as it is.
    chrome_dirty: bool,
    lost: u32,
    /// UI-thread side shared state; used to publish the IME caret position.
    shared: Arc<Shared>,
}

impl EditorState {
    fn new(
        width_px: u32,
        height_px: u32,
        dpi: u32,
        source_size: (u32, u32),
        surface: Surface,
        shared: Arc<Shared>,
    ) -> Result<Self, String> {
        let labels = Labels::build(&surface.dwrite, i18n::s())?;
        let text_format = create_text_format(
            &surface.dwrite,
            font::UI_FAMILY,
            TEXT_SIZE,
            DWRITE_FONT_WEIGHT_SEMI_BOLD,
        )?;
        let chrome = Chrome::new(&surface.rt)?;
        let assets = Assets::new(surface.rt.clone())?;
        let dpi = dpi as f32;
        let width_dip = width_px as f32 * 96.0 / dpi;
        let height_dip = height_px as f32 * 96.0 / dpi;
        let layout = layout::fit(width_dip, height_dip, source_size);
        let canvas = CanvasLayout {
            image: layout.photo,
            scale: layout.scale,
        };
        let mut state = Self {
            surface,
            chrome,
            assets,
            labels,
            text_format,
            texts: Vec::new(),
            composing: None,
            doc: None,
            tool: Tool::Arrow,
            color: COLORS[0].0,
            width: WIDTHS[1],
            drag: None,
            erase_before: None,
            erase_hits: Vec::new(),
            hover: None,
            layout,
            photo_mask: None,
            canvas,
            width_px,
            height_px,
            dpi,
            source_size,
            texts_dirty: false,
            content_dirty: true,
            chrome_dirty: true,
            lost: 0,
            shared,
        };
        state.rebuild_photo_mask();
        Ok(state)
    }

    /// Rebuilds the rounded mask the photo is clipped with.
    fn rebuild_photo_mask(&mut self) {
        let radius = layout::CARD_RADIUS - layout::CARD_INSET;
        let rounded = D2D1_ROUNDED_RECT {
            rect: d2d_rect(self.layout.photo),
            radiusX: radius,
            radiusY: radius,
        };
        // SAFETY: the factory and the struct are alive for the call.
        match unsafe {
            self.surface
                .factory
                .CreateRoundedRectangleGeometry(&rounded)
        } {
            Ok(geometry) => match geometry.cast() {
                Ok(mask) => self.photo_mask = Some(mask),
                Err(error) => {
                    logging::error(format!("editor photo mask: {error}"));
                    self.photo_mask = None;
                }
            },
            Err(error) => {
                logging::error(format!("editor photo mask: {error}"));
                self.photo_mask = None;
            }
        }
    }

    fn width_dip(&self) -> f32 {
        self.width_px as f32 * 96.0 / self.dpi
    }

    fn height_dip(&self) -> f32 {
        self.height_px as f32 * 96.0 / self.dpi
    }

    /// Rebuilds the card and pill geometry after a resize, a DPI change or the
    /// decoded picture's real size, and publishes it for the cursor and the
    /// window procedure.
    fn relayout(&mut self) {
        self.layout = layout::fit(self.width_dip(), self.height_dip(), self.source_size);
        self.canvas = CanvasLayout {
            image: self.layout.photo,
            scale: self.layout.scale,
        };
        self.rebuild_photo_mask();
        if let Ok(mut shared) = self.shared.layout.lock() {
            *shared = self.layout;
        }
    }

    /// Decodes the picture at full resolution and uploads it for the preview.
    fn load(&mut self, path: &Path) -> Result<(), String> {
        let image = wic::decode_full(path)?;
        if !image.is_valid() {
            return Err(format!(
                "decoded image is invalid ({}x{}, {} bytes)",
                image.width,
                image.height,
                image.bgra.len()
            ));
        }
        self.source_size = (image.width, image.height);
        self.assets.set_source(&image)?;
        self.doc = Some(Doc::new(image));
        self.relayout();
        self.texts.clear();
        self.composing = None;
        self.texts_dirty = true;
        // A new picture and a new card size: the whole card is repainted.
        self.content_dirty = true;
        logging::trace(format!(
            "editor picture: {}x{}",
            self.source_size.0, self.source_size.1
        ));
        Ok(())
    }

    fn resize(&mut self, width_px: u32, height_px: u32, dpi: u32) -> Result<(), String> {
        if width_px == self.width_px && height_px == self.height_px && dpi as f32 == self.dpi {
            return Ok(());
        }
        self.surface.resize(width_px, height_px, dpi)?;
        self.width_px = width_px;
        self.height_px = height_px;
        self.dpi = dpi as f32;
        self.relayout();
        // The surface dropped its card bitmap and the photo was re-fitted.
        self.content_dirty = true;
        Ok(())
    }

    /// Rebuilds the GPU stack after a device loss, keeping the document.
    fn rebuild(&mut self) -> Result<(), String> {
        let surface = Surface::new(
            self.surface.hwnd,
            self.width_px,
            self.height_px,
            self.dpi as u32,
        )?;
        let labels = Labels::build(&surface.dwrite, i18n::s())?;
        let text_format = create_text_format(
            &surface.dwrite,
            font::UI_FAMILY,
            TEXT_SIZE,
            DWRITE_FONT_WEIGHT_SEMI_BOLD,
        )?;
        let chrome = Chrome::new(&surface.rt)?;
        let mut assets = Assets::new(surface.rt.clone())?;
        if let Some(doc) = self.doc.as_ref() {
            assets.set_source(doc.source())?;
        }
        self.surface = surface;
        self.labels = labels;
        self.text_format = text_format;
        self.chrome = chrome;
        self.assets = assets;
        self.texts.clear();
        self.composing = None;
        self.texts_dirty = true;
        self.relayout();
        // A fresh surface has no card bitmap: the card is painted again, and
        // the window is composed with it in the same frame.
        self.content_dirty = true;
        self.lost = 0;
        logging::trace("editor surface rebuilt");
        Ok(())
    }

    /// Runs `edit` on the document, marking the text and the card bitmap dirty
    /// when it changed something, and reports whether it did.
    fn with_doc(&mut self, edit: impl FnOnce(&mut Doc) -> bool) -> bool {
        let changed = self.doc.as_mut().is_some_and(edit);
        if changed {
            self.texts_dirty = true;
            self.content_dirty = true;
        }
        changed
    }

    fn apply(&mut self, command: Command) -> Step {
        match command {
            Command::Resize {
                width_px,
                height_px,
                dpi,
            } => {
                if let Err(error) = self.resize(width_px, height_px, dpi) {
                    logging::error(format!("editor resize: {error}"));
                }
            }
            Command::PointerDown { x, y } => return self.pointer_down(x, y),
            Command::PointerMove { x, y } => self.pointer_move(x, y),
            Command::PointerUp { x, y } => self.pointer_up(x, y),
            Command::PointerCancel => self.cancel_gesture(),
            Command::HoverClear => {
                if self.hover.take().is_some() {
                    // A hover moves a pill highlight only: the card bitmap is
                    // reused.
                    self.chrome_dirty = true;
                }
            }
            Command::Char(character) => {
                self.with_doc(|doc| doc.type_char(character));
            }
            Command::Backspace => {
                self.with_doc(|doc| doc.backspace());
            }
            Command::Composition { text, cursor } => {
                self.with_doc(|doc| doc.set_composition(&text, cursor));
            }
            Command::CommitComposition(text) => {
                self.with_doc(|doc| doc.commit_composition(&text));
            }
            Command::EndComposition => {
                self.with_doc(|doc| doc.clear_composition());
            }
            Command::FinishText => {
                if let Some(doc) = self.doc.as_mut() {
                    if doc.is_typing() {
                        doc.finish_text();
                        self.texts_dirty = true;
                        self.content_dirty = true;
                    }
                }
            }
            Command::Escape => {
                // Esc during an eraser stroke puts the marks back instead of
                // closing the editor.
                if self.drag.is_some() {
                    self.cancel_gesture();
                    return Step::Continue;
                }
                // A composition Esc that slipped past the UI thread's guard
                // still only cancels the pinyin: the text box stays open.
                if self.with_doc(|doc| doc.clear_composition()) {
                    return Step::Continue;
                }
                if self.doc.as_ref().map(|doc| doc.is_typing()) == Some(true) {
                    self.with_doc(|doc| {
                        doc.finish_text();
                        true
                    });
                    return Step::Continue;
                }
                return self.cancel_event();
            }
            Command::Undo => {
                // The IME owns the document while it is composing.
                if self.doc.as_ref().map(|doc| doc.is_composing()) == Some(true) {
                    return Step::Continue;
                }
                self.with_doc(|doc| doc.undo());
            }
            Command::Redo => {
                if self.doc.as_ref().map(|doc| doc.is_composing()) == Some(true) {
                    return Step::Continue;
                }
                self.with_doc(|doc| doc.redo());
            }
            Command::Save => return self.save_step(),
            Command::Stop => return Step::Stop,
        }
        Step::Continue
    }

    fn to_dip(&self, x_px: i32, y_px: i32) -> (f32, f32) {
        let scale = 96.0 / self.dpi;
        (x_px as f32 * scale, y_px as f32 * scale)
    }

    /// Source coordinates under a DIP point, or `None` outside the picture.
    fn source_at(&self, x: f32, y: f32) -> Option<(f32, f32)> {
        let image = self.canvas.image;
        if image.width() <= 0.0 || !image.contains(x, y) {
            return None;
        }
        Some(self.source_clamped(x, y))
    }

    /// Source coordinates, clamped to the picture even outside it.
    fn source_clamped(&self, x: f32, y: f32) -> (f32, f32) {
        let image = self.canvas.image;
        let scale = self.canvas.scale.max(0.0001);
        (
            ((x - image.left) / scale).clamp(0.0, self.source_size.0 as f32),
            ((y - image.top) / scale).clamp(0.0, self.source_size.1 as f32),
        )
    }

    fn pointer_down(&mut self, x_px: i32, y_px: i32) -> Step {
        let (x, y) = self.to_dip(x_px, y_px);
        if let Some(hit) = self.layout.hit(x, y) {
            return self.activate(hit);
        }
        if self.layout.on_pill(x, y) {
            // A press between two buttons belongs to the floating toolbar, not
            // to the desktop behind it.
            return Step::Continue;
        }
        if !self.layout.on_card(x, y) {
            // Anywhere else in the window finishes the edit, exactly like a
            // click outside it (which arrives as focus loss).
            return self.save_step();
        }
        // Any click on the picture ends the text that was being typed.
        if self.doc.as_ref().map(|doc| doc.is_typing()) == Some(true) {
            if let Some(doc) = self.doc.as_mut() {
                doc.finish_text();
            }
            self.texts_dirty = true;
        }
        // The glass frame is part of the card but not part of the picture: a
        // press there does nothing instead of marking the border.
        let Some(source) = self.source_at(x, y) else {
            return Step::Continue;
        };
        match self.tool {
            Tool::Text => {
                if let Some(doc) = self.doc.as_mut() {
                    doc.begin_text(source, self.color, TEXT_SIZE);
                    self.texts_dirty = true;
                }
            }
            Tool::Eraser => {
                // One drag is one undo step: keep the marks as they were and
                // remove whole marks as the disc sweeps over them.
                self.erase_before = self.doc.as_ref().map(|doc| doc.marks().to_vec());
                self.drag = Some(Drag {
                    start: source,
                    current: source,
                });
                self.erase_stroke(source, source);
            }
            _ => {
                self.drag = Some(Drag {
                    start: source,
                    current: source,
                });
            }
        }
        self.content_dirty = true;
        Step::Continue
    }

    fn pointer_move(&mut self, x_px: i32, y_px: i32) {
        let (x, y) = self.to_dip(x_px, y_px);
        if self.drag.is_none() {
            let hover = self.layout.hit(x, y);
            if self.hover != hover {
                self.hover = hover;
                // The hover only lights a pill button: repaint the chrome and
                // keep the photo and every mark as they are.
                self.chrome_dirty = true;
            }
            return;
        }
        let source = self.source_clamped(x, y);
        let previous = match self.drag.as_mut() {
            Some(drag) => {
                let previous = drag.current;
                drag.current = source;
                previous
            }
            None => return,
        };
        if self.tool == Tool::Eraser {
            self.erase_stroke(previous, source);
        }
        // The mark being dragged lives in the card bitmap.
        self.content_dirty = true;
    }

    fn pointer_up(&mut self, x_px: i32, y_px: i32) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        let (x, y) = self.to_dip(x_px, y_px);
        let end = self.source_clamped(x, y);
        if self.tool == Tool::Eraser {
            self.erase_stroke(drag.current, end);
            if let Some(before) = self.erase_before.take() {
                if let Some(doc) = self.doc.as_mut() {
                    doc.record_erase(before);
                }
            }
            self.content_dirty = true;
            return;
        }
        let (left, top, width, height) = drag_rect(drag.start, end);
        let mark = match self.tool {
            Tool::Arrow => {
                let dx = end.0 - drag.start.0;
                let dy = end.1 - drag.start.1;
                if dx * dx + dy * dy < 1.0 {
                    None
                } else {
                    Some(Mark::Arrow {
                        from: drag.start,
                        to: end,
                        color: self.color,
                        width: self.width,
                    })
                }
            }
            Tool::Rectangle if width >= 1.0 && height >= 1.0 => Some(Mark::Rectangle {
                rect: (left, top, width, height),
                color: self.color,
                width: self.width,
            }),
            Tool::Mosaic if width >= 2.0 && height >= 2.0 => Some(Mark::Mosaic {
                rect: (left, top, width, height),
                block: MOSAIC_BLOCK,
            }),
            _ => None,
        };
        if let Some(mark) = mark {
            logging::trace(format!(
                "editor mark {:?} drag {:?}->{:?}",
                match self.tool {
                    Tool::Arrow => "arrow",
                    Tool::Rectangle => "rect",
                    Tool::Text => "text",
                    Tool::Eraser => "eraser",
                    Tool::Mosaic => "mosaic",
                },
                drag.start,
                end
            ));
            if let Some(doc) = self.doc.as_mut() {
                doc.push(mark);
            }
        }
        self.content_dirty = true;
    }

    /// Cancels the gesture in progress, restoring the marks an eraser had
    /// already removed (so Esc or a lost capture never keeps a half erasure).
    fn cancel_gesture(&mut self) {
        self.drag = None;
        if let Some(before) = self.erase_before.take() {
            if let Some(doc) = self.doc.as_mut() {
                doc.restore_marks(before);
            }
            self.texts_dirty = true;
        }
        self.content_dirty = true;
    }

    /// Erases along a line in source pixels so a fast drag leaves no gaps.
    fn erase_stroke(&mut self, from: (f32, f32), to: (f32, f32)) {
        let radius = ERASER_RADIUS_DIP / self.canvas.scale.max(0.0001);
        let (dx, dy) = (to.0 - from.0, to.1 - from.1);
        let distance = (dx * dx + dy * dy).sqrt();
        let steps = ((distance / (radius * 0.6)).ceil() as usize).clamp(1, 48);
        for step in 1..=steps {
            let t = step as f32 / steps as f32;
            self.erase_at((from.0 + dx * t, from.1 + dy * t), radius);
        }
    }

    /// Removes every mark the eraser disc touches at `point`.
    fn erase_at(&mut self, point: (f32, f32), radius: f32) {
        // The flags live on the surface so a fast drag reuses one buffer
        // instead of allocating one vector per step.
        let mut hits = std::mem::take(&mut self.erase_hits);
        hits.clear();
        if let Some(doc) = self.doc.as_ref() {
            hits.extend(
                doc.marks()
                    .iter()
                    .map(|mark| self.mark_hits_eraser(mark, point, radius)),
            );
        }
        if !hits.iter().any(|hit| *hit) {
            self.erase_hits = hits;
            return;
        }
        if let Some(doc) = self.doc.as_mut() {
            let mut index = 0;
            doc.erase_marks(|_| {
                let hit = hits[index];
                index += 1;
                hit
            });
            self.texts_dirty = true;
            self.content_dirty = true;
        }
        self.erase_hits = hits;
    }

    /// Eraser hit test. Text uses its real layout box; the other marks use the
    /// pure geometry test in the model.
    fn mark_hits_eraser(&self, mark: &Mark, point: (f32, f32), radius: f32) -> bool {
        if let Mark::Text {
            origin, text, size, ..
        } = mark
        {
            let (width, height) = self
                .texts
                .iter()
                .find(|entry| entry.text == *text)
                .map(|entry| {
                    let metrics = text_metrics(&entry.layout);
                    (metrics.width, metrics.height)
                })
                .unwrap_or((size * 0.62 * text.chars().count().max(1) as f32, *size));
            return point.0 >= origin.0 - radius
                && point.0 <= origin.0 + width + radius
                && point.1 >= origin.1 - radius
                && point.1 <= origin.1 + height + radius;
        }
        crate::editor::model::mark_hits_eraser(mark, point, radius)
    }

    fn activate(&mut self, hit: Hit) -> Step {
        match hit {
            Hit::Tool(index) => {
                if let Some(tool) = TOOLS.get(index) {
                    self.tool = *tool;
                    // The UI thread reads this for the I-beam and the IME
                    // caret; publishing it here keeps the toolbar geometry in
                    // one place.
                    self.shared
                        .text_tool
                        .store(*tool == Tool::Text, Ordering::Release);
                    // The tool button and its glyph are chrome; the picture and
                    // the marks already on it do not change.
                    self.chrome_dirty = true;
                }
            }
            Hit::Colour(index) => {
                if let Some(entry) = COLORS.get(index) {
                    self.color = entry.0;
                    self.chrome_dirty = true;
                }
            }
            Hit::Width(index) => {
                if let Some(width) = WIDTHS.get(index) {
                    self.width = *width;
                    self.chrome_dirty = true;
                }
            }
            Hit::Undo => {
                self.with_doc(|doc| doc.undo());
            }
            Hit::Redo => {
                self.with_doc(|doc| doc.redo());
            }
            Hit::Cancel => return self.cancel_event(),
            Hit::Save => return self.save_step(),
        }
        Step::Continue
    }

    fn save_step(&mut self) -> Step {
        if let Some(doc) = self.doc.as_mut() {
            doc.finish_text();
        }
        if self.doc.as_ref().map(|doc| doc.marks().is_empty()) == Some(true) {
            // Nothing was drawn: do not re-encode and rewrite the same picture.
            return self.cancel_event();
        }
        match self.save() {
            Ok((png, preview, flight)) => Step::Done(EditorEvent::Saved {
                png,
                preview,
                flight,
            }),
            Err(error) => {
                // The window stays open: the marks are the user's work, and
                // the next outside click may try again.
                self.shared.commit_requested.store(false, Ordering::Release);
                logging::error(format!("editor save: {error}"));
                Step::Continue
            }
        }
    }

    // ---- drawing ----------------------------------------------------------

    /// Whether a frame is pending: either the card content or the window around
    /// it changed.
    fn needs_draw(&self) -> bool {
        self.content_dirty || self.chrome_dirty
    }

    /// Paints what changed. The card pass (photo and marks) runs only when its
    /// content is out of date and otherwise leaves the card bitmap alone; the
    /// window pass always recomposes the frame around that bitmap. Sweeping the
    /// pointer across the pills therefore costs a compose, not a full-resolution
    /// source blit and every mark.
    fn draw(&mut self) -> Result<(), String> {
        if self.texts_dirty {
            self.rebuild_texts();
            self.texts_dirty = false;
            // Text marks and the typing box live in the card bitmap, so a text
            // rebuild always needs the card pass.
            self.content_dirty = true;
        }
        // A surface rebuilt after a device loss has no card bitmap: never
        // compose the window over an empty card.
        if self.surface.card.is_none() {
            self.content_dirty = true;
        }
        let content = self.content_dirty;
        if content {
            self.prepare_mosaics();
            // The photo and the marks are painted into an opaque card bitmap so
            // text can keep ClearType; the window itself stays transparent
            // around the card and the floating pills. The bitmap is persistent,
            // so the next chrome-only frame samples exactly these pixels.
            self.surface.begin_card(self.layout.photo, self.dpi)?;
            self.paint_photo();
            self.surface.end_card()?;
            self.content_dirty = false;
        }
        if !content && !self.chrome_dirty {
            return Ok(());
        }
        self.surface.begin_window()?;
        // The window target is up: from here a failed `EndDraw` or `Present`
        // leaves nothing pending, exactly like a painted frame.
        self.chrome_dirty = false;
        self.paint_chrome();
        self.surface.end()?;
        match self.surface.present(true) {
            crate::render::Present::Ok => {
                self.lost = 0;
                Ok(())
            }
            crate::render::Present::Retry => {
                // The card bitmap kept its content; only the window has to be
                // composed again.
                self.chrome_dirty = true;
                Ok(())
            }
            crate::render::Present::DeviceLost => Err("device lost".to_string()),
        }
    }

    /// Rebuilds the text layouts only when the document's text changed.
    fn rebuild_texts(&mut self) {
        let mut old = std::mem::take(&mut self.texts);
        let mut next: Vec<TextEntry> = Vec::with_capacity(old.len());
        if let Some(doc) = self.doc.as_ref() {
            for mark in doc.marks() {
                let Mark::Text { text, .. } = mark else {
                    continue;
                };
                if let Some(index) = old.iter().position(|entry| entry.text == *text) {
                    next.push(old.remove(index));
                } else if let Ok(layout) =
                    create_text_layout(&self.surface.dwrite, &self.text_format, text)
                {
                    next.push(TextEntry {
                        text: text.clone(),
                        layout,
                    });
                }
            }
        }
        self.texts = next;
        self.rebuild_composition();
    }

    /// Lays out the text mark being typed together with the IME composition.
    /// The cache key is the pair of strings, so each pinyin keystroke rebuilds
    /// exactly one layout and every other frame reuses it.
    fn rebuild_composition(&mut self) {
        let key = self.doc.as_ref().and_then(|doc| {
            let composition = doc.composition()?;
            let committed = match doc.typing_mark() {
                Some(Mark::Text { text, .. }) => text.clone(),
                _ => return None,
            };
            Some((committed, composition.text().to_string()))
        });
        let Some((committed, composition)) = key else {
            self.composing = None;
            return;
        };
        if self.composing.as_ref().is_some_and(|current| {
            current.committed == committed && current.composition == composition
        }) {
            return;
        }
        self.composing = None;
        let text = format!("{committed}{composition}");
        match create_composed_layout(
            &self.surface.dwrite,
            &self.text_format,
            &text,
            committed.len(),
        ) {
            Ok(layout) => {
                let metrics = text_metrics(&layout);
                self.composing = Some(ComposedText {
                    committed,
                    composition,
                    layout,
                    width: metrics.width,
                    height: metrics.height,
                });
            }
            Err(error) => logging::error(format!("editor composition layout: {error}")),
        }
    }

    /// Uploads the mosaic textures the document needs, once per mark.
    fn prepare_mosaics(&mut self) {
        let Some(doc) = self.doc.as_ref() else {
            return;
        };
        let source = doc.source();
        let marks = doc.marks();
        for mark in marks {
            let Mark::Mosaic { rect, block } = mark else {
                continue;
            };
            let Some(key) = mosaic_key(*rect, *block, self.source_size) else {
                continue;
            };
            if self.assets.has_mosaic(key) {
                continue;
            }
            if let Some(image) = build_mosaic(source, key) {
                if let Err(error) = self.assets.upload_mosaic(key, &image) {
                    logging::error(format!("editor mosaic: {error}"));
                } else {
                    logging::trace(format!("editor mosaic texture {key:?}"));
                }
            }
        }
    }

    // ---- saving -----------------------------------------------------------

    /// Encodes the source-resolution PNG and renders the card-sized preview
    /// plus the larger flight image.
    fn save(&mut self) -> Result<(Vec<u8>, ImageData, ImageData), String> {
        if self.texts_dirty {
            self.rebuild_texts();
            self.texts_dirty = false;
        }
        let (source_width, source_height) = self.source_size;
        let width = source_width.max(1);
        let height = source_height.max(1);
        let full = self.render_document(width, height, 1.0, false, true)?;
        let png = wic::encode_png_bytes(&full)?;
        let longest = width.max(height) as f32;
        let scale = (layout::PREVIEW_MAX as f32 / longest).min(1.0);
        let preview_width = ((width as f32 * scale).round() as u32).max(1);
        let preview_height = ((height as f32 * scale).round() as u32).max(1);
        let preview = self.render_document(preview_width, preview_height, scale, true, true)?;
        let flight = self.render_flight_document(true)?;
        Ok((png, preview, flight))
    }

    /// A larger copy of the picture for the flight to or from the line. It is
    /// drawn at up to [`layout::FLIGHT_PREVIEW_MAX`] on the longest side, which
    /// keeps the editor card's enlargement sharp without pushing a 6K surface
    /// through the flight window.
    fn render_flight_document(&mut self, marks: bool) -> Result<ImageData, String> {
        let started = std::time::Instant::now();
        if marks && self.texts_dirty {
            self.rebuild_texts();
            self.texts_dirty = false;
        }
        let (width, height) = self.source_size;
        let longest = width.max(height) as f32;
        let scale = (layout::FLIGHT_PREVIEW_MAX as f32 / longest).min(1.0);
        let flight_width = ((width as f32 * scale).round() as u32).max(1);
        let flight_height = ((height as f32 * scale).round() as u32).max(1);
        let image = self.render_document(flight_width, flight_height, scale, true, marks)?;
        logging::trace(format!(
            "editor flight image {flight_width}x{flight_height} marks={marks} in {:.1} ms",
            started.elapsed().as_secs_f64() * 1000.0
        ));
        Ok(image)
    }

    /// Cancels the edit but keeps a sharp copy of the untouched picture for the
    /// return flight. A render failure still cancels; the app then falls back to
    /// the card thumbnail.
    fn cancel_event(&mut self) -> Step {
        let flight = match self.render_flight_document(false) {
            Ok(image) => Some(image),
            Err(error) => {
                logging::error(format!("editor cancel flight image: {error}"));
                None
            }
        };
        Step::Done(EditorEvent::Cancelled { flight })
    }
}

/// A normalized drag rectangle: (x, y, width, height) in source pixels.
fn drag_rect(start: (f32, f32), end: (f32, f32)) -> (f32, f32, f32, f32) {
    (
        start.0.min(end.0),
        start.1.min(end.1),
        (end.0 - start.0).abs(),
        (end.1 - start.1).abs(),
    )
}

/// Converts a text mark's anchors into client physical pixels for
/// `COMPOSITIONFORM` / `CANDIDATEFORM` (which expect client coordinates, never
/// screen coordinates): the insertion point first, then the point the candidate
/// list hangs from, at the bottom of the white box.
fn ime_client_px(
    canvas: CanvasLayout,
    origin: (f32, f32),
    caret_dx: f32,
    text_h: f32,
    dpi: f32,
) -> ((i32, i32), (i32, i32)) {
    let pad = 4.0;
    let pixel_scale = (dpi / 96.0).max(1.0);
    let client_x = canvas.image.left + (origin.0 + caret_dx.max(0.0)) * canvas.scale;
    let client_top = canvas.image.top + origin.1 * canvas.scale;
    let client_bottom = canvas.image.top + (origin.1 + text_h.max(24.0) + pad + 2.0) * canvas.scale;
    let x = (client_x * pixel_scale).round().max(0.0) as i32;
    (
        (x, (client_top * pixel_scale).round().max(0.0) as i32),
        (x, (client_bottom * pixel_scale).round().max(0.0) as i32),
    )
}

/// X offset, in DIP from the text origin, of the composition caret: the width
/// of the committed text plus the part of the composition before its cursor.
fn composition_caret_x(
    layout: &IDWriteTextLayout,
    committed: &str,
    composition: &str,
    cursor: usize,
) -> Option<f32> {
    let cursor = cursor.min(composition.len());
    let units = committed.encode_utf16().count() + composition[..cursor].encode_utf16().count();
    let mut x = 0.0;
    let mut y = 0.0;
    let mut metrics = DWRITE_HIT_TEST_METRICS::default();
    // SAFETY: live layout and out-params; the offset is inside the text.
    unsafe { layout.HitTestTextPosition(units as u32, false, &mut x, &mut y, &mut metrics) }
        .ok()?;
    Some(x)
}

fn rect_of(rect: (f32, f32, f32, f32)) -> RectF {
    RectF::from_xywh(rect.0, rect.1, rect.2, rect.3)
}

/// Clamps a mark rectangle to the picture and quantizes it for the cache key.
fn mosaic_key(rect: (f32, f32, f32, f32), block: u32, source: (u32, u32)) -> Option<MosaicKey> {
    let (x, y, width, height) = rect;
    if ![x, y, width, height].iter().all(|value| value.is_finite()) {
        return None;
    }
    let left = x.min(x + width).floor().max(0.0) as i32;
    let top = y.min(y + height).floor().max(0.0) as i32;
    let right = ((x.max(x + width)).ceil() as i32).min(source.0 as i32);
    let bottom = ((y.max(y + height)).ceil() as i32).min(source.1 as i32);
    if right - left < 2 || bottom - top < 2 {
        return None;
    }
    Some(MosaicKey {
        x: left,
        y: top,
        width: (right - left) as u32,
        height: (bottom - top) as u32,
        block,
    })
}

/// One mosaic texture: the block averages of a source rectangle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MosaicKey {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    block: u32,
}

impl MosaicKey {
    fn blocks(self) -> (u32, u32) {
        (
            self.width.div_ceil(self.block),
            self.height.div_ceil(self.block),
        )
    }
}

/// Averages every `block x block` cell of `key`'s area in the source image.
fn build_mosaic(source: &ImageData, key: MosaicKey) -> Option<ImageData> {
    let (blocks_x, blocks_y) = key.blocks();
    let bytes = blocks_x as u64 * blocks_y as u64 * 4;
    if bytes > 64 * 1024 * 1024 {
        return None;
    }
    let mut bgra = vec![0u8; bytes as usize];
    let source_stride = source.width as usize;
    for by in 0..blocks_y {
        for bx in 0..blocks_x {
            let x0 = key.x + (bx * key.block) as i32;
            let y0 = key.y + (by * key.block) as i32;
            let x1 = (x0 + key.block as i32).min(key.x + key.width as i32);
            let y1 = (y0 + key.block as i32).min(key.y + key.height as i32);
            let mut sums = [0u64; 4];
            let mut count = 0u64;
            for y in y0..y1 {
                let row = y as usize * source_stride;
                for x in x0..x1 {
                    let index = (row + x as usize) * 4;
                    let pixel = source.bgra.get(index..index + 4)?;
                    for (sum, channel) in sums.iter_mut().zip(pixel) {
                        *sum += u64::from(*channel);
                    }
                    count += 1;
                }
            }
            if count == 0 {
                continue;
            }
            let cell = ((by * blocks_x + bx) * 4) as usize;
            for (channel, sum) in sums.iter().enumerate() {
                bgra[cell + channel] = (sum / count) as u8;
            }
        }
    }
    Some(ImageData::new(blocks_x, blocks_y, bgra))
}

// ---- painting -------------------------------------------------------------

/// The shaft end and the head transform for an arrow, in source pixels.
///
/// The cached head is a unit triangle with its tip at the origin, so the
/// returned transform scales it to `head`, turns local +x down the shaft and
/// puts the tip exactly on `to`. The shaft stops 60 % of a head short of that
/// tip, which keeps its round cap inside the filled triangle.
///
/// `Matrix3x2::rotation` takes **degrees** (it wraps `D2D1MakeRotateMatrix`),
/// while `atan2` returns radians; converting here keeps the two in step.
fn arrow_head(
    from: (f32, f32),
    to: (f32, f32),
    width: f32,
    base: Matrix3x2,
) -> Option<((f32, f32), Matrix3x2)> {
    let dx = to.0 - from.0;
    let dy = to.1 - from.1;
    if dx * dx + dy * dy < 1.0 {
        return None;
    }
    let angle = dy.atan2(dx);
    let head = (width * 3.2).max(18.0);
    let shaft = (
        to.0 - angle.cos() * head * 0.6,
        to.1 - angle.sin() * head * 0.6,
    );
    let transform = Matrix3x2::scale(head, head)
        * Matrix3x2::rotation(angle.to_degrees())
        * Matrix3x2::translation(to.0, to.1)
        * base;
    Some((shaft, transform))
}

/// Per-target resources: the pen brushes, the source bitmap, the mosaics.
struct Assets {
    rt: ID2D1RenderTarget,
    pens: Vec<(u32, ID2D1SolidColorBrush)>,
    mosaics: Vec<(MosaicKey, ID2D1Bitmap)>,
    source: Option<ID2D1Bitmap>,
    source_size: (u32, u32),
}

/// Uploads `image` on the editor's render target.
fn upload(rt: &ID2D1RenderTarget, image: &ImageData) -> Result<ID2D1Bitmap, String> {
    let context: ID2D1DeviceContext = rt
        .cast()
        .map_err(|error| format!("ID2D1RenderTarget -> ID2D1DeviceContext: {error}"))?;
    crate::gfx::upload_bitmap(&context, 96.0, image).map(Into::into)
}

impl Assets {
    fn new(rt: ID2D1RenderTarget) -> Result<Self, String> {
        let mut pens = Vec::with_capacity(COLORS.len());
        for (rgb, _) in COLORS {
            pens.push((rgb, solid(&rt, rgb, 1.0)?));
        }
        Ok(Self {
            rt,
            pens,
            mosaics: Vec::new(),
            source: None,
            source_size: (0, 0),
        })
    }

    /// The pen for `color`; an unknown colour uses the first pen.
    fn brush(&self, color: u32) -> &ID2D1SolidColorBrush {
        self.pens
            .iter()
            .find(|(rgb, _)| *rgb == color)
            .map(|(_, brush)| brush)
            .unwrap_or(&self.pens[0].1)
    }

    fn set_source(&mut self, image: &ImageData) -> Result<(), String> {
        let bitmap = upload(&self.rt, image)?;
        self.source = Some(bitmap);
        self.source_size = (image.width, image.height);
        Ok(())
    }

    fn has_mosaic(&self, key: MosaicKey) -> bool {
        self.mosaics.iter().any(|(cached, _)| *cached == key)
    }

    fn find_mosaic(&self, key: MosaicKey) -> Option<&ID2D1Bitmap> {
        self.mosaics
            .iter()
            .find(|(cached, _)| *cached == key)
            .map(|(_, bitmap)| bitmap)
    }

    fn upload_mosaic(&mut self, key: MosaicKey, image: &ImageData) -> Result<(), String> {
        if self.mosaics.len() >= MOSAIC_CACHE {
            self.mosaics.clear();
        }
        let bitmap = upload(&self.rt, image)?;
        self.mosaics.push((key, bitmap));
        Ok(())
    }
}

// ---- the editor's GPU stack (the device itself comes from `crate::gfx`) ----

/// The editor's own Direct3D + Direct2D + DirectComposition stack.
struct Surface {
    hwnd: HWND,
    swap_chain: IDXGISwapChain1,
    swap_chain3: IDXGISwapChain3,
    context: ID2D1DeviceContext,
    rt: ID2D1RenderTarget,
    factory: ID2D1Factory,
    dwrite: IDWriteFactory,
    stroke_round: ID2D1StrokeStyle,
    stroke_dash: ID2D1StrokeStyle,
    triangle: ID2D1Geometry,
    /// Solid eraser glyph (two slanted quads with a sleeve gap), in design units.
    eraser: ID2D1Geometry,
    targets: [Option<ID2D1Bitmap1>; 2],
    target_props: D2D1_BITMAP_PROPERTIES1,
    /// The opaque photo-plus-marks target; the window itself is transparent.
    card: Option<ID2D1Bitmap1>,
    card_props: D2D1_BITMAP_PROPERTIES1,
    /// The card's baked shadow, the same texture the line uses.
    shadow: Option<crate::render::ShadowBake>,
    /// Wallpaper Mica for the card frame and the floating pills.
    mica: RefCell<crate::mica::Mica>,
    width_px: u32,
    height_px: u32,
    dpi: f32,
    _factory1: ID2D1Factory1,
    _dcomp: IDCompositionDevice,
    _visual: IDCompositionVisual,
    _target: IDCompositionTarget,
    _d3d_context: ID3D11DeviceContext,
    _d2d_device: ID2D1Device,
    _d3d_device: ID3D11Device,
}

impl Surface {
    fn new(hwnd: HWND, width_px: u32, height_px: u32, dpi: u32) -> Result<Self, String> {
        let dpi_f = dpi as f32;
        let (device, immediate) = create_d3d_device()?;
        let dxgi_device: IDXGIDevice = device
            .cast()
            .map_err(|error| format!("ID3D11Device -> IDXGIDevice: {error}"))?;
        let (factory2, dcomp, target, visual) = create_composition(&dxgi_device, hwnd)?;
        let (swap_chain, swap_chain3) = create_swap_chain(&factory2, &device, width_px, height_px)?;
        // The editor's card and paper targets are opaque, so the text marks and
        // the toolbar can use subpixel ClearType even though the window itself
        // is transparent; the saved pictures use a separate alpha target, which
        // stays grayscale.
        let (d2d_factory, d2d_device, context) =
            create_d2d(&dxgi_device, dpi_f, D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE)?;
        let factory: ID2D1Factory = d2d_factory
            .cast()
            .map_err(|error| format!("ID2D1Factory1 -> ID2D1Factory: {error}"))?;
        let rt: ID2D1RenderTarget = context
            .cast()
            .map_err(|error| format!("ID2D1DeviceContext -> ID2D1RenderTarget: {error}"))?;
        // The window is transparent around the card and the pills, so the swap
        // chain is premultiplied; the opaque card bitmap below it keeps
        // ClearType for the text marks.
        let target_props = crate::gfx::swap_chain_props(dpi_f);
        let card_props = crate::gfx::bitmap_target_props(dpi_f);
        let stroke_round = stroke_style(&factory, D2D1_DASH_STYLE_SOLID, None)?;
        let stroke_dash = stroke_style(&factory, D2D1_DASH_STYLE_CUSTOM, Some(&[4.0, 3.0]))?;
        let triangle = create_triangle(&factory)?;
        let eraser = create_eraser(&factory)?;
        let dwrite = create_dwrite()?;
        let shadow = crate::render::bake_card_shadow(&context, dpi_f, 20.0, 5.0)
            .map_err(|error| format!("bake card shadow: {error}"))?;
        let surface = Self {
            hwnd,
            swap_chain,
            swap_chain3,
            context,
            rt,
            factory,
            dwrite,
            stroke_round,
            stroke_dash,
            triangle,
            eraser,
            targets: [None, None],
            target_props,
            card: None,
            card_props,
            shadow: Some(shadow),
            mica: RefCell::new(crate::mica::Mica::new(hwnd, dpi_f)),
            width_px,
            height_px,
            dpi: dpi_f,
            _factory1: d2d_factory,
            _dcomp: dcomp,
            _visual: visual,
            _target: target,
            _d3d_context: immediate,
            _d2d_device: d2d_device,
            _d3d_device: device,
        };
        crate::gfx::attach_content(&surface.swap_chain, &surface._visual, &surface._dcomp)?;
        Ok(surface)
    }

    fn resize(&mut self, width_px: u32, height_px: u32, dpi: u32) -> Result<(), String> {
        let dpi_f = dpi as f32;
        if width_px == self.width_px && height_px == self.height_px && dpi_f == self.dpi {
            return Ok(());
        }
        self.card = None;
        crate::gfx::resize_swap_chain(
            &self.swap_chain,
            &self.context,
            &mut self.targets,
            &mut self.target_props,
            width_px,
            height_px,
            dpi_f,
        )?;
        self.width_px = width_px;
        self.height_px = height_px;
        self.dpi = dpi_f;
        self.card_props.dpiX = self.dpi;
        self.card_props.dpiY = self.dpi;
        match crate::render::bake_card_shadow(&self.context, self.dpi, 20.0, 5.0) {
            Ok(shadow) => self.shadow = Some(shadow),
            // The previous shadow is still usable; a DPI change is the likely
            // cause and the next rebuild can succeed.
            Err(error) => logging::error(format!("editor shadow: {error}")),
        }
        Ok(())
    }

    /// Binds the opaque card bitmap for `photo` and starts painting on it.
    fn begin_card(&mut self, photo: RectF, dpi: f32) -> Result<(), String> {
        let scale = dpi / 96.0;
        let width = ((photo.width() * scale).round() as u32).max(1);
        let height = ((photo.height() * scale).round() as u32).max(1);
        let card = match self.card.take() {
            Some(bitmap) => {
                // SAFETY: a pure query on a live bitmap.
                let size = unsafe { bitmap.GetPixelSize() };
                if size.width == width && size.height == height {
                    bitmap
                } else {
                    self.create_card(width, height)?
                }
            }
            None => self.create_card(width, height)?,
        };
        // SAFETY: the card is a target for this context and stays alive.
        unsafe {
            self.context.SetTarget(&card);
            self.context.SetDpi(self.dpi, self.dpi);
            self.context.SetTransform(&Matrix3x2::identity());
            self.context
                .SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE);
            self.context.BeginDraw();
            self.context.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            }));
        }
        self.card = Some(card);
        Ok(())
    }

    fn end_card(&mut self) -> Result<(), String> {
        crate::gfx::end_draw(&self.context, "EndDraw(card)")
    }

    fn create_card(&mut self, width: u32, height: u32) -> Result<ID2D1Bitmap1, String> {
        // SAFETY: the context is ours and the target bitmap is created with the
        // photo's exact pixel size.
        unsafe {
            self.context
                .CreateBitmap(D2D_SIZE_U { width, height }, None, 0, &self.card_props)
        }
        .map_err(|error| format!("CreateBitmap(card {width}x{height}): {error}"))
    }

    /// Binds the transparent swap-chain buffer and starts a window frame.
    fn begin_window(&mut self) -> Result<(), String> {
        crate::gfx::bind_back_buffer(
            &self.swap_chain,
            &self.swap_chain3,
            &self.context,
            &mut self.targets,
            &self.target_props,
            self.dpi,
        )?;
        self.mica.borrow_mut().refresh(&self.context);
        // Text sits on the translucent pills; ClearType is reserved for the
        // opaque card bitmap.
        crate::gfx::begin_draw(
            &self.context,
            Some(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE),
            Some(0.0),
        );
        Ok(())
    }

    fn end(&mut self) -> Result<(), String> {
        crate::gfx::end_draw(&self.context, "EndDraw")
    }

    fn present(&self, sync: bool) -> crate::render::Present {
        crate::gfx::present(&self.swap_chain, sync, "editor Present")
    }
}

fn stroke_style(
    factory: &ID2D1Factory,
    dash: D2D1_DASH_STYLE,
    dashes: Option<&[f32]>,
) -> Result<ID2D1StrokeStyle, String> {
    let properties = D2D1_STROKE_STYLE_PROPERTIES {
        startCap: D2D1_CAP_STYLE_ROUND,
        endCap: D2D1_CAP_STYLE_ROUND,
        dashCap: D2D1_CAP_STYLE_ROUND,
        lineJoin: D2D1_LINE_JOIN_ROUND,
        miterLimit: 2.0,
        dashStyle: dash,
        dashOffset: 0.0,
    };
    // SAFETY: the factory is alive and the dash pattern slice is live for the
    // call, which copies it.
    unsafe { factory.CreateStrokeStyle(&properties, dashes) }
        .map_err(|error| format!("CreateStrokeStyle: {error}"))
}

/// The unit arrow head: tip at the origin, base at x = -1, y = ±0.5.
fn create_triangle(factory: &ID2D1Factory) -> Result<ID2D1Geometry, String> {
    // SAFETY: the factory is alive; the sink is opened and closed before the
    // geometry is used.
    let geometry = unsafe { factory.CreatePathGeometry() }
        .map_err(|error| format!("CreatePathGeometry: {error}"))?;
    let points = [
        Vector2 { X: 0.0, Y: 0.0 },
        Vector2 { X: -1.0, Y: -0.5 },
        Vector2 { X: -1.0, Y: 0.5 },
    ];
    // SAFETY: `geometry` and its sink are live; `points` outlives the calls.
    unsafe {
        let sink = geometry
            .Open()
            .map_err(|error| format!("PathGeometry::Open: {error}"))?;
        sink.BeginFigure(points[0], D2D1_FIGURE_BEGIN_FILLED);
        sink.AddLines(&points[1..]);
        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
        sink.Close()
            .map_err(|error| format!("PathGeometry::Close: {error}"))?;
    }
    geometry
        .cast()
        .map_err(|error| format!("ID2D1PathGeometry -> ID2D1Geometry: {error}"))
}

/// Lucide `eraser` (ISC), traced with the same arcs as its SVG `d` attribute:
/// a slanted rounded block with a flat bottom and a sleeve band.
fn create_eraser(factory: &ID2D1Factory) -> Result<ID2D1Geometry, String> {
    // SAFETY: the factory is alive; the sink is opened and closed before the
    // geometry is used.
    let geometry = unsafe { factory.CreatePathGeometry() }
        .map_err(|error| format!("CreatePathGeometry(eraser): {error}"))?;
    let arc = |x: f32, y: f32| D2D1_ARC_SEGMENT {
        point: Vector2 { X: x, Y: y },
        size: D2D_SIZE_F {
            width: 2.0,
            height: 2.0,
        },
        rotationAngle: 0.0,
        sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
        arcSize: D2D1_ARC_SIZE_SMALL,
    };
    // SAFETY: `geometry` and its sink are live for the calls.
    unsafe {
        let sink = geometry
            .Open()
            .map_err(|error| format!("PathGeometry::Open(eraser): {error}"))?;
        sink.BeginFigure(Vector2 { X: 21.0, Y: 21.0 }, D2D1_FIGURE_BEGIN_FILLED);
        sink.AddLine(Vector2 { X: 8.0, Y: 21.0 });
        sink.AddArc(&arc(6.58, 20.41));
        sink.AddLine(Vector2 { X: 2.59, Y: 16.41 });
        sink.AddArc(&arc(2.59, 13.59));
        sink.AddLine(Vector2 { X: 12.59, Y: 3.59 });
        sink.AddArc(&arc(15.41, 3.59));
        sink.AddLine(Vector2 { X: 21.41, Y: 9.59 });
        sink.AddArc(&arc(21.41, 12.41));
        sink.AddLine(Vector2 { X: 12.83, Y: 21.0 });
        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
        // The sleeve band is a second, open stroke.
        sink.BeginFigure(Vector2 { X: 5.08, Y: 11.09 }, D2D1_FIGURE_BEGIN_FILLED);
        sink.AddLine(Vector2 { X: 13.91, Y: 19.92 });
        sink.EndFigure(D2D1_FIGURE_END_OPEN);
        sink.Close()
            .map_err(|error| format!("PathGeometry::Close(eraser): {error}"))?;
    }
    geometry
        .cast()
        .map_err(|error| format!("ID2D1PathGeometry -> ID2D1Geometry: {error}"))
}

// ---- small helpers --------------------------------------------------------

fn wic_factory() -> Result<IWICImagingFactory, String> {
    // SAFETY: COM is initialized on the render thread before this is called; the
    // WIC class id is static and no aggregation is requested.
    unsafe {
        CoCreateInstance(
            &CLSID_WICImagingFactory2,
            None::<&windows::core::IUnknown>,
            CLSCTX_INPROC_SERVER,
        )
    }
    .map_err(|error| format!("CoCreateInstance(WIC): {error}"))
}

/// Lays out `text` with the range starting at `underline_from` underlined: the
/// IME composition. The run keeps the mark's font and colour; only the
/// underline marks it as not yet committed.
fn create_composed_layout(
    dwrite: &IDWriteFactory,
    format: &IDWriteTextFormat,
    text: &str,
    underline_from: usize,
) -> Result<IDWriteTextLayout, String> {
    let layout = create_text_layout(dwrite, format, text)?;
    let start = underline_from.min(text.len());
    let start_position = text[..start].encode_utf16().count() as u32;
    let length = text[start..].encode_utf16().count() as u32;
    if length > 0 {
        // SAFETY: the layout is live and the range lies inside its text.
        unsafe {
            let _ = layout.SetUnderline(
                true,
                DWRITE_TEXT_RANGE {
                    startPosition: start_position,
                    length,
                },
            );
        }
    }
    Ok(layout)
}

fn label(dwrite: &IDWriteFactory, format: &IDWriteTextFormat, text: &str) -> Result<Label, String> {
    let layout = create_text_layout(dwrite, format, text)?;
    let metrics = text_metrics(&layout);
    Ok(Label {
        layout,
        width: metrics.width,
        height: metrics.height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    // The device plumbing lives in `crate::gfx`; this one test still builds a
    // DirectWrite factory of its own to lay out text without a D3D device.
    use windows::Win32::Graphics::DirectWrite::{DWRITE_FACTORY_TYPE_SHARED, DWriteCreateFactory};

    #[test]
    fn drags_normalize_negative_directions() {
        assert_eq!(drag_rect((10.0, 20.0), (4.0, 5.0)), (4.0, 5.0, 6.0, 15.0));
        assert_eq!(drag_rect((1.0, 1.0), (1.0, 1.0)), (1.0, 1.0, 0.0, 0.0));
    }

    /// Row-vector transform of one point through a `Matrix3x2`.
    fn transformed(matrix: Matrix3x2, x: f32, y: f32) -> (f32, f32) {
        (
            x * matrix.M11 + y * matrix.M21 + matrix.M31,
            x * matrix.M12 + y * matrix.M22 + matrix.M32,
        )
    }

    #[test]
    fn arrow_heads_point_down_the_shaft_from_the_drag_end() {
        // The screenshot that reported the bug: four arrows pulled out of the
        // centre, plus one diagonal. Every head must sit on the end of its own
        // shaft, pointing the way the drag went, not off to the right.
        let drags = [
            ((100.0f32, 100.0f32), (300.0f32, 100.0f32)),
            ((300.0, 100.0), (100.0, 100.0)),
            ((200.0, 100.0), (200.0, 10.0)),
            ((200.0, 100.0), (200.0, 300.0)),
            ((50.0, 60.0), (140.0, 118.0)),
        ];
        for (from, to) in drags {
            let width = 8.0f32;
            let (shaft_end, transform) =
                arrow_head(from, to, width, Matrix3x2::identity()).expect("an arrow head");
            let head = (width * 3.2).max(18.0);
            let (dx, dy) = (to.0 - from.0, to.1 - from.1);
            let length = (dx * dx + dy * dy).sqrt();
            let direction = (dx / length, dy / length);
            let trailing = (to.0 - direction.0 * head, to.1 - direction.1 * head);

            // The triangle's tip is the drag end and its base trails back along
            // the shaft, so the head covers the shaft instead of missing it.
            let tip = transformed(transform, 0.0, 0.0);
            assert!(
                (tip.0 - to.0).abs() < 0.05 && (tip.1 - to.1).abs() < 0.05,
                "tip {tip:?} is not the drag end {to:?}"
            );
            let base = transformed(transform, -1.0, 0.0);
            assert!(
                (base.0 - trailing.0).abs() < 0.05 && (base.1 - trailing.1).abs() < 0.05,
                "head base {base:?} does not trail {to:?} along {direction:?}"
            );
            // The shaft stops inside the head, short of the tip.
            let along = (shaft_end.0 - from.0) * direction.0 + (shaft_end.1 - from.1) * direction.1;
            assert!(
                along > length - head && along < length,
                "shaft ends outside the head"
            );
        }
    }

    #[test]
    fn mosaic_keys_clamp_and_reject_tiny_rects() {
        let key = mosaic_key((3.2, 4.8, 10.4, 9.6), 16, (100, 100)).expect("key");
        assert_eq!((key.x, key.y, key.width, key.height), (3, 4, 11, 11));
        assert_eq!(mosaic_key((3.0, 3.0, 1.0, 1.0), 16, (100, 100)), None);
        assert_eq!(
            mosaic_key((90.0, 90.0, 100.0, 100.0), 16, (100, 100)).map(|k| (k.width, k.height)),
            Some((10, 10))
        );
    }

    #[test]
    fn mosaic_averages_source_blocks() {
        let mut bgra = Vec::with_capacity(4 * 4);
        for red in [0u8, 10, 20, 30] {
            bgra.extend_from_slice(&[red, 0, 0, 255]);
        }
        let source = ImageData::new(4, 1, bgra);
        let key = MosaicKey {
            x: 0,
            y: 0,
            width: 4,
            height: 1,
            block: 2,
        };
        let mosaic = build_mosaic(&source, key).expect("mosaic");
        assert_eq!((mosaic.width, mosaic.height), (2, 1));
        assert_eq!(mosaic.bgra[0], 5);
        assert_eq!(mosaic.bgra[4], 25);
        assert_eq!(mosaic.bgra[3], 255);
    }

    #[test]
    fn ime_anchors_put_the_composition_at_the_caret_and_the_list_below_the_box() {
        let (width, height) = layout::window_size((697, 584), (1920.0, 1040.0));
        let fitted = layout::fit(width, height, (697, 584));
        let canvas = CanvasLayout {
            image: fitted.photo,
            scale: fitted.scale,
        };
        let ((x96, y96), (cx96, cy96)) = ime_client_px(canvas, (100.0, 120.0), 96.0, 38.0, 96.0);
        let ((x192, y192), (cx192, cy192)) =
            ime_client_px(canvas, (100.0, 120.0), 96.0, 38.0, 192.0);
        assert!(x96 < pixels(width, 96));
        assert!(y96 < pixels(height, 96));
        assert!(
            (x192 - x96 * 2).abs() <= 1,
            "the anchors must follow the DPI"
        );
        assert!((y192 - y96 * 2).abs() <= 1);
        assert!((cx192 - cx96 * 2).abs() <= 1);
        assert!((cy192 - cy96 * 2).abs() <= 1);
        // The composition point is the top of the text line; the candidate
        // point is the bottom of the white box: four DIP of padding plus a two
        // DIP gap below the 38 DIP line.
        let top_px = ((canvas.image.top + 120.0 * canvas.scale) * 2.0).round() as i32;
        let bottom_px =
            ((canvas.image.top + (120.0 + 38.0 + 6.0) * canvas.scale) * 2.0).round() as i32;
        assert!((y192 - top_px).abs() <= 1);
        assert!((cy192 - bottom_px).abs() <= 1);
        assert_eq!(cx96, x96, "the list hangs from the caret's column");
        assert!(
            cy96 > y96 + 30,
            "the list must clear the 38 DIP text line (got {} vs {})",
            cy96,
            y96
        );
    }

    #[test]
    fn a_composition_widens_the_box_and_moves_the_caret_with_it() {
        // DirectWrite lays text out without a window or a GPU device.
        let dwrite: IDWriteFactory =
            unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }.expect("DWrite factory");
        let format = create_text_format(
            &dwrite,
            font::UI_FAMILY,
            TEXT_SIZE,
            DWRITE_FONT_WEIGHT_SEMI_BOLD,
        )
        .expect("text format");
        let plain = create_text_layout(&dwrite, &format, "ab").expect("plain layout");
        let composed = create_composed_layout(&dwrite, &format, "abni'hao", 2).expect("layout");
        let plain_w = text_metrics(&plain).width;
        let composed_w = text_metrics(&composed).width;
        assert!(
            composed_w > plain_w + 10.0,
            "the pinyin must widen the box ({plain_w} -> {composed_w})"
        );
        // The caret sits after the part of the composition before its cursor.
        let at_end = composition_caret_x(&composed, "ab", "ni'hao", 6).expect("caret");
        let at_start = composition_caret_x(&composed, "ab", "ni'hao", 0).expect("caret");
        assert!((at_end - composed_w).abs() < 1.0);
        assert!(at_start < at_end);
        assert!(
            (at_start - plain_w).abs() < 1.0,
            "the caret starts on the committed text"
        );
    }

    #[test]
    fn utf16_offsets_land_on_byte_boundaries() {
        assert_eq!(utf16_to_byte("abc", 0), 0);
        assert_eq!(utf16_to_byte("abc", 2), 2);
        assert_eq!(utf16_to_byte("abc", 9), 3);
        // One CJK character is three bytes but one UTF-16 unit.
        assert_eq!(utf16_to_byte("测试", 1), 3);
        assert_eq!(utf16_to_byte("测试", 2), 6);
        // A surrogate pair is two UTF-16 units and four bytes.
        assert_eq!(utf16_to_byte("😀x", 0), 0);
        assert_eq!(utf16_to_byte("😀x", 2), 4);
    }
}
