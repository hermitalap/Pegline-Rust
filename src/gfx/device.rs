//! The device-creation sequence every composition window runs, once.
//!
//! The line (`crate::render::Renderer`), the annotation editor
//! (`crate::editor::window::Surface`) and the note editor
//! (`crate::note_editor::render::Surface`) all build the same stack in the same
//! order: D3D11 device → DXGI device → DComp factory/target/visual → swap chain
//! → Direct2D, then DirectWrite. This module holds that sequence; the owners
//! keep their own objects and call `SetContent` + `Commit` themselves, after
//! they have built their own resources.
//!
//! The only input that genuinely differs is the Direct2D text antialias mode
//! (`create_d2d`); everything else is byte-for-byte the same sequence.

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D_SIZE_U, D2D1_ALPHA_MODE, D2D1_ALPHA_MODE_IGNORE, D2D1_ALPHA_MODE_PREMULTIPLIED,
    D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_NONE,
    D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1, D2D1_DEVICE_CONTEXT_OPTIONS_NONE,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_ROUNDED_RECT, D2D1_TEXT_ANTIALIAS_MODE,
    D2D1CreateFactory, ID2D1Bitmap1, ID2D1Device, ID2D1DeviceContext, ID2D1Factory1, ID2D1Image,
    ID2D1RenderTarget, ID2D1SolidColorBrush,
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
    DWRITE_FONT_WEIGHT, DWRITE_TEXT_METRICS, DWriteCreateFactory, IDWriteFactory,
    IDWriteTextFormat, IDWriteTextLayout,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET, DXGI_ERROR_WAS_STILL_DRAWING, DXGI_PRESENT,
    DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG,
    DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIAdapter, IDXGIDevice,
    IDXGIFactory2, IDXGIOutput, IDXGISurface, IDXGISwapChain1, IDXGISwapChain3,
};
use windows::core::{Interface, PCWSTR, w};
use windows_numerics::Matrix3x2;

use crate::types::{ImageData, RectF};

/// Creates the hardware D3D11 device and its immediate context.
///
/// 11.1 is requested first and 11.0 is the documented floor; there is no adapter
/// argument, so DXGI picks the default hardware adapter.
pub(crate) fn create_d3d_device() -> Result<(ID3D11Device, ID3D11DeviceContext), String> {
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

/// Creates the DComp device, its HWND target and the root visual, and commits.
pub(crate) fn create_composition(
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
    // SAFETY: `dxgi_device` is a live DXGI device owned by this thread, and the
    // DComp objects are created from it and kept alive together.
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

/// Creates the flip-model composition swap chain and its DXGI 3 view.
///
/// The window needs no output adapter. The chain is premultiplied because every
/// window here is transparent around its content; the card and the paper are
/// opaque bitmaps composited underneath.
pub(crate) fn create_swap_chain(
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

/// Creates the Direct2D factory, device and device context at `dpi`.
///
/// `text_antialias` is the caller's decision, because it depends on what the
/// window composites:
///
/// - `D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE` where the swap chain carries
///   premultiplied alpha and text is drawn straight into it (the line and the
///   note editor). ClearType is not valid there; ask for grayscale explicitly
///   instead of relying on Direct2D's silent fallback.
/// - `D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE` in the annotation editor, which has a
///   transparent window like the others: its text marks and toolbar are drawn
///   into the opaque card bitmap, so subpixel rendering is valid there. Its
///   saved pictures use a separate alpha target, which must stay grayscale.
///
/// The antialias mode for geometry is per-primitive everywhere.
pub(crate) fn create_d2d(
    dxgi_device: &IDXGIDevice,
    dpi: f32,
    text_antialias: D2D1_TEXT_ANTIALIAS_MODE,
) -> Result<(ID2D1Factory1, ID2D1Device, ID2D1DeviceContext), String> {
    // SAFETY: a single-threaded factory used only on this thread.
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
        context.SetTextAntialiasMode(text_antialias);
        context.SetDpi(dpi, dpi);
        Ok((factory, device, context))
    }
}

/// A D2D rectangle from the app's float rect.
pub(crate) fn d2d_rect(rect: RectF) -> D2D_RECT_F {
    D2D_RECT_F {
        left: rect.left,
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
    }
}

/// A rounded D2D rectangle.
pub(crate) fn rounded(rect: RectF, radius: f32) -> D2D1_ROUNDED_RECT {
    D2D1_ROUNDED_RECT {
        rect: d2d_rect(rect),
        radiusX: radius,
        radiusY: radius,
    }
}

/// A D2D pixel size.
pub(crate) fn size_u(width: u32, height: u32) -> D2D_SIZE_U {
    D2D_SIZE_U { width, height }
}

/// The BGRA pixel format every bitmap here uses.
pub(crate) fn pixel_format(alpha: D2D1_ALPHA_MODE) -> D2D1_PIXEL_FORMAT {
    D2D1_PIXEL_FORMAT {
        format: DXGI_FORMAT_B8G8R8A8_UNORM,
        alphaMode: alpha,
    }
}

/// A solid brush from `0xRRGGBB` and an alpha multiplier.
pub(crate) fn solid(
    rt: &ID2D1RenderTarget,
    rgb: u32,
    alpha: f32,
) -> Result<ID2D1SolidColorBrush, String> {
    let color = D2D1_COLOR_F {
        r: ((rgb >> 16) & 0xFF) as f32 / 255.0,
        g: ((rgb >> 8) & 0xFF) as f32 / 255.0,
        b: (rgb & 0xFF) as f32 / 255.0,
        a: alpha,
    };
    // SAFETY: the render target is alive; the colour is passed by value.
    unsafe { rt.CreateSolidColorBrush(&color, None) }
        .map_err(|error| format!("CreateSolidColorBrush: {error}"))
}

/// Creates a text format for `family` at `size`.
pub(crate) fn create_text_format(
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

/// Lays `text` out in a very wide box: the metrics report the natural width,
/// which is what the pills are sized from. No wrapping is wanted here.
pub(crate) fn create_text_layout(
    dwrite: &IDWriteFactory,
    format: &IDWriteTextFormat,
    text: &str,
) -> Result<IDWriteTextLayout, String> {
    let wide: Vec<u16> = text.encode_utf16().collect();
    // SAFETY: `wide` is alive for the call and the factory/format are borrowed.
    unsafe { dwrite.CreateTextLayout(&wide, format, 4096.0, 4096.0) }
        .map_err(|error| format!("CreateTextLayout: {error}"))
}

/// The layout's metrics; a zero-sized result would draw nothing, so a failure
/// is logged before it is returned.
pub(crate) fn text_metrics(layout: &IDWriteTextLayout) -> DWRITE_TEXT_METRICS {
    let mut metrics = DWRITE_TEXT_METRICS::default();
    // SAFETY: writes one struct owned by this stack frame.
    let result = unsafe { layout.GetMetrics(&mut metrics) };
    if let Err(error) = result {
        crate::logging::error(format!("text metrics: {error}"));
    }
    metrics
}

/// Uploads decoded BGRA pixels as a premultiplied bitmap at `dpi`.
pub(crate) fn upload_bitmap(
    context: &ID2D1DeviceContext,
    dpi: f32,
    image: &ImageData,
) -> Result<ID2D1Bitmap1, String> {
    let props = D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: pixel_format(D2D1_ALPHA_MODE_PREMULTIPLIED),
        dpiX: dpi,
        dpiY: dpi,
        bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
        colorContext: std::mem::ManuallyDrop::new(None),
    };
    // SAFETY: `image.bgra` is alive for the call; D2D copies the pixels before
    // returning, and the pitch matches the BGRA layout.
    unsafe {
        context.CreateBitmap(
            size_u(image.width, image.height),
            Some(image.bgra.as_ptr() as *const std::ffi::c_void),
            image.width * 4,
            &props,
        )
    }
    .map_err(|error| format!("CreateBitmap: {error}"))
}

/// Creates the shared DirectWrite factory.
pub(crate) fn create_dwrite() -> Result<IDWriteFactory, String> {
    // SAFETY: DWrite is a free-threaded factory and the out-pointer is valid.
    unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }
        .map_err(|error| format!("DWriteCreateFactory: {error}"))
}

/// Binds `swap_chain` to `visual` and commits the composition device.
///
/// The caller owns the three objects; this only connects them, once, after the
/// caller has built its own D2D resources.
pub(crate) fn attach_content(
    swap_chain: &IDXGISwapChain1,
    visual: &IDCompositionVisual,
    dcomp: &IDCompositionDevice,
) -> Result<(), String> {
    let content: windows::core::IUnknown = swap_chain
        .cast()
        .map_err(|error| format!("swap chain -> IUnknown: {error}"))?;
    // SAFETY: the visual and the composition device are alive and owned by the
    // caller, which keeps them for as long as the swap chain is presented.
    unsafe {
        visual
            .SetContent(&content)
            .map_err(|error| format!("SetContent: {error}"))?;
        dcomp.Commit().map_err(|error| format!("Commit: {error}"))?;
    }
    Ok(())
}

/// The bitmap properties a swap-chain back buffer is created with.
pub(crate) fn swap_chain_props(dpi: f32) -> D2D1_BITMAP_PROPERTIES1 {
    D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: pixel_format(D2D1_ALPHA_MODE_PREMULTIPLIED),
        dpiX: dpi,
        dpiY: dpi,
        bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
        colorContext: std::mem::ManuallyDrop::new(None),
    }
}

/// The bitmap properties an opaque offscreen target (the editor's card, the
/// note's paper) is created with.
pub(crate) fn bitmap_target_props(dpi: f32) -> D2D1_BITMAP_PROPERTIES1 {
    D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: pixel_format(D2D1_ALPHA_MODE_IGNORE),
        dpiX: dpi,
        dpiY: dpi,
        bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET,
        colorContext: std::mem::ManuallyDrop::new(None),
    }
}

/// Releases the D2D targets, resizes the swap chain's buffers and pushes the
/// new DPI down.
///
/// Extra targets the caller owns (the editor card, the note paper, cached
/// scenery) must be dropped before this is called, and the caller's own
/// rebake hooks run after it returns.
pub(crate) fn resize_swap_chain(
    swap_chain: &IDXGISwapChain1,
    context: &ID2D1DeviceContext,
    targets: &mut [Option<ID2D1Bitmap1>; 2],
    props: &mut D2D1_BITMAP_PROPERTIES1,
    width_px: u32,
    height_px: u32,
    dpi: f32,
) -> Result<(), String> {
    // SAFETY: the target is released before its buffers are resized, and the
    // caller keeps the context, the swap chain and `targets` alive.
    unsafe {
        context.SetTarget(None::<&ID2D1Image>);
    }
    *targets = [None, None];
    // SAFETY: every D2D target was released above.
    unsafe {
        swap_chain.ResizeBuffers(
            2,
            width_px,
            height_px,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            DXGI_SWAP_CHAIN_FLAG(0),
        )
    }
    .map_err(|error| format!("ResizeBuffers: {error}"))?;
    if props.dpiX != dpi || props.dpiY != dpi {
        props.dpiX = dpi;
        props.dpiY = dpi;
        // SAFETY: the context is the caller's and no frame is open.
        unsafe {
            context.SetDpi(dpi, dpi);
        }
    }
    Ok(())
}

/// Binds the next back buffer to `context`.
pub(crate) fn bind_back_buffer(
    swap_chain: &IDXGISwapChain1,
    swap_chain3: &IDXGISwapChain3,
    context: &ID2D1DeviceContext,
    targets: &mut [Option<ID2D1Bitmap1>; 2],
    props: &D2D1_BITMAP_PROPERTIES1,
    dpi: f32,
) -> Result<(), String> {
    // SAFETY: the swap chain is the caller's; the index comes from it.
    let index = unsafe { swap_chain3.GetCurrentBackBufferIndex() } as usize;
    let Some(slot) = targets.get_mut(index) else {
        return Err(format!("back buffer index {index}"));
    };
    if slot.is_none() {
        // SAFETY: the swap chain owns the buffer and the surface is alive for
        // the duration of `CreateBitmapFromDxgiSurface`.
        let surface: IDXGISurface = unsafe { swap_chain.GetBuffer(index as u32) }
            .map_err(|error| format!("GetBuffer: {error}"))?;
        let bitmap = unsafe { context.CreateBitmapFromDxgiSurface(&surface, Some(props)) }
            .map_err(|error| format!("CreateBitmapFromDxgiSurface: {error}"))?;
        *slot = Some(bitmap);
    }
    let bitmap = slot
        .as_ref()
        .ok_or_else(|| "swap-chain target missing".to_string())?;
    // SAFETY: the bitmap is a target for this context and the caller keeps
    // `targets` alive for the whole frame.
    unsafe {
        context.SetTarget(bitmap);
        context.SetDpi(dpi, dpi);
        context.SetTransform(&Matrix3x2::identity());
    }
    Ok(())
}

/// Starts drawing on the bound target: optionally sets the text antialias mode,
/// calls `BeginDraw`, and clears to transparent black when `clear_alpha` is
/// set. The note editor paints its whole paper bitmap and does not clear.
pub(crate) fn begin_draw(
    context: &ID2D1DeviceContext,
    antialias: Option<D2D1_TEXT_ANTIALIAS_MODE>,
    clear_alpha: Option<f32>,
) {
    // SAFETY: the target was bound by `bind_back_buffer`; no frame is open.
    unsafe {
        if let Some(mode) = antialias {
            context.SetTextAntialiasMode(mode);
        }
        context.BeginDraw();
        if let Some(a) = clear_alpha {
            context.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a,
            }));
        }
    }
}

/// Presents `swap_chain`; `label` names the window in the failure log, so the
/// three presenters keep their own wording.
pub(crate) fn present(
    swap_chain: &IDXGISwapChain1,
    sync: bool,
    label: &str,
) -> crate::render::Present {
    // SAFETY: the swap chain is the caller's; `Present` is the only call that
    // may block, and the caller runs it on the render thread.
    let result = unsafe { swap_chain.Present(u32::from(sync), DXGI_PRESENT(0)) };
    if result.is_ok() {
        crate::render::Present::Ok
    } else if result == DXGI_ERROR_WAS_STILL_DRAWING {
        crate::render::Present::Retry
    } else if result == DXGI_ERROR_DEVICE_REMOVED || result == DXGI_ERROR_DEVICE_RESET {
        crate::render::Present::DeviceLost
    } else {
        crate::logging::error(format!("{label} failed: 0x{:08X}", result.0));
        crate::render::Present::DeviceLost
    }
}
/// Ends the frame opened by [`begin_draw`] or [`begin_card`].
pub(crate) fn end_draw(context: &ID2D1DeviceContext, label: &str) -> Result<(), String> {
    // SAFETY: the caller paired this with `BeginDraw` on the same context.
    unsafe { context.EndDraw(None, None) }.map_err(|error| format!("{label}: {error}"))
}
