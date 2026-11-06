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

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D_SIZE_F, D2D_SIZE_U, D2D1_ALPHA_MODE, D2D1_ALPHA_MODE_IGNORE,
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_FIGURE_BEGIN_FILLED, D2D1_FIGURE_END_CLOSED,
    D2D1_FIGURE_END_OPEN, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_ARC_SEGMENT, D2D1_ARC_SIZE_SMALL,
    D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
    D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES,
    D2D1_BITMAP_PROPERTIES1, D2D1_CAP_STYLE_ROUND, D2D1_DASH_STYLE, D2D1_DASH_STYLE_CUSTOM,
    D2D1_DASH_STYLE_SOLID, D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_DRAW_TEXT_OPTIONS_NONE,
    D2D1_ELLIPSE, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT,
    D2D1_LAYER_OPTIONS_NONE, D2D1_LAYER_PARAMETERS, D2D1_LINE_JOIN_ROUND,
    D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_SOFTWARE, D2D1_RENDER_TARGET_USAGE_NONE,
    D2D1_ROUNDED_RECT, D2D1_STROKE_STYLE_PROPERTIES, D2D1_SWEEP_DIRECTION_CLOCKWISE,
    D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, D2D1CreateFactory,
    ID2D1Bitmap, ID2D1Bitmap1, ID2D1Device, ID2D1DeviceContext, ID2D1Factory, ID2D1Factory1,
    ID2D1Geometry, ID2D1Image, ID2D1RenderTarget, ID2D1SolidColorBrush, ID2D1StrokeStyle,
};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
    ID3D11DeviceContext,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT, DWRITE_FONT_WEIGHT_MEDIUM, DWRITE_FONT_WEIGHT_SEMI_BOLD,
    DWRITE_TEXT_METRICS, DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, IDWriteTextLayout,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET, DXGI_ERROR_WAS_STILL_DRAWING, DXGI_PRESENT,
    DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
    DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIAdapter, IDXGIDevice, IDXGIFactory2, IDXGIOutput,
    IDXGISurface, IDXGISwapChain1, IDXGISwapChain3,
};
use windows::Win32::Graphics::Gdi::{ScreenToClient, ValidateRect};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory2, GUID_WICPixelFormat32bppBGRA, GUID_WICPixelFormat32bppPBGRA,
    IWICImagingFactory, IWICPalette, WICBitmapCacheOnLoad, WICBitmapDitherTypeNone,
    WICBitmapPaletteTypeCustom, WICRect,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::Ime::{
    CANDIDATEFORM, CFS_CANDIDATEPOS, CFS_POINT, COMPOSITIONFORM, HIMC, ImmAssociateContext,
    ImmCreateContext, ImmDestroyContext, ImmGetContext, ImmReleaseContext, ImmSetCandidateWindow,
    ImmSetCompositionWindow, ImmSetOpenStatus,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, SetFocus, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
    VK_CONTROL, VK_ESCAPE, VK_S, VK_Y, VK_Z,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetClientRect, GetCursorPos,
    GetWindowLongPtrW, HTCLIENT, IDC_ARROW, IDC_CROSS, IDC_HAND, IDC_IBEAM, LoadCursorW,
    PostMessageW, SW_HIDE, SW_SHOW, SWP_NOACTIVATE, SWP_NOZORDER, SetCursor, SetForegroundWindow,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, WM_ACTIVATE, WM_APP, WM_CAPTURECHANGED, WM_CHAR,
    WM_CLOSE, WM_DESTROY, WM_DPICHANGED, WM_ERASEBKGND, WM_IME_CHAR, WM_IME_COMPOSITION,
    WM_IME_STARTCOMPOSITION, WM_KEYDOWN, WM_KILLFOCUS, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
    WM_PAINT, WM_SETCURSOR, WM_SETFOCUS, WM_SIZE, WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::{Interface, PCWSTR, w};
use windows_numerics::{Matrix3x2, Vector2};

use crate::capture::wic;
use crate::editor::layout::{self, Hit, Layout};
use crate::editor::model::{COLORS, Doc, Mark, Tool, WIDTHS};
use crate::font;
use crate::i18n;
use crate::logging;
use crate::types::{ImageData, RectF, RectI};
use crate::win32;
use crate::window;

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
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        path: &Path,
        source_size: (u32, u32),
        dpi: u32,
        monitor: win32::Monitor,
        visible: bool,
        notify: Box<dyn Fn(EditorEvent) + Send>,
    ) -> Result<Self, String> {
        window::register_class(CLASS_NAME, Some(editor_wndproc))?;
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
        // SAFETY: the module handle is this process and the class is registered
        // above; the call has no other preconditions.
        let instance = unsafe { GetModuleHandleW(None) }
            .map_err(|error| format!("GetModuleHandleW: {error}"))?;
        let title = win32::wide(i18n::s().editor_title);
        // A frameless, topmost popup: the editor is a floating hanging picture,
        // not a document window. It has no caption buttons and no border; the
        // card, the pin and the floating pills are the whole image.
        // SAFETY: the class is registered, `title` is NUL-terminated and alive
        // for the call, and no parent or menu is requested.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_NOREDIRECTIONBITMAP | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                CLASS_NAME,
                PCWSTR(title.as_ptr()),
                WS_POPUP,
                left,
                top,
                width,
                height,
                None,
                None,
                Some(HINSTANCE(instance.0)),
                None,
            )
        }
        .map_err(|error| format!("CreateWindowExW(editor): {error}"))?;

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
            caret_x: AtomicI32::new(-1),
            caret_y: AtomicI32::new(-1),
        });
        // SAFETY: the pointer stays valid while the window exists: `Editor`
        // holds the `Arc` and `shutdown` destroys the window before that `Arc`
        // is dropped.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, Arc::as_ptr(&shared) as isize) };

        // A plain Win32 window does not get an IME context on Windows 11 TSF
        // unless it asks for one. Without this Chinese/Japanese input never
        // starts composing in the text tool.
        // SAFETY: our window and a fresh context owned by this thread.
        let ime = unsafe { ImmCreateContext() };
        if !ime.0.is_null() {
            // SAFETY: the HWND is ours; the returned previous context is not
            // owned by us and is ignored.
            unsafe {
                let _ = ImmAssociateContext(hwnd, ime);
                // A fresh context starts closed; open it so the active input
                // method actually composes instead of passing Latin keys.
                let _ = ImmSetOpenStatus(ime, true);
            }
            shared.ime.store(ime.0 as isize, Ordering::Release);
        } else {
            logging::error("editor: ImmCreateContext failed");
        }

        let (client_width, client_height) = client_size(hwnd).unwrap_or((1, 1));
        let render_dpi = win32::window_dpi(hwnd).max(96);
        let start = Start {
            hwnd: SendHwnd(hwnd),
            path: path.to_path_buf(),
            width_px: client_width,
            height_px: client_height,
            dpi: render_dpi,
            source_size,
        };
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread_shared = Arc::clone(&shared);
        // The fallback `report` runs after the render thread ends; it needs a
        // `Send` handle, not the raw `HWND` captured directly (which would make
        // the closure non-`Send`).
        let fallback_hwnd = SendHwnd(hwnd);
        let join = match thread::Builder::new()
            .name("pegline-editor".to_string())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    render_thread(start, rx, Arc::clone(&thread_shared), ready_tx);
                }));
                if outcome.is_err() {
                    logging::error("panic in the editor render thread");
                }
                // A thread that ends without a result (a panic, a WIC failure)
                // must still close the window instead of leaving it stuck.
                if !thread_shared.stopping.load(Ordering::Acquire)
                    && !thread_shared.reported.load(Ordering::Acquire)
                {
                    report(
                        &thread_shared,
                        fallback_hwnd.handle(),
                        EditorEvent::Cancelled { flight: None },
                    );
                }
            }) {
            Ok(join) => join,
            Err(error) => {
                // SAFETY: the window exists and no other thread uses it.
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                return Err(format!("spawn editor thread: {error}"));
            }
        };

        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                shared.stopping.store(true, Ordering::Release);
                let _ = join.join();
                // SAFETY: the render thread has stopped using the window.
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                return Err(error);
            }
            Err(_) => {
                shared.stopping.store(true, Ordering::Release);
                let _ = join.join();
                // SAFETY: the render thread has stopped using the window.
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                return Err("editor render thread died during setup".to_string());
            }
        }

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
        let dpi = win32::window_dpi(self.hwnd).max(96);
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
        self.shared.stopping.store(true, Ordering::Release);
        let _ = self.shared.send(Command::Stop);
        if let Some(join) = self.join.take() {
            if join.join().is_err() {
                logging::error("editor render thread panicked");
            }
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
    /// Last text caret position in client pixels, for the IME candidate window.
    caret_x: AtomicI32,
    caret_y: AtomicI32,
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
    FinishText,
    Escape,
    Undo,
    Redo,
    Save,
    Cancel,
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
    match message {
        WM_ERASEBKGND => return LRESULT(1),
        WM_PAINT => {
            // SAFETY: clearing our own update region.
            unsafe {
                let _ = ValidateRect(Some(hwnd), None);
            }
            return LRESULT(0);
        }
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
        WM_ACTIVATE => {
            // Clicking anywhere outside the editor — another app, the desktop,
            // the line — finishes the edit exactly like clicking the empty part
            // of this window. Not before the window has had focus once: the
            // activation churn while the opening flight lands would otherwise
            // save the picture before anyone could draw on it.
            let active = (wparam.0 as u32) & 0xFFFF;
            logging::trace(format!("editor activate {active}"));
            if active == 0 && shared.focus_ready.load(Ordering::Acquire) {
                request_save(shared);
            }
            return LRESULT(0);
        }
        WM_SETFOCUS => {
            logging::trace("editor focus");
            shared.focus_ready.store(true, Ordering::Release);
            return LRESULT(0);
        }
        WM_KILLFOCUS => {
            logging::trace("editor focus lost");
            if shared.focus_ready.load(Ordering::Acquire) {
                request_save(shared);
            }
            return LRESULT(0);
        }
        WM_CLOSE => {
            request_save(shared);
            return LRESULT(0);
        }
        WM_APP_EDITOR_DONE => {
            deliver_pending(shared, hwnd);
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
            return LRESULT(0);
        }
        WM_LBUTTONDOWN => {
            let (x, y) = mouse_xy(lparam);
            // The render thread owns the tool state and decides what the click
            // hit; it publishes the Text-tool flag that drives the I-beam and
            // the IME caret. Keeping the hit math in one place means the
            // responsive toolbar geometry cannot drift from input.
            shared.captured.store(true, Ordering::Release);
            // SAFETY: capturing the mouse for our own window.
            unsafe { SetCapture(hwnd) };
            shared.send(Command::PointerDown { x, y });
            return LRESULT(0);
        }
        WM_SETCURSOR => {
            // Hand over the floating pills, cross/I-beam over the photo and a
            // plain arrow in the transparent gaps around the card.
            if (lparam.0 as u32 & 0xFFFF) == HTCLIENT {
                let mut point = POINT::default();
                // SAFETY: pure queries on our own window and the thread cursor.
                unsafe {
                    let _ = GetCursorPos(&mut point);
                    let _ = ScreenToClient(hwnd, &mut point);
                }
                let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96) as f32;
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
            return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
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
            return LRESULT(0);
        }
        WM_MOUSELEAVE => {
            shared.send(Command::HoverClear);
            return LRESULT(0);
        }
        WM_LBUTTONUP => {
            let (x, y) = mouse_xy(lparam);
            shared.captured.store(false, Ordering::Release);
            // SAFETY: balances `SetCapture`; releasing without capture is a no-op.
            unsafe {
                let _ = ReleaseCapture();
            }
            shared.send(Command::PointerUp { x, y });
            return LRESULT(0);
        }
        WM_CAPTURECHANGED => {
            if shared.captured.swap(false, Ordering::AcqRel) {
                shared.send(Command::PointerCancel);
            }
            return LRESULT(0);
        }
        WM_CHAR => {
            forward_char(shared, wparam);
            return LRESULT(0);
        }
        WM_IME_STARTCOMPOSITION | WM_IME_COMPOSITION => {
            position_ime(hwnd, shared);
        }
        WM_IME_CHAR => {
            // A composing IME (e.g. Microsoft Pinyin) commits the chosen
            // characters as WM_IME_CHAR, not WM_CHAR. Without this the text
            // tool silently dropped Chinese/Japanese input.
            let code = (wparam.0 as u32) & 0xFFFF;
            if let Some(character) = char::from_u32(code) {
                shared.send(Command::Char(character));
            }
            return LRESULT(0);
        }
        WM_KEYDOWN => {
            // SAFETY: `GetKeyState` is a thread-local query with no preconditions.
            let control = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
            let key = (wparam.0 as u32) & 0xFFFF;
            if key == VK_ESCAPE.0 as u32 {
                shared.send(Command::Escape);
                return LRESULT(0);
            }
            if control && key == VK_Z.0 as u32 {
                shared.send(Command::Undo);
                return LRESULT(0);
            }
            if control && key == VK_Y.0 as u32 {
                shared.send(Command::Redo);
                return LRESULT(0);
            }
            if control && key == VK_S.0 as u32 {
                request_save(shared);
                return LRESULT(0);
            }
        }
        _ => {}
    }
    // SAFETY: default processing for messages the editor does not handle.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
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

/// Hands the queued result to the app, then destroys the window.
fn deliver_pending(shared: &Shared, hwnd: HWND) {
    if shared.stopping.load(Ordering::Acquire) {
        return;
    }
    let event = match shared.event.lock() {
        Ok(mut slot) => slot.take(),
        Err(_) => None,
    };
    if let Some(event) = event {
        deliver(shared, hwnd, event);
    }
}

/// Calls `notify` exactly once on the UI thread. The window is deliberately
/// left standing: the app starts the flight back to the line, hides this window
/// when the flight's first frame is up, and destroys it when the flight ends.
/// Destroying it here would leave one frame with neither the editor nor the
/// flight on screen.
fn deliver(shared: &Shared, _hwnd: HWND, event: EditorEvent) {
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

/// Moves the IME composition/candidate windows to the text caret published by
/// the render thread. `COMPOSITIONFORM` and `CANDIDATEFORM` take client-area
/// coordinates relative to `hwnd`, not screen coordinates.
fn position_ime(hwnd: HWND, shared: &Shared) {
    let x = shared.caret_x.load(Ordering::Relaxed);
    let y = shared.caret_y.load(Ordering::Relaxed);
    logging::trace(format!("IME position {x},{y}"));
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
    // SAFETY: both forms are live for the calls; the HIMC came from
    // ImmGetContext and is released with the matching HWND.
    unsafe {
        let _ = ImmSetCompositionWindow(context, &composition);
        let _ = ImmSetCandidateWindow(context, &candidate);
        let _ = ImmReleaseContext(hwnd, context);
    }
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

/// DIP to physical pixels, never zero.
fn pixels(dip: f32, dpi: u32) -> i32 {
    ((dip * dpi.max(96) as f32 / 96.0).round() as i32).max(1)
}

// ---- the render thread ----------------------------------------------------

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
            if !state.dirty {
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
            if !state.dirty {
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
            }
            state.dirty = false;
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
        let ui = create_format(dwrite, font::UI_FAMILY, 13.0, DWRITE_FONT_WEIGHT_MEDIUM)?;
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
    doc: Option<Doc>,
    tool: Tool,
    color: u32,
    width: f32,
    drag: Option<Drag>,
    /// Marks as they were when the current eraser gesture began; committed as
    /// one undo step when the gesture ends.
    erase_before: Option<Vec<Mark>>,
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
    dirty: bool,
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
        let text_format = create_format(
            &surface.dwrite,
            font::UI_FAMILY,
            TEXT_SIZE,
            DWRITE_FONT_WEIGHT_SEMI_BOLD,
        )?;
        let chrome = Chrome::new(&surface.rt)?;
        let assets = Assets::new(surface.rt.clone())?;
        let width_px = width_px.max(1);
        let height_px = height_px.max(1);
        let dpi = dpi.max(96) as f32;
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
            doc: None,
            tool: Tool::Arrow,
            color: COLORS[0].0,
            width: WIDTHS[1],
            drag: None,
            erase_before: None,
            hover: None,
            layout,
            photo_mask: None,
            canvas,
            width_px,
            height_px,
            dpi,
            source_size,
            texts_dirty: false,
            dirty: true,
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
        self.photo_mask = unsafe {
            self.surface
                .factory
                .CreateRoundedRectangleGeometry(&rounded)
        }
        .ok()
        .and_then(|geometry| geometry.cast().ok());
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
        self.texts_dirty = true;
        self.dirty = true;
        logging::trace(format!(
            "editor picture: {}x{}",
            self.source_size.0, self.source_size.1
        ));
        Ok(())
    }

    fn resize(&mut self, width_px: u32, height_px: u32, dpi: u32) -> Result<(), String> {
        let width_px = width_px.max(1);
        let height_px = height_px.max(1);
        let dpi = dpi.max(96);
        if width_px == self.width_px && height_px == self.height_px && dpi as f32 == self.dpi {
            return Ok(());
        }
        self.surface.resize(width_px, height_px, dpi)?;
        self.width_px = width_px;
        self.height_px = height_px;
        self.dpi = dpi as f32;
        self.relayout();
        self.dirty = true;
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
        let text_format = create_format(
            &surface.dwrite,
            font::UI_FAMILY,
            TEXT_SIZE,
            DWRITE_FONT_WEIGHT_SEMI_BOLD,
        )?;
        let chrome = Chrome::new(&surface.rt)?;
        let mut assets = Assets::new(surface.rt.clone())?;
        if let Some(source) = self.doc.as_ref().and_then(|doc| doc.source()) {
            assets.set_source(source)?;
        }
        self.surface = surface;
        self.labels = labels;
        self.text_format = text_format;
        self.chrome = chrome;
        self.assets = assets;
        self.texts.clear();
        self.texts_dirty = true;
        self.relayout();
        self.dirty = true;
        self.lost = 0;
        logging::error("editor surface rebuilt");
        Ok(())
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
                    self.dirty = true;
                }
            }
            Command::Char(character) => {
                if self.doc.as_mut().map(|doc| doc.type_char(character)) == Some(true) {
                    self.texts_dirty = true;
                    self.dirty = true;
                }
            }
            Command::Backspace => {
                if self.doc.as_mut().map(|doc| doc.backspace()) == Some(true) {
                    self.texts_dirty = true;
                    self.dirty = true;
                }
            }
            Command::FinishText => {
                if let Some(doc) = self.doc.as_mut() {
                    if doc.is_typing() {
                        doc.finish_text();
                        self.texts_dirty = true;
                        self.dirty = true;
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
                if self.doc.as_ref().map(|doc| doc.is_typing()) == Some(true) {
                    if let Some(doc) = self.doc.as_mut() {
                        doc.finish_text();
                    }
                    self.texts_dirty = true;
                    self.dirty = true;
                    return Step::Continue;
                }
                return self.cancel_event();
            }
            Command::Undo => {
                if self.doc.as_mut().map(|doc| doc.undo()) == Some(true) {
                    self.texts_dirty = true;
                    self.dirty = true;
                }
            }
            Command::Redo => {
                if self.doc.as_mut().map(|doc| doc.redo()) == Some(true) {
                    self.texts_dirty = true;
                    self.dirty = true;
                }
            }
            Command::Save => return self.save_step(),
            Command::Cancel => return self.cancel_event(),
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
        let scale = self.canvas.scale.max(0.0001);
        Some((
            ((x - image.left) / scale).clamp(0.0, self.source_size.0 as f32),
            ((y - image.top) / scale).clamp(0.0, self.source_size.1 as f32),
        ))
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
        self.dirty = true;
        Step::Continue
    }

    fn pointer_move(&mut self, x_px: i32, y_px: i32) {
        let (x, y) = self.to_dip(x_px, y_px);
        if self.drag.is_none() {
            let hover = self.layout.hit(x, y);
            if self.hover != hover {
                self.hover = hover;
                self.dirty = true;
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
        self.dirty = true;
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
            self.dirty = true;
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
        self.dirty = true;
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
        self.dirty = true;
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
        let Some(doc) = self.doc.as_ref() else {
            return;
        };
        let hits: Vec<bool> = doc
            .marks()
            .iter()
            .map(|mark| self.mark_hits_eraser(mark, point, radius))
            .collect();
        if !hits.iter().any(|hit| *hit) {
            return;
        }
        let Some(doc) = self.doc.as_mut() else {
            return;
        };
        let mut index = 0;
        doc.erase_marks(|_| {
            let hit = hits[index];
            index += 1;
            hit
        });
        self.texts_dirty = true;
        self.dirty = true;
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

    /// The toolbar entry under a DIP point.
    fn hit_action(&self, x: f32, y: f32) -> Option<Hit> {
        self.layout.hit(x, y)
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
                    self.dirty = true;
                }
            }
            Hit::Colour(index) => {
                if let Some(entry) = COLORS.get(index) {
                    self.color = entry.0;
                    self.dirty = true;
                }
            }
            Hit::Width(index) => {
                if let Some(width) = WIDTHS.get(index) {
                    self.width = *width;
                    self.dirty = true;
                }
            }
            Hit::Undo => {
                if self.doc.as_mut().map(|doc| doc.undo()) == Some(true) {
                    self.texts_dirty = true;
                    self.dirty = true;
                }
            }
            Hit::Redo => {
                if self.doc.as_mut().map(|doc| doc.redo()) == Some(true) {
                    self.texts_dirty = true;
                    self.dirty = true;
                }
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

    fn draw(&mut self) -> Result<(), String> {
        if self.texts_dirty {
            self.rebuild_texts();
            self.texts_dirty = false;
        }
        self.prepare_mosaics();
        // The photo and the marks are painted into an opaque card bitmap so
        // text can keep ClearType; the window itself stays transparent around
        // the card and the floating pills.
        self.surface.begin_card(self.layout.photo, self.dpi)?;
        self.paint_photo();
        self.surface.end_card()?;
        self.surface.begin_window()?;
        self.paint_chrome();
        self.surface.end()?;
        match self.surface.present(true) {
            crate::render::Present::Ok => {
                self.lost = 0;
                Ok(())
            }
            crate::render::Present::Retry => {
                self.dirty = true;
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
    }

    /// Uploads the mosaic textures the document needs, once per mark.
    fn prepare_mosaics(&mut self) {
        let Some(source) = self.doc.as_ref().and_then(|doc| doc.source()) else {
            return;
        };
        let Some(marks) = self.doc.as_ref().map(|doc| doc.marks()) else {
            return;
        };
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

    /// Paints the floating window: the card's baked shadow, the glass frame and
    /// its picture, the clothespin and the five toolbar pills.
    fn paint_chrome(&self) {
        let rt = &self.assets.rt;
        // SAFETY: the window target is in a draw and every brush is ours.
        unsafe {
            rt.SetTransform(&Matrix3x2::identity());
        }
        if let Some(shadow) = self.surface.shadow.as_ref() {
            crate::render::draw_shadow(&self.surface.context, shadow, self.layout.card, 0.18);
        }
        let card = self.layout.card;
        let frame = rounded(card, layout::CARD_RADIUS);
        // SAFETY: the brushes and the render target are alive.
        unsafe {
            rt.FillRoundedRectangle(&frame, &self.chrome.glass);
        }
        if let Some(bitmap) = self.surface.card.as_ref() {
            self.paint_card_bitmap(bitmap);
        }
        // SAFETY: as above.
        unsafe {
            rt.DrawRoundedRectangle(&frame, &self.chrome.border, 0.75, None);
        }
        self.paint_pin();
        for pill in self.layout.top_pills {
            self.paint_pill(pill);
        }
        for pill in self.layout.bottom_pills {
            self.paint_pill(pill);
        }
        self.paint_tools();
        self.paint_colours();
        self.paint_widths();
        self.paint_actions();
    }

    /// Draws the opaque photo bitmap inside the rounded photo corners.
    fn paint_card_bitmap(&self, bitmap: &ID2D1Bitmap1) {
        let Some(mask) = self.photo_mask.as_ref() else {
            return;
        };
        let photo = self.layout.photo;
        let parameters = D2D1_LAYER_PARAMETERS {
            contentBounds: d2d_rect(photo),
            geometricMask: ManuallyDrop::new(Some(mask.clone())),
            maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
            maskTransform: Matrix3x2::identity(),
            opacity: 1.0,
            opacityBrush: ManuallyDrop::new(None),
            layerOptions: D2D1_LAYER_OPTIONS_NONE,
        };
        let destination = d2d_rect(photo);
        let source = D2D_RECT_F {
            left: 0.0,
            top: 0.0,
            right: self.layout.photo.width(),
            bottom: self.layout.photo.height(),
        };
        // SAFETY: the layer parameters and the rounded mask are alive for the
        // calls; an internal layer is used because none is passed.
        unsafe {
            self.assets.rt.PushLayer(&parameters, None);
            self.assets.rt.DrawBitmap(
                bitmap,
                Some(&destination),
                1.0,
                D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                Some(&source),
            );
            self.assets.rt.PopLayer();
        }
    }

    fn paint_pill(&self, pill: RectF) {
        let rounded = rounded(pill, layout::PILL_RADIUS);
        // SAFETY: the brushes and the render target are alive.
        unsafe {
            self.assets
                .rt
                .FillRoundedRectangle(&rounded, &self.chrome.glass);
            self.assets
                .rt
                .DrawRoundedRectangle(&rounded, &self.chrome.border, 0.75, None);
        }
    }

    fn paint_pin(&self) {
        let pin = self.layout.pin;
        let body = rounded(pin, 3.5);
        // SAFETY: the brushes and the render target are alive.
        unsafe {
            self.assets
                .rt
                .FillRoundedRectangle(&body, &self.chrome.metal);
            self.assets
                .rt
                .DrawRoundedRectangle(&body, &self.chrome.metal_edge, 0.6, None);
        }
        let slot = RectF::from_xywh(pin.left + 2.0, pin.top + 8.0, pin.width() - 4.0, 1.4);
        self.fill_round(slot, 0.7, &self.chrome.pin_slot);
    }

    fn fill_round(&self, rect: RectF, radius: f32, brush: &ID2D1SolidColorBrush) {
        // SAFETY: the brush and the render target are alive.
        unsafe {
            self.assets
                .rt
                .FillRoundedRectangle(&rounded(rect, radius), brush);
        }
    }

    fn paint_tools(&self) {
        for (index, rect) in self.layout.tools.iter().enumerate() {
            let selected = TOOLS[index] == self.tool;
            let hovered = self.hover == Some(Hit::Tool(index));
            self.paint_button(*rect, selected, hovered);
            let brush = if selected {
                &self.chrome.white
            } else {
                &self.chrome.ink
            };
            self.paint_tool_icon(index, *rect, brush);
        }
    }

    fn paint_colours(&self) {
        for (index, rect) in self.layout.colours.iter().enumerate() {
            let rgb = COLORS[index].0;
            let center = rect.center();
            // The dots are round, concentric with the pill's end radius.
            let disc = D2D1_ELLIPSE {
                point: Vector2 {
                    X: center.0,
                    Y: center.1,
                },
                radiusX: rect.width() * 0.5,
                radiusY: rect.height() * 0.5,
            };
            if let Some(brush) = self.assets.brush(rgb) {
                // SAFETY: the brush and the render target are alive.
                unsafe {
                    self.assets.rt.FillEllipse(&disc, brush);
                }
            }
            let selected = rgb == self.color;
            let hovered = self.hover == Some(Hit::Colour(index));
            // The hairline keeps a white dot readable on the glass; the accent
            // ring sits just outside the dot so it never eats into the colour.
            // SAFETY: the brush and the render target are alive.
            unsafe {
                self.assets
                    .rt
                    .DrawEllipse(&disc, &self.chrome.ink_dim, 1.0, None);
            }
            if selected || hovered {
                let ring = D2D1_ELLIPSE {
                    radiusX: disc.radiusX + 1.6,
                    radiusY: disc.radiusY + 1.6,
                    ..disc
                };
                // SAFETY: the brush and the render target are alive.
                unsafe {
                    self.assets.rt.DrawEllipse(
                        &ring,
                        &self.chrome.accent,
                        if selected { 2.0 } else { 1.5 },
                        None,
                    );
                }
            }
        }
    }

    fn paint_widths(&self) {
        for (index, rect) in self.layout.widths.iter().enumerate() {
            let selected = (WIDTHS[index] - self.width).abs() < 0.01;
            let hovered = self.hover == Some(Hit::Width(index));
            // Keep the button unfilled when selected so the dot's real size
            // stays readable; the accent ring marks the choice instead.
            self.paint_button(*rect, false, hovered);
            if selected {
                let ring = D2D1_ELLIPSE {
                    point: Vector2 {
                        X: rect.center().0,
                        Y: rect.center().1,
                    },
                    radiusX: rect.width() * 0.5 - 1.2,
                    radiusY: rect.height() * 0.5 - 1.2,
                };
                // SAFETY: the brush and the render target are alive.
                unsafe {
                    self.assets
                        .rt
                        .DrawEllipse(&ring, &self.chrome.accent, 2.0, None);
                }
            }
            // A dot whose diameter follows the stroke weight reads at a glance:
            // 4/8/12 source pixels, scaled 1.25x so the 4 px dot stays visible.
            let diameter = (WIDTHS[index] * 1.25).min(rect.height() - 10.0);
            let disc = D2D1_ELLIPSE {
                point: Vector2 {
                    X: rect.center().0,
                    Y: rect.center().1,
                },
                radiusX: diameter * 0.5,
                radiusY: diameter * 0.5,
            };
            let brush = self.assets.brush(self.color).unwrap_or(&self.chrome.ink);
            let outline = if selected {
                &self.chrome.accent
            } else {
                &self.chrome.ink_dim
            };
            // SAFETY: the render target is in a draw and the brushes are alive.
            unsafe {
                self.assets.rt.FillEllipse(&disc, brush);
                self.assets.rt.DrawEllipse(&disc, outline, 1.0, None);
            }
        }
    }

    fn paint_actions(&self) {
        let undo_enabled = self.doc.as_ref().map(|doc| doc.can_undo()) == Some(true);
        let redo_enabled = self.doc.as_ref().map(|doc| doc.can_redo()) == Some(true);
        self.paint_action(
            self.layout.undo,
            &self.labels.undo,
            undo_enabled,
            false,
            self.hover == Some(Hit::Undo),
        );
        self.paint_action(
            self.layout.redo,
            &self.labels.redo,
            redo_enabled,
            false,
            self.hover == Some(Hit::Redo),
        );
        self.paint_action(
            self.layout.cancel,
            &self.labels.cancel,
            true,
            false,
            self.hover == Some(Hit::Cancel),
        );
        self.paint_action(
            self.layout.save,
            &self.labels.save,
            true,
            true,
            self.hover == Some(Hit::Save),
        );
    }

    /// Paints the picture and its marks into the opaque card bitmap. The
    /// transform maps source pixels to the bitmap's DIP space, whose origin is
    /// the photo's top-left corner.
    fn paint_photo(&self) {
        let rt = &self.assets.rt;
        let base = Matrix3x2::scale(self.canvas.scale, self.canvas.scale);
        // SAFETY: the card target is in a draw and the bitmap is alive.
        unsafe {
            rt.SetTransform(&base);
        }
        if let Some(bitmap) = self.assets.source.as_ref() {
            let full = d2d_rect(RectF::from_xywh(
                0.0,
                0.0,
                self.source_size.0 as f32,
                self.source_size.1 as f32,
            ));
            let source = D2D_RECT_F {
                left: 0.0,
                top: 0.0,
                right: self.source_size.0 as f32,
                bottom: self.source_size.1 as f32,
            };
            // SAFETY: the bitmap covers the whole destination.
            unsafe {
                rt.DrawBitmap(
                    bitmap,
                    Some(&full),
                    1.0,
                    D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                    Some(&source),
                );
            }
        }
        let painter = Painter {
            rt,
            assets: &self.assets,
            stroke: &self.surface.stroke_round,
            dash: &self.surface.stroke_dash,
            triangle: &self.surface.triangle,
            base,
        };
        // WPF's text tool showed a white TextBox with a caret while typing;
        // without it the glyphs float on a busy picture with no insertion
        // point. It sits above older marks and below the text being typed, and
        // is on-screen only: `save()` renders the marks alone.
        if let Some(doc) = self.doc.as_ref() {
            match (doc.typing_index(), doc.typing_mark()) {
                (
                    Some(index),
                    Some(Mark::Text {
                        origin,
                        text,
                        color,
                        ..
                    }),
                ) => {
                    painter.marks_except(doc.marks(), &self.texts, index);
                    let (text_w, text_h) = self
                        .texts
                        .iter()
                        .find(|entry| entry.text == *text)
                        .map(|entry| {
                            let metrics = text_metrics(&entry.layout);
                            (metrics.width, metrics.height)
                        })
                        .unwrap_or((8.0, 38.0));
                    let pad = 4.0;
                    let box_rect = RectF::from_xywh(
                        origin.0 - pad,
                        origin.1 - pad,
                        text_w.max(8.0) + pad * 2.0,
                        text_h.max(24.0) + pad * 2.0,
                    );
                    let rounded_box = rounded(box_rect, 3.0);
                    let pen = self.assets.brush(*color);
                    // SAFETY: the render target is in a draw; brushes are live.
                    unsafe {
                        rt.FillRoundedRectangle(&rounded_box, &self.chrome.text_box);
                        if let Some(pen) = pen {
                            rt.DrawRoundedRectangle(&rounded_box, pen, 1.0, None);
                            let caret = d2d_rect(RectF::from_xywh(
                                origin.0 + text_w.max(0.0) + 1.0,
                                origin.1,
                                2.0,
                                text_h.max(18.0),
                            ));
                            rt.FillRectangle(&caret, pen);
                        }
                    }
                    // Publish the bottom of the caret in client pixels so the
                    // IME composition and candidate windows open right below
                    // the text box instead of at (0, 0).
                    let (caret_x, caret_y) =
                        ime_caret_client_px(self.canvas, *origin, text_w, text_h, self.dpi);
                    self.shared.caret_x.store(caret_x, Ordering::Relaxed);
                    self.shared.caret_y.store(caret_y, Ordering::Relaxed);
                    if let Some(mark) = doc.marks().get(index) {
                        painter.mark(mark, &self.texts);
                    }
                }
                _ => painter.marks(doc.marks(), &self.texts),
            }
        }
        if let Some(drag) = self.drag.as_ref() {
            painter.drag(self.tool, self.color, self.width, self.canvas.scale, drag);
        }
        // SAFETY: restores the identity transform for the next target.
        unsafe {
            rt.SetTransform(&Matrix3x2::identity());
        }
    }

    /// Radius that keeps a button concentric with the pill around it: half its
    /// short side, so the tool buttons and the colour dots are circles and the
    /// wide action buttons are stadiums.
    fn button_radius(rect: RectF) -> f32 {
        rect.width().min(rect.height()) * 0.5
    }

    fn paint_button(&self, rect: RectF, selected: bool, hovered: bool) {
        let rounded = rounded(rect, Self::button_radius(rect));
        let fill = if selected {
            Some(if hovered {
                &self.chrome.accent_hover
            } else {
                &self.chrome.accent
            })
        } else if hovered {
            Some(&self.chrome.glass_lit)
        } else {
            None
        };
        // SAFETY: the brushes and the render target are alive.
        unsafe {
            if let Some(fill) = fill {
                self.assets.rt.FillRoundedRectangle(&rounded, fill);
            }
            if selected {
                self.assets.rt.DrawRoundedRectangle(
                    &rounded,
                    &self.chrome.accent_border,
                    1.0,
                    None,
                );
            }
        }
    }

    /// Draws one of the four Lucide tool icons (ISC/MIT; the SVG sources and
    /// license are in `assets/icons/`). Each icon lives on Lucide's 24x24 grid
    /// with a 2 px stroke; here it is centred in the button at 18 DIP and the
    /// stroke is scaled to match, so all four share one optical weight.
    fn paint_tool_icon(&self, index: usize, rect: RectF, brush: &ID2D1SolidColorBrush) {
        let (cx, cy) = rect.center();
        let rt = &self.assets.rt;
        let stroke = &self.surface.stroke_round;
        // The arrow glyph sits in a 14x14 part of Lucide's 24x24 grid, so it is
        // grown a little to share the optical size of the rectangle and grid;
        // the filled eraser is compact, so it gets a slightly bigger box too.
        let design_scale = match index {
            0 => 1.12,
            3 => 1.2,
            _ => 1.0,
        };
        let scale = ICON_BOX / ICON_DESIGN * design_scale;
        let width = ICON_STROKE * ICON_BOX / ICON_DESIGN;
        let point = |x: f32, y: f32| Vector2 {
            X: cx + (x - 12.0) * scale,
            Y: cy + (y - 12.0) * scale,
        };
        let box_rect = |x: f32, y: f32, w: f32, h: f32| {
            RectF::from_xywh(
                cx + (x - 12.0) * scale,
                cy + (y - 12.0) * scale,
                w * scale,
                h * scale,
            )
        };
        let line = |from: (f32, f32), to: (f32, f32)| {
            // SAFETY: the render target is in a draw, the brush is alive and
            // the stroke style belongs to the surface.
            unsafe {
                rt.DrawLine(
                    point(from.0, from.1),
                    point(to.0, to.1),
                    brush,
                    width,
                    Some(stroke),
                );
            }
        };
        match index {
            // Lucide `move-up-right`: M13 5H19V11 and M19 5L5 19.
            0 => {
                line((13.0, 5.0), (19.0, 5.0));
                line((19.0, 5.0), (19.0, 11.0));
                line((19.0, 5.0), (5.0, 19.0));
            }
            // Lucide `rectangle-horizontal`: rect x=2 y=6 w=20 h=12 rx=2.
            1 => {
                let frame = box_rect(2.0, 6.0, 20.0, 12.0);
                // SAFETY: the render target is in a draw and the brush is alive.
                unsafe {
                    rt.DrawRoundedRectangle(
                        &rounded(frame, 2.0 * scale),
                        brush,
                        width,
                        Some(stroke),
                    );
                }
            }
            // Lucide `type`: M12 4v16, M9 20h6 and the rounded top bar
            // M4 7V5a1 1 0 0 1 1-1h14a1 1 0 0 1 1 1v2.
            2 => {
                line((12.0, 4.0), (12.0, 20.0));
                line((9.0, 20.0), (15.0, 20.0));
                line((4.0, 7.0), (4.0, 5.0));
                line((4.0, 5.0), (5.0, 4.0));
                line((5.0, 4.0), (19.0, 4.0));
                line((19.0, 4.0), (20.0, 5.0));
                line((20.0, 5.0), (20.0, 7.0));
            }
            // Lucide `eye-off` for the mosaic/redaction tool: an eye with a
            // slash, drawn as an oval, a pupil and the slash.
            4 => {
                let eye = D2D1_ELLIPSE {
                    point: point(12.0, 12.0),
                    radiusX: 9.2 * scale,
                    radiusY: 5.8 * scale,
                };
                let pupil = D2D1_ELLIPSE {
                    point: point(12.0, 12.0),
                    radiusX: 2.4 * scale,
                    radiusY: 2.4 * scale,
                };
                // SAFETY: the render target is in a draw and the brush is alive.
                unsafe {
                    rt.DrawEllipse(&eye, brush, width, Some(stroke));
                    rt.DrawEllipse(&pupil, brush, width, Some(stroke));
                }
                line((2.0, 2.0), (22.0, 22.0));
            }
            // Lucide `eraser`, drawn with the same outline and sleeve stroke.
            3 => {
                let transform = Matrix3x2::scale(scale, scale)
                    * Matrix3x2::translation(cx - 12.0 * scale, cy - 12.0 * scale);
                // SAFETY: the render target is in a draw and the brush is alive.
                unsafe {
                    rt.SetTransform(&transform);
                    rt.DrawGeometry(
                        &self.surface.eraser,
                        brush,
                        width / scale.max(0.0001),
                        Some(stroke),
                    );
                    rt.SetTransform(&Matrix3x2::identity());
                }
            }
            _ => {}
        }
    }

    fn paint_action(
        &self,
        rect: RectF,
        label: &Label,
        enabled: bool,
        primary: bool,
        hovered: bool,
    ) {
        self.paint_button(rect, primary, hovered);
        let brush = if primary {
            &self.chrome.white
        } else if enabled {
            &self.chrome.ink
        } else {
            &self.chrome.ink_dim
        };
        self.paint_label(label, rect, brush);
    }

    fn paint_label(&self, label: &Label, rect: RectF, brush: &ID2D1SolidColorBrush) {
        let x = rect.left + (rect.width() - label.width) * 0.5;
        let y = rect.top + (rect.height() - label.height) * 0.5 - 1.0;
        // SAFETY: the layout, the brush and the render target are alive.
        unsafe {
            self.assets.rt.DrawTextLayout(
                Vector2 { X: x, Y: y },
                &label.layout,
                brush,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
            );
        }
    }

    // ---- saving -----------------------------------------------------------

    /// Renders the source picture plus marks into an image of `width` x
    /// `height` pixels. `scale` maps source pixels to those pixels: 1.0 for the
    /// saved file, a fraction for the flight preview. Runs on the render
    /// thread, never on the UI thread.
    fn render_document(
        &mut self,
        width: u32,
        height: u32,
        scale: f32,
        smooth: bool,
        marks: bool,
    ) -> Result<ImageData, String> {
        let Some(doc) = self.doc.as_ref() else {
            return Err("there is no picture to save".to_string());
        };
        let source = doc
            .source()
            .ok_or_else(|| "there is no picture to save".to_string())?;
        if !source.is_valid() {
            return Err(format!(
                "invalid source image ({}x{}, {} bytes)",
                source.width,
                source.height,
                source.bgra.len()
            ));
        }
        let (source_width, source_height) = (source.width, source.height);
        let factory = wic_factory()?;
        // SAFETY: the factory is live, the size is at least one pixel and the
        // pixel format is a static WIC GUID.
        let target = unsafe {
            factory.CreateBitmap(
                width,
                height,
                &GUID_WICPixelFormat32bppPBGRA,
                WICBitmapCacheOnLoad,
            )
        }
        .map_err(|error| format!("CreateBitmap({width}x{height}): {error}"))?;
        let properties = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            pixelFormat: pixel_format(D2D1_ALPHA_MODE_PREMULTIPLIED),
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        // SAFETY: the factory and the WIC bitmap are live; the target keeps a
        // reference to the bitmap for as long as it exists.
        let rt = unsafe {
            self.surface
                .factory
                .CreateWicBitmapRenderTarget(&target, &properties)
        }
        .map_err(|error| format!("CreateWicBitmapRenderTarget: {error}"))?;

        let mut assets = Assets::new(rt.clone())?;
        assets.set_source(source)?;
        if marks {
            for mark in doc.marks() {
                let Mark::Mosaic { rect, block } = mark else {
                    continue;
                };
                let Some(key) = mosaic_key(*rect, *block, (source_width, source_height)) else {
                    continue;
                };
                if assets.has_mosaic(key) {
                    continue;
                }
                if let Some(image) = build_mosaic(source, key) {
                    assets.upload_mosaic(key, &image)?;
                }
            }
        }

        let transform = Matrix3x2::scale(scale, scale);
        let full = d2d_rect(RectF::from_xywh(
            0.0,
            0.0,
            source_width as f32,
            source_height as f32,
        ));
        // SAFETY: the target is a fresh WIC render target on this thread.
        unsafe {
            rt.SetDpi(96.0, 96.0);
            rt.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            // The file has an alpha channel; subpixel ClearType would both be
            // unsupported and leave coloured fringes in it.
            rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            rt.SetTransform(&Matrix3x2::identity());
            rt.BeginDraw();
            rt.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }));
            rt.SetTransform(&transform);
        }
        if let Some(bitmap) = assets.source.as_ref() {
            // SAFETY: `bitmap` is alive and covers the full destination.
            unsafe {
                rt.DrawBitmap(
                    bitmap,
                    Some(&full),
                    1.0,
                    if smooth {
                        D2D1_BITMAP_INTERPOLATION_MODE_LINEAR
                    } else {
                        D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR
                    },
                    None,
                );
            }
        }
        let painter = Painter {
            rt: &rt,
            assets: &assets,
            stroke: &self.surface.stroke_round,
            dash: &self.surface.stroke_dash,
            triangle: &self.surface.triangle,
            base: transform,
        };
        if marks {
            painter.marks(doc.marks(), &self.texts);
        }
        // SAFETY: `BeginDraw` was called above on this thread.
        unsafe { rt.EndDraw(None, None) }.map_err(|error| format!("EndDraw(save): {error}"))?;

        // The render target holds premultiplied pixels; PNG wants straight
        // alpha, so the bitmap is converted before it is copied out.
        // SAFETY: the factory is live.
        let converter = unsafe { factory.CreateFormatConverter() }
            .map_err(|error| format!("CreateFormatConverter: {error}"))?;
        // SAFETY: the converter is fresh and the source bitmap is alive.
        unsafe {
            converter.Initialize(
                &target,
                &GUID_WICPixelFormat32bppBGRA,
                WICBitmapDitherTypeNone,
                None::<&IWICPalette>,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
        }
        .map_err(|error| format!("IWICFormatConverter::Initialize: {error}"))?;
        let stride = width * 4;
        let mut bgra = vec![0u8; width as usize * height as usize * 4];
        let rect = WICRect {
            X: 0,
            Y: 0,
            Width: width as i32,
            Height: height as i32,
        };
        // SAFETY: `bgra` is exactly `stride * height` bytes for the size above.
        unsafe { converter.CopyPixels(&rect, stride, &mut bgra) }
            .map_err(|error| format!("CopyPixels({width}x{height}): {error}"))?;
        Ok(ImageData::new(width, height, bgra))
    }

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

/// Converts a text mark's caret and box bottom into client physical pixels for
/// `COMPOSITIONFORM` / `CANDIDATEFORM` (which expect client coordinates, never
/// screen coordinates).
fn ime_caret_client_px(
    canvas: CanvasLayout,
    origin: (f32, f32),
    text_w: f32,
    text_h: f32,
    dpi: f32,
) -> (i32, i32) {
    let pad = 4.0;
    let pixel_scale = (dpi / 96.0).max(1.0);
    let client_x = canvas.image.left + (origin.0 + text_w.max(0.0) + 1.0) * canvas.scale;
    let client_y = canvas.image.top + (origin.1 + text_h.max(24.0) + pad + 2.0) * canvas.scale;
    (
        (client_x * pixel_scale).round().max(0.0) as i32,
        (client_y * pixel_scale).round().max(0.0) as i32,
    )
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
        block: block.max(1),
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
    if blocks_x == 0 || blocks_y == 0 {
        return None;
    }
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

/// Draws marks in source coordinates through the current base transform.
struct Painter<'a> {
    rt: &'a ID2D1RenderTarget,
    assets: &'a Assets,
    stroke: &'a ID2D1StrokeStyle,
    dash: &'a ID2D1StrokeStyle,
    triangle: &'a ID2D1Geometry,
    base: Matrix3x2,
}

impl<'a> Painter<'a> {
    fn marks(&self, marks: &[Mark], texts: &[TextEntry]) {
        for mark in marks {
            self.mark(mark, texts);
        }
    }

    /// Draws every mark except `skip` (the text being typed, which goes above
    /// its input box).
    fn marks_except(&self, marks: &[Mark], texts: &[TextEntry], skip: usize) {
        for (index, mark) in marks.iter().enumerate() {
            if index != skip {
                self.mark(mark, texts);
            }
        }
    }

    fn mark(&self, mark: &Mark, texts: &[TextEntry]) {
        match mark {
            Mark::Arrow {
                from,
                to,
                color,
                width,
            } => self.arrow(*from, *to, *color, *width),
            Mark::Rectangle { rect, color, width } => {
                self.rectangle(*rect, *color, *width);
            }
            Mark::Text {
                origin,
                text,
                color,
                ..
            } => self.text(*origin, text, *color, texts),
            Mark::Mosaic { rect, block } => self.mosaic(*rect, *block),
        }
    }

    fn text(&self, origin: (f32, f32), text: &str, color: u32, texts: &[TextEntry]) {
        let brush = self.assets.brush(color);
        let entry = texts.iter().find(|entry| entry.text == text);
        if let (Some(brush), Some(entry)) = (brush, entry) {
            // SAFETY: the layout, the brush and the target are alive.
            unsafe {
                self.rt.DrawTextLayout(
                    Vector2 {
                        X: origin.0,
                        Y: origin.1,
                    },
                    &entry.layout,
                    brush,
                    D2D1_DRAW_TEXT_OPTIONS_NONE,
                );
            }
        }
    }

    fn arrow(&self, from: (f32, f32), to: (f32, f32), color: u32, width: f32) {
        let Some(brush) = self.assets.brush(color) else {
            return;
        };
        let Some((shaft, transform)) = arrow_head(from, to, width, self.base) else {
            return;
        };
        // SAFETY: the brush, the triangle and the target are alive; the head is
        // a transformed fill of the cached unit triangle.
        unsafe {
            self.rt.DrawLine(
                Vector2 {
                    X: from.0,
                    Y: from.1,
                },
                Vector2 {
                    X: shaft.0,
                    Y: shaft.1,
                },
                brush,
                width,
                Some(self.stroke),
            );
            self.rt.SetTransform(&transform);
            self.rt.FillGeometry(self.triangle, brush, None);
            self.rt.SetTransform(&self.base);
        }
    }

    fn rectangle(&self, rect: (f32, f32, f32, f32), color: u32, width: f32) {
        if let Some(brush) = self.assets.brush(color) {
            let d = d2d_rect(rect_of(rect));
            // SAFETY: the brush and the target are alive.
            unsafe {
                self.rt.DrawRectangle(&d, brush, width, Some(self.stroke));
            }
        }
    }

    fn mosaic(&self, rect: (f32, f32, f32, f32), block: u32) {
        let Some(key) = mosaic_key(rect, block, self.assets.source_size) else {
            return;
        };
        let Some(bitmap) = self.assets.find_mosaic(key) else {
            return;
        };
        let (blocks_x, blocks_y) = key.blocks();
        let destination = d2d_rect(rect_of((
            key.x as f32,
            key.y as f32,
            key.width as f32,
            key.height as f32,
        )));
        let source = D2D_RECT_F {
            left: 0.0,
            top: 0.0,
            right: blocks_x as f32,
            bottom: blocks_y as f32,
        };
        // SAFETY: the bitmap is alive; nearest-neighbour keeps the 16-pixel
        // blocks square, which is what a mosaic is.
        unsafe {
            self.rt.DrawBitmap(
                bitmap,
                Some(&destination),
                1.0,
                D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                Some(&source),
            );
        }
    }

    /// The in-progress gesture, drawn exactly like the mark it will become.
    fn drag(&self, tool: Tool, color: u32, width: f32, scale: f32, drag: &Drag) {
        match tool {
            Tool::Arrow => self.arrow(drag.start, drag.current, color, width),
            Tool::Rectangle => {
                self.rectangle(drag_rect(drag.start, drag.current), color, width);
            }
            Tool::Mosaic => {
                // Dashed white outline: no pixels are hidden before the drop.
                let Some(brush) = self.assets.brush(0xFF_FF_FF) else {
                    return;
                };
                let outline = (1.5 / scale.max(0.0001)).max(1.0);
                let d = d2d_rect(rect_of(drag_rect(drag.start, drag.current)));
                // SAFETY: the brush and the target are alive.
                unsafe {
                    self.rt.DrawRectangle(&d, brush, outline, Some(self.dash));
                }
            }
            Tool::Eraser => {
                // A dashed eraser disc at the pointer. A dark ring under the
                // white dashes keeps it visible on light and dark pictures.
                let radius = ERASER_RADIUS_DIP / scale.max(0.0001);
                let disc = D2D1_ELLIPSE {
                    point: Vector2 {
                        X: drag.current.0,
                        Y: drag.current.1,
                    },
                    radiusX: radius,
                    radiusY: radius,
                };
                let width = (1.5 / scale.max(0.0001)).max(0.8);
                // SAFETY: the brushes and the target are alive.
                unsafe {
                    if let Some(dark) = self.assets.brush(0x00_00_00) {
                        self.rt.DrawEllipse(&disc, dark, width * 2.0, None);
                    }
                    if let Some(white) = self.assets.brush(0xFF_FF_FF) {
                        self.rt.DrawEllipse(&disc, white, width, Some(self.dash));
                    }
                }
            }
            Tool::Text => {}
        }
    }
}

/// Solid brushes for the toolbar chrome, created once per surface.
struct Chrome {
    /// The same white glass a hanging card wears.
    glass: ID2D1SolidColorBrush,
    /// Hover fill inside a pill.
    glass_lit: ID2D1SolidColorBrush,
    /// Hairline around a pill or the card.
    border: ID2D1SolidColorBrush,
    ink: ID2D1SolidColorBrush,
    ink_dim: ID2D1SolidColorBrush,
    accent: ID2D1SolidColorBrush,
    accent_hover: ID2D1SolidColorBrush,
    accent_border: ID2D1SolidColorBrush,
    white: ID2D1SolidColorBrush,
    /// The translucent sheet behind the text being typed (WPF's TextBox).
    text_box: ID2D1SolidColorBrush,
    /// The clothespin.
    metal: ID2D1SolidColorBrush,
    metal_edge: ID2D1SolidColorBrush,
    pin_slot: ID2D1SolidColorBrush,
}

impl Chrome {
    fn new(rt: &ID2D1RenderTarget) -> Result<Self, String> {
        Ok(Self {
            glass: solid(rt, CHROME_GLASS, CHROME_GLASS_ALPHA)?,
            glass_lit: solid(rt, CHROME_GLASS_LIT, CHROME_GLASS_LIT_ALPHA)?,
            border: solid(rt, 0xFF_FF_FF, CHROME_BORDER_ALPHA)?,
            ink: solid(rt, CHROME_INK, 1.0)?,
            ink_dim: solid(rt, CHROME_INK_DIM, 1.0)?,
            accent: solid(rt, CHROME_ACCENT, 1.0)?,
            accent_hover: solid(rt, CHROME_ACCENT_HOVER, 1.0)?,
            accent_border: solid(rt, CHROME_ACCENT_BORDER, 1.0)?,
            white: solid(rt, 0xFF_FF_FF, 1.0)?,
            text_box: solid(rt, 0xFF_FF_FF, 0.78)?,
            metal: solid(rt, 0xD8_D8_D8, 1.0)?,
            metal_edge: solid(rt, 0x8A_8A_8A, 0.9)?,
            pin_slot: solid(rt, 0x00_00_00, 0.32)?,
        })
    }
}

/// Per-target resources: the pen brushes, the source bitmap, the mosaics.
struct Assets {
    rt: ID2D1RenderTarget,
    pens: Vec<(u32, ID2D1SolidColorBrush)>,
    mosaics: Vec<(MosaicKey, ID2D1Bitmap)>,
    source: Option<ID2D1Bitmap>,
    source_size: (u32, u32),
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

    fn brush(&self, color: u32) -> Option<&ID2D1SolidColorBrush> {
        self.pens
            .iter()
            .find(|(rgb, _)| *rgb == color)
            .map(|(_, brush)| brush)
            .or_else(|| self.pens.first().map(|(_, brush)| brush))
    }

    fn set_source(&mut self, image: &ImageData) -> Result<(), String> {
        if !image.is_valid() {
            return Err(format!(
                "upload: invalid image ({}x{}, {} bytes)",
                image.width,
                image.height,
                image.bgra.len()
            ));
        }
        let properties = D2D1_BITMAP_PROPERTIES {
            pixelFormat: pixel_format(D2D1_ALPHA_MODE_PREMULTIPLIED),
            dpiX: 96.0,
            dpiY: 96.0,
        };
        // SAFETY: `image.bgra` is alive for the call and D2D copies the pixels
        // before returning; the pitch matches the BGRA layout.
        let bitmap = unsafe {
            self.rt.CreateBitmap(
                size_u(image.width, image.height),
                Some(image.bgra.as_ptr() as *const c_void),
                image.width * 4,
                &properties,
            )
        }
        .map_err(|error| format!("CreateBitmap(source): {error}"))?;
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
        let properties = D2D1_BITMAP_PROPERTIES {
            pixelFormat: pixel_format(D2D1_ALPHA_MODE_PREMULTIPLIED),
            dpiX: 96.0,
            dpiY: 96.0,
        };
        // SAFETY: `image.bgra` is alive for the call and D2D copies the pixels.
        let bitmap = unsafe {
            self.rt.CreateBitmap(
                size_u(image.width, image.height),
                Some(image.bgra.as_ptr() as *const c_void),
                image.width * 4,
                &properties,
            )
        }
        .map_err(|error| format!("CreateBitmap(mosaic): {error}"))?;
        self.mosaics.push((key, bitmap));
        Ok(())
    }
}

// ---- the GPU stack (adapted from `render/mod.rs`) -------------------------

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
        let width_px = width_px.max(1);
        let height_px = height_px.max(1);
        let dpi_f = dpi.max(96) as f32;
        let (device, immediate) = create_d3d_device()?;
        let dxgi_device: IDXGIDevice = device
            .cast()
            .map_err(|error| format!("ID3D11Device -> IDXGIDevice: {error}"))?;
        let (factory2, dcomp, target, visual) = create_composition(&dxgi_device, hwnd)?;
        let (swap_chain, swap_chain3) = create_swap_chain(&factory2, &device, width_px, height_px)?;
        let (d2d_factory, d2d_device, context) = create_d2d(&dxgi_device, dpi_f)?;
        let factory: ID2D1Factory = d2d_factory
            .cast()
            .map_err(|error| format!("ID2D1Factory1 -> ID2D1Factory: {error}"))?;
        let rt: ID2D1RenderTarget = context
            .cast()
            .map_err(|error| format!("ID2D1DeviceContext -> ID2D1RenderTarget: {error}"))?;
        // The window is transparent around the card and the pills, so the swap
        // chain is premultiplied; the opaque card bitmap below it keeps
        // ClearType for the text marks.
        let target_props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: pixel_format(D2D1_ALPHA_MODE_PREMULTIPLIED),
            dpiX: dpi_f,
            dpiY: dpi_f,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
            colorContext: ManuallyDrop::new(None),
        };
        let card_props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: pixel_format(D2D1_ALPHA_MODE_IGNORE),
            dpiX: dpi_f,
            dpiY: dpi_f,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET,
            colorContext: ManuallyDrop::new(None),
        };
        let stroke_round = stroke_style(&factory, D2D1_DASH_STYLE_SOLID, None)?;
        let stroke_dash = stroke_style(&factory, D2D1_DASH_STYLE_CUSTOM, Some(&[4.0, 3.0]))?;
        let triangle = create_triangle(&factory)?;
        let eraser = create_eraser(&factory)?;
        let dwrite = create_dwrite()?;
        let shadow = crate::render::bake_card_shadow(&context, dpi_f, 20.0, 5.0)
            .map_err(|error| format!("bake card shadow: {error}"))?;
        let mut surface = Self {
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
        surface.attach_content()?;
        Ok(surface)
    }

    fn attach_content(&mut self) -> Result<(), String> {
        let content: windows::core::IUnknown = self
            .swap_chain
            .cast()
            .map_err(|error| format!("swap chain -> IUnknown: {error}"))?;
        // SAFETY: the visual and the DComp device are alive and owned by self.
        unsafe {
            self._visual
                .SetContent(&content)
                .map_err(|error| format!("SetContent: {error}"))?;
            self._dcomp
                .Commit()
                .map_err(|error| format!("Commit: {error}"))?;
        }
        Ok(())
    }

    fn resize(&mut self, width_px: u32, height_px: u32, dpi: u32) -> Result<(), String> {
        let width_px = width_px.max(1);
        let height_px = height_px.max(1);
        let dpi_f = dpi.max(96) as f32;
        if width_px == self.width_px && height_px == self.height_px && dpi_f == self.dpi {
            return Ok(());
        }
        // SAFETY: release the D2D targets before resizing their buffers.
        unsafe {
            self.context.SetTarget(None::<&ID2D1Image>);
        }
        self.targets = [None, None];
        self.card = None;
        // SAFETY: every D2D target was released above.
        unsafe {
            self.swap_chain.ResizeBuffers(
                2,
                width_px,
                height_px,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                windows::Win32::Graphics::Dxgi::DXGI_SWAP_CHAIN_FLAG(0),
            )
        }
        .map_err(|error| format!("ResizeBuffers: {error}"))?;
        self.width_px = width_px;
        self.height_px = height_px;
        self.dpi = dpi_f;
        // SAFETY: the context is ours; a DPI change must be pushed down.
        unsafe {
            self.context.SetDpi(self.dpi, self.dpi);
        }
        self.target_props.dpiX = self.dpi;
        self.target_props.dpiY = self.dpi;
        self.card_props.dpiX = self.dpi;
        self.card_props.dpiY = self.dpi;
        self.shadow = crate::render::bake_card_shadow(&self.context, self.dpi, 20.0, 5.0).ok();
        Ok(())
    }

    /// Binds the opaque card bitmap for `photo` and starts painting on it.
    fn begin_card(&mut self, photo: RectF, dpi: f32) -> Result<(), String> {
        self.ensure_card(photo, dpi)?;
        let Some(card) = self.card.as_ref() else {
            return Err("card bitmap missing".to_string());
        };
        // SAFETY: the card is a target for this context and stays alive.
        unsafe {
            self.context.SetTarget(card);
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
        Ok(())
    }

    fn end_card(&mut self) -> Result<(), String> {
        // SAFETY: `begin_card` called `BeginDraw` on this thread.
        unsafe { self.context.EndDraw(None, None) }
            .map_err(|error| format!("EndDraw(card): {error}"))
    }

    fn ensure_card(&mut self, photo: RectF, dpi: f32) -> Result<(), String> {
        let scale = dpi.max(96.0) / 96.0;
        let width = ((photo.width() * scale).round() as u32).max(1);
        let height = ((photo.height() * scale).round() as u32).max(1);
        if self
            .card
            .as_ref()
            .map(|bitmap| {
                // SAFETY: a pure query on a live bitmap.
                let size = unsafe { bitmap.GetPixelSize() };
                size.width == width && size.height == height
            })
            .unwrap_or(false)
        {
            return Ok(());
        }
        // SAFETY: the context is ours and the target bitmap is created with the
        // photo's exact pixel size.
        self.card = Some(
            unsafe {
                self.context
                    .CreateBitmap(D2D_SIZE_U { width, height }, None, 0, &self.card_props)
            }
            .map_err(|error| format!("CreateBitmap(card {width}x{height}): {error}"))?,
        );
        Ok(())
    }

    /// Binds the transparent swap-chain buffer and starts a window frame.
    fn begin_window(&mut self) -> Result<(), String> {
        // SAFETY: the swap chain is ours and the returned index is checked.
        let index = unsafe { self.swap_chain3.GetCurrentBackBufferIndex() } as usize;
        if index >= self.targets.len() {
            return Err(format!("back buffer index {index}"));
        }
        if self.targets[index].is_none() {
            // SAFETY: the swap chain owns the buffer and the surface is alive
            // for the duration of `CreateBitmapFromDxgiSurface`.
            let surface: IDXGISurface = unsafe { self.swap_chain.GetBuffer(index as u32) }
                .map_err(|error| format!("GetBuffer: {error}"))?;
            let bitmap = unsafe {
                self.context
                    .CreateBitmapFromDxgiSurface(&surface, Some(&self.target_props))
            }
            .map_err(|error| format!("CreateBitmapFromDxgiSurface: {error}"))?;
            self.targets[index] = Some(bitmap);
        }
        let Some(bitmap) = self.targets[index].as_ref() else {
            return Ok(());
        };
        // SAFETY: the bitmap is a target for this context and stays alive.
        unsafe {
            self.context.SetTarget(bitmap);
            self.context.SetDpi(self.dpi, self.dpi);
            self.context.SetTransform(&Matrix3x2::identity());
            // Text sits on the translucent pills; ClearType is reserved for
            // the opaque card bitmap.
            self.context
                .SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            self.context.BeginDraw();
            self.context.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }));
        }
        Ok(())
    }

    fn end(&mut self) -> Result<(), String> {
        // SAFETY: `begin_window` called `BeginDraw` on this thread.
        unsafe { self.context.EndDraw(None, None) }.map_err(|error| format!("EndDraw: {error}"))
    }

    fn present(&self, sync: bool) -> crate::render::Present {
        // SAFETY: the swap chain is ours; `Present` is the only call that may
        // block, and it is deliberately on the render thread.
        let result = unsafe { self.swap_chain.Present(u32::from(sync), DXGI_PRESENT(0)) };
        if result.is_ok() {
            crate::render::Present::Ok
        } else if result == DXGI_ERROR_WAS_STILL_DRAWING {
            crate::render::Present::Retry
        } else if result == DXGI_ERROR_DEVICE_REMOVED || result == DXGI_ERROR_DEVICE_RESET {
            crate::render::Present::DeviceLost
        } else {
            logging::error(format!("editor Present failed: 0x{:08X}", result.0));
            crate::render::Present::DeviceLost
        }
    }
}

fn create_composition(
    dxgi_device: &IDXGIDevice,
    hwnd: HWND,
) -> Result<
    (
        IDXGIFactory2,
        IDCompositionDevice,
        IDCompositionTarget,
        IDCompositionVisual,
    ),
    String,
> {
    // SAFETY: `dxgi_device` is a live DXGI device owned by this thread; the DComp
    // objects are created from it and kept alive together.
    unsafe {
        let adapter = dxgi_device
            .GetAdapter()
            .map_err(|error| format!("GetAdapter: {error}"))?;
        let factory: IDXGIFactory2 = adapter
            .GetParent()
            .map_err(|error| format!("GetParent: {error}"))?;
        let dcomp: IDCompositionDevice = DCompositionCreateDevice(dxgi_device)
            .map_err(|error| format!("DCompositionCreateDevice: {error}"))?;
        let target = dcomp
            .CreateTargetForHwnd(hwnd, true)
            .map_err(|error| format!("CreateTargetForHwnd: {error}"))?;
        let visual = dcomp
            .CreateVisual()
            .map_err(|error| format!("CreateVisual: {error}"))?;
        target
            .SetRoot(&visual)
            .map_err(|error| format!("SetRoot: {error}"))?;
        dcomp.Commit().map_err(|error| format!("Commit: {error}"))?;
        Ok((factory, dcomp, target, visual))
    }
}

fn create_swap_chain(
    factory: &IDXGIFactory2,
    device: &ID3D11Device,
    width_px: u32,
    height_px: u32,
) -> Result<(IDXGISwapChain1, IDXGISwapChain3), String> {
    let desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: width_px,
        Height: height_px,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        Stereo: false.into(),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
        // The window is transparent around the card and the pills; the card's
        // own bitmap is opaque, so premultiplied alpha composites cleanly.
        AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
        Flags: 0,
    };
    // SAFETY: the factory and device are alive; a composition swap chain takes
    // no output adapter.
    unsafe {
        let swap_chain: IDXGISwapChain1 = factory
            .CreateSwapChainForComposition(device, &desc, None::<&IDXGIOutput>)
            .map_err(|error| format!("CreateSwapChainForComposition: {error}"))?;
        let swap_chain3: IDXGISwapChain3 = swap_chain
            .cast()
            .map_err(|error| format!("IDXGISwapChain3: {error}"))?;
        Ok((swap_chain, swap_chain3))
    }
}

fn create_d2d(
    dxgi_device: &IDXGIDevice,
    dpi: f32,
) -> Result<(ID2D1Factory1, ID2D1Device, ID2D1DeviceContext), String> {
    // SAFETY: a single-threaded factory used only on this thread; the D2D
    // device and context are handed to `Surface` and dropped with it.
    unsafe {
        let factory: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)
            .map_err(|error| format!("D2D1CreateFactory: {error}"))?;
        let device = factory
            .CreateDevice(dxgi_device)
            .map_err(|error| format!("CreateDevice(d2d): {error}"))?;
        let context = device
            .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)
            .map_err(|error| format!("CreateDeviceContext: {error}"))?;
        context.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
        // The editor swap chain is opaque (DXGI_ALPHA_MODE_IGNORE), so Direct2D
        // can use subpixel ClearType for the toolbar and the on-screen text
        // marks. Saved pictures use a separate alpha target below, which must
        // stay grayscale.
        context.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE);
        context.SetDpi(dpi, dpi);
        Ok((factory, device, context))
    }
}

fn create_d3d_device() -> Result<(ID3D11Device, ID3D11DeviceContext), String> {
    let mut device: Option<ID3D11Device> = None;
    let mut immediate: Option<ID3D11DeviceContext> = None;
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    let mut level = D3D_FEATURE_LEVEL_11_0;
    // SAFETY: the out-params are owned by this function and the level array is
    // live for the call.
    let created = unsafe {
        D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            D3D_DRIVER_TYPE_HARDWARE,
            Default::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut level),
            Some(&mut immediate),
        )
    };
    if created.is_err() {
        let levels = [D3D_FEATURE_LEVEL_11_0];
        // SAFETY: as above; 11.0 is the documented floor.
        unsafe {
            D3D11CreateDevice(
                None::<&IDXGIAdapter>,
                D3D_DRIVER_TYPE_HARDWARE,
                Default::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut level),
                Some(&mut immediate),
            )
        }
        .map_err(|error| format!("D3D11CreateDevice(11.0): {error}"))?;
    }
    let device = device.ok_or_else(|| "D3D11 device missing".to_string())?;
    let immediate = immediate.ok_or_else(|| "D3D11 context missing".to_string())?;
    Ok((device, immediate))
}

fn create_dwrite() -> Result<IDWriteFactory, String> {
    // SAFETY: DWrite is a free-threaded factory and the out-pointer is valid.
    unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }
        .map_err(|error| format!("DWriteCreateFactory: {error}"))
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

fn create_format(
    dwrite: &IDWriteFactory,
    family: PCWSTR,
    size: f32,
    weight: DWRITE_FONT_WEIGHT,
) -> Result<IDWriteTextFormat, String> {
    // SAFETY: the factory is alive; the family and locale are static strings.
    unsafe {
        dwrite.CreateTextFormat(
            family,
            None,
            weight,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size,
            w!("en-us"),
        )
    }
    .map_err(|error| format!("CreateTextFormat: {error}"))
}

fn create_text_layout(
    dwrite: &IDWriteFactory,
    format: &IDWriteTextFormat,
    text: &str,
) -> Result<IDWriteTextLayout, String> {
    let wide: Vec<u16> = text.encode_utf16().collect();
    // SAFETY: `wide` is alive for the call and the factory/format are borrowed.
    unsafe { dwrite.CreateTextLayout(&wide, format, 4096.0, 4096.0) }
        .map_err(|error| format!("CreateTextLayout: {error}"))
}

fn text_metrics(layout: &IDWriteTextLayout) -> DWRITE_TEXT_METRICS {
    let mut metrics = DWRITE_TEXT_METRICS::default();
    // SAFETY: writes one struct owned by this stack frame.
    unsafe {
        let _ = layout.GetMetrics(&mut metrics);
    }
    metrics
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

fn solid(rt: &ID2D1RenderTarget, rgb: u32, alpha: f32) -> Result<ID2D1SolidColorBrush, String> {
    let color = D2D1_COLOR_F {
        r: ((rgb >> 16) & 0xFF) as f32 / 255.0,
        g: ((rgb >> 8) & 0xFF) as f32 / 255.0,
        b: (rgb & 0xFF) as f32 / 255.0,
        a: alpha,
    };
    // SAFETY: the render target is alive; the colour is by value.
    unsafe { rt.CreateSolidColorBrush(&color, None) }
        .map_err(|error| format!("CreateSolidColorBrush: {error}"))
}

fn pixel_format(alpha: D2D1_ALPHA_MODE) -> D2D1_PIXEL_FORMAT {
    D2D1_PIXEL_FORMAT {
        format: DXGI_FORMAT_B8G8R8A8_UNORM,
        alphaMode: alpha,
    }
}

fn size_u(width: u32, height: u32) -> D2D_SIZE_U {
    D2D_SIZE_U { width, height }
}

fn d2d_rect(rect: RectF) -> D2D_RECT_F {
    D2D_RECT_F {
        left: rect.left,
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
    }
}

fn rounded(rect: RectF, radius: f32) -> D2D1_ROUNDED_RECT {
    D2D1_ROUNDED_RECT {
        rect: d2d_rect(rect),
        radiusX: radius,
        radiusY: radius,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn ime_caret_is_below_the_text_box_in_client_pixels() {
        let (width, height) = layout::window_size((697, 584), (1920.0, 1040.0));
        let fitted = layout::fit(width, height, (697, 584));
        let canvas = CanvasLayout {
            image: fitted.photo,
            scale: fitted.scale,
        };
        // The text box bottom is at source y = 120 + 38, the IME anchor two
        // DIPs below it (the four DIP box padding plus the two DIP gap).
        let (x96, y96) = ime_caret_client_px(canvas, (100.0, 120.0), 96.0, 38.0, 96.0);
        let (x192, y192) = ime_caret_client_px(canvas, (100.0, 120.0), 96.0, 38.0, 192.0);
        assert!(x96 < pixels(width, 96));
        assert!(y96 < pixels(height, 96));
        assert!(
            (x192 - x96 * 2).abs() <= 1,
            "the anchor must follow the DPI"
        );
        assert!((y192 - y96 * 2).abs() <= 1);
        let top_px = ((canvas.image.top + 120.0 * canvas.scale) * 2.0).round() as i32;
        assert!(y192 > top_px + 70, "IME anchor must sit below the text box");
    }
}
