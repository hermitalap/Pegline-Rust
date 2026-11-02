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
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D_SIZE_U, D2D1_ALPHA_MODE, D2D1_ALPHA_MODE_IGNORE, D2D1_ALPHA_MODE_PREMULTIPLIED,
    D2D1_COLOR_F, D2D1_FIGURE_BEGIN_FILLED, D2D1_FIGURE_END_CLOSED, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
    D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR, D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
    D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES, D2D1_BITMAP_PROPERTIES1,
    D2D1_CAP_STYLE_ROUND, D2D1_DASH_STYLE, D2D1_DASH_STYLE_CUSTOM, D2D1_DASH_STYLE_SOLID,
    D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_DRAW_TEXT_OPTIONS_NONE,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT, D2D1_LINE_JOIN_ROUND,
    D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_SOFTWARE, D2D1_RENDER_TARGET_USAGE_NONE,
    D2D1_ROUNDED_RECT, D2D1_STROKE_STYLE_PROPERTIES, D2D1CreateFactory, ID2D1Bitmap, ID2D1Bitmap1,
    ID2D1Device, ID2D1DeviceContext, ID2D1Factory, ID2D1Factory1, ID2D1Geometry, ID2D1Image,
    ID2D1RenderTarget, ID2D1SolidColorBrush, ID2D1StrokeStyle,
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
    DWRITE_FONT_WEIGHT, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_TEXT_METRICS, DWriteCreateFactory,
    IDWriteFactory, IDWriteTextFormat, IDWriteTextLayout,
};
use windows::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
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
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
    VK_CONTROL, VK_ESCAPE, VK_S, VK_Y, VK_Z,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetClientRect, GetCursorPos,
    GetWindowLongPtrW, HTCLIENT, IDC_CROSS, IDC_HAND, IDC_IBEAM, LoadCursorW, MINMAXINFO,
    PostMessageW, SW_SHOWNORMAL, SWP_NOACTIVATE, SWP_NOZORDER, SetCursor, SetForegroundWindow,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, WM_APP, WM_CAPTURECHANGED, WM_CHAR, WM_CLOSE,
    WM_DESTROY, WM_DPICHANGED, WM_ERASEBKGND, WM_GETMINMAXINFO, WM_KEYDOWN, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WM_PAINT, WM_SETCURSOR, WM_SIZE, WS_EX_NOREDIRECTIONBITMAP,
    WS_OVERLAPPEDWINDOW,
};
use windows::core::{Interface, PCWSTR, w};
use windows_numerics::{Matrix3x2, Vector2};

use crate::capture::wic;
use crate::editor::model::{COLORS, Doc, Mark, Tool, WIDTHS};
use crate::i18n;
use crate::logging;
use crate::types::{ImageData, RectF};
use crate::win32;
use crate::window;

/// The result of one edit, delivered on the UI thread.
#[derive(Clone, Debug)]
pub enum EditorEvent {
    /// The marked-up picture as PNG bytes, at the source resolution.
    Saved(Vec<u8>),
    /// The user cancelled; the file is untouched.
    Cancelled,
}

const CLASS_NAME: PCWSTR = w!("PeglineEditor");
/// Private notification: a result is waiting in `Shared::event`.
const WM_APP_EDITOR_DONE: u32 = WM_APP + 10;
/// `WM_MOUSELEAVE` lives in `Win32_UI_Controls`, a feature this crate does not
/// otherwise need.
const WM_MOUSELEAVE: u32 = 0x02A3;

/// Client size at 100 % scale, DIP.
const CLIENT_WIDTH_DIP: f32 = 1120.0;
const CLIENT_HEIGHT_DIP: f32 = 760.0;
/// Smallest client size, DIP.
const MIN_WIDTH_DIP: f32 = 720.0;
const MIN_HEIGHT_DIP: f32 = 520.0;
/// Toolbar height and picture margin, DIP.
const TOOLBAR_HEIGHT: f32 = 52.0;
const CANVAS_MARGIN: f32 = 12.0;
/// Side of one mosaic block in source pixels (BEHAVIOR §11).
const MOSAIC_BLOCK: u32 = 16;
/// Text marks are 32 source pixels tall, SemiBold, like the WPF text box.
const TEXT_SIZE: f32 = 32.0;
/// Cached mosaic textures kept for undo/redo; the cache is cleared when full.
const MOSAIC_CACHE: usize = 16;

// Toolbar geometry, DIP. Hit rectangles and drawing share `toolbar_layout`.
const TOOL_SIZE: f32 = 34.0;
const TOOL_GAP: f32 = 3.0;
const SWATCH_SIZE: f32 = 22.0;
const SWATCH_GAP: f32 = 4.0;
const WIDTH_SIZE: f32 = 30.0;
const WIDTH_GAP: f32 = 2.0;
const DIVIDER: f32 = 13.0;
const EDGE: f32 = 8.0;
const ACTION_GAP: f32 = 4.0;
const BUTTON_HEIGHT: f32 = 34.0;
const BUTTON_RADIUS: f32 = 5.0;

/// Toolbar chrome colours.
const CHROME_TOOLBAR: u32 = 0x25_25_26;
const CHROME_SEPARATOR: u32 = 0x3E_3E_42;
const CHROME_BUTTON: u32 = 0x2D_2D_30;
const CHROME_BUTTON_HOVER: u32 = 0x3E_3E_42;
const CHROME_BUTTON_BORDER: u32 = 0x50_50_54;
const CHROME_ACCENT: u32 = 0x0E_63_9C;
const CHROME_ACCENT_HOVER: u32 = 0x11_77_BB;
const CHROME_ACCENT_BORDER: u32 = 0x82_C4_FF;
const CHROME_TEXT: u32 = 0xF0_F0_F0;
const CHROME_TEXT_DIM: u32 = 0x7F_7F_7F;

/// The four tools in toolbar order.
const TOOLS: [Tool; 4] = [Tool::Arrow, Tool::Rectangle, Tool::Text, Tool::Mosaic];

/// One open editor.
pub struct Editor {
    hwnd: HWND,
    shared: Arc<Shared>,
    join: Option<JoinHandle<()>>,
}

impl Editor {
    /// Opens `path` in a new editor window. `notify` is called on the UI thread
    /// when the edit is saved or cancelled.
    ///
    /// The call creates the window and waits for the GPU stack (not for the
    /// picture, which is decoded on the render thread), so a 6K PNG never blocks
    /// the UI thread here.
    pub fn open(
        path: &Path,
        dpi: u32,
        notify: Box<dyn Fn(EditorEvent) + Send>,
    ) -> Result<Self, String> {
        window::register_class(CLASS_NAME, Some(editor_wndproc))?;
        // The window lands on the monitor under the cursor, so its DPI wins.
        let monitor = win32::cursor_pos().and_then(|(x, y)| win32::monitor_at(x, y));
        let dpi = monitor.map(|monitor| monitor.dpi).unwrap_or(dpi).max(96);
        // SAFETY: the module handle is this process and the class is registered
        // above; the call has no other preconditions.
        let instance = unsafe { GetModuleHandleW(None) }
            .map_err(|error| format!("GetModuleHandleW: {error}"))?;

        let style = WS_OVERLAPPEDWINDOW;
        let ex_style = WS_EX_NOREDIRECTIONBITMAP;
        let client_width = pixels(CLIENT_WIDTH_DIP, dpi);
        let client_height = pixels(CLIENT_HEIGHT_DIP, dpi);
        let mut frame = RECT {
            left: 0,
            top: 0,
            right: client_width,
            bottom: client_height,
        };
        // SAFETY: `frame` is a live RECT and the styles are the ones the window
        // is created with below.
        unsafe { AdjustWindowRectExForDpi(&mut frame, style, false, ex_style, dpi) }
            .map_err(|error| format!("AdjustWindowRectExForDpi: {error}"))?;
        let mut width = frame.right - frame.left;
        let mut height = frame.bottom - frame.top;
        let mut left = 0;
        let mut top = 0;
        if let Some(monitor) = monitor {
            let work = monitor.work;
            // A 1120x760 window does not fit every laptop panel; clamp it so the
            // title bar and the toolbar stay reachable.
            width = width.min(work.width()).max(320);
            height = height.min(work.height()).max(320);
            left = work.left + (work.width() - width).max(0) / 2;
            top = work.top + (work.height() - height).max(0) / 2;
        }

        let title = win32::wide(i18n::s().editor_title);
        // SAFETY: the class is registered, `title` is NUL-terminated and alive
        // for the call, and no parent or menu is requested.
        let hwnd = unsafe {
            CreateWindowExW(
                ex_style,
                CLASS_NAME,
                PCWSTR(title.as_ptr()),
                style,
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

        // The canvas and toolbar are dark, so ask DWM for a dark title bar too.
        // This is a no-op on Windows versions before 10 1809; the window is
        // still usable with the normal light caption.
        let dark = 1i32;
        // SAFETY: our own window; the attribute expects a DWORD-sized pointer.
        unsafe {
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE,
                &dark as *const i32 as *const c_void,
                std::mem::size_of::<i32>() as u32,
            );
        }

        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            tx: Mutex::new(tx),
            notify: Mutex::new(Some(notify)),
            event: Mutex::new(None),
            pending_high: AtomicU32::new(0),
            captured: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            reported: AtomicBool::new(false),
            delivered: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            text_tool: AtomicBool::new(false),
        });
        // SAFETY: the pointer stays valid while the window exists: `Editor`
        // holds the `Arc` and `shutdown` destroys the window before that `Arc`
        // is dropped.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, Arc::as_ptr(&shared) as isize) };

        let (client_width, client_height) = client_size(hwnd).unwrap_or((1, 1));
        let render_dpi = win32::window_dpi(hwnd);
        let start = Start {
            hwnd: SendHwnd(hwnd),
            path: path.to_path_buf(),
            width_px: client_width,
            height_px: client_height,
            dpi: render_dpi,
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
                        EditorEvent::Cancelled,
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

        // SAFETY: the window was created on this thread and has finished setup;
        // taking focus is wanted here, unlike the overlay.
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
            let _ = SetForegroundWindow(hwnd);
        }
        shared.send(Command::Resize {
            width_px: client_width,
            height_px: client_height,
            dpi: render_dpi,
        });
        logging::trace(format!(
            "editor open: {} {}x{} px dpi {}",
            path.display(),
            client_width,
            client_height,
            render_dpi
        ));
        Ok(Self {
            hwnd,
            shared,
            join: Some(join),
        })
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
    /// Pending high surrogate of a `WM_CHAR` pair.
    pending_high: AtomicU32,
    captured: AtomicBool,
    closed: AtomicBool,
    reported: AtomicBool,
    delivered: AtomicBool,
    stopping: AtomicBool,
    /// The Text tool was the last tool button pressed; drives the I-beam
    /// cursor over the canvas. The render thread owns the real tool state.
    text_tool: AtomicBool,
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
        WM_GETMINMAXINFO => {
            // SAFETY: the system passes a live MINMAXINFO for this message.
            let info = unsafe { &mut *(lparam.0 as *mut MINMAXINFO) };
            // SAFETY: pure query; valid during creation as well.
            let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
            if let Some((width, height)) = min_window_size(dpi) {
                info.ptMinTrackSize = POINT {
                    x: width,
                    y: height,
                };
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
        WM_CLOSE => {
            // The render thread decides; the window closes when it reports.
            if !shared.delivered.load(Ordering::Acquire)
                && !shared.send(Command::Cancel)
                && !shared.reported.load(Ordering::Acquire)
            {
                deliver(shared, hwnd, EditorEvent::Cancelled);
            }
            return LRESULT(0);
        }
        WM_APP_EDITOR_DONE => {
            deliver_pending(shared, hwnd);
            return LRESULT(0);
        }
        WM_DESTROY => {
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
            // Remember whether the click selected the Text tool, so the canvas
            // can show an I-beam instead of a cross. Tool buttons live at fixed
            // positions, independent of the label widths.
            // SAFETY: pure query on our own window.
            let scale = unsafe { GetDpiForWindow(hwnd) }.max(96) as f32 / 96.0;
            let (dx, dy) = (x as f32 / scale, y as f32 / scale);
            let top = (TOOLBAR_HEIGHT - BUTTON_HEIGHT) * 0.5;
            if dy >= top && dy < top + BUTTON_HEIGHT {
                let step = TOOL_SIZE + TOOL_GAP;
                for index in 0..4 {
                    let left = EDGE + index as f32 * step;
                    if dx >= left && dx < left + TOOL_SIZE {
                        shared.text_tool.store(index == 2, Ordering::Release);
                        break;
                    }
                }
            }
            shared.captured.store(true, Ordering::Release);
            // SAFETY: capturing the mouse for our own window.
            unsafe { SetCapture(hwnd) };
            shared.send(Command::PointerDown { x, y });
            return LRESULT(0);
        }
        WM_SETCURSOR => {
            // Hand over the toolbar, cross/I-beam over the canvas. Without this
            // the whole editor kept the default arrow (the WPF editor set
            // Cursors.Hand on buttons and Cross/IBeam on the drawing surface).
            if (lparam.0 as u32 & 0xFFFF) == HTCLIENT {
                let mut point = POINT::default();
                // SAFETY: pure queries on our own window and the thread cursor.
                unsafe {
                    let _ = GetCursorPos(&mut point);
                    let _ = ScreenToClient(hwnd, &mut point);
                }
                let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96) as f32;
                let dy = point.y as f32 / (dpi / 96.0);
                let cursor = if dy <= TOOLBAR_HEIGHT {
                    IDC_HAND
                } else if shared.text_tool.load(Ordering::Acquire) {
                    IDC_IBEAM
                } else {
                    IDC_CROSS
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
                shared.send(Command::Save);
                return LRESULT(0);
            }
        }
        _ => {}
    }
    // SAFETY: default processing for messages the editor does not handle.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
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

/// Calls `notify` exactly once on the UI thread and closes the window.
fn deliver(shared: &Shared, hwnd: HWND, event: EditorEvent) {
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
    if !shared.closed.load(Ordering::Acquire) {
        // SAFETY: destroying the editor window from its own procedure;
        // `DestroyWindow` is synchronous and sends `WM_DESTROY` before returning.
        unsafe {
            let _ = DestroyWindow(hwnd);
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

/// The smallest window size for a client of 720x520 DIP, in window pixels.
fn min_window_size(dpi: u32) -> Option<(i32, i32)> {
    let dpi = dpi.max(96);
    let mut frame = RECT {
        left: 0,
        top: 0,
        right: pixels(MIN_WIDTH_DIP, dpi),
        bottom: pixels(MIN_HEIGHT_DIP, dpi),
    };
    // SAFETY: `frame` is a live RECT; the styles match the editor window.
    unsafe {
        AdjustWindowRectExForDpi(
            &mut frame,
            WS_OVERLAPPEDWINDOW,
            false,
            WS_EX_NOREDIRECTIONBITMAP,
            dpi,
        )
    }
    .ok()?;
    Some((frame.right - frame.left, frame.bottom - frame.top))
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
    let mut state = match EditorState::new(start.width_px, start.height_px, start.dpi, surface) {
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
        done = Some(EditorEvent::Cancelled);
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
                    done = Some(EditorEvent::Cancelled);
                    break;
                }
                if let Err(error) = state.rebuild() {
                    logging::error(format!("editor rebuild: {error}"));
                }
            }
            state.dirty = false;
        }
    }
    // Release the swap chain and the DirectComposition target before the UI
    // thread destroys the window they belong to.
    drop(state);
    if let Some(event) = done {
        report(&shared, start.hwnd.0, event);
    }
    logging::trace("editor render thread stopped");
}

// ---- the editing state ----------------------------------------------------

/// One shortcut/button label, with its measured size in DIP.
struct Label {
    layout: IDWriteTextLayout,
    width: f32,
    height: f32,
}

/// The measured widths the toolbar layout needs.
#[derive(Clone, Copy, Debug, Default)]
struct LabelWidths {
    undo: f32,
    redo: f32,
    cancel: f32,
    save: f32,
}

/// Every text layout the toolbar draws. Created once per surface.
struct Labels {
    tools: [Label; 4],
    undo: Label,
    redo: Label,
    cancel: Label,
    save: Label,
    undo_glyph: Label,
    redo_glyph: Label,
}

impl Labels {
    fn build(dwrite: &IDWriteFactory, strings: &crate::i18n::Strings) -> Result<Self, String> {
        let ui = create_format(dwrite, w!("Segoe UI"), 13.0, DWRITE_FONT_WEIGHT_SEMI_BOLD)?;
        let glyph = create_format(
            dwrite,
            w!("Segoe UI Symbol"),
            17.0,
            DWRITE_FONT_WEIGHT_SEMI_BOLD,
        )
        .or_else(|_| create_format(dwrite, w!("Segoe UI"), 17.0, DWRITE_FONT_WEIGHT_SEMI_BOLD))?;
        Ok(Self {
            tools: [
                label(dwrite, &glyph, "\u{2197}")?,
                label(dwrite, &glyph, "\u{25A1}")?,
                label(dwrite, &glyph, "T")?,
                label(dwrite, &glyph, "\u{25A6}")?,
            ],
            undo: label(dwrite, &ui, strings.undo)?,
            redo: label(dwrite, &ui, strings.redo)?,
            cancel: label(dwrite, &ui, strings.cancel)?,
            save: label(dwrite, &ui, strings.save)?,
            undo_glyph: label(dwrite, &glyph, "\u{21B6}")?,
            redo_glyph: label(dwrite, &glyph, "\u{21B7}")?,
        })
    }

    fn widths(&self) -> LabelWidths {
        LabelWidths {
            undo: self.undo.width,
            redo: self.redo.width,
            cancel: self.cancel.width,
            save: self.save.width,
        }
    }
}

/// Hit rectangles for one toolbar width, in DIP.
#[derive(Default)]
struct ToolbarLayout {
    tools: [RectF; 4],
    colors: [RectF; 6],
    widths: [RectF; 3],
    undo: RectF,
    redo: RectF,
    cancel: RectF,
    save: RectF,
    /// Undo/redo show glyphs instead of labels on a narrow window.
    compact: bool,
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

/// One toolbar hit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Action {
    Tool(usize),
    Color(usize),
    Width(usize),
    Undo,
    Redo,
    Cancel,
    Save,
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
    /// The toolbar entry under the pointer, for the WPF-style hover state.
    hover: Option<Action>,
    layout: ToolbarLayout,
    canvas: CanvasLayout,
    width_px: u32,
    height_px: u32,
    dpi: f32,
    source_size: (u32, u32),
    texts_dirty: bool,
    dirty: bool,
    lost: u32,
}

impl EditorState {
    fn new(width_px: u32, height_px: u32, dpi: u32, surface: Surface) -> Result<Self, String> {
        let labels = Labels::build(&surface.dwrite, i18n::s())?;
        let text_format = create_format(
            &surface.dwrite,
            w!("Segoe UI"),
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
        let layout = toolbar_layout(width_dip, labels.widths());
        let canvas = canvas_layout(width_dip, height_dip, (0, 0));
        Ok(Self {
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
            hover: None,
            layout,
            canvas,
            width_px,
            height_px,
            dpi,
            source_size: (0, 0),
            texts_dirty: false,
            dirty: true,
            lost: 0,
        })
    }

    fn width_dip(&self) -> f32 {
        self.width_px as f32 * 96.0 / self.dpi
    }

    fn height_dip(&self) -> f32 {
        self.height_px as f32 * 96.0 / self.dpi
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
        self.canvas = canvas_layout(self.width_dip(), self.height_dip(), self.source_size);
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
        self.layout = toolbar_layout(self.width_dip(), self.labels.widths());
        self.canvas = canvas_layout(self.width_dip(), self.height_dip(), self.source_size);
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
            w!("Segoe UI"),
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
        self.layout = toolbar_layout(self.width_dip(), self.labels.widths());
        self.canvas = canvas_layout(self.width_dip(), self.height_dip(), self.source_size);
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
            Command::PointerCancel => {
                self.drag = None;
                self.dirty = true;
            }
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
                if self.doc.as_ref().map(|doc| doc.is_typing()) == Some(true) {
                    if let Some(doc) = self.doc.as_mut() {
                        doc.finish_text();
                    }
                    self.texts_dirty = true;
                    self.dirty = true;
                    return Step::Continue;
                }
                return Step::Done(EditorEvent::Cancelled);
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
            Command::Cancel => return Step::Done(EditorEvent::Cancelled),
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
        if y <= TOOLBAR_HEIGHT {
            if let Some(action) = self.hit_action(x, y) {
                return self.activate(action);
            }
            return Step::Continue;
        }
        // Any click on the picture ends the text that was being typed.
        if self.doc.as_ref().map(|doc| doc.is_typing()) == Some(true) {
            if let Some(doc) = self.doc.as_mut() {
                doc.finish_text();
            }
            self.texts_dirty = true;
        }
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
            // Hover only exists on the toolbar. WPF lit every button under the
            // pointer; the same state drives the hand cursor and the paint.
            let hover = if y <= TOOLBAR_HEIGHT {
                self.hit_action(x, y)
            } else {
                None
            };
            if self.hover != hover {
                self.hover = hover;
                self.dirty = true;
            }
            return;
        }
        let source = self.source_clamped(x, y);
        if let Some(drag) = self.drag.as_mut() {
            drag.current = source;
        }
        self.dirty = true;
    }

    fn pointer_up(&mut self, x_px: i32, y_px: i32) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        let (x, y) = self.to_dip(x_px, y_px);
        let end = self.source_clamped(x, y);
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
            if let Some(doc) = self.doc.as_mut() {
                doc.push(mark);
            }
        }
        self.dirty = true;
    }

    /// The toolbar entry under a DIP point.
    fn hit_action(&self, x: f32, y: f32) -> Option<Action> {
        if y > TOOLBAR_HEIGHT {
            return None;
        }
        for (index, rect) in self.layout.tools.iter().enumerate() {
            if rect.contains(x, y) {
                return Some(Action::Tool(index));
            }
        }
        for (index, rect) in self.layout.colors.iter().enumerate() {
            if rect.contains(x, y) {
                return Some(Action::Color(index));
            }
        }
        for (index, rect) in self.layout.widths.iter().enumerate() {
            if rect.contains(x, y) {
                return Some(Action::Width(index));
            }
        }
        if self.layout.undo.contains(x, y) {
            return Some(Action::Undo);
        }
        if self.layout.redo.contains(x, y) {
            return Some(Action::Redo);
        }
        if self.layout.cancel.contains(x, y) {
            return Some(Action::Cancel);
        }
        if self.layout.save.contains(x, y) {
            return Some(Action::Save);
        }
        None
    }

    fn activate(&mut self, action: Action) -> Step {
        match action {
            Action::Tool(index) => {
                if let Some(tool) = TOOLS.get(index) {
                    self.tool = *tool;
                    self.dirty = true;
                }
            }
            Action::Color(index) => {
                if let Some(entry) = COLORS.get(index) {
                    self.color = entry.0;
                    self.dirty = true;
                }
            }
            Action::Width(index) => {
                if let Some(width) = WIDTHS.get(index) {
                    self.width = *width;
                    self.dirty = true;
                }
            }
            Action::Undo => {
                if self.doc.as_mut().map(|doc| doc.undo()) == Some(true) {
                    self.texts_dirty = true;
                    self.dirty = true;
                }
            }
            Action::Redo => {
                if self.doc.as_mut().map(|doc| doc.redo()) == Some(true) {
                    self.texts_dirty = true;
                    self.dirty = true;
                }
            }
            Action::Cancel => return Step::Done(EditorEvent::Cancelled),
            Action::Save => return self.save_step(),
        }
        Step::Continue
    }

    fn save_step(&mut self) -> Step {
        if let Some(doc) = self.doc.as_mut() {
            doc.finish_text();
        }
        match self.save() {
            Ok(bytes) => Step::Done(EditorEvent::Saved(bytes)),
            Err(error) => {
                // The window stays open: the marks are the user's work.
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
        self.surface.begin()?;
        self.paint_toolbar();
        self.paint_canvas();
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
                }
            }
        }
    }

    fn paint_toolbar(&self) {
        let rt = &self.assets.rt;
        let width = self.width_dip();
        let bar = d2d_rect(RectF::from_xywh(0.0, 0.0, width, TOOLBAR_HEIGHT));
        let line_y = TOOLBAR_HEIGHT - 0.5;
        // SAFETY: the render target is in a draw and the brushes are ours.
        unsafe {
            rt.SetTransform(&Matrix3x2::identity());
            rt.FillRectangle(&bar, &self.chrome.toolbar);
            rt.DrawLine(
                Vector2 { X: 0.0, Y: line_y },
                Vector2 {
                    X: width,
                    Y: line_y,
                },
                &self.chrome.separator,
                1.0,
                None,
            );
        }
        for (index, rect) in self.layout.tools.iter().enumerate() {
            let selected = TOOLS[index] == self.tool;
            self.paint_button(
                *rect,
                selected,
                false,
                self.hover == Some(Action::Tool(index)),
            );
            let brush = if selected {
                &self.chrome.white
            } else {
                &self.chrome.text
            };
            self.paint_label(&self.labels.tools[index], *rect, brush);
        }
        for (index, rect) in self.layout.colors.iter().enumerate() {
            let rgb = COLORS[index].0;
            let rounded = rounded(*rect, 4.0);
            if let Some(brush) = self.assets.brush(rgb) {
                // SAFETY: the brush and the render target are alive.
                unsafe {
                    rt.FillRoundedRectangle(&rounded, brush);
                }
            }
            let selected = rgb == self.color;
            let hovered = self.hover == Some(Action::Color(index));
            let border = if selected {
                if rgb == 0xFF_FF_FF {
                    &self.chrome.accent_border
                } else {
                    &self.chrome.white
                }
            } else if hovered {
                &self.chrome.white
            } else {
                &self.chrome.button_border
            };
            // SAFETY: the brush and the render target are alive.
            unsafe {
                rt.DrawRoundedRectangle(
                    &rounded,
                    border,
                    if selected {
                        2.5
                    } else if hovered {
                        1.5
                    } else {
                        1.0
                    },
                    None,
                );
            }
        }
        for (index, rect) in self.layout.widths.iter().enumerate() {
            let selected = (WIDTHS[index] - self.width).abs() < 0.01;
            self.paint_button(
                *rect,
                selected,
                false,
                self.hover == Some(Action::Width(index)),
            );
            let radius = (WIDTHS[index] * 0.5).min(6.0);
            let center = rect.center();
            let dot = RectF::new(
                center.0 - radius,
                center.1 - radius,
                center.0 + radius,
                center.1 + radius,
            );
            let brush = self.assets.brush(self.color).unwrap_or(&self.chrome.white);
            // SAFETY: a circle is a rounded rectangle with equal radii.
            unsafe {
                rt.FillRoundedRectangle(&rounded(dot, radius), brush);
            }
        }
        let undo_enabled = self.doc.as_ref().map(|doc| doc.can_undo()) == Some(true);
        let redo_enabled = self.doc.as_ref().map(|doc| doc.can_redo()) == Some(true);
        self.paint_action(
            self.layout.undo,
            if self.layout.compact {
                &self.labels.undo_glyph
            } else {
                &self.labels.undo
            },
            undo_enabled,
            false,
            self.hover == Some(Action::Undo),
        );
        self.paint_action(
            self.layout.redo,
            if self.layout.compact {
                &self.labels.redo_glyph
            } else {
                &self.labels.redo
            },
            redo_enabled,
            false,
            self.hover == Some(Action::Redo),
        );
        self.paint_action(
            self.layout.cancel,
            &self.labels.cancel,
            true,
            false,
            self.hover == Some(Action::Cancel),
        );
        self.paint_action(
            self.layout.save,
            &self.labels.save,
            true,
            true,
            self.hover == Some(Action::Save),
        );
    }

    fn paint_canvas(&self) {
        let rt = &self.assets.rt;
        let image = self.canvas.image;
        let Some(bitmap) = self.assets.source.as_ref() else {
            return;
        };
        let clip = d2d_rect(image);
        let source = D2D_RECT_F {
            left: 0.0,
            top: 0.0,
            right: self.source_size.0 as f32,
            bottom: self.source_size.1 as f32,
        };
        let base = Matrix3x2::scale(self.canvas.scale, self.canvas.scale)
            * Matrix3x2::translation(image.left, image.top);
        // SAFETY: the render target is in a draw and the bitmap is alive; the
        // clip keeps marks from spilling over the dark window.
        unsafe {
            rt.PushAxisAlignedClip(&clip, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            rt.DrawBitmap(
                bitmap,
                Some(&clip),
                1.0,
                D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                Some(&source),
            );
            rt.SetTransform(&base);
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
        // SAFETY: restores the identity transform and pops the matching clip.
        unsafe {
            rt.SetTransform(&Matrix3x2::identity());
            rt.PopAxisAlignedClip();
            let frame = d2d_rect(image);
            rt.DrawRectangle(&frame, &self.chrome.separator, 1.0, None);
        }
    }

    fn paint_button(&self, rect: RectF, selected: bool, primary: bool, hovered: bool) {
        let rounded = rounded(rect, BUTTON_RADIUS);
        let (fill, border) = if primary || selected {
            let fill = if hovered {
                &self.chrome.accent_hover
            } else {
                &self.chrome.accent
            };
            (fill, &self.chrome.accent_border)
        } else if hovered {
            (&self.chrome.button_hover, &self.chrome.button_border)
        } else {
            (&self.chrome.button, &self.chrome.button_border)
        };
        // SAFETY: the brushes and the render target are alive.
        unsafe {
            self.assets.rt.FillRoundedRectangle(&rounded, fill);
            self.assets
                .rt
                .DrawRoundedRectangle(&rounded, border, 1.0, None);
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
        self.paint_button(rect, primary, primary, hovered);
        let brush = if enabled {
            &self.chrome.white
        } else {
            &self.chrome.text_dim
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

    /// Renders the source resolution plus marks into a WIC bitmap and encodes
    /// it. Runs on the render thread, never on the UI thread.
    fn save(&mut self) -> Result<Vec<u8>, String> {
        if self.texts_dirty {
            self.rebuild_texts();
            self.texts_dirty = false;
        }
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
        let (width, height) = (source.width, source.height);
        let factory = wic_factory()?;
        // SAFETY: the factory is live, the size comes from a validated image and
        // the pixel format is a static WIC GUID.
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
        for mark in doc.marks() {
            let Mark::Mosaic { rect, block } = mark else {
                continue;
            };
            let Some(key) = mosaic_key(*rect, *block, (width, height)) else {
                continue;
            };
            if assets.has_mosaic(key) {
                continue;
            }
            if let Some(image) = build_mosaic(source, key) {
                assets.upload_mosaic(key, &image)?;
            }
        }

        let full = d2d_rect(RectF::from_xywh(0.0, 0.0, width as f32, height as f32));
        // SAFETY: the target is a fresh WIC render target on this thread.
        unsafe {
            rt.SetDpi(96.0, 96.0);
            rt.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            rt.SetTransform(&Matrix3x2::identity());
            rt.BeginDraw();
            rt.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }));
        }
        if let Some(bitmap) = assets.source.as_ref() {
            // SAFETY: `bitmap` is alive and covers the full target.
            unsafe {
                rt.DrawBitmap(
                    bitmap,
                    None,
                    1.0,
                    D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                    Some(&full),
                );
            }
        }
        let painter = Painter {
            rt: &rt,
            assets: &assets,
            stroke: &self.surface.stroke_round,
            dash: &self.surface.stroke_dash,
            triangle: &self.surface.triangle,
            base: Matrix3x2::identity(),
        };
        painter.marks(doc.marks(), &self.texts);
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
        wic::encode_png_bytes(&ImageData::new(width, height, bgra))
    }
}

// ---- toolbar and canvas layout --------------------------------------------

/// Places the toolbar controls in DIP; the same math drives hit testing.
fn toolbar_layout(width_dip: f32, widths: LabelWidths) -> ToolbarLayout {
    let width_dip = width_dip.max(1.0);
    let top = ((TOOLBAR_HEIGHT - BUTTON_HEIGHT) * 0.5).max(0.0);
    let swatch_top = ((TOOLBAR_HEIGHT - SWATCH_SIZE) * 0.5).max(0.0);
    let mut x = EDGE;
    let mut tools = [RectF::default(); 4];
    for slot in tools.iter_mut() {
        *slot = RectF::from_xywh(x, top, TOOL_SIZE, BUTTON_HEIGHT);
        x += TOOL_SIZE + TOOL_GAP;
    }
    x += DIVIDER;
    let mut colors = [RectF::default(); 6];
    for slot in colors.iter_mut() {
        *slot = RectF::from_xywh(x, swatch_top, SWATCH_SIZE, SWATCH_SIZE);
        x += SWATCH_SIZE + SWATCH_GAP;
    }
    x += DIVIDER;
    let mut widths_rect = [RectF::default(); 3];
    for slot in widths_rect.iter_mut() {
        *slot = RectF::from_xywh(x, top, WIDTH_SIZE, BUTTON_HEIGHT);
        x += WIDTH_SIZE + WIDTH_GAP;
    }
    x += DIVIDER;

    let undo_width = (widths.undo + 20.0).max(54.0);
    let redo_width = (widths.redo + 20.0).max(54.0);
    let save_width = (widths.save + 24.0).max(58.0);
    let cancel_width = (widths.cancel + 20.0).max(54.0);
    let right = width_dip - EDGE;
    let save = RectF::from_xywh(right - save_width, top, save_width, BUTTON_HEIGHT);
    let cancel = RectF::from_xywh(
        save.left - ACTION_GAP - cancel_width,
        top,
        cancel_width,
        BUTTON_HEIGHT,
    );
    let compact = x + undo_width + ACTION_GAP + redo_width > cancel.left - 8.0;
    let (undo, redo) = if compact {
        (
            RectF::from_xywh(x, top, TOOL_SIZE, BUTTON_HEIGHT),
            RectF::from_xywh(x + TOOL_SIZE + ACTION_GAP, top, TOOL_SIZE, BUTTON_HEIGHT),
        )
    } else {
        (
            RectF::from_xywh(x, top, undo_width, BUTTON_HEIGHT),
            RectF::from_xywh(x + undo_width + ACTION_GAP, top, redo_width, BUTTON_HEIGHT),
        )
    };
    ToolbarLayout {
        tools,
        colors,
        widths: widths_rect,
        undo,
        redo,
        cancel,
        save,
        compact,
    }
}

/// Fits the picture under the toolbar, centred, never above 1:1.
fn canvas_layout(width_dip: f32, height_dip: f32, source: (u32, u32)) -> CanvasLayout {
    let width_dip = width_dip.max(1.0);
    let height_dip = height_dip.max(1.0);
    let available_width = (width_dip - 2.0 * CANVAS_MARGIN).max(1.0);
    let available_height = (height_dip - TOOLBAR_HEIGHT - 2.0 * CANVAS_MARGIN).max(1.0);
    if source.0 == 0 || source.1 == 0 {
        return CanvasLayout {
            image: RectF::from_xywh(
                width_dip * 0.5,
                TOOLBAR_HEIGHT + available_height * 0.5,
                0.0,
                0.0,
            ),
            scale: 1.0,
        };
    }
    let scale = (available_width / source.0 as f32)
        .min(available_height / source.1 as f32)
        .min(1.0);
    let image_width = source.0 as f32 * scale;
    let image_height = source.1 as f32 * scale;
    CanvasLayout {
        image: RectF::from_xywh(
            (width_dip - image_width) * 0.5,
            TOOLBAR_HEIGHT + (height_dip - TOOLBAR_HEIGHT - image_height) * 0.5,
            image_width,
            image_height,
        ),
        scale,
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
        let dx = to.0 - from.0;
        let dy = to.1 - from.1;
        if dx * dx + dy * dy < 1.0 {
            return;
        }
        let angle = dy.atan2(dx);
        let head = (width * 3.2).max(18.0);
        let shaft = (
            to.0 - angle.cos() * head * 0.6,
            to.1 - angle.sin() * head * 0.6,
        );
        let transform = Matrix3x2::scale(head, head)
            * Matrix3x2::rotation(angle)
            * Matrix3x2::translation(to.0, to.1)
            * self.base;
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
            Tool::Text => {}
        }
    }
}

/// Solid brushes for the toolbar chrome, created once per surface.
struct Chrome {
    toolbar: ID2D1SolidColorBrush,
    separator: ID2D1SolidColorBrush,
    button: ID2D1SolidColorBrush,
    button_hover: ID2D1SolidColorBrush,
    button_border: ID2D1SolidColorBrush,
    accent: ID2D1SolidColorBrush,
    accent_hover: ID2D1SolidColorBrush,
    accent_border: ID2D1SolidColorBrush,
    text: ID2D1SolidColorBrush,
    text_dim: ID2D1SolidColorBrush,
    white: ID2D1SolidColorBrush,
    /// The translucent sheet behind the text being typed (WPF's TextBox).
    text_box: ID2D1SolidColorBrush,
}

impl Chrome {
    fn new(rt: &ID2D1RenderTarget) -> Result<Self, String> {
        Ok(Self {
            toolbar: solid(rt, CHROME_TOOLBAR, 1.0)?,
            separator: solid(rt, CHROME_SEPARATOR, 1.0)?,
            button: solid(rt, CHROME_BUTTON, 1.0)?,
            button_hover: solid(rt, CHROME_BUTTON_HOVER, 1.0)?,
            button_border: solid(rt, CHROME_BUTTON_BORDER, 1.0)?,
            accent: solid(rt, CHROME_ACCENT, 1.0)?,
            accent_hover: solid(rt, CHROME_ACCENT_HOVER, 1.0)?,
            accent_border: solid(rt, CHROME_ACCENT_BORDER, 1.0)?,
            text: solid(rt, CHROME_TEXT, 1.0)?,
            text_dim: solid(rt, CHROME_TEXT_DIM, 1.0)?,
            white: solid(rt, 0xFF_FF_FF, 1.0)?,
            text_box: solid(rt, 0xFF_FF_FF, 0.78)?,
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
    targets: [Option<ID2D1Bitmap1>; 2],
    target_props: D2D1_BITMAP_PROPERTIES1,
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
        let target_props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: pixel_format(D2D1_ALPHA_MODE_IGNORE),
            dpiX: dpi_f,
            dpiY: dpi_f,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
            colorContext: ManuallyDrop::new(None),
        };
        let stroke_round = stroke_style(&factory, D2D1_DASH_STYLE_SOLID, None)?;
        let stroke_dash = stroke_style(&factory, D2D1_DASH_STYLE_CUSTOM, Some(&[4.0, 3.0]))?;
        let triangle = create_triangle(&factory)?;
        let dwrite = create_dwrite()?;
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
            targets: [None, None],
            target_props,
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
        Ok(())
    }

    fn begin(&mut self) -> Result<(), String> {
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
            self.context.BeginDraw();
            self.context.Clear(Some(&D2D1_COLOR_F {
                r: 0x22 as f32 / 255.0,
                g: 0x22 as f32 / 255.0,
                b: 0x22 as f32 / 255.0,
                a: 1.0,
            }));
        }
        Ok(())
    }

    fn end(&mut self) -> Result<(), String> {
        // SAFETY: `BeginDraw` was called by `begin` on this thread.
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
        // The editor window is opaque, so its content needs no alpha.
        AlphaMode: DXGI_ALPHA_MODE_IGNORE,
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

    fn widths() -> LabelWidths {
        LabelWidths {
            undo: 62.0,
            redo: 58.0,
            cancel: 70.0,
            save: 66.0,
        }
    }

    #[test]
    fn toolbar_fits_the_minimum_window() {
        let layout = toolbar_layout(MIN_WIDTH_DIP, widths());
        assert!(layout.compact, "the minimum width needs glyph undo/redo");
        assert!(layout.undo.right < layout.cancel.left);
        assert!(layout.save.right <= MIN_WIDTH_DIP);
        assert_eq!(layout.colors.len(), COLORS.len());
        let mut previous = 0.0;
        for rect in layout.tools.iter() {
            assert!(rect.left >= previous);
            previous = rect.right;
        }
    }

    #[test]
    fn toolbar_uses_labels_on_a_wide_window() {
        let layout = toolbar_layout(CLIENT_WIDTH_DIP, widths());
        assert!(!layout.compact);
        assert!(layout.undo.width() > TOOL_SIZE);
        assert!(layout.save.right <= CLIENT_WIDTH_DIP);
    }

    #[test]
    fn canvas_never_upscales_and_stays_inside() {
        let small = canvas_layout(CLIENT_WIDTH_DIP, CLIENT_HEIGHT_DIP, (100, 50));
        assert_eq!(small.scale, 1.0);
        assert_eq!((small.image.width(), small.image.height()), (100.0, 50.0));

        let large = canvas_layout(MIN_WIDTH_DIP, MIN_HEIGHT_DIP, (1920, 1080));
        assert!(large.scale < 1.0);
        assert!(large.image.left >= CANVAS_MARGIN - 0.01);
        assert!(large.image.top >= TOOLBAR_HEIGHT + CANVAS_MARGIN - 0.01);
        assert!(large.image.bottom <= MIN_HEIGHT_DIP - CANVAS_MARGIN + 0.01);
    }

    #[test]
    fn drags_normalize_negative_directions() {
        assert_eq!(drag_rect((10.0, 20.0), (4.0, 5.0)), (4.0, 5.0, 6.0, 15.0));
        assert_eq!(drag_rect((1.0, 1.0), (1.0, 1.0)), (1.0, 1.0, 0.0, 0.0));
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
}
