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
use windows::Win32::Graphics::Direct2D::{
    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_DEVICE_CONTEXT_OPTIONS_NONE,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_TEXT_ANTIALIAS_MODE, D2D1CreateFactory, ID2D1Device,
    ID2D1DeviceContext, ID2D1Factory1,
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
    DWRITE_FACTORY_TYPE_SHARED, DWriteCreateFactory, IDWriteFactory,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
    DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIAdapter, IDXGIDevice, IDXGIFactory2, IDXGIOutput,
    IDXGISwapChain1, IDXGISwapChain3,
};
use windows::core::Interface;

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

/// Creates the shared DirectWrite factory.
pub(crate) fn create_dwrite() -> Result<IDWriteFactory, String> {
    // SAFETY: DWrite is a free-threaded factory and the out-pointer is valid.
    unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }
        .map_err(|error| format!("DWriteCreateFactory: {error}"))
}
