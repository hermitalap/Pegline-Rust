//! SPIKE-01: transparent DirectComposition overlay.
//!
//! This binary exists only to prove the rendering path before M1. It draws one
//! glass card with a baked nine-slice shadow and a generated test texture. There
//! is no product logic here: no tray, no screenshots, no settings.
//!
//! Layers, in the order they are built:
//!   1. a `WS_EX_NOREDIRECTIONBITMAP` top-level window,
//!   2. a composition swap chain on a DirectComposition visual,
//!   3. Direct2D content (glass card, texture, baked shadow),
//!   4. a real-`dt` spring animation,
//!   5. the click-through switch and the measurement loop.
//!
//! The UI thread creates the window, the D3D11 device, the DirectComposition
//! target and visual. The render thread creates the swap chain and the D2D
//! device, calls `SetContent`/`Commit` and owns the frame clock. That is the
//! cross-thread assumption ARCHITECTURE.md §12.6 asks us to test.
//!
//! SAFETY: this is a thin Win32/COM layer. Every `unsafe` block below is a
//! direct call into user32/d3d11/dxgi/d2d1/dcomp with handles, COM interfaces or
//! buffers created and kept alive by this process for the duration of the call.
//! COM objects come from the `windows` crate and are released by `Drop`; nothing
//! is released by hand. The UI thread hands D3D11/DXGI/DComp objects to the
//! render thread once, under the `unsafe impl Send for RenderInit` contract.

use std::ffi::c_void;
use std::mem::{ManuallyDrop, size_of};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use windows::Win32::Foundation::{FILETIME, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D_SIZE_U, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_GRADIENT_STOP,
    D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_NONE,
    D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1, D2D1_BUFFER_PRECISION_8BPC_UNORM,
    D2D1_COLOR_INTERPOLATION_MODE_PREMULTIPLIED, D2D1_COLOR_SPACE_SRGB,
    D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_EXTEND_MODE_CLAMP, D2D1_FACTORY_TYPE_SINGLE_THREADED,
    D2D1_INTERPOLATION_MODE_LINEAR, D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES, D2D1_ROUNDED_RECT,
    D2D1CreateFactory, ID2D1Bitmap1, ID2D1DeviceContext, ID2D1Factory1, ID2D1LinearGradientBrush,
    ID2D1SolidColorBrush,
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
use windows::Win32::Graphics::Dwm::{DWM_TIMING_INFO, DwmGetCompositionTimingInfo};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET, DXGI_ERROR_WAS_STILL_DRAWING, DXGI_PRESENT,
    DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
    DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIAdapter, IDXGIDevice, IDXGIFactory2, IDXGIOutput,
    IDXGISurface, IDXGISwapChain1, IDXGISwapChain3,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, DEVMODEW, ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_MODE, EndPaint,
    EnumDisplaySettingsW, HBRUSH, PAINTSTRUCT,
};
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GWL_EXSTYLE, GetMessageW,
    GetSystemMetrics, GetWindowLongPtrW, HCURSOR, HICON, HTCLIENT, HTTRANSPARENT, HWND_TOPMOST,
    IDC_ARROW, LoadCursorW, MA_NOACTIVATE, MSG, PostMessageW, PostQuitMessage, RegisterClassW,
    SM_CXSCREEN, SM_CYSCREEN, SPI_GETWORKAREA, SW_SHOWNOACTIVATE, SWP_FRAMECHANGED, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetWindowLongPtrW, SetWindowPos,
    ShowWindow, SystemParametersInfoW, TranslateMessage, WA_INACTIVE, WM_ACTIVATE, WM_CLOSE,
    WM_DESTROY, WM_ERASEBKGND, WM_LBUTTONDOWN, WM_MOUSEACTIVATE, WM_NCHITTEST, WM_PAINT,
    WM_SETFOCUS, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{Interface, PCWSTR, w};
use windows_numerics::{Matrix3x2, Vector2};

const WINDOW_TITLE: PCWSTR = w!("Pegline SPIKE-01");
const CLASS_NAME: PCWSTR = w!("PeglineSpike01");

const COMPACT_HEIGHT_PX: i32 = 420;
const FLIGHT_WIDTH_PX: i32 = 2442;
const FLIGHT_HEIGHT_PX: i32 = 2344;

const FLIGHT_CARD: f32 = 760.0;
const CARD_RADIUS: f32 = 16.0;
const PHOTO_INSET: f32 = 12.0;
const SHADOW_PAD_DIP: f32 = 24.0;
const SHADOW_OFFSET_Y: f32 = 4.0;
const SHADOW_CORNER_DIP: f32 = 56.0;
const SHADOW_TOTAL_DIP: f32 = 160.0;
const SPRING_DX_LIMIT: f32 = 200.0;
const MAX_INTERVALS: usize = 65_536;

// Global state owned by the message loop. Atomics keep the render thread free
// of shared mutable aliases; the values are only read when printing or when
// the window procedure decides how to route a hit test.
static STOP: AtomicBool = AtomicBool::new(false);
static CLICKTHROUGH: AtomicBool = AtomicBool::new(false);
static CLICKS: AtomicU64 = AtomicU64::new(0);
static ACTIVATIONS: AtomicU64 = AtomicU64::new(0);
static FOCUS_EVENTS: AtomicU64 = AtomicU64::new(0);
static MOUSE_ACTIVATES: AtomicU64 = AtomicU64::new(0);

/// Win32/COM/D3D call wrapper, with extra context on failure.
///
/// The caller must already own the handles/interfaces used by `$expr`; every
/// use of this macro is an `unsafe` Win32/COM/D3D call. Only the error path
/// allocates.
macro_rules! wtry {
    ($expr:expr) => {
        unsafe { $expr }.map_err(|e| format!("{}: {e}", stringify!($expr)))?
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Static,
    Animate,
    Flight,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::Animate => "animate",
            Self::Flight => "flight",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Config {
    mode: Mode,
    clickthrough: bool,
    seconds: u64,
    nolimit: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: Mode::Animate,
            clickthrough: false,
            seconds: 10,
            nolimit: false,
        }
    }
}

fn usage() -> &'static str {
    "Usage: spike [--mode=static|animate|flight] [--clickthrough=0|1]\n\
     \x20            [--seconds=N] [--nolimit]\n\
     \n\
     --seconds=0 runs until the process is closed. --nolimit presents without\n\
     waiting for a vertical blank; use it to measure the draw budget on a\n\
     60 Hz display when no 120 Hz display is attached."
}

fn parse_config<I: Iterator<Item = String>>(args: I) -> Result<Config, String> {
    let mut cfg = Config::default();
    for arg in args {
        match arg.as_str() {
            "--nolimit" => cfg.nolimit = true,
            _ => {
                let (key, value) = arg
                    .split_once('=')
                    .ok_or_else(|| format!("unknown argument: {arg}"))?;
                match key {
                    "--mode" => {
                        cfg.mode = match value {
                            "static" => Mode::Static,
                            "animate" => Mode::Animate,
                            "flight" => Mode::Flight,
                            other => return Err(format!("unknown mode: {other}")),
                        };
                    }
                    "--clickthrough" => {
                        cfg.clickthrough = match value {
                            "0" => false,
                            "1" => true,
                            other => return Err(format!("clickthrough must be 0 or 1: {other}")),
                        };
                    }
                    "--seconds" => {
                        cfg.seconds = value.parse::<u64>().map_err(|_| {
                            format!("seconds must be a non-negative integer: {value}")
                        })?;
                    }
                    other => return Err(format!("unknown argument: {other}")),
                }
            }
        }
    }
    Ok(cfg)
}

struct ComGuard;

impl ComGuard {
    fn init(threading: windows::Win32::System::Com::COINIT) -> Result<Self, String> {
        let hr = unsafe { CoInitializeEx(None, threading) };
        if hr.is_err() {
            return Err(format!("CoInitializeEx failed: 0x{:08X}", hr.0));
        }
        Ok(Self)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        // Safe: every CoInitializeEx in this process is paired with one call.
        unsafe { CoUninitialize() };
    }
}

fn filetime_to_u64(ft: &FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
}

fn process_cpu_100ns() -> u64 {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    if ok.is_err() {
        return 0;
    }
    filetime_to_u64(&kernel) + filetime_to_u64(&user)
}

fn process_memory() -> (u64, u64) {
    let mut counters = PROCESS_MEMORY_COUNTERS_EX {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    let ok = unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters as *mut _ as *mut PROCESS_MEMORY_COUNTERS,
            counters.cb,
        )
    };
    if ok.is_err() {
        return (0, 0);
    }
    (counters.PrivateUsage as u64, counters.WorkingSetSize as u64)
}

fn qpc_frequency() -> f64 {
    let mut frequency = 0i64;
    let ok = unsafe { QueryPerformanceFrequency(&mut frequency) };
    if ok.is_err() || frequency <= 0 {
        10_000_000.0
    } else {
        frequency as f64
    }
}

fn qpc_ticks() -> i64 {
    let mut ticks = 0i64;
    let ok = unsafe { QueryPerformanceCounter(&mut ticks) };
    if ok.is_err() { 0 } else { ticks }
}

fn hresult_is_device_lost(hr: windows::core::HRESULT) -> bool {
    hr == DXGI_ERROR_DEVICE_REMOVED || hr == DXGI_ERROR_DEVICE_RESET
}

#[derive(Clone, Copy, Debug)]
struct Layout {
    width_px: i32,
    height_px: i32,
    dpi: u32,
    scale: f32,
    card_w: f32,
    card_h: f32,
    card_x: f32,
    card_y: f32,
    photo: D2D_RECT_F,
    shadow_offset: f32,
}

impl Layout {
    fn new(mode: Mode, width_px: i32, height_px: i32, dpi: u32) -> Self {
        let scale = dpi as f32 / 96.0;
        let width_dip = width_px as f32 / scale;
        let height_dip = height_px as f32 / scale;
        let (card_w, card_h, card_x, card_y) = match mode {
            Mode::Flight => {
                let card_h = (height_dip - 120.0).clamp(240.0, FLIGHT_CARD);
                let card_w = card_h;
                (
                    card_w,
                    card_h,
                    (width_dip - card_w) * 0.5,
                    (height_dip - card_h) * 0.5,
                )
            }
            Mode::Static | Mode::Animate => {
                let card_h = (height_dip - 50.0).clamp(120.0, 260.0);
                let card_w = (card_h * 1.7).min(360.0);
                (card_w, card_h, (width_dip - card_w) * 0.5, 20.0)
            }
        };
        Self {
            width_px,
            height_px,
            dpi,
            scale,
            card_w,
            card_h,
            card_x,
            card_y,
            photo: D2D_RECT_F {
                left: PHOTO_INSET,
                top: PHOTO_INSET,
                right: card_w - PHOTO_INSET,
                bottom: card_h - PHOTO_INSET,
            },
            shadow_offset: SHADOW_OFFSET_Y,
        }
    }

    fn card_px(&self, dx: f32, origin_x: i32, origin_y: i32) -> (i32, i32, i32, i32) {
        let x = origin_x + ((self.card_x + dx) * self.scale).round() as i32;
        let y = origin_y + (self.card_y * self.scale).round() as i32;
        let w = (self.card_w * self.scale).round() as i32;
        let h = (self.card_h * self.scale).round() as i32;
        (x, y, w, h)
    }
}

struct Gpu {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    dcomp: IDCompositionDevice,
    visual: IDCompositionVisual,
    target: IDCompositionTarget,
    factory: IDXGIFactory2,
}

/// Hand-off bundle from the UI thread to the render thread.
///
/// SAFETY: D3D11, DXGI and DirectComposition interfaces are free-threaded; the
/// UI thread stops using them after this move. `HWND` is only used for
/// `PostMessageW` and `DwmGetCompositionTimingInfo`, both of which are safe
/// from another thread while the window is alive.
struct RenderInit {
    gpu: Gpu,
    cfg: Config,
    layout: Layout,
    hwnd: HWND,
    window_x: i32,
    window_y: i32,
    window_w: i32,
    window_h: i32,
}

unsafe impl Send for RenderInit {}

fn work_area() -> RECT {
    let mut rect = RECT::default();
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut rect as *mut _ as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    if ok.is_err() || rect.right <= rect.left || rect.bottom <= rect.top {
        let w = unsafe { GetSystemMetrics(SM_CXSCREEN) };
        let h = unsafe { GetSystemMetrics(SM_CYSCREEN) };
        RECT {
            left: 0,
            top: 0,
            right: w,
            bottom: h,
        }
    } else {
        rect
    }
}

fn window_rect(cfg: &Config) -> (i32, i32, i32, i32) {
    let work = work_area();
    match cfg.mode {
        Mode::Flight => {
            let w = FLIGHT_WIDTH_PX.min(work.right - work.left);
            let h = FLIGHT_HEIGHT_PX.min(work.bottom - work.top);
            (
                work.left + (work.right - work.left - w) / 2,
                work.top + (work.bottom - work.top - h) / 2,
                w,
                h,
            )
        }
        Mode::Static | Mode::Animate => (
            work.left,
            work.top,
            work.right - work.left,
            COMPACT_HEIGHT_PX.min(work.bottom - work.top),
        ),
    }
}

fn create_window(cfg: &Config) -> Result<HWND, String> {
    let hinstance = wtry!(GetModuleHandleW(None));
    let cursor: HCURSOR = wtry!(LoadCursorW(None, IDC_ARROW));
    let wnd_class = WNDCLASSW {
        style: windows::Win32::UI::WindowsAndMessaging::CS_HREDRAW
            | windows::Win32::UI::WindowsAndMessaging::CS_VREDRAW,
        lpfnWndProc: Some(wndproc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: HINSTANCE(hinstance.0),
        hIcon: HICON::default(),
        hCursor: cursor,
        hbrBackground: HBRUSH::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: CLASS_NAME,
    };
    let atom = unsafe { RegisterClassW(&wnd_class) };
    if atom == 0 {
        return Err(format!(
            "RegisterClassW failed: {}",
            windows::core::Error::from_thread()
        ));
    }

    let mut ex_style =
        WS_EX_NOREDIRECTIONBITMAP | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST;
    if cfg.clickthrough {
        // WS_EX_LAYERED is only an input-transparency flag here: the window has
        // no redirection bitmap and no UpdateLayeredWindow call, the pixels come
        // from the DirectComposition swap chain.
        ex_style |= WS_EX_TRANSPARENT | WS_EX_LAYERED;
    }
    let (x, y, w, h) = window_rect(cfg);
    let hwnd = wtry!(CreateWindowExW(
        ex_style,
        CLASS_NAME,
        WINDOW_TITLE,
        WS_POPUP,
        x,
        y,
        w,
        h,
        None,
        None,
        Some(HINSTANCE(hinstance.0)),
        None,
    ));
    Ok(hwnd)
}

fn create_gpu(hwnd: HWND) -> Result<Gpu, String> {
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    let mut level = D3D_FEATURE_LEVEL_11_0;
    let create = unsafe {
        D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            D3D_DRIVER_TYPE_HARDWARE,
            Default::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut level),
            Some(&mut context),
        )
    };
    if create.is_err() {
        // 11.1 is not available on every Windows 10 machine; retry with 11.0.
        let levels = [D3D_FEATURE_LEVEL_11_0];
        let retry = unsafe {
            D3D11CreateDevice(
                None::<&IDXGIAdapter>,
                D3D_DRIVER_TYPE_HARDWARE,
                Default::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut level),
                Some(&mut context),
            )
        };
        retry.map_err(|e| format!("D3D11CreateDevice failed: {e}"))?;
    }
    let device = device.ok_or_else(|| "D3D11 device was not created".to_string())?;
    let context = context.ok_or_else(|| "D3D11 immediate context was not created".to_string())?;
    let dxgi_device: IDXGIDevice = device
        .cast()
        .map_err(|e| format!("ID3D11Device -> IDXGIDevice: {e}"))?;
    let adapter = wtry!(dxgi_device.GetAdapter());
    let factory: IDXGIFactory2 = wtry!(adapter.GetParent());
    let dcomp: IDCompositionDevice = wtry!(DCompositionCreateDevice(&dxgi_device));
    let target = wtry!(dcomp.CreateTargetForHwnd(hwnd, true));
    let visual = wtry!(dcomp.CreateVisual());
    wtry!(target.SetRoot(&visual));
    wtry!(dcomp.Commit());

    let _ = level;
    Ok(Gpu {
        device,
        context,
        dcomp,
        visual,
        target,
        factory,
    })
}

struct ShadowTexture {
    bitmap: ID2D1Bitmap1,
    total_dip: f32,
    pad_dip: f32,
    corner_dip: f32,
}

struct TestTexture {
    bitmap: ID2D1Bitmap1,
    width_px: u32,
    height_px: u32,
}

struct Renderer {
    swap_chain: IDXGISwapChain1,
    swap_chain3: IDXGISwapChain3,
    _d3d_context: ID3D11DeviceContext,
    context: ID2D1DeviceContext,
    targets: [Option<ID2D1Bitmap1>; 2],
    target_props: D2D1_BITMAP_PROPERTIES1,
    fill: ID2D1LinearGradientBrush,
    border: ID2D1LinearGradientBrush,
    photo_backdrop: ID2D1SolidColorBrush,
    photo_edge: ID2D1SolidColorBrush,
    photo: TestTexture,
    shadow: ShadowTexture,
    _dcomp: IDCompositionDevice,
    _visual: IDCompositionVisual,
    _target: IDCompositionTarget,
    hwnd: HWND,
    cfg: Config,
    layout: Layout,
    window_x: i32,
    window_y: i32,
    window_w: i32,
    window_h: i32,
}

impl Renderer {
    fn new(init: RenderInit) -> Result<Self, String> {
        // SAFETY: the factory and device were created on the UI thread and
        // handed over with RenderInit; this render thread is the only user from
        // here on. The swap chain and D2D device are created on this thread.
        let swap_desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: init.window_w as u32,
            Height: init.window_h as u32,
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
            AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
            Flags: 0,
        };
        let swap_chain: IDXGISwapChain1 = wtry!(init.gpu.factory.CreateSwapChainForComposition(
            &init.gpu.device,
            &swap_desc,
            None::<&IDXGIOutput>,
        ));
        let swap_chain3: IDXGISwapChain3 = swap_chain
            .cast()
            .map_err(|e| format!("IDXGISwapChain1 -> IDXGISwapChain3: {e}"))?;

        let d2d_factory: ID2D1Factory1 =
            wtry!(D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None));
        let dxgi_device: IDXGIDevice = init
            .gpu
            .device
            .cast()
            .map_err(|e| format!("ID3D11Device -> IDXGIDevice: {e}"))?;
        let d2d_device = wtry!(d2d_factory.CreateDevice(&dxgi_device));
        let context: ID2D1DeviceContext =
            wtry!(d2d_device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE));
        unsafe {
            context.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            context.SetDpi(init.layout.dpi as f32, init.layout.dpi as f32);
        }

        let target_props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
            colorContext: ManuallyDrop::new(None),
        };
        let mut targets = [None, None];
        let target0 = create_target(&context, &swap_chain, 0, &target_props)?;
        unsafe { context.SetTarget(&target0) };
        targets[0] = Some(target0);

        let (fill, border, photo_backdrop, photo_edge) = create_brushes(&context, &init.layout)?;
        let photo = create_test_texture(&context, init.cfg.mode)?;
        let shadow = create_shadow_texture(&context, init.layout.scale)?;

        // SetContent and Commit happen on the render thread. This is the
        // cross-thread DComp check in ARCHITECTURE.md §12.6.
        let content: windows::core::IUnknown = swap_chain
            .cast()
            .map_err(|e| format!("swap chain -> IUnknown: {e}"))?;
        wtry!(init.gpu.visual.SetContent(&content));
        wtry!(init.gpu.dcomp.Commit());

        Ok(Self {
            swap_chain,
            swap_chain3,
            _d3d_context: init.gpu.context,
            context,
            targets,
            target_props,
            fill,
            border,
            photo_backdrop,
            photo_edge,
            photo,
            shadow,
            _dcomp: init.gpu.dcomp,
            _visual: init.gpu.visual,
            _target: init.gpu.target,
            hwnd: init.hwnd,
            cfg: init.cfg,
            layout: init.layout,
            window_x: init.window_x,
            window_y: init.window_y,
            window_w: init.window_w,
            window_h: init.window_h,
        })
    }

    fn ensure_target(&mut self) -> Result<(), String> {
        // SAFETY: the swap chain is owned by this renderer and the returned
        // index is bounds-checked below. A D2D target is created lazily from the
        // current back buffer; only one target per buffer is kept.
        let index = unsafe { self.swap_chain3.GetCurrentBackBufferIndex() } as usize;
        if index >= self.targets.len() {
            return Err(format!("swap chain returned buffer index {index}"));
        }
        if self.targets[index].is_none() {
            let bitmap = create_target(
                &self.context,
                &self.swap_chain,
                index as u32,
                &self.target_props,
            )?;
            self.targets[index] = Some(bitmap);
        }
        if let Some(bitmap) = self.targets[index].as_ref() {
            unsafe {
                self.context.SetTarget(bitmap);
                self.context
                    .SetDpi(self.layout.dpi as f32, self.layout.dpi as f32);
            }
        }
        Ok(())
    }

    fn draw(&mut self, dx: f32) -> Result<(), String> {
        self.ensure_target()?;
        // SAFETY: the target was set in ensure_target(); the context, brushes,
        // textures and shadow bitmap were all created on this thread and are
        // kept alive by `self`. All geometry below is a stack value valid for
        // the duration of the draw calls.
        unsafe {
            self.context.BeginDraw();
            self.context.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }));
        }
        let layout = self.layout;
        unsafe {
            self.context
                .SetTransform(&Matrix3x2::translation(layout.card_x + dx, layout.card_y));
        }
        self.draw_shadow()?;

        let card = D2D1_ROUNDED_RECT {
            rect: D2D_RECT_F {
                left: 0.0,
                top: 0.0,
                right: layout.card_w,
                bottom: layout.card_h,
            },
            radiusX: CARD_RADIUS,
            radiusY: CARD_RADIUS,
        };
        unsafe {
            self.context.FillRoundedRectangle(&card, &self.fill);
        }
        unsafe {
            self.context
                .FillRectangle(&layout.photo, &self.photo_backdrop);
        }

        let dst = fit_rect(
            layout.photo,
            self.photo.width_px as f32,
            self.photo.height_px as f32,
        );
        let src = D2D_RECT_F {
            left: 0.0,
            top: 0.0,
            right: self.photo.width_px as f32,
            bottom: self.photo.height_px as f32,
        };
        unsafe {
            self.context.DrawBitmap(
                &self.photo.bitmap,
                Some(&dst),
                1.0,
                D2D1_INTERPOLATION_MODE_LINEAR,
                Some(&src),
                None,
            );
        }

        let photo_edge = D2D1_ROUNDED_RECT {
            rect: layout.photo,
            radiusX: 4.0,
            radiusY: 4.0,
        };
        unsafe {
            self.context.DrawRoundedRectangle(
                &photo_edge,
                &self.photo_edge,
                1.0 / layout.scale,
                None,
            );
        }

        let border_width = 1.0 / layout.scale;
        unsafe {
            self.context
                .DrawRoundedRectangle(&card, &self.border, border_width, None);
        }
        wtry!(self.context.EndDraw(None, None));
        Ok(())
    }

    fn draw_shadow(&mut self) -> Result<(), String> {
        // SAFETY: the shadow bitmap and the D2D target belong to this renderer;
        // the nine source/destination rectangles are stack values.
        let layout = self.layout;
        let pad = self.shadow.pad_dip;
        let corner = self.shadow.corner_dip;
        let total = self.shadow.total_dip;
        let x0 = -pad;
        let y0 = -pad + layout.shadow_offset;
        let x1 = layout.card_w + pad;
        let y1 = layout.card_h + pad + layout.shadow_offset;
        let inner_w = (x1 - x0) - 2.0 * corner;
        let inner_h = (y1 - y0) - 2.0 * corner;
        if inner_w <= 0.0 || inner_h <= 0.0 || total - 2.0 * corner <= 0.0 {
            return Err("card is too small for the nine-slice shadow".to_string());
        }

        let src_x = [0.0, corner, total - corner];
        let src_w = [corner, total - 2.0 * corner, corner];
        let dst_x = [x0, x0 + corner, x1 - corner];
        let dst_w = [corner, inner_w, corner];
        let src_y = [0.0, corner, total - corner];
        let src_w_y = [corner, total - 2.0 * corner, corner];
        let dst_y = [y0, y0 + corner, y1 - corner];
        let dst_h = [corner, inner_h, corner];

        for row in 0..3 {
            for col in 0..3 {
                let src = D2D_RECT_F {
                    left: src_x[col],
                    top: src_y[row],
                    right: src_x[col] + src_w[col],
                    bottom: src_y[row] + src_w_y[row],
                };
                let dst = D2D_RECT_F {
                    left: dst_x[col],
                    top: dst_y[row],
                    right: dst_x[col] + dst_w[col],
                    bottom: dst_y[row] + dst_h[row],
                };
                unsafe {
                    self.context.DrawBitmap(
                        &self.shadow.bitmap,
                        Some(&dst),
                        1.0,
                        D2D1_INTERPOLATION_MODE_LINEAR,
                        Some(&src),
                        None,
                    );
                }
            }
        }
        Ok(())
    }
}

fn create_target(
    context: &ID2D1DeviceContext,
    swap_chain: &IDXGISwapChain1,
    index: u32,
    props: &D2D1_BITMAP_PROPERTIES1,
) -> Result<ID2D1Bitmap1, String> {
    let surface: IDXGISurface = wtry!(swap_chain.GetBuffer(index));
    let bitmap = wtry!(context.CreateBitmapFromDxgiSurface(&surface, Some(props)));
    Ok(bitmap)
}

fn create_brushes(
    context: &ID2D1DeviceContext,
    layout: &Layout,
) -> Result<
    (
        ID2D1LinearGradientBrush,
        ID2D1LinearGradientBrush,
        ID2D1SolidColorBrush,
        ID2D1SolidColorBrush,
    ),
    String,
> {
    let fill_stops = [
        D2D1_GRADIENT_STOP {
            position: 0.0,
            color: D2D1_COLOR_F {
                r: 0.98,
                g: 0.99,
                b: 1.0,
                a: 0.70,
            },
        },
        D2D1_GRADIENT_STOP {
            position: 1.0,
            color: D2D1_COLOR_F {
                r: 0.80,
                g: 0.88,
                b: 0.98,
                a: 0.42,
            },
        },
    ];
    let fill_stops = wtry!(context.CreateGradientStopCollection(
        &fill_stops,
        D2D1_COLOR_SPACE_SRGB,
        D2D1_COLOR_SPACE_SRGB,
        D2D1_BUFFER_PRECISION_8BPC_UNORM,
        D2D1_EXTEND_MODE_CLAMP,
        D2D1_COLOR_INTERPOLATION_MODE_PREMULTIPLIED,
    ));
    let fill_props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
        startPoint: Vector2 { X: 0.0, Y: 0.0 },
        endPoint: Vector2 {
            X: 0.0,
            Y: layout.card_h,
        },
    };
    let fill = wtry!(context.CreateLinearGradientBrush(&fill_props, None, &fill_stops));

    let border_stops = [
        D2D1_GRADIENT_STOP {
            position: 0.0,
            color: D2D1_COLOR_F {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 0.95,
            },
        },
        D2D1_GRADIENT_STOP {
            position: 0.55,
            color: D2D1_COLOR_F {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 0.18,
            },
        },
        D2D1_GRADIENT_STOP {
            position: 1.0,
            color: D2D1_COLOR_F {
                r: 0.9,
                g: 0.95,
                b: 1.0,
                a: 0.62,
            },
        },
    ];
    let border_stops = wtry!(context.CreateGradientStopCollection(
        &border_stops,
        D2D1_COLOR_SPACE_SRGB,
        D2D1_COLOR_SPACE_SRGB,
        D2D1_BUFFER_PRECISION_8BPC_UNORM,
        D2D1_EXTEND_MODE_CLAMP,
        D2D1_COLOR_INTERPOLATION_MODE_PREMULTIPLIED,
    ));
    let border_props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
        startPoint: Vector2 { X: 0.0, Y: 0.0 },
        endPoint: Vector2 {
            X: layout.card_w,
            Y: layout.card_h,
        },
    };
    let border = wtry!(context.CreateLinearGradientBrush(&border_props, None, &border_stops));

    let photo_backdrop = wtry!(context.CreateSolidColorBrush(
        &D2D1_COLOR_F {
            r: 0.03,
            g: 0.05,
            b: 0.08,
            a: 0.60,
        },
        None,
    ));
    let photo_edge = wtry!(context.CreateSolidColorBrush(
        &D2D1_COLOR_F {
            r: 1.0,
            g: 1.0,
            b: 1.0,
            a: 0.28,
        },
        None,
    ));
    Ok((fill, border, photo_backdrop, photo_edge))
}

fn create_test_texture(context: &ID2D1DeviceContext, mode: Mode) -> Result<TestTexture, String> {
    let (width_px, height_px) = match mode {
        Mode::Flight => (1600u32, 1600u32),
        Mode::Static | Mode::Animate => (480u32, 270u32),
    };
    let mut pixels = vec![0u8; (width_px * height_px * 4) as usize];
    for y in 0..height_px {
        let v = y as f32 / (height_px - 1).max(1) as f32;
        for x in 0..width_px {
            let u = x as f32 / (width_px - 1).max(1) as f32;
            let band = ((x as f32 * 0.055 + y as f32 * 0.085).sin() * 0.5 + 0.5) * 0.16;
            let glow = (-((u - 0.28).powi(2) + (v - 0.22).powi(2)) * 7.0).exp();
            let r = (42.0 + 196.0 * u + 26.0 * band + 34.0 * glow).clamp(0.0, 255.0);
            let g = (64.0 + 112.0 * (1.0 - v) + 70.0 * band + 40.0 * glow).clamp(0.0, 255.0);
            let b = (150.0 + 88.0 * (1.0 - u) - 44.0 * band + 26.0 * glow).clamp(0.0, 255.0);
            let i = ((y * width_px + x) * 4) as usize;
            pixels[i] = b as u8;
            pixels[i + 1] = g as u8;
            pixels[i + 2] = r as u8;
            pixels[i + 3] = 255;
        }
    }
    let props = D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: 96.0,
        dpiY: 96.0,
        bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
        colorContext: ManuallyDrop::new(None),
    };
    // SAFETY: `pixels` is alive for the call and D2D copies the source data
    // into the bitmap before returning.
    let bitmap = wtry!(context.CreateBitmap(
        D2D_SIZE_U {
            width: width_px,
            height: height_px,
        },
        Some(pixels.as_ptr() as *const c_void),
        width_px * 4,
        &props,
    ));
    Ok(TestTexture {
        bitmap,
        width_px,
        height_px,
    })
}

fn create_shadow_texture(
    context: &ID2D1DeviceContext,
    scale: f32,
) -> Result<ShadowTexture, String> {
    // Baked once. A blurred rounded silhouette in a 160 DIP square; the centre
    // is stretched by the nine-slice draw, the corners keep the blur.
    let total_px = (SHADOW_TOTAL_DIP * scale).round().max(32.0) as u32;
    let pad_px = (SHADOW_PAD_DIP * scale).round().max(4.0);
    let radius_px = (CARD_RADIUS * scale).round().max(2.0);
    let sigma_px = (8.0 * scale).max(1.0);
    let corner_px = (SHADOW_CORNER_DIP * scale)
        .round()
        .min(total_px as f32 * 0.5 - 1.0);
    let half = total_px as f32 * 0.5 - pad_px;
    let center = total_px as f32 * 0.5;
    let mut pixels = vec![0u8; (total_px * total_px * 4) as usize];
    for y in 0..total_px {
        for x in 0..total_px {
            let px = x as f32 + 0.5 - center;
            let py = y as f32 + 0.5 - center;
            let qx = px.abs() - (half - radius_px);
            let qy = py.abs() - (half - radius_px);
            let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
            let inside = qx.max(qy).min(0.0);
            let distance = outside + inside - radius_px;
            let coverage = if distance <= 0.0 {
                1.0
            } else {
                (-0.5 * (distance / sigma_px).powi(2)).exp()
            };
            let alpha = (coverage * 0.34 * 255.0).clamp(0.0, 255.0) as u8;
            let i = ((y * total_px + x) * 4) as usize;
            pixels[i] = 0;
            pixels[i + 1] = 0;
            pixels[i + 2] = 0;
            pixels[i + 3] = alpha;
        }
    }
    let props = D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: 96.0 * scale,
        dpiY: 96.0 * scale,
        bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
        colorContext: ManuallyDrop::new(None),
    };
    // SAFETY: `pixels` is alive for the call and D2D copies the source data
    // into the bitmap before returning.
    let bitmap = wtry!(context.CreateBitmap(
        D2D_SIZE_U {
            width: total_px,
            height: total_px,
        },
        Some(pixels.as_ptr() as *const c_void),
        total_px * 4,
        &props,
    ));
    Ok(ShadowTexture {
        bitmap,
        total_dip: total_px as f32 / scale,
        pad_dip: pad_px / scale,
        corner_dip: corner_px / scale,
    })
}

fn fit_rect(bounds: D2D_RECT_F, width: f32, height: f32) -> D2D_RECT_F {
    let bw = (bounds.right - bounds.left).max(1.0);
    let bh = (bounds.bottom - bounds.top).max(1.0);
    let sx = bw / width.max(1.0);
    let sy = bh / height.max(1.0);
    let s = sx.min(sy);
    let w = width * s;
    let h = height * s;
    let cx = (bounds.left + bounds.right) * 0.5;
    let cy = (bounds.top + bounds.bottom) * 0.5;
    D2D_RECT_F {
        left: cx - w * 0.5,
        top: cy - h * 0.5,
        right: cx + w * 0.5,
        bottom: cy + h * 0.5,
    }
}

struct Spring {
    x: f32,
    v: f32,
    target: f32,
    stiffness: f32,
    damping: f32,
}

impl Spring {
    fn new(response: f32, damping_ratio: f32) -> Self {
        let k = (std::f32::consts::TAU / response).powi(2);
        let c = 4.0 * std::f32::consts::PI * damping_ratio / response;
        Self {
            x: 0.0,
            v: 0.0,
            target: 0.0,
            stiffness: k,
            damping: c,
        }
    }

    fn step(&mut self, dt: f32) {
        let mut remaining = dt.clamp(0.0, 0.05);
        while remaining > 0.0 {
            let h = remaining.min(1.0 / 240.0);
            let accel = -self.stiffness * (self.x - self.target) - self.damping * self.v;
            self.v += accel * h;
            self.x += self.v * h;
            remaining -= h;
        }
    }
}

struct Stats {
    intervals: Vec<f64>,
    draws: Vec<f64>,
    frames: u64,
    presented: u64,
    skipped: u64,
    device_lost: u64,
    start_ticks: i64,
    start_cpu: u64,
    last_frame_ms: f64,
    last_memory_ms: f64,
    mem_first: (u64, u64),
    mem_last: (u64, u64),
    mem_min_private: u64,
    mem_max_private: u64,
    mem_max_ws: u64,
    frequency: f64,
}

impl Stats {
    fn new(frequency: f64, sample_capacity: usize) -> Self {
        let (private, working_set) = process_memory();
        Self {
            intervals: Vec::with_capacity(sample_capacity),
            draws: Vec::with_capacity(sample_capacity),
            frames: 0,
            presented: 0,
            skipped: 0,
            device_lost: 0,
            start_ticks: qpc_ticks(),
            start_cpu: process_cpu_100ns(),
            last_frame_ms: 0.0,
            last_memory_ms: -1_000.0,
            mem_first: (private, working_set),
            mem_last: (private, working_set),
            mem_min_private: private,
            mem_max_private: private,
            mem_max_ws: working_set,
            frequency,
        }
    }

    fn sample_memory(&mut self) {
        let (private, working_set) = process_memory();
        if private == 0 && working_set == 0 {
            return;
        }
        if self.mem_first.0 == 0 {
            self.mem_first = (private, working_set);
        }
        self.mem_last = (private, working_set);
        self.mem_min_private = self.mem_min_private.min(private);
        self.mem_max_private = self.mem_max_private.max(private);
        self.mem_max_ws = self.mem_max_ws.max(working_set);
    }

    fn note_frame(&mut self, start_ms: f64, draw_ms: f64) {
        if self.frames > 0 && self.intervals.len() < MAX_INTERVALS {
            self.intervals
                .push((start_ms - self.last_frame_ms).max(0.0));
        }
        if self.draws.len() < MAX_INTERVALS {
            self.draws.push(draw_ms);
        }
        self.frames += 1;
        self.last_frame_ms = start_ms;
    }

    fn finish(&mut self, hz: f64) -> Summary {
        let end_ticks = qpc_ticks();
        let end_cpu = process_cpu_100ns();
        self.sample_memory();
        let wall_ms = ((end_ticks - self.start_ticks) as f64 * 1000.0 / self.frequency).max(0.001);
        let cpu_ms = (end_cpu.saturating_sub(self.start_cpu)) as f64 / 10_000.0;
        let fps = if self.frames > 1 && self.last_frame_ms > 0.0 {
            (self.frames - 1) as f64 * 1000.0 / self.last_frame_ms
        } else {
            0.0
        };
        self.intervals.sort_unstable_by(f64::total_cmp);
        self.draws.sort_unstable_by(f64::total_cmp);
        let period_ms = 1000.0 / hz.max(1.0);
        let jank = if self.intervals.is_empty() {
            0.0
        } else {
            let over = self
                .intervals
                .iter()
                .filter(|ms| **ms > period_ms * 1.5)
                .count();
            over as f64 * 100.0 / self.intervals.len() as f64
        };
        Summary {
            wall_ms,
            cpu_ms,
            fps,
            p50: percentile(&self.intervals, 0.50),
            p95: percentile(&self.intervals, 0.95),
            max: self.intervals.last().copied().unwrap_or(0.0),
            jank,
            draw_p50: percentile(&self.draws, 0.50),
            draw_p95: percentile(&self.draws, 0.95),
            draw_max: self.draws.last().copied().unwrap_or(0.0),
            frames: self.frames,
            presented: self.presented,
            skipped: self.skipped,
            device_lost: self.device_lost,
            cpu_ms_frame: cpu_ms / self.frames.max(1) as f64,
            cpu_pct: cpu_ms * 100.0 / wall_ms,
            mem_first: self.mem_first,
            mem_last: self.mem_last,
            mem_min_private: self.mem_min_private,
            mem_max_private: self.mem_max_private,
            mem_max_ws: self.mem_max_ws,
        }
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (p * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

struct Summary {
    wall_ms: f64,
    cpu_ms: f64,
    fps: f64,
    p50: f64,
    p95: f64,
    max: f64,
    jank: f64,
    draw_p50: f64,
    draw_p95: f64,
    draw_max: f64,
    frames: u64,
    presented: u64,
    skipped: u64,
    device_lost: u64,
    cpu_ms_frame: f64,
    cpu_pct: f64,
    mem_first: (u64, u64),
    mem_last: (u64, u64),
    mem_min_private: u64,
    mem_max_private: u64,
    mem_max_ws: u64,
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn detect_refresh(hwnd: HWND) -> f64 {
    let mut timing = DWM_TIMING_INFO {
        cbSize: size_of::<DWM_TIMING_INFO>() as u32,
        ..Default::default()
    };
    if unsafe { DwmGetCompositionTimingInfo(hwnd, &mut timing) }.is_ok()
        && timing.rateRefresh.uiDenominator > 0
        && timing.rateRefresh.uiNumerator > 0
    {
        return timing.rateRefresh.uiNumerator as f64 / timing.rateRefresh.uiDenominator as f64;
    }
    let mut devmode = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    if unsafe { EnumDisplaySettingsW(PCWSTR::null(), ENUM_CURRENT_SETTINGS, &mut devmode) }
        .as_bool()
        && devmode.dmDisplayFrequency > 1
    {
        return devmode.dmDisplayFrequency as f64;
    }
    60.0
}

fn display_summary() -> (u32, u32, f64, f64, bool) {
    let mut current = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    let current_ok =
        unsafe { EnumDisplaySettingsW(PCWSTR::null(), ENUM_CURRENT_SETTINGS, &mut current) }
            .as_bool();
    let width = if current_ok { current.dmPelsWidth } else { 0 };
    let height = if current_ok { current.dmPelsHeight } else { 0 };
    let current_hz = if current_ok {
        current.dmDisplayFrequency as f64
    } else {
        0.0
    };
    let mut max_hz = 0.0f64;
    let mut high_refresh_at_current = false;
    for index in 0..2048u32 {
        let mut mode = DEVMODEW {
            dmSize: size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        let ok = unsafe {
            EnumDisplaySettingsW(PCWSTR::null(), ENUM_DISPLAY_SETTINGS_MODE(index), &mut mode)
        };
        if !ok.as_bool() {
            break;
        }
        let hz = mode.dmDisplayFrequency as f64;
        max_hz = max_hz.max(hz);
        if width > 0
            && height > 0
            && mode.dmPelsWidth == width
            && mode.dmPelsHeight == height
            && hz >= 100.0
        {
            high_refresh_at_current = true;
        }
    }
    (width, height, current_hz, max_hz, high_refresh_at_current)
}

fn run_render(
    init: RenderInit,
    setup_tx: mpsc::SyncSender<Result<(), String>>,
) -> Result<(), String> {
    let _com = ComGuard::init(COINIT_MULTITHREADED)?;
    let mut renderer = match Renderer::new(init) {
        Ok(renderer) => renderer,
        Err(error) => {
            let _ = setup_tx.send(Err(error.clone()));
            return Err(error);
        }
    };
    let cfg = renderer.cfg;
    let frequency = qpc_frequency();
    let hz = detect_refresh(renderer.hwnd);
    let (display_w, display_h, current_hz, max_hz, high_refresh_at_current) = display_summary();
    let (card_x, card_y, card_w, card_h) =
        renderer
            .layout
            .card_px(0.0, renderer.window_x, renderer.window_y);
    let topmost = renderer.topmost();
    println!(
        "SPIKE start mode={} w={} h={} dpi={} scale={:.2} window=({}, {}, {}, {}) card={}x{}px@({}, {}) display={}x{}@{} mode_max_hz={} high_refresh_at_current={} clickthrough={} nolimit={} seconds={} hz={:.2} topmost={} dcomp=cross-thread",
        cfg.mode.as_str(),
        renderer.layout.width_px,
        renderer.layout.height_px,
        renderer.layout.dpi,
        renderer.layout.scale,
        renderer.window_x,
        renderer.window_y,
        renderer.window_w,
        renderer.window_h,
        card_w,
        card_h,
        card_x,
        card_y,
        display_w,
        display_h,
        current_hz,
        max_hz,
        high_refresh_at_current,
        cfg.clickthrough,
        cfg.nolimit,
        cfg.seconds,
        hz,
        topmost
    );
    // Warm up Direct2D shaders and the first target before the measured frame.
    // This is a one-time cost and must not be charged to the steady-state frame.
    for _ in 0..3 {
        if let Err(error) = renderer.draw(0.0) {
            let _ = setup_tx.send(Err(error.clone()));
            return Err(error);
        }
    }
    let mut stats = Stats::new(frequency, MAX_INTERVALS);

    // First frame, presented before the window becomes visible.
    let first_start = (qpc_ticks() - stats.start_ticks) as f64 * 1000.0 / frequency;
    let draw_start = qpc_ticks();
    let first_draw = match renderer.draw(0.0) {
        Ok(()) => qpc_ticks(),
        Err(error) => {
            let _ = setup_tx.send(Err(error.clone()));
            return Err(error);
        }
    };
    let first_draw_ms = (first_draw - draw_start) as f64 / 10_000.0;
    let first_hr = unsafe { renderer.swap_chain.Present(1, DXGI_PRESENT(0)) };
    if first_hr.is_err() {
        let error = format!("first Present failed: 0x{:08X}", first_hr.0);
        let _ = setup_tx.send(Err(error.clone()));
        return Err(error);
    }
    stats.note_frame(first_start, first_draw_ms);
    stats.presented = 1;
    let _ = setup_tx.send(Ok(()));

    let deadline = if cfg.seconds == 0 {
        None
    } else {
        Some(qpc_ticks() + (cfg.seconds as f64 * frequency) as i64)
    };

    if cfg.mode == Mode::Static {
        // Static means one frame, then no frame loop: the window is a still
        // image composited by DWM. That is the steady state we want for the
        // memory baseline and the idle CPU number.
        while !STOP.load(Ordering::Relaxed) {
            let now = qpc_ticks();
            if let Some(deadline) = deadline {
                if now >= deadline {
                    break;
                }
            }
            let elapsed_ms = (now - stats.start_ticks) as f64 * 1000.0 / frequency;
            if elapsed_ms - stats.last_memory_ms >= 1000.0 {
                stats.sample_memory();
                stats.last_memory_ms = elapsed_ms;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    } else {
        let mut spring = Spring::new(0.85, 0.62);
        spring.target = SPRING_DX_LIMIT;
        let mut anim_time = 0.0f64;
        let mut last_ticks = qpc_ticks();
        loop {
            if STOP.load(Ordering::Relaxed) {
                break;
            }
            let now_ticks = qpc_ticks();
            if let Some(deadline) = deadline {
                if now_ticks >= deadline {
                    break;
                }
            }
            let dt = ((now_ticks - last_ticks) as f64 / frequency).clamp(0.0, 0.05) as f32;
            last_ticks = now_ticks;
            let frame_start_ms = (now_ticks - stats.start_ticks) as f64 * 1000.0 / frequency;
            anim_time += dt as f64;
            let phase = ((anim_time / 2.5) as u32) % 2;
            spring.target = if phase == 0 {
                SPRING_DX_LIMIT
            } else {
                -SPRING_DX_LIMIT
            };
            spring.step(dt);

            let draw_begin = qpc_ticks();
            let draw_result = renderer.draw(spring.x);
            let draw_end = qpc_ticks();
            let draw_ms = (draw_end - draw_begin) as f64 / 10_000.0;
            if let Err(error) = draw_result {
                println!("SPIKE draw_fail mode={} error={error}", cfg.mode.as_str());
                break;
            }

            let sync = if cfg.nolimit { 0u32 } else { 1u32 };
            let present = unsafe { renderer.swap_chain.Present(sync, DXGI_PRESENT(0)) };
            if present == DXGI_ERROR_WAS_STILL_DRAWING {
                stats.skipped += 1;
            } else if hresult_is_device_lost(present) {
                stats.device_lost += 1;
                break;
            } else if present.is_err() {
                println!("SPIKE present_fail hresult=0x{:08X}", present.0);
                break;
            } else {
                stats.presented += 1;
            }
            stats.note_frame(frame_start_ms, draw_ms);
            if frame_start_ms - stats.last_memory_ms >= 1000.0 {
                stats.sample_memory();
                stats.last_memory_ms = frame_start_ms;
            }
        }
    }

    let mut summary = stats.finish(hz);
    let cpu_scope = if cfg.mode == Mode::Static {
        summary.cpu_ms_frame = summary.draw_p50;
        "draw"
    } else {
        "process"
    };
    println!(
        "SPIKE mode={} w={} h={} hz={:.2} frames={} fps={:.2} p50={:.2} p95={:.2} max={:.2} jank={:.2}% cpu_ms_frame={:.3} cpu_scope={} private={:.1}MB ws={:.1}MB draw_p50={:.3} draw_p95={:.3} draw_max={:.3} presented={} skipped={} device_lost={} clicks={} activations={} focus={} mouseactivate={} private_first={:.1}MB private_min={:.1}MB private_max={:.1}MB ws_max={:.1}MB wall_s={:.2} cpu_pct={:.2}% cpu_ms_total={:.1} nolimit={}",
        cfg.mode.as_str(),
        renderer.layout.width_px,
        renderer.layout.height_px,
        hz,
        summary.frames,
        summary.fps,
        summary.p50,
        summary.p95,
        summary.max,
        summary.jank,
        summary.cpu_ms_frame,
        cpu_scope,
        mb(summary.mem_last.0),
        mb(summary.mem_last.1),
        summary.draw_p50,
        summary.draw_p95,
        summary.draw_max,
        summary.presented,
        summary.skipped,
        summary.device_lost,
        CLICKS.load(Ordering::Relaxed),
        ACTIVATIONS.load(Ordering::Relaxed),
        FOCUS_EVENTS.load(Ordering::Relaxed),
        MOUSE_ACTIVATES.load(Ordering::Relaxed),
        mb(summary.mem_first.0),
        mb(summary.mem_min_private),
        mb(summary.mem_max_private),
        mb(summary.mem_max_ws),
        summary.wall_ms / 1000.0,
        summary.cpu_pct,
        summary.cpu_ms,
        cfg.nolimit
    );
    let _ = unsafe { PostMessageW(Some(renderer.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) };
    Ok(())
}

impl Renderer {
    fn topmost(&self) -> bool {
        let ex = unsafe { GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE) } as u32;
        ex & WS_EX_TOPMOST.0 != 0
    }
}

// SAFETY: `hwnd` is the live window owned by this process. Unimplemented
// messages go to DefWindowProcW; WM_CLOSE destroys the window and WM_DESTROY
// posts the quit message that ends the UI thread's message loop.
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => {
            MOUSE_ACTIVATES.fetch_add(1, Ordering::Relaxed);
            LRESULT(MA_NOACTIVATE as isize)
        }
        WM_NCHITTEST => {
            if CLICKTHROUGH.load(Ordering::Relaxed) {
                LRESULT(HTTRANSPARENT as isize)
            } else {
                LRESULT(HTCLIENT as isize)
            }
        }
        WM_LBUTTONDOWN => {
            CLICKS.fetch_add(1, Ordering::Relaxed);
            LRESULT(0)
        }
        WM_ACTIVATE => {
            if (wparam.0 as u32) & 0xffff != WA_INACTIVE {
                ACTIVATIONS.fetch_add(1, Ordering::Relaxed);
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_SETFOCUS => {
            FOCUS_EVENTS.fetch_add(1, Ordering::Relaxed);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            unsafe {
                let _ = BeginPaint(hwnd, &mut paint);
                let _ = EndPaint(hwnd, &paint);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        WM_DESTROY => {
            STOP.store(true, Ordering::Relaxed);
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn run(cfg: Config) -> Result<(), String> {
    let _com = ComGuard::init(COINIT_APARTMENTTHREADED)?;
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };

    let hwnd = create_window(&cfg)?;
    let gpu = match create_gpu(hwnd) {
        Ok(gpu) => gpu,
        Err(error) => {
            let _ = unsafe { DestroyWindow(hwnd) };
            return Err(error);
        }
    };
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    let (window_x, window_y, window_w, window_h) = window_rect(&cfg);
    let layout = Layout::new(cfg.mode, window_w, window_h, dpi);
    CLICKTHROUGH.store(cfg.clickthrough, Ordering::Relaxed);
    STOP.store(false, Ordering::Relaxed);

    let mut ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    if cfg.clickthrough {
        ex_style |= WS_EX_TRANSPARENT.0 | WS_EX_LAYERED.0;
    } else {
        ex_style &= !(WS_EX_TRANSPARENT.0 | WS_EX_LAYERED.0);
    }
    // SAFETY: hwnd is the live, still hidden window created above; the ex-style
    // value only contains bits defined for top-level windows.
    unsafe {
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex_style as isize);
        // Hit-testing only changes after a frame change; the window is still
        // hidden at this point, so no visible frame is invalidated.
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }

    let (setup_tx, setup_rx) = mpsc::sync_channel(1);
    let init = RenderInit {
        gpu,
        cfg,
        layout,
        hwnd,
        window_x,
        window_y,
        window_w,
        window_h,
    };
    let renderer = std::thread::Builder::new()
        .name("pegline-render".to_string())
        .spawn(move || run_render(init, setup_tx))
        .map_err(|e| format!("spawn render thread: {e}"))?;

    let setup = setup_rx.recv_timeout(Duration::from_secs(15));
    match setup {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            let _ = unsafe { DestroyWindow(hwnd) };
            let _ = renderer.join();
            return Err(error);
        }
        Err(error) => {
            STOP.store(true, Ordering::Relaxed);
            let _ = unsafe { DestroyWindow(hwnd) };
            let _ = renderer.join();
            return Err(format!("render thread setup did not complete: {error}"));
        }
    }

    // SAFETY: hwnd is live and still hidden; SW_SHOWNOACTIVATE and the
    // NOACTIVATE topmost call do not move keyboard focus.
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }

    let mut message = MSG::default();
    loop {
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if result.0 == 0 || result.0 == -1 {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }

    match renderer.join() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error),
        Err(_) => Err("render thread panicked".to_string()),
    }
}

fn main() {
    let mut args = std::env::args().skip(1).peekable();
    if args
        .peek()
        .is_some_and(|arg| arg == "--help" || arg == "-h")
    {
        println!("{}", usage());
        return;
    }
    let cfg = match parse_config(args) {
        Ok(cfg) => cfg,
        Err(error) => {
            eprintln!("SPIKE argument error: {error}");
            eprintln!("{}", usage());
            std::process::exit(2);
        }
    };
    if let Err(error) = run(cfg) {
        eprintln!("SPIKE error: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spring_converges_by_real_time() {
        let mut spring = Spring::new(0.4, 0.9);
        spring.target = 100.0;
        for _ in 0..120 {
            spring.step(1.0 / 60.0);
        }
        assert!((spring.x - 100.0).abs() < 2.0, "x={}", spring.x);
        assert!(spring.v.abs() < 5.0, "v={}", spring.v);
    }

    #[test]
    fn spring_is_frame_rate_independent() {
        let mut a = Spring::new(0.5, 0.7);
        let mut b = Spring::new(0.5, 0.7);
        a.target = 200.0;
        b.target = 200.0;
        for _ in 0..60 {
            a.step(1.0 / 60.0);
        }
        for _ in 0..480 {
            b.step(1.0 / 480.0);
        }
        assert!((a.x - b.x).abs() < 1.0, "a={} b={}", a.x, b.x);
    }

    #[test]
    fn percentile_uses_nearest_rank() {
        let values = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(percentile(&values, 0.50), 2.0);
        assert_eq!(percentile(&values, 0.95), 4.0);
        assert_eq!(percentile(&[], 0.95), 0.0);
    }

    #[test]
    fn compact_layout_fits_the_strip() {
        let layout = Layout::new(Mode::Animate, 6016, 420, 192);
        assert!((layout.card_w - 272.0).abs() < 0.5);
        assert!((layout.card_h - 160.0).abs() < 0.5);
        assert!(layout.card_y + layout.card_h + SHADOW_PAD_DIP + layout.shadow_offset <= 210.0);
        assert!(layout.card_x > 0.0);
    }

    #[test]
    fn flight_layout_is_square_and_centered() {
        let layout = Layout::new(Mode::Flight, 2442, 2344, 192);
        assert!((layout.card_w - layout.card_h).abs() < 0.01);
        assert!(
            (layout.card_x * 2.0 + layout.card_w - layout.width_px as f32 / layout.scale).abs()
                < 1.0
        );
    }

    #[test]
    fn config_parses_the_documented_switches() {
        let cfg = parse_config(
            [
                "--mode=flight",
                "--clickthrough=1",
                "--seconds=300",
                "--nolimit",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .expect("valid arguments");
        assert_eq!(cfg.mode, Mode::Flight);
        assert!(cfg.clickthrough);
        assert_eq!(cfg.seconds, 300);
        assert!(cfg.nolimit);
    }

    #[test]
    fn config_rejects_unknown_switches() {
        assert!(parse_config(["--nope=1".to_string()].into_iter()).is_err());
        assert!(parse_config(["--clickthrough=2".to_string()].into_iter()).is_err());
        assert!(parse_config(["--seconds=nope".to_string()].into_iter()).is_err());
    }
}
