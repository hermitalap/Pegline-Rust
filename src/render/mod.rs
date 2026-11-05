//! The GPU half of a window: D3D11 device, DirectComposition visual, a
//! flip-model composition swap chain, Direct2D content and DirectWrite text.
//!
//! One `Renderer` belongs to exactly one render thread and one HWND. The UI
//! thread only creates the window; everything here is created on the render
//! thread, which M0 proved is allowed (ARCHITECTURE.md §12.6).
//!
//! Coordinates handed to the draw methods are DIP; the swap chain is device
//! pixels and the device context is told the monitor DPI, so Direct2D does the
//! DIP-to-pixel mapping on the GPU.
//!
//! SAFETY: every `unsafe` block below calls user32/d3d11/dxgi/d2d1/dwrite with
//! COM interfaces that this struct owns. Interfaces come from the `windows`
//! crate and are released by `Drop`; nothing is released by hand.

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::ManuallyDrop;

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_FIGURE_BEGIN_FILLED, D2D1_GRADIENT_STOP,
    D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_NONE,
    D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1, D2D1_BUFFER_PRECISION_8BPC_UNORM,
    D2D1_COLOR_INTERPOLATION_MODE_PREMULTIPLIED, D2D1_COLOR_SPACE_SRGB,
    D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_DRAW_TEXT_OPTIONS, D2D1_ELLIPSE, D2D1_EXTEND_MODE_CLAMP,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_INTERPOLATION_MODE_LINEAR, D2D1_LAYER_OPTIONS1_NONE,
    D2D1_LAYER_PARAMETERS1, D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES, D2D1_ROUNDED_RECT,
    D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, D2D1CreateFactory, ID2D1Bitmap1, ID2D1Device,
    ID2D1DeviceContext, ID2D1Factory1, ID2D1Geometry, ID2D1Layer, ID2D1LinearGradientBrush,
    ID2D1RoundedRectangleGeometry, ID2D1SolidColorBrush,
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
    DWRITE_FONT_WEIGHT, DWRITE_FONT_WEIGHT_MEDIUM, DWRITE_TEXT_METRICS, DWriteCreateFactory,
    IDWriteFactory, IDWriteTextFormat, IDWriteTextLayout,
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
use windows::core::{Interface, PCWSTR, w};
use windows_numerics::{Matrix3x2, Vector2};

use crate::font;
use crate::i18n;
use crate::types::{ImageData, RectF};

/// How a `Present` went.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Present {
    /// Presented; the next frame may be drawn.
    Ok,
    /// `DXGI_ERROR_WAS_STILL_DRAWING`; nothing was shown.
    Retry,
    /// The device or swap chain is gone and must be rebuilt.
    DeviceLost,
}

/// The unblurred card area baked into the card shadow, DIP.
const SHADOW_CARD_DIP: f32 = 112.0;
/// Card corner radius inside that area, DIP.
const SHADOW_RADIUS_DIP: f32 = 16.0;
/// WPF `DropShadowEffect.BlurRadius`: 20 idle, 28 hovered. The Gaussian sigma
/// used for the bake is 0.4 × the radius, which matches the WPF falloff.
const SHADOW_BLUR_DIP: f32 = 20.0;
const SHADOW_HOVER_BLUR_DIP: f32 = 28.0;
/// Downward offset of the card shadow, DIP (WPF `ShadowDepth`).
const SHADOW_OFFSET_DIP: f32 = 5.0;
const SHADOW_HOVER_OFFSET_DIP: f32 = 8.0;
/// Number of Direct2D layers kept for nested opacity groups.
const LAYER_POOL: usize = 6;

/// The solid brushes used by the scene.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Brush {
    /// Body text: #1E1E1E.
    Ink,
    /// The empty-line hint: #464646.
    HintInk,
    /// Glass card background: #B8F7F7F7.
    Glass,
    /// Hovered button background: #F2FFFFFF.
    GlassLit,
    /// Solid white, used for highlights.
    White,
    /// Opaque paper: the hover preview's frame, #F8F8F8.
    Paper,
    /// The cross glyph.
    CrossInk,
    /// The blank entry note's label: a grey that reads as a hint on the note
    /// paper, #606060.
    Placeholder,
    /// The default note paper, used by the entry's peeking corner: #FFF4B8.
    NotePaper,
}

/// Cached text layouts.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TextId {
    Hint,
    Copy,
    Open,
    Copied,
    /// The blank entry note's centred label.
    NewNote,
}

/// Where a card's button row goes, in DIP, derived from the text metrics.
#[derive(Clone, Copy, Debug, Default)]
pub struct ActionsLayout {
    pub copy_width: f32,
    pub open_width: f32,
    pub gap: f32,
    pub total_width: f32,
    pub height: f32,
}

struct TextItem {
    layout: IDWriteTextLayout,
    width: f32,
    height: f32,
}

struct Texts {
    items: HashMap<TextId, TextItem>,
    actions: ActionsLayout,
    copied_height: f32,
}

impl Texts {
    fn get(&self, id: TextId) -> Option<&TextItem> {
        self.items.get(&id)
    }

    fn build(dwrite: &IDWriteFactory) -> Result<Self, String> {
        // One UI family for every language. The overlay is a premultiplied
        // surface, so Direct2D must use grayscale antialiasing here; ClearType
        // is reserved for the opaque editor and shortcut-dialog surfaces.
        let ui = create_format(dwrite, font::UI_FAMILY, 13.0, DWRITE_FONT_WEIGHT_MEDIUM)?;
        let hint = create_format(dwrite, font::UI_FAMILY, 12.0, DWRITE_FONT_WEIGHT_MEDIUM)?;
        let strings = i18n::s();

        let mut items: HashMap<TextId, TextItem> = HashMap::new();
        {
            let mut add =
                |id: TextId, text: &str, format: &IDWriteTextFormat| -> Result<(), String> {
                    let layout = create_layout(dwrite, format, text)?;
                    let metrics = text_metrics(&layout);
                    items.insert(
                        id,
                        TextItem {
                            layout,
                            width: metrics.width,
                            height: metrics.height,
                        },
                    );
                    Ok(())
                };
            add(TextId::Hint, strings.hint, &hint)?;
            add(TextId::Copy, strings.copy, &ui)?;
            add(TextId::Open, strings.open, &ui)?;
            add(TextId::NewNote, strings.new_note, &ui)?;
            let copied = format!("\u{2713} {}", strings.copied);
            add(TextId::Copied, &copied, &ui)?;
        }

        let copy_width = items[&TextId::Copy].width + 24.0;
        let open_width = items[&TextId::Open].width + 24.0;
        let gap = 6.0;
        let copied_height = items[&TextId::Copied].height + 9.0;
        Ok(Self {
            items,
            actions: ActionsLayout {
                copy_width,
                open_width,
                gap,
                total_width: copy_width + gap + open_width,
                height: 26.0,
            },
            copied_height,
        })
    }
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
    .map_err(|e| format!("CreateTextFormat: {e}"))
}

fn create_layout(
    dwrite: &IDWriteFactory,
    format: &IDWriteTextFormat,
    text: &str,
) -> Result<IDWriteTextLayout, String> {
    let wide: Vec<u16> = text.encode_utf16().collect();
    // A very wide layout box: the metrics report the natural text width, which
    // is what the pills are sized from. No wrapping is wanted here.
    // SAFETY: `wide` is alive for the call and the factory/format are borrowed.
    unsafe { dwrite.CreateTextLayout(&wide, format, 4096.0, 4096.0) }
        .map_err(|e| format!("CreateTextLayout: {e}"))
}

fn text_metrics(layout: &IDWriteTextLayout) -> DWRITE_TEXT_METRICS {
    let mut metrics = DWRITE_TEXT_METRICS::default();
    // SAFETY: writes one struct owned by this stack frame.
    unsafe {
        let _ = layout.GetMetrics(&mut metrics);
    }
    metrics
}

/// One baked nine-patch shadow.
struct ShadowBake {
    bitmap: ID2D1Bitmap1,
    total_dip: f32,
    pad_dip: f32,
    corner_dip: f32,
    offset_dip: f32,
}

/// Idle and hovered card shadows. They are baked once at startup; changing the
/// hover state only selects which texture is blitted (BEHAVIOR §3.3).
struct Shadow {
    idle: ShadowBake,
    hover: ShadowBake,
}

struct Rope {
    width_dip: f32,
    /// Filled ribbons: shadow, core, highlight. Fills use one pipeline, while
    /// three long stroked curves cost another ~18 MB on the Intel driver.
    shadow_geometry: ID2D1Geometry,
    core_geometry: ID2D1Geometry,
    highlight_geometry: ID2D1Geometry,
    shadow: ID2D1LinearGradientBrush,
    core: ID2D1LinearGradientBrush,
    highlight: ID2D1LinearGradientBrush,
}

/// The brush palette. Created once per renderer; all brushes are immutable
/// except the border gradient, whose transform follows the card height.
struct Palette {
    ink: ID2D1SolidColorBrush,
    hint_ink: ID2D1SolidColorBrush,
    glass: ID2D1SolidColorBrush,
    glass_lit: ID2D1SolidColorBrush,
    white: ID2D1SolidColorBrush,
    paper: ID2D1SolidColorBrush,
    cross_ink: ID2D1SolidColorBrush,
    placeholder: ID2D1SolidColorBrush,
    note_paper: ID2D1SolidColorBrush,
    pin_slot: ID2D1SolidColorBrush,
    metal: ID2D1LinearGradientBrush,
    metal_edge: ID2D1LinearGradientBrush,
    /// Vertical gradient from (0,0) to (0,1); scaled per card.
    border: ID2D1LinearGradientBrush,
    /// The four note colours, indexed like `notes::PALETTE`.
    swatches: [ID2D1SolidColorBrush; 4],
    /// Hairline around a colour dot: black at 31% opacity.
    swatch_edge: ID2D1SolidColorBrush,
}

impl Palette {
    fn new(context: &ID2D1DeviceContext) -> Result<Self, String> {
        let metal_stops = [
            stop(0.0, [179, 179, 179], 1.0),
            stop(0.35, [237, 237, 237], 1.0),
            stop(0.65, [209, 209, 209], 1.0),
            stop(1.0, [158, 158, 158], 1.0),
        ];
        let metal = gradient(context, &metal_stops, (0.0, 0.0), (9.0, 0.0))?;
        let edge_stops = [
            stop(0.0, [255, 255, 255], 0xE6 as f32 / 255.0),
            stop(1.0, [0, 0, 0], 0x2E as f32 / 255.0),
        ];
        let metal_edge = gradient(context, &edge_stops, (0.0, 0.0), (0.0, 26.0))?;
        let border_stops = [
            stop(0.0, [255, 255, 255], 0xE0 as f32 / 255.0),
            stop(1.0, [255, 255, 255], 0x40 as f32 / 255.0),
        ];
        let border = gradient(context, &border_stops, (0.0, 0.0), (0.0, 1.0))?;

        Ok(Self {
            ink: solid(context, [0x1E, 0x1E, 0x1E], 1.0)?,
            hint_ink: solid(context, [0x46, 0x46, 0x46], 1.0)?,
            glass: solid(context, [0xF7, 0xF7, 0xF7], 0xB8 as f32 / 255.0)?,
            glass_lit: solid(context, [0xFF, 0xFF, 0xFF], 0xF2 as f32 / 255.0)?,
            white: solid(context, [0xFF, 0xFF, 0xFF], 1.0)?,
            paper: solid(context, [0xF8, 0xF8, 0xF8], 1.0)?,
            cross_ink: solid(context, [0x28, 0x28, 0x28], 1.0)?,
            placeholder: solid(context, [0x60, 0x60, 0x60], 1.0)?,
            note_paper: solid(context, split(crate::notes::PALETTE[0].paper), 1.0)?,
            pin_slot: solid(context, [0, 0, 0], 0x52 as f32 / 255.0)?,
            metal,
            metal_edge,
            border,
            swatches: [
                solid(context, split(crate::notes::PALETTE[0].edge), 1.0)?,
                solid(context, split(crate::notes::PALETTE[1].edge), 1.0)?,
                solid(context, split(crate::notes::PALETTE[2].edge), 1.0)?,
                solid(context, split(crate::notes::PALETTE[3].edge), 1.0)?,
            ],
            swatch_edge: solid(context, [0, 0, 0], 0x50 as f32 / 255.0)?,
        })
    }

    fn solid(&self, brush: Brush) -> &ID2D1SolidColorBrush {
        match brush {
            Brush::Ink => &self.ink,
            Brush::HintInk => &self.hint_ink,
            Brush::Glass => &self.glass,
            Brush::GlassLit => &self.glass_lit,
            Brush::White => &self.white,
            Brush::Paper => &self.paper,
            Brush::CrossInk => &self.cross_ink,
            Brush::Placeholder => &self.placeholder,
            Brush::NotePaper => &self.note_paper,
        }
    }
}

/// Everything one window needs to render itself.
pub struct Renderer {
    swap_chain: IDXGISwapChain1,
    swap_chain3: IDXGISwapChain3,
    context: ID2D1DeviceContext,
    targets: [Option<ID2D1Bitmap1>; 2],
    target_props: D2D1_BITMAP_PROPERTIES1,
    d2d_factory: ID2D1Factory1,
    dwrite: IDWriteFactory,
    palette: Palette,
    border_scale: f32,
    shadow: Shadow,
    /// The clothespin's soft shadow, baked at the same DPI.
    pin_shadow: ID2D1Bitmap1,
    rope: Option<Rope>,
    texts: Texts,
    masks: HashMap<(u32, u32, u32), ID2D1Geometry>,
    layers: Vec<ID2D1Layer>,
    layer_depth: usize,
    current: Matrix3x2,
    /// Kept alive for as long as the swap chain is attached to the visual.
    _dcomp: IDCompositionDevice,
    _visual: IDCompositionVisual,
    _target: IDCompositionTarget,
    _d3d_context: ID3D11DeviceContext,
    _d2d_device: ID2D1Device,
    _d3d_device: ID3D11Device,
    pub width_px: u32,
    pub height_px: u32,
    pub dpi: f32,
}

impl Renderer {
    /// Builds the whole stack for `hwnd`.
    ///
    /// `width_px`/`height_px` are the window's client size in physical pixels;
    /// `dpi` is the monitor DPI (96 = 100%).
    pub fn new(hwnd: HWND, width_px: u32, height_px: u32, dpi: u32) -> Result<Self, String> {
        let width_px = width_px.max(1);
        let height_px = height_px.max(1);
        let dpi = dpi.max(96);

        let (device, immediate) = create_d3d_device()?;
        let dxgi_device: IDXGIDevice = device
            .cast()
            .map_err(|e| format!("ID3D11Device -> IDXGIDevice: {e}"))?;
        let (factory, dcomp, target, visual) = create_composition(&dxgi_device, hwnd)?;
        let (swap_chain, swap_chain3) = create_swap_chain(&factory, &device, width_px, height_px)?;
        let dpi_f = dpi as f32;
        let (d2d_factory, d2d_device, context) = create_d2d(&dxgi_device, dpi_f)?;

        let target_props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: dpi_f,
            dpiY: dpi_f,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
            colorContext: ManuallyDrop::new(None),
        };

        let palette = Palette::new(&context)?;
        let dwrite = create_dwrite()?;
        let texts = Texts::build(&dwrite)?;

        // SAFETY: layers belong to this render thread's context.
        let mut layers = Vec::with_capacity(LAYER_POOL);
        for _ in 0..LAYER_POOL {
            layers.push(
                unsafe { context.CreateLayer(None) }.map_err(|e| format!("CreateLayer: {e}"))?,
            );
        }
        let shadow = Shadow {
            idle: bake_card_shadow(&context, dpi_f, SHADOW_BLUR_DIP, SHADOW_OFFSET_DIP)?,
            hover: bake_card_shadow(
                &context,
                dpi_f,
                SHADOW_HOVER_BLUR_DIP,
                SHADOW_HOVER_OFFSET_DIP,
            )?,
        };
        let pin_shadow = create_blurred_rect(
            &context,
            dpi_f,
            dpi_f,
            crate::layout::PIN_WIDTH,
            crate::layout::PIN_HEIGHT,
            3.5,
            1.6,
            8.0,
            0.3,
        )?;

        let mut renderer = Self {
            swap_chain,
            swap_chain3,
            context,
            targets: [None, None],
            target_props,
            d2d_factory,
            dwrite,
            palette,
            border_scale: 1.0,
            shadow,
            pin_shadow,
            rope: None,
            texts,
            masks: HashMap::new(),
            layers,
            layer_depth: 0,
            current: Matrix3x2::identity(),
            _dcomp: dcomp,
            _visual: visual,
            _target: target,
            _d3d_context: immediate,
            _d2d_device: d2d_device,
            _d3d_device: device,
            width_px,
            height_px,
            dpi: dpi_f,
        };
        renderer.attach_content()?;
        Ok(renderer)
    }

    /// Sets the swap chain as the visual's content; called once after creation.
    fn attach_content(&mut self) -> Result<(), String> {
        let content: windows::core::IUnknown = self
            .swap_chain
            .cast()
            .map_err(|e| format!("swap chain -> IUnknown: {e}"))?;
        // SAFETY: the visual and DComp device are alive and owned by self.
        unsafe {
            self._visual
                .SetContent(&content)
                .map_err(|e| format!("SetContent: {e}"))?;
            self._dcomp.Commit().map_err(|e| format!("Commit: {e}"))?;
        }
        Ok(())
    }

    pub fn scale(&self) -> f32 {
        self.dpi / 96.0
    }

    pub fn actions_layout(&self) -> ActionsLayout {
        self.texts.actions
    }

    pub fn copied_height(&self) -> f32 {
        self.texts.copied_height
    }

    pub fn text_size(&self, id: TextId) -> (f32, f32) {
        self.texts
            .get(id)
            .map(|item| (item.width, item.height))
            .unwrap_or((0.0, 0.0))
    }

    /// Resizes the swap chain and updates the DPI. The window itself is resized
    /// by the UI thread first.
    pub fn resize(&mut self, width_px: u32, height_px: u32, dpi: u32) -> Result<(), String> {
        let width_px = width_px.max(1);
        let height_px = height_px.max(1);
        let dpi = dpi.max(96);
        if width_px == self.width_px && height_px == self.height_px && dpi as f32 == self.dpi {
            return Ok(());
        }
        // SAFETY: release the D2D targets before resizing their buffers.
        unsafe {
            self.context
                .SetTarget(None::<&windows::Win32::Graphics::Direct2D::ID2D1Image>);
        }
        self.targets = [None, None];
        // SAFETY: all D2D targets were released above.
        unsafe {
            self.swap_chain.ResizeBuffers(
                2,
                width_px,
                height_px,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                windows::Win32::Graphics::Dxgi::DXGI_SWAP_CHAIN_FLAG(0),
            )
        }
        .map_err(|e| format!("ResizeBuffers: {e}"))?;
        self.width_px = width_px;
        self.height_px = height_px;
        self.dpi = dpi as f32;
        // SAFETY: the context is ours; DPI changes must be pushed down.
        unsafe {
            self.context.SetDpi(self.dpi, self.dpi);
        }
        self.target_props.dpiX = self.dpi;
        self.target_props.dpiY = self.dpi;
        self.shadow.idle =
            bake_card_shadow(&self.context, self.dpi, SHADOW_BLUR_DIP, SHADOW_OFFSET_DIP)?;
        self.shadow.hover = bake_card_shadow(
            &self.context,
            self.dpi,
            SHADOW_HOVER_BLUR_DIP,
            SHADOW_HOVER_OFFSET_DIP,
        )?;
        self.pin_shadow = create_blurred_rect(
            &self.context,
            self.dpi,
            self.dpi,
            crate::layout::PIN_WIDTH,
            crate::layout::PIN_HEIGHT,
            3.5,
            1.6,
            8.0,
            0.3,
        )?;
        self.rope = None;
        Ok(())
    }

    /// Binds the next back buffer and starts a frame.
    pub fn begin(&mut self) -> Result<(), String> {
        // SAFETY: the swap chain is ours and the returned index is checked.
        let index = unsafe { self.swap_chain3.GetCurrentBackBufferIndex() } as usize;
        if index >= self.targets.len() {
            return Err(format!("back buffer index {index}"));
        }
        if self.targets[index].is_none() {
            // SAFETY: the swap chain owns the buffer and the surface is alive
            // for the duration of CreateBitmapFromDxgiSurface.
            let surface: IDXGISurface = unsafe { self.swap_chain.GetBuffer(index as u32) }
                .map_err(|e| format!("GetBuffer: {e}"))?;
            let bitmap = unsafe {
                self.context
                    .CreateBitmapFromDxgiSurface(&surface, Some(&self.target_props))
            }
            .map_err(|e| format!("CreateBitmapFromDxgiSurface: {e}"))?;
            self.targets[index] = Some(bitmap);
        }
        if let Some(bitmap) = self.targets[index].as_ref() {
            // SAFETY: the bitmap is a target for this context and stays alive.
            unsafe {
                self.context.SetTarget(bitmap);
                self.context.SetDpi(self.dpi, self.dpi);
                self.context.SetTransform(&Matrix3x2::identity());
                self.current = Matrix3x2::identity();
                self.context.BeginDraw();
                self.context.Clear(Some(&D2D1_COLOR_F {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 0.0,
                }));
            }
        }
        self.layer_depth = 0;
        Ok(())
    }

    pub fn end(&mut self) -> Result<(), String> {
        // SAFETY: BeginDraw was called by `begin` on this thread.
        unsafe { self.context.EndDraw(None, None) }.map_err(|e| format!("EndDraw: {e}"))
    }

    pub fn present(&self, sync: bool) -> Present {
        // SAFETY: the swap chain is ours; Present is the only call that may
        // block, and it is deliberately on the render thread.
        let result = unsafe { self.swap_chain.Present(u32::from(sync), DXGI_PRESENT(0)) };
        if result.is_ok() {
            Present::Ok
        } else if result == DXGI_ERROR_WAS_STILL_DRAWING {
            Present::Retry
        } else if result == DXGI_ERROR_DEVICE_REMOVED || result == DXGI_ERROR_DEVICE_RESET {
            Present::DeviceLost
        } else {
            crate::logging::error(format!("Present failed: 0x{:08X}", result.0));
            Present::DeviceLost
        }
    }

    pub fn set_transform(&mut self, transform: Matrix3x2) {
        self.current = transform;
        // SAFETY: the context is ours.
        unsafe { self.context.SetTransform(&transform) };
    }

    pub fn reset_transform(&mut self) {
        self.set_transform(Matrix3x2::identity());
    }

    /// Fills a rounded rectangle in DIP.
    pub fn fill_round(&self, rect: RectF, radius: f32, brush: Brush) {
        let rounded = rounded(rect, radius);
        let brush = self.palette.solid(brush);
        // SAFETY: the brush and context are alive; the rect is a stack value.
        unsafe { self.context.FillRoundedRectangle(&rounded, brush) };
    }

    /// Strokes a rounded rectangle with the glass border gradient.
    pub fn stroke_card(&mut self, rect: RectF, radius: f32, stroke: f32) {
        if (self.border_scale - rect.height()).abs() > 0.01 {
            self.border_scale = rect.height().max(1.0);
            // SAFETY: the brush is ours and single-threaded.
            unsafe {
                self.palette
                    .border
                    .SetTransform(&Matrix3x2::scale(1.0, self.border_scale));
            }
        }
        let rounded = rounded(rect, radius);
        let brush = &self.palette.border;
        // SAFETY: brush and context are alive.
        unsafe {
            self.context
                .DrawRoundedRectangle(&rounded, brush, stroke, None)
        };
    }

    /// Starts a group that fades as a whole (a card, its buttons, the hint).
    ///
    /// Returns true when a layer was pushed. **The caller draws its content
    /// either way** and pops only when this returned true: an opacity of 1 (or
    /// an exhausted layer pool) is the fast path, not a reason to skip drawing.
    pub fn push_opacity(&mut self, bounds: RectF, opacity: f32) -> bool {
        if self.layer_depth >= self.layers.len() || opacity >= 1.0 {
            return false;
        }
        let params = D2D1_LAYER_PARAMETERS1 {
            contentBounds: d2d_rect(bounds),
            geometricMask: ManuallyDrop::new(None),
            maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
            maskTransform: Matrix3x2::identity(),
            opacity,
            opacityBrush: ManuallyDrop::new(None),
            layerOptions: D2D1_LAYER_OPTIONS1_NONE,
        };
        // SAFETY: the layer belongs to this context and the parameters are
        // valid for the duration of the call.
        unsafe {
            self.context
                .PushLayer(&params, &self.layers[self.layer_depth])
        };
        self.layer_depth += 1;
        true
    }

    pub fn pop_opacity(&mut self) {
        if self.layer_depth == 0 {
            return;
        }
        // SAFETY: balances the successful PushLayer above.
        unsafe { self.context.PopLayer() };
        self.layer_depth -= 1;
    }

    /// Draws a photo clipped to a rounded rectangle.
    pub fn draw_photo(&mut self, bitmap: &ID2D1Bitmap1, rect: RectF, radius: f32) {
        // Rounded corners via a masked layer. Measured: one layer per photo
        // costs about 1 MB and keeps the corners concentric with the frame.
        let Some(geometry) = self.mask_geometry(rect, radius) else {
            return;
        };
        if self.layer_depth >= self.layers.len() {
            return;
        }
        let params = D2D1_LAYER_PARAMETERS1 {
            contentBounds: d2d_rect(rect),
            geometricMask: ManuallyDrop::new(Some(geometry.clone())),
            maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
            maskTransform: Matrix3x2::translation(rect.left, rect.top),
            opacity: 1.0,
            opacityBrush: ManuallyDrop::new(None),
            layerOptions: D2D1_LAYER_OPTIONS1_NONE,
        };
        let dst = d2d_rect(rect);
        let src = D2D_RECT_F {
            left: 0.0,
            top: 0.0,
            right: bitmap_width(&self.context, bitmap) as f32,
            bottom: bitmap_height(&self.context, bitmap) as f32,
        };
        // SAFETY: the layer and geometry are alive; the bitmap is borrowed
        // and outlives the calls.
        unsafe {
            self.context
                .PushLayer(&params, &self.layers[self.layer_depth]);
            self.layer_depth += 1;
            self.context.DrawBitmap(
                bitmap,
                Some(&dst),
                1.0,
                D2D1_INTERPOLATION_MODE_LINEAR,
                Some(&src),
                None,
            );
            self.context.PopLayer();
            self.layer_depth -= 1;
        }
    }

    /// Probe helper: six sub-region blits from one atlas bitmap.
    pub fn draw_atlas_probe(&mut self, bitmap: &ID2D1Bitmap1, count: usize) {
        let cell_w = 160.0f32;
        let cell_h = 99.0f32;
        for index in 0..count {
            let x = 200.0 + index as f32 * 240.0;
            let dst = D2D_RECT_F {
                left: x,
                top: 60.0,
                right: x + cell_w,
                bottom: 60.0 + cell_h,
            };
            let column = (index % 2) as f32;
            let row = (index / 2) as f32;
            let src = D2D_RECT_F {
                left: column * 512.0,
                top: row * 256.0,
                right: column * 512.0 + 512.0,
                bottom: row * 256.0 + 256.0,
            };
            unsafe {
                self.context.DrawBitmap(
                    bitmap,
                    Some(&dst),
                    1.0,
                    D2D1_INTERPOLATION_MODE_LINEAR,
                    Some(&src),
                    None,
                );
            }
        }
    }
    /// Draws a card shadow around `rect`, in DIP.
    ///
    /// `opacity` is the shadow's alpha (0.18 idle, 0.26 hovered); the texture
    /// itself is baked once, so this is nine `DrawBitmap` calls and no blur.
    pub fn draw_shadow(&self, rect: RectF, opacity: f32, hovered: bool) {
        if opacity <= 0.0 {
            return;
        }
        let shadow = if hovered {
            &self.shadow.hover
        } else {
            &self.shadow.idle
        };
        let pad = shadow.pad_dip;
        let corner = shadow.corner_dip;
        let total = shadow.total_dip;
        let x0 = rect.left - pad;
        let y0 = rect.top - pad + shadow.offset_dip;
        let x1 = rect.right + pad;
        let y1 = rect.bottom + pad + shadow.offset_dip;
        let inner_w = (x1 - x0) - 2.0 * corner;
        let inner_h = (y1 - y0) - 2.0 * corner;
        if inner_w <= 0.0 || inner_h <= 0.0 || total - 2.0 * corner <= 0.0 {
            return;
        }
        let src_x = [0.0, corner, total - corner];
        let src_w = [corner, total - 2.0 * corner, corner];
        let dst_x = [x0, x0 + corner, x1 - corner];
        let dst_w = [corner, inner_w, corner];
        let src_y = [0.0, corner, total - corner];
        let src_h = [corner, total - 2.0 * corner, corner];
        let dst_y = [y0, y0 + corner, y1 - corner];
        let dst_h = [corner, inner_h, corner];
        // SAFETY: the baked bitmap is alive and owned by the renderer.
        unsafe {
            for row in 0..3 {
                for column in 0..3 {
                    let src = D2D_RECT_F {
                        left: src_x[column],
                        top: src_y[row],
                        right: src_x[column] + src_w[column],
                        bottom: src_y[row] + src_h[row],
                    };
                    let dst = D2D_RECT_F {
                        left: dst_x[column],
                        top: dst_y[row],
                        right: dst_x[column] + dst_w[column],
                        bottom: dst_y[row] + dst_h[row],
                    };
                    self.context.DrawBitmap(
                        &shadow.bitmap,
                        Some(&dst),
                        opacity,
                        D2D1_INTERPOLATION_MODE_LINEAR,
                        Some(&src),
                        None,
                    );
                }
            }
        }
    }

    /// Draws the clothespin at `top` in the current transform's coordinates.
    pub fn draw_pin(&mut self, left: f32, top: f32) {
        // The baked pin shadow: 8 DIP of blur pad, 1.5 DIP down, alpha 0.3.
        let pad = 8.0;
        let shadow_dst = RectF::from_xywh(
            left - pad,
            top - pad + 1.5,
            9.0 + pad * 2.0,
            26.0 + pad * 2.0,
        );
        let dst = d2d_rect(shadow_dst);
        // SAFETY: the baked bitmap is alive and owned by this renderer.
        unsafe {
            self.context.DrawBitmap(
                &self.pin_shadow,
                Some(&dst),
                1.0,
                D2D1_INTERPOLATION_MODE_LINEAR,
                None,
                None,
            );
        }
        let body = RectF::from_xywh(left, top, 9.0, 26.0);
        let rounded = rounded(body, 3.5);
        // SAFETY: brushes and context are alive.
        unsafe {
            self.context
                .FillRoundedRectangle(&rounded, &self.palette.metal);
            self.context
                .DrawRoundedRectangle(&rounded, &self.palette.metal_edge, 0.6, None);
        }
        let slot = RectF::from_xywh(left + 2.0, top + 8.5, 5.0, 1.4);
        self.fill_round(slot, 0.7, Brush::CrossInk);
    }

    /// Draws the discard cross in the current transform's coordinates.
    pub fn draw_cross(&mut self, left: f32, top: f32, alpha: f32) {
        if alpha <= 0.0 {
            return;
        }
        let size = 20.0;
        let center = (left + size * 0.5, top + size * 0.5);
        let scale = 0.6 + 0.4 * alpha;
        let radius = size * 0.5 * scale;
        let circle = RectF::new(
            center.0 - radius,
            center.1 - radius,
            center.0 + radius,
            center.1 + radius,
        );
        self.fill_round(circle, radius, Brush::Glass);
        self.stroke_card(circle, radius, 0.75);
        let glyph = 6.0 * scale * 0.5;
        let ink = self.palette.solid(Brush::CrossInk);
        // SAFETY: the brush and context are alive; points are by value.
        unsafe {
            self.context.DrawLine(
                Vector2 {
                    X: center.0 - glyph,
                    Y: center.1 - glyph,
                },
                Vector2 {
                    X: center.0 + glyph,
                    Y: center.1 + glyph,
                },
                ink,
                1.6,
                None,
            );
            self.context.DrawLine(
                Vector2 {
                    X: center.0 + glyph,
                    Y: center.1 - glyph,
                },
                Vector2 {
                    X: center.0 - glyph,
                    Y: center.1 + glyph,
                },
                ink,
                1.6,
                None,
            );
        }
    }

    /// Draws one note colour dot, with a check mark when it is the active one.
    pub fn draw_swatch(&self, rect: RectF, color: usize, selected: bool) {
        let (cx, cy) = rect.center();
        let radius = rect.width().min(rect.height()) * 0.5;
        let ellipse = D2D1_ELLIPSE {
            point: Vector2 { X: cx, Y: cy },
            radiusX: radius,
            radiusY: radius,
        };
        let brush = &self.palette.swatches[color.min(3)];
        // SAFETY: the ellipse and both brushes are live values owned by this
        // renderer; the context is drawing.
        unsafe {
            self.context.FillEllipse(&ellipse, brush);
            self.context
                .DrawEllipse(&ellipse, &self.palette.swatch_edge, 0.75, None);
        }
        if selected {
            let ink = self.palette.solid(Brush::CrossInk);
            // A tick, not a cross: down-right then up-right, scaled to the dot.
            let unit = radius * 0.55;
            // SAFETY: as above; the brush stays alive for the call.
            unsafe {
                self.context.DrawLine(
                    Vector2 {
                        X: cx - unit * 0.8,
                        Y: cy,
                    },
                    Vector2 {
                        X: cx - unit * 0.15,
                        Y: cy + unit * 0.6,
                    },
                    ink,
                    1.4,
                    None,
                );
                self.context.DrawLine(
                    Vector2 {
                        X: cx - unit * 0.15,
                        Y: cy + unit * 0.6,
                    },
                    Vector2 {
                        X: cx + unit * 0.9,
                        Y: cy - unit * 0.55,
                    },
                    ink,
                    1.4,
                    None,
                );
            }
        }
    }

    /// Draws a glass pill with its label centred, in the current transform.
    /// `lit` is the hover state: the original paints the pill nearly white.
    pub fn draw_pill(&mut self, rect: RectF, label: TextId, lit: bool) {
        let radius = rect.height() * 0.5;
        self.fill_round(
            rect,
            radius,
            if lit { Brush::GlassLit } else { Brush::Glass },
        );
        self.stroke_card(rect, radius, 0.75);
        let (width, height) = self.text_size(label);
        self.draw_text(
            label,
            (
                rect.left + (rect.width() - width) * 0.5,
                rect.top + (rect.height() - height) * 0.5 - 1.0,
            ),
            Brush::Ink,
        );
    }

    /// Draws the hint pill, centred at `(center_x, center_y)`.
    pub fn draw_hint(&mut self, center_x: f32, center_y: f32) {
        let (width, height) = self.text_size(TextId::Hint);
        let pill = RectF::from_xywh(
            center_x - (width + 24.0) * 0.5,
            center_y - (height + 11.0) * 0.5,
            width + 24.0,
            height + 11.0,
        );
        let radius = 14.0;
        self.fill_round(pill, radius, Brush::Glass);
        self.stroke_card(pill, radius, 0.75);
        self.draw_text(
            TextId::Hint,
            (
                pill.left + 12.0,
                pill.top + (pill.height() - height) * 0.5 - 1.0,
            ),
            Brush::HintInk,
        );
    }

    /// Draws the rope across the window width, in the current transform.
    pub fn draw_rope(&mut self, width_dip: f32) {
        let rebuild = self
            .rope
            .as_ref()
            .map(|rope| (rope.width_dip - width_dip).abs() >= 0.5)
            .unwrap_or(true);
        if rebuild {
            if let Err(error) = self.rebuild_rope(width_dip) {
                crate::logging::error(error);
                return;
            }
        }
        let Some(rope) = self.rope.as_ref() else {
            return;
        };
        // SAFETY: geometries and brushes are alive for the duration of the
        // calls; the current transform places the whole line.
        unsafe {
            self.context
                .FillGeometry(&rope.shadow_geometry, &rope.shadow, None);
            self.context
                .FillGeometry(&rope.core_geometry, &rope.core, None);
            self.context
                .FillGeometry(&rope.highlight_geometry, &rope.highlight, None);
        }
    }

    fn rebuild_rope(&mut self, width_dip: f32) -> Result<(), String> {
        let width = width_dip.max(1.0);
        let sag = crate::layout::sag(width);
        let shadow_geometry = ribbon(&self.d2d_factory, width, sag, 1.4, 1.2)?;
        let core_geometry = ribbon(&self.d2d_factory, width, sag, 1.2, 0.0)?;
        let highlight_geometry = ribbon(&self.d2d_factory, width, sag, 0.4, -0.35)?;
        let stops = |color: [u8; 3], alpha: f32| {
            [
                stop(0.0, color, 0.0),
                stop(0.08, color, alpha),
                stop(0.92, color, alpha),
                stop(1.0, color, 0.0),
            ]
        };
        let span = width + 40.0;
        let shadow_stops = stops([0, 0, 0], 0x38 as f32 / 255.0);
        let core_stops = stops([140, 140, 140], 1.0);
        let highlight_stops = stops([255, 255, 255], 0x73 as f32 / 255.0);
        let shadow = gradient(
            &self.context,
            &shadow_stops,
            (-20.0, 0.0),
            (-20.0 + span, 0.0),
        )?;
        let core = gradient(
            &self.context,
            &core_stops,
            (-20.0, 0.0),
            (-20.0 + span, 0.0),
        )?;
        let highlight = gradient(
            &self.context,
            &highlight_stops,
            (-20.0, 0.0),
            (-20.0 + span, 0.0),
        )?;
        self.rope = Some(Rope {
            width_dip: width,
            shadow_geometry,
            core_geometry,
            highlight_geometry,
            shadow,
            core,
            highlight,
        });
        Ok(())
    }

    /// Draws a cached text layout.
    pub fn draw_text(&self, id: TextId, origin: (f32, f32), brush: Brush) {
        let Some(item) = self.texts.get(id) else {
            return;
        };
        let brush = self.palette.solid(brush);
        // Centred text lands on fractions of a device pixel; at button size
        // that is the difference between sharp strokes and a grey smear, so
        // the origin goes to the pixel grid before drawing.
        let scale = self.scale();
        let x = (origin.0 * scale).round() / scale;
        let y = (origin.1 * scale).round() / scale;
        // SAFETY: layout and brush are alive; the origin is by value.
        unsafe {
            self.context.DrawTextLayout(
                Vector2 { X: x, Y: y },
                &item.layout,
                brush,
                D2D1_DRAW_TEXT_OPTIONS(0),
            );
        }
    }

    /// Uploads a decoded image as a D2D bitmap.
    pub fn upload(&self, image: &ImageData) -> Result<ID2D1Bitmap1, String> {
        if !image.is_valid() {
            return Err("upload: invalid image buffer".to_string());
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
        // SAFETY: `bgra` is alive for the call; D2D copies the pixels before
        // returning.
        unsafe {
            self.context.CreateBitmap(
                windows::Win32::Graphics::Direct2D::Common::D2D_SIZE_U {
                    width: image.width,
                    height: image.height,
                },
                Some(image.bgra.as_ptr() as *const c_void),
                image.width * 4,
                &props,
            )
        }
        .map_err(|e| format!("CreateBitmap: {e}"))
    }

    fn mask_geometry(&mut self, rect: RectF, radius: f32) -> Option<ID2D1Geometry> {
        let key = (
            (rect.width() * 8.0).round().max(1.0) as u32,
            (rect.height() * 8.0).round().max(1.0) as u32,
            (radius * 8.0).round().max(1.0) as u32,
        );
        if let Some(geometry) = self.masks.get(&key) {
            return Some(geometry.clone());
        }
        let rounded = D2D1_ROUNDED_RECT {
            rect: D2D_RECT_F {
                left: 0.0,
                top: 0.0,
                right: rect.width(),
                bottom: rect.height(),
            },
            radiusX: radius,
            radiusY: radius,
        };
        // SAFETY: the factory is ours; the geometry is created once per size.
        let geometry: ID2D1RoundedRectangleGeometry =
            unsafe { self.d2d_factory.CreateRoundedRectangleGeometry(&rounded) }.ok()?;
        let geometry: ID2D1Geometry = geometry.cast().ok()?;
        self.masks.insert(key, geometry.clone());
        Some(geometry)
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
    // SAFETY: `dxgi_device` is a live DXGI device owned by this thread, and the
    // DComp objects are created from it and kept alive together.
    unsafe {
        let adapter = dxgi_device
            .GetAdapter()
            .map_err(|e| format!("GetAdapter: {e}"))?;
        let factory: IDXGIFactory2 = adapter.GetParent().map_err(|e| format!("GetParent: {e}"))?;
        let dcomp: IDCompositionDevice = DCompositionCreateDevice(dxgi_device)
            .map_err(|e| format!("DCompositionCreateDevice: {e}"))?;
        let target = dcomp
            .CreateTargetForHwnd(hwnd, true)
            .map_err(|e| format!("CreateTargetForHwnd: {e}"))?;
        let visual = dcomp
            .CreateVisual()
            .map_err(|e| format!("CreateVisual: {e}"))?;
        target
            .SetRoot(&visual)
            .map_err(|e| format!("SetRoot: {e}"))?;
        dcomp.Commit().map_err(|e| format!("Commit: {e}"))?;
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
        AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
        Flags: 0,
    };
    // SAFETY: the factory and device are alive; a composition swap chain takes
    // no output adapter.
    unsafe {
        let swap_chain: IDXGISwapChain1 = factory
            .CreateSwapChainForComposition(device, &desc, None::<&IDXGIOutput>)
            .map_err(|e| format!("CreateSwapChainForComposition: {e}"))?;
        let swap_chain3: IDXGISwapChain3 = swap_chain
            .cast()
            .map_err(|e| format!("IDXGISwapChain3: {e}"))?;
        Ok((swap_chain, swap_chain3))
    }
}

fn create_d2d(
    dxgi_device: &IDXGIDevice,
    dpi: f32,
) -> Result<(ID2D1Factory1, ID2D1Device, ID2D1DeviceContext), String> {
    // SAFETY: a single-threaded factory used only on this thread; the D2D
    // device and context are handed to the renderer and dropped with it.
    unsafe {
        let factory: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)
            .map_err(|e| format!("D2D1CreateFactory: {e}"))?;
        let device = factory
            .CreateDevice(dxgi_device)
            .map_err(|e| format!("CreateDevice(d2d): {e}"))?;
        let context = device
            .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)
            .map_err(|e| format!("CreateDeviceContext: {e}"))?;
        context.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
        // The overlay swap chain carries premultiplied alpha, so ClearType is
        // not valid. Ask for grayscale explicitly instead of relying on
        // Direct2D's silent fallback; text origins are already snapped to the
        // device pixel grid in `draw_text`.
        context.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
        context.SetDpi(dpi, dpi);
        Ok((factory, device, context))
    }
}

fn create_d3d_device() -> Result<(ID3D11Device, ID3D11DeviceContext), String> {
    let mut device: Option<ID3D11Device> = None;
    let mut immediate: Option<ID3D11DeviceContext> = None;
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    let mut level = D3D_FEATURE_LEVEL_11_0;
    // SAFETY: out-params are owned by this function; the level array is live.
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
        // SAFETY: same as above; 11.0 is the documented floor.
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
        .map_err(|e| format!("D3D11CreateDevice(11.0): {e}"))?;
    }
    let device = device.ok_or_else(|| "D3D11 device missing".to_string())?;
    let immediate = immediate.ok_or_else(|| "D3D11 context missing".to_string())?;
    Ok((device, immediate))
}

fn create_dwrite() -> Result<IDWriteFactory, String> {
    // SAFETY: DWrite is a free-threaded factory; the out-pointer is valid.
    unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }
        .map_err(|e| format!("DWriteCreateFactory: {e}"))
}

/// Bakes the soft shadow of one rounded rectangle.
///
/// `width_dip`/`height_dip` describe the unblurred shape; `sigma_dip` is the
/// Gaussian sigma (0.4 × a WPF `BlurRadius`); `pad_dip` is the transparent
/// margin baked around it. `alpha` scales the shadow's opacity into the bitmap.
/// The result is a premultiplied black bitmap, drawn once per frame.
#[allow(clippy::too_many_arguments)]
fn create_blurred_rect(
    context: &ID2D1DeviceContext,
    dpi_x: f32,
    dpi_y: f32,
    width_dip: f32,
    height_dip: f32,
    radius_dip: f32,
    sigma_dip: f32,
    pad_dip: f32,
    alpha: f32,
) -> Result<ID2D1Bitmap1, String> {
    let scale = dpi_x / 96.0;
    let width_px = (width_dip * scale).round().max(2.0);
    let height_px = (height_dip * scale).round().max(2.0);
    let pad_px = (pad_dip * scale).round().max(2.0);
    let total_w = (width_px + pad_px * 2.0) as u32;
    let total_h = (height_px + pad_px * 2.0) as u32;
    let radius_px = (radius_dip * scale).round().max(1.0);
    let sigma_px = (sigma_dip * scale).max(0.75);
    let center_x = pad_px + width_px * 0.5;
    let center_y = pad_px + height_px * 0.5;
    let half_w = width_px * 0.5;
    let half_h = height_px * 0.5;
    let mut pixels = vec![0u8; (total_w * total_h * 4) as usize];
    for y in 0..total_h {
        for x in 0..total_w {
            let px = x as f32 + 0.5 - center_x;
            let py = y as f32 + 0.5 - center_y;
            // Signed distance to a rounded rectangle, the usual q/d form.
            let qx = px.abs() - (half_w - radius_px);
            let qy = py.abs() - (half_h - radius_px);
            let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
            let inside = qx.max(qy).min(0.0);
            let distance = outside + inside - radius_px;
            let coverage = if distance <= 0.0 {
                1.0
            } else {
                (-0.5 * (distance / sigma_px).powi(2)).exp()
            };
            let value = (coverage * alpha * 255.0).clamp(0.0, 255.0) as u8;
            let i = ((y * total_w + x) * 4) as usize;
            pixels[i] = 0;
            pixels[i + 1] = 0;
            pixels[i + 2] = 0;
            pixels[i + 3] = value;
        }
    }
    let props = D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: dpi_x,
        dpiY: dpi_y,
        bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
        colorContext: ManuallyDrop::new(None),
    };
    // SAFETY: `pixels` is alive for the call; D2D copies it.
    unsafe {
        context.CreateBitmap(
            windows::Win32::Graphics::Direct2D::Common::D2D_SIZE_U {
                width: total_w,
                height: total_h,
            },
            Some(pixels.as_ptr() as *const c_void),
            total_w * 4,
            &props,
        )
    }
    .map_err(|e| format!("CreateBitmap(shadow): {e}"))
}

/// Bakes one of the two card shadows (idle or hovered) with its nine-slice
/// margins. The texture is 112 DIP of card plus 3σ of blur on each side.
fn bake_card_shadow(
    context: &ID2D1DeviceContext,
    dpi: f32,
    blur_dip: f32,
    offset_dip: f32,
) -> Result<ShadowBake, String> {
    let sigma = blur_dip * 0.4;
    let pad = (sigma * 3.0).ceil().max(8.0);
    Ok(ShadowBake {
        bitmap: create_blurred_rect(
            context,
            dpi,
            dpi,
            SHADOW_CARD_DIP,
            SHADOW_CARD_DIP,
            SHADOW_RADIUS_DIP,
            sigma,
            pad,
            1.0,
        )?,
        total_dip: SHADOW_CARD_DIP + pad * 2.0,
        pad_dip: pad,
        corner_dip: pad + 32.0,
        offset_dip,
    })
}

fn bitmap_width(_context: &ID2D1DeviceContext, bitmap: &ID2D1Bitmap1) -> u32 {
    // SAFETY: the bitmap is alive; GetPixelSize writes one struct.
    unsafe { bitmap.GetPixelSize() }.width
}

fn bitmap_height(_context: &ID2D1DeviceContext, bitmap: &ID2D1Bitmap1) -> u32 {
    // SAFETY: the bitmap is alive; GetPixelSize writes one struct.
    unsafe { bitmap.GetPixelSize() }.height
}

/// Builds a closed ribbon polygon along the rope parabola.
///
/// Filling is used instead of stroking because three long stroked curves cost
/// an extra ~18 MB of driver pipeline memory on the Intel adapter, while fills
/// share the cheap path already used by the cards.
fn ribbon(
    factory: &ID2D1Factory1,
    width: f32,
    sag: f32,
    thickness: f32,
    y_offset: f32,
) -> Result<ID2D1Geometry, String> {
    const SEGMENTS: usize = 64;
    let (p0, p1, p2) = (
        (-20.0f32, 10.0f32),
        (width * 0.5, 10.0 + 2.0 * sag),
        (width + 20.0, 10.0f32),
    );
    let mut points = Vec::with_capacity((SEGMENTS + 1) * 2);
    let mut right: Vec<Vector2> = Vec::with_capacity(SEGMENTS + 1);
    for index in 0..=SEGMENTS {
        let t = index as f32 / SEGMENTS as f32;
        let mt = 1.0 - t;
        let x = mt * mt * p0.0 + 2.0 * mt * t * p1.0 + t * t * p2.0;
        let y = mt * mt * p0.1 + 2.0 * mt * t * p1.1 + t * t * p2.1 + y_offset;
        let dx = 2.0 * mt * (p1.0 - p0.0) + 2.0 * t * (p2.0 - p1.0);
        let dy = 2.0 * mt * (p1.1 - p0.1) + 2.0 * t * (p2.1 - p1.1);
        let length = (dx * dx + dy * dy).sqrt().max(0.0001);
        let nx = -dy / length * thickness * 0.5;
        let ny = dx / length * thickness * 0.5;
        points.push(Vector2 {
            X: x + nx,
            Y: y + ny,
        });
        right.push(Vector2 {
            X: x - nx,
            Y: y - ny,
        });
    }
    points.extend(right.iter().rev().copied());

    // SAFETY: the sink is opened from this geometry and closed before the
    // geometry is used.
    let geometry =
        unsafe { factory.CreatePathGeometry() }.map_err(|e| format!("CreatePathGeometry: {e}"))?;
    unsafe {
        let sink = geometry
            .Open()
            .map_err(|e| format!("PathGeometry::Open: {e}"))?;
        sink.BeginFigure(points[0], D2D1_FIGURE_BEGIN_FILLED);
        sink.AddLines(&points[1..]);
        sink.EndFigure(windows::Win32::Graphics::Direct2D::Common::D2D1_FIGURE_END_CLOSED);
        sink.Close()
            .map_err(|e| format!("PathGeometry::Close: {e}"))?;
    }
    geometry
        .cast()
        .map_err(|e| format!("PathGeometry -> ID2D1Geometry: {e}"))
}
fn rounded(rect: RectF, radius: f32) -> D2D1_ROUNDED_RECT {
    D2D1_ROUNDED_RECT {
        rect: d2d_rect(rect),
        radiusX: radius,
        radiusY: radius,
    }
}

fn d2d_rect(rect: RectF) -> D2D_RECT_F {
    D2D_RECT_F {
        left: rect.left,
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
    }
}

/// `0xRRGGBB` to the `[r, g, b]` triples the brush helpers take.
fn split(value: u32) -> [u8; 3] {
    [
        ((value >> 16) & 0xFF) as u8,
        ((value >> 8) & 0xFF) as u8,
        (value & 0xFF) as u8,
    ]
}

fn stop(position: f32, rgb: [u8; 3], alpha: f32) -> D2D1_GRADIENT_STOP {
    D2D1_GRADIENT_STOP {
        position,
        color: D2D1_COLOR_F {
            r: rgb[0] as f32 / 255.0,
            g: rgb[1] as f32 / 255.0,
            b: rgb[2] as f32 / 255.0,
            a: alpha,
        },
    }
}

fn solid(
    context: &ID2D1DeviceContext,
    rgb: [u8; 3],
    alpha: f32,
) -> Result<ID2D1SolidColorBrush, String> {
    // SAFETY: the context is alive; the colour is by value.
    unsafe {
        context.CreateSolidColorBrush(
            &D2D1_COLOR_F {
                r: rgb[0] as f32 / 255.0,
                g: rgb[1] as f32 / 255.0,
                b: rgb[2] as f32 / 255.0,
                a: alpha,
            },
            None,
        )
    }
    .map_err(|e| format!("CreateSolidColorBrush: {e}"))
}

fn gradient(
    context: &ID2D1DeviceContext,
    stops: &[D2D1_GRADIENT_STOP],
    start: (f32, f32),
    end: (f32, f32),
) -> Result<ID2D1LinearGradientBrush, String> {
    // SAFETY: the context is alive and the stop slice is a live array.
    unsafe {
        let collection = context
            .CreateGradientStopCollection(
                stops,
                D2D1_COLOR_SPACE_SRGB,
                D2D1_COLOR_SPACE_SRGB,
                D2D1_BUFFER_PRECISION_8BPC_UNORM,
                D2D1_EXTEND_MODE_CLAMP,
                D2D1_COLOR_INTERPOLATION_MODE_PREMULTIPLIED,
            )
            .map_err(|e| format!("CreateGradientStopCollection: {e}"))?;
        let props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
            startPoint: Vector2 {
                X: start.0,
                Y: start.1,
            },
            endPoint: Vector2 { X: end.0, Y: end.1 },
        };
        context
            .CreateLinearGradientBrush(&props, None, &collection)
            .map_err(|e| format!("CreateLinearGradientBrush: {e}"))
    }
}

/// Compile-time check that `PCWSTR` is still the type the text APIs want.
const _: fn() = || {
    let _ = PCWSTR::null();
};
