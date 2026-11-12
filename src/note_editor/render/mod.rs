//! Rendering for the note editor window (NOTE-EDITOR.md §5.1, §5.3, §6.5).
//!
//! The paper is painted into an **opaque** offscreen `ID2D1Bitmap1` so text can
//! use ClearType and the rounded corners and shadow stay pixel-precise on the
//! premultiplied composition swap chain. The bitmap is composited through a
//! rounded clip, then the glass border is stroked over it.
//!
//! The whole render state (text, layouts, scroll, caret) lives on the editor's
//! render thread; the UI thread only forwards input. A frame is drawn when the
//! text changes, the pointer moves a selection, or the caret blinks.

mod paint;

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::ops::Range;

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D_SIZE_U, D2D1_ALPHA_MODE_IGNORE, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F,
    D2D1_FIGURE_BEGIN_FILLED, D2D1_FIGURE_END_CLOSED, D2D1_GRADIENT_STOP, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_TARGET,
    D2D1_BITMAP_PROPERTIES1, D2D1_FEATURE_LEVEL_DEFAULT, D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES,
    D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_SOFTWARE, D2D1_RENDER_TARGET_USAGE_NONE,
    D2D1_ROUNDED_RECT, D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
    ID2D1Bitmap1, ID2D1Device, ID2D1DeviceContext, ID2D1Factory, ID2D1Factory1, ID2D1Geometry,
    ID2D1Image, ID2D1LinearGradientBrush, ID2D1RenderTarget, ID2D1SolidColorBrush,
};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext};
use windows::Win32::Graphics::DirectComposition::{
    IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_ITALIC, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT, DWRITE_FONT_WEIGHT_BOLD, DWRITE_FONT_WEIGHT_NORMAL,
    DWRITE_HIT_TEST_METRICS, DWRITE_LINE_SPACING_METHOD_UNIFORM, DWRITE_PARAGRAPH_ALIGNMENT_NEAR,
    DWRITE_TEXT_ALIGNMENT_TRAILING, DWRITE_TEXT_METRICS, DWRITE_TEXT_RANGE,
    DWRITE_WORD_WRAPPING_WRAP, IDWriteFactory, IDWriteTextFormat, IDWriteTextLayout,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET, DXGI_ERROR_WAS_STILL_DRAWING, DXGI_PRESENT,
    DXGI_SWAP_CHAIN_FLAG, IDXGIDevice, IDXGISurface, IDXGISwapChain1, IDXGISwapChain3,
};
use windows::Win32::Graphics::Gdi::{COLOR_HIGHLIGHT, GetSysColor};
use windows::Win32::Graphics::Imaging::{
    GUID_WICPixelFormat32bppBGRA, GUID_WICPixelFormat32bppPBGRA, IWICPalette, WICBitmapCacheOnLoad,
    WICBitmapDitherTypeNone, WICBitmapPaletteTypeCustom, WICRect,
};
use windows::core::{Interface, PCWSTR, w};
use windows_numerics::{Matrix3x2, Vector2};

use crate::gfx::{
    create_composition, create_d2d, create_d3d_device, create_dwrite, create_swap_chain,
};
use crate::logging;
use crate::note_editor::display::{
    self, Block, DisplayDoc, DisplayLine, PAD_LEFT, PAD_TOP, PaperSize, ResizeZone, Style,
};
use crate::note_editor::model::{self, Doc, Motion};
use crate::render::Present;
use crate::types::{ImageData, RectF};

/// The window is the paper plus a 20 DIP transparent margin for the shadow
/// (NOTE-EDITOR.md §5.1). Owned by `display`; re-exported so callers that only
/// know the render module keep working.
pub use crate::note_editor::display::{
    MARGIN_DIP, RADIUS_DIP, WINDOW_HEIGHT_DIP, WINDOW_WIDTH_DIP,
};
/// Glass border width, DIP.
pub const BORDER_DIP: f32 = 1.5;
/// Dog-ear size, DIP.
const FOLD_DIP: f32 = 18.0;
/// Shadow: 18 % black, 24 DIP blur, 6 DIP down.
const SHADOW_ALPHA: f32 = 0.18;
const SHADOW_BLUR_DIP: f32 = 24.0;
const SHADOW_OFFSET_DIP: f32 = 6.0;
/// Corner of the 9-patch shadow atlas, DIP: the shadow margin plus the paper
/// radius, with room to spare for the blur. Must be larger than
/// `MARGIN_DIP + RADIUS_DIP`. Edges and the centre stretch, so a resize never
/// re-bakes the shadow.
pub const SHADOW_CORNER_DIP: f32 = 48.0;
/// Width of the stretchable middle strip in the atlas, DIP.
const SHADOW_MIDDLE_DIP: f32 = 8.0;
/// Long edge of the paper an editor flight carries, device pixels. The editor
/// stutters above this; 250 % displays crop slightly rather than allocate more.
const FLIGHT_MAX_PX: f32 = 2048.0;
/// Caret blink half-period, seconds.
const BLINK_SECONDS: f64 = 0.53;
/// Mouse wheel scroll, DIP per notch.
pub(super) const WHEEL_STEP: f32 = 40.0;
/// The body and code font families.
const BODY_FAMILY: PCWSTR = w!("Microsoft YaHei UI");
const CODE_FAMILY: PCWSTR = w!("Consolas");
/// Link/selection accent on paper.
const ACCENT: u32 = 0x0E_63_9C;

/// What the render thread tells the UI thread after a command.
#[derive(Clone, Debug)]
pub enum Step {
    Continue,
    /// The document should be committed with this text. `image` is the paper
    /// as painted, cached for the flight back to the line.
    Commit {
        text: String,
        image: Option<ImageData>,
    },
    /// Text was copied or cut; the UI thread owns the clipboard.
    Copy(String),
    Stop,
}

/// A command from the window procedure to the render thread.
#[derive(Clone, Debug)]
pub enum Command {
    Resize {
        width_px: u32,
        height_px: u32,
        dpi: u32,
    },
    Focus(bool),
    PointerDown {
        x: i32,
        y: i32,
    },
    PointerMove {
        x: i32,
        y: i32,
    },
    PointerUp,
    /// Double-click: select the word under the pointer/caret.
    SelectWord,
    Char(char),
    Key {
        code: KeyCode,
        shift: bool,
        control: bool,
    },
    Composition(String),
    CommitComposition(String),
    Paste(String),
    Copy,
    Cut,
    Scroll(f32),
    /// The pointer entered or left a resize band; `None` clears it. Sent only
    /// when the zone changes, so the highlight repaints once per entry/exit.
    Hover(Option<ResizeZone>),
    Commit,
    Resume,
    /// Write failed: flash the glass border red for 120 ms (NOTE-EDITOR.md
    /// §5.6) and re-enable input.
    ErrorFlash,
    Stop,
}

/// The keys the editor interprets itself; character keys arrive as
/// [`Command::Char`] through `WM_CHAR`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyCode {
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    Backspace,
    Delete,
    Enter,
    Tab,
    Escape,
    A,
    B,
    C,
    I,
    S,
    V,
    X,
    Y,
    Z,
    Backtick,
}

/// One laid-out display line.
struct LaidLine {
    /// Index into [`DisplayDoc::lines`].
    index: usize,
    block: Block,
    active: bool,
    /// Box top, including `space_before`, content-relative DIP.
    top: f32,
    /// Where the text layout starts, content-relative DIP.
    text_top: f32,
    /// Measured height of the (possibly wrapped) text.
    text_height: f32,
    box_height: f32,
    indent: f32,
    /// Font size of the block, DIP. The first baseline sits at 0.8 of it, which
    /// is how the list markers find the text they belong to.
    font_size: f32,
    text: String,
    layout: Option<IDWriteTextLayout>,
    /// Composition range in `text`, display bytes.
    composition: Option<Range<usize>>,
}

/// One styled run as [`Assets::layout`] consumes it: the style it applies, the
/// marker flag it branches on, and the UTF-16 length of the range it styles.
///
/// DirectWrite never sees the run's bytes, only the length of the styled range,
/// and the whole laid-out text is compared separately (it is the cache key), so
/// this is the exact signature of a run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RunSignature {
    style: Style,
    marker: bool,
    length: u32,
}

/// Every input [`Assets::layout`] reads, except the laid-out text (that is the
/// cache key) and the render target the drawing effects belong to (that is
/// [`CachedLayout::generation`]).
///
/// The fields are compared one by one instead of hashed, so a hit is never a
/// collision: two lines that share their text but differ in Markdown style
/// (`**x**` and `x` both lay out `x`) keep their own layouts.
#[derive(Debug, Eq, PartialEq)]
struct LayoutInputs {
    /// Layout width in bits; `f32` is not `Eq`.
    width: u32,
    /// Picks the text format: headings use a bold, larger one, code a mono one.
    block: Block,
    /// Passed to `SetLineSpacing` (and the first baseline at 0.8 of it).
    font_size: u32,
    line_spacing: u32,
    mono: bool,
    /// The active line shows its Markdown markers and keeps its block style.
    active: bool,
    /// Composition byte range in the laid-out text: underlined and drawn with
    /// the link brush.
    composition: Option<Range<usize>>,
    /// Style runs in order. Marker runs are absent on inactive lines, exactly
    /// as [`Assets::layout`] skips them.
    runs: Vec<RunSignature>,
}

impl LayoutInputs {
    fn new(
        line: &DisplayLine,
        active: bool,
        width: f32,
        composition: Option<&Range<usize>>,
    ) -> Self {
        Self {
            width: width.to_bits(),
            block: line.block,
            font_size: line.metrics.font_size.to_bits(),
            line_spacing: line.metrics.line_spacing.to_bits(),
            mono: line.metrics.mono,
            active,
            composition: composition.cloned(),
            runs: line
                .runs
                .iter()
                .filter(|run| active || !run.marker)
                .map(|run| RunSignature {
                    style: run.style,
                    marker: run.marker,
                    length: run.text.encode_utf16().count() as u32,
                })
                .collect(),
        }
    }

    /// True when [`Assets::layout`] would build the same layout again. Compares
    /// in place, so a cache hit allocates nothing.
    fn matches(
        &self,
        line: &DisplayLine,
        active: bool,
        width: f32,
        composition: Option<&Range<usize>>,
    ) -> bool {
        self.width == width.to_bits()
            && self.block == line.block
            && self.font_size == line.metrics.font_size.to_bits()
            && self.line_spacing == line.metrics.line_spacing.to_bits()
            && self.mono == line.metrics.mono
            && self.active == active
            && self.composition.as_ref() == composition
            && self.runs_match(line, active)
    }

    fn runs_match(&self, line: &DisplayLine, active: bool) -> bool {
        let mut stored = self.runs.iter();
        let mut current = line.runs.iter().filter(|run| active || !run.marker);
        loop {
            match (stored.next(), current.next()) {
                (None, None) => return true,
                (Some(signature), Some(run)) => {
                    if signature.style != run.style
                        || signature.marker != run.marker
                        || signature.length != run.text.encode_utf16().count() as u32
                    {
                        return false;
                    }
                }
                _ => return false,
            }
        }
    }
}

/// A layout kept from a previous rebuild. Cloning its `layout` is an `AddRef`
/// on the existing `IDWriteTextLayout`, not another `CreateTextLayout`.
struct CachedLayout {
    inputs: LayoutInputs,
    /// Height the wrapped text measured when the layout was built. It depends
    /// on the layout alone, so it is cached with it; `top`, `box_height` and
    /// `content_height` are still recomputed every rebuild because they depend
    /// on which line is active.
    text_height: f32,
    /// Render target generation the layout's drawing effects belong to.
    generation: u64,
    /// [`EditorState::layout_stamp`] of the rebuild that last asked for it.
    stamp: u64,
    layout: IDWriteTextLayout,
}

/// Cached text formats and brushes.
struct Assets {
    body: IDWriteTextFormat,
    headings: Vec<IDWriteTextFormat>,
    code: IDWriteTextFormat,
    number: IDWriteTextFormat,
    ink: ID2D1SolidColorBrush,
    quote: ID2D1SolidColorBrush,
    marker: ID2D1SolidColorBrush,
    link: ID2D1SolidColorBrush,
    selection: ID2D1SolidColorBrush,
    code_background: ID2D1SolidColorBrush,
    quote_bar: ID2D1SolidColorBrush,
    rule: ID2D1SolidColorBrush,
    fold_edge: ID2D1SolidColorBrush,
    fold_shade: ID2D1SolidColorBrush,
    /// The failed-write border flash (NOTE-EDITOR.md §5.6).
    error: ID2D1SolidColorBrush,
    /// The resize hover highlight: the system highlight colour, drawn on the
    /// window layer so the paper bitmap and its ClearType stay untouched.
    hover: ID2D1SolidColorBrush,
}

impl Assets {
    fn new(
        context: &ID2D1RenderTarget,
        dwrite: &IDWriteFactory,
        color: usize,
    ) -> Result<Self, String> {
        let palette = crate::notes::color(color);
        let mut headings = Vec::with_capacity(6);
        for level in 1..=6u8 {
            let size = match level {
                1 => 24.0,
                2 => 21.0,
                3 => 18.0,
                4 => 16.0,
                _ => 15.0,
            };
            headings.push(format(dwrite, BODY_FAMILY, size, DWRITE_FONT_WEIGHT_BOLD)?);
        }
        Ok(Self {
            body: format(
                dwrite,
                BODY_FAMILY,
                display::BODY_SIZE,
                DWRITE_FONT_WEIGHT_NORMAL,
            )?,
            headings,
            code: format(
                dwrite,
                CODE_FAMILY,
                display::CODE_SIZE,
                DWRITE_FONT_WEIGHT_NORMAL,
            )?,
            number: number_format(dwrite)?,
            ink: solid(context, palette.ink, 1.0)?,
            quote: solid(context, palette.ink, 0.8)?,
            marker: solid(context, palette.ink, 0.45)?,
            link: solid(context, ACCENT, 1.0)?,
            selection: solid(context, ACCENT, 0.3)?,
            code_background: solid(context, palette.ink, 0.04)?,
            quote_bar: solid(context, palette.ink, 0.25)?,
            rule: solid(context, palette.ink, 0.2)?,
            fold_edge: solid(context, palette.edge, 1.0)?,
            fold_shade: solid(context, 0x000000, 0.12)?,
            error: solid(context, 0xC4_2B_1C, 1.0)?,
            hover: solid(context, system_highlight(), 1.0)?,
        })
    }

    fn format(&self, line: &DisplayLine) -> &IDWriteTextFormat {
        if line.metrics.mono {
            &self.code
        } else if let Block::Heading(level) = line.block {
            &self.headings[(level.saturating_sub(1) as usize).min(5)]
        } else {
            &self.body
        }
    }
}

/// The inputs of the last full rebuild: the text the current [`DisplayDoc`] was
/// parsed from, the active line index, the IME composition and the render
/// target generation the cached layouts belong to.
///
/// Moving the caret or extending the selection inside the active line changes
/// none of them, so [`EditorState::rebuild`] can skip both the document parse
/// and every laid-out line and only keep the caret on screen.
#[derive(Debug, Default)]
struct Parsed {
    /// A copy of the document text `display` was parsed from, compared byte for
    /// byte by [`Parsed::is_current`] (no allocation per rebuild).
    text: String,
    /// The active line index of that parse (`display.line_index(caret)`).
    active: usize,
    /// The IME composition that parse laid out, if one was open.
    composition: Option<String>,
    /// The caret position of that parse. Only the composition insertion offset
    /// depends on it, so [`Parsed::is_current`] compares it only then.
    caret: usize,
    /// Text width the layouts were wrapped to. A resize that changes it must
    /// re-lay out every line.
    width: f32,
    /// The `layout_generation` the cached layouts belong to.
    generation: u64,
    /// Cleared until the first rebuild finishes and when the surface is resized.
    ready: bool,
}

impl Parsed {
    /// Whether the current display and its laid-out lines are still valid for
    /// `text` with the caret in `active`, the given composition/generation and
    /// the given content width.
    fn is_current(
        &self,
        text: &str,
        active: usize,
        composition: Option<&str>,
        caret: usize,
        width: f32,
        generation: u64,
    ) -> bool {
        self.ready
            && self.generation == generation
            && self.active == active
            && self.composition.as_deref() == composition
            && self.width == width
            && self.text == text
            // An open composition is laid out at the caret, so moving the caret
            // within the active line moves the composition too.
            && (composition.is_none() || self.caret == caret)
    }
}

/// The render thread's whole editor state.
pub struct EditorState {
    doc: Doc,
    display: DisplayDoc,
    color: usize,
    lines: Vec<LaidLine>,
    /// Layouts kept from the previous rebuilds, keyed by their laid-out text
    /// and checked against [`LayoutInputs`]. Bounded by the number of display
    /// lines: every entry the current rebuild does not touch is evicted.
    layouts: HashMap<String, Vec<CachedLayout>>,
    /// Stamp of the rebuild in progress, see [`CachedLayout::stamp`].
    layout_stamp: u64,
    /// Bumped whenever `assets` is replaced. Every layout carries a brush from
    /// the target it was built against, so a new target invalidates them all.
    layout_generation: u64,
    content_height: f32,
    scroll: f32,
    view_height: f32,
    dirty: bool,
    enabled: bool,
    focused: bool,
    /// Wall clock of the current frame, used by the failed-write flash.
    now: f64,
    error_until: f64,
    composition: Option<String>,
    pointer_down: bool,
    /// The resize band the pointer is over, for the 2 DIP edge highlight.
    hover: Option<ResizeZone>,
    pending_commit: bool,
    blink_at: f64,
    caret_on: bool,
    surface: Surface,
    assets: Assets,
    paper_brush: Option<ID2D1LinearGradientBrush>,
    border_brush: Option<ID2D1LinearGradientBrush>,
    clip: Option<ID2D1Geometry>,
    fold_edge: Option<ID2D1Geometry>,
    fold_shade: Option<ID2D1Geometry>,
    shadow: Option<ShadowAtlas>,
    /// The paper drawn inside the window, DIP. Derived from the client area: the
    /// window is always the paper plus two `MARGIN_DIP` bands.
    paper: PaperSize,
    dpi: u32,
    width_px: u32,
    height_px: u32,
    scale: f32,
    /// The parse and layout inputs of the last full rebuild, see [`Parsed`].
    parsed: Parsed,
    /// Presented frames; the probe uses it to time a resize repaint.
    frames: u64,
    /// CPU time of the last painted frame, before `Present` blocks on vsync.
    last_paint_us: u32,
}

impl EditorState {
    pub fn new(
        hwnd: HWND,
        width_px: u32,
        height_px: u32,
        dpi: u32,
        color: usize,
        text: String,
    ) -> Result<Self, String> {
        let surface = Surface::new(hwnd, width_px, height_px, dpi)?;
        let assets = Assets::new(&surface.rt, &surface.dwrite, color)?;
        let mut doc = Doc::new(text);
        doc.set_caret(0, false);
        let display = DisplayDoc::parse(doc.text());
        let scale = dpi.max(96) as f32 / 96.0;
        let paper = paper_of_window(width_px.max(1), height_px.max(1), dpi.max(96));
        let mut state = Self {
            doc,
            display,
            color,
            lines: Vec::new(),
            layouts: HashMap::new(),
            layout_stamp: 0,
            layout_generation: 0,
            content_height: 0.0,
            scroll: 0.0,
            view_height: paper.view_height(),
            dirty: true,
            enabled: true,
            focused: false,
            now: 0.0,
            error_until: 0.0,
            composition: None,
            pointer_down: false,
            hover: None,
            pending_commit: false,
            blink_at: 0.0,
            caret_on: true,
            surface,
            assets,
            paper_brush: None,
            border_brush: None,
            clip: None,
            fold_edge: None,
            fold_shade: None,
            shadow: None,
            paper,
            dpi: dpi.max(96),
            width_px: width_px.max(1),
            height_px: height_px.max(1),
            scale,
            parsed: Parsed::default(),
            frames: 0,
            last_paint_us: 0,
        };
        state.rebuild_scenery()?;
        state.rebuild_shadow()?;
        state.rebuild();
        Ok(state)
    }

    /// Recreates the brushes and geometries that depend on paper size/DPI.
    fn rebuild_scenery(&mut self) -> Result<(), String> {
        let palette = crate::notes::color(self.color);
        let paper = paper_rect(self.paper);
        self.paper_brush = Some(paper_gradient(&self.surface.rt, palette, self.paper)?);
        let glass = [
            D2D1_GRADIENT_STOP {
                position: 0.0,
                color: color_of(0xFF_FF_FF, 0xE0 as f32 / 255.0),
            },
            D2D1_GRADIENT_STOP {
                position: 1.0,
                color: color_of(0xFF_FF_FF, 0x40 as f32 / 255.0),
            },
        ];
        self.border_brush = Some(gradient(
            &self.surface.rt,
            &glass,
            (paper.left, paper.top),
            (paper.left, paper.bottom),
        )?);
        let rounded = D2D1_ROUNDED_RECT {
            rect: d2d_rect(paper),
            radiusX: RADIUS_DIP,
            radiusY: RADIUS_DIP,
        };
        // SAFETY: the factory is alive; the struct is by value.
        self.clip = Some(
            unsafe {
                self.surface
                    .factory
                    .CreateRoundedRectangleGeometry(&rounded)
            }
            .map_err(|error| format!("CreateRoundedRectangleGeometry: {error}"))?
            .cast()
            .map_err(|error| format!("RoundedRectangleGeometry -> ID2D1Geometry: {error}"))?,
        );
        let (fold_edge, fold_shade) = fold_geometries(&self.surface.factory, self.paper)?;
        self.fold_edge = Some(fold_edge);
        self.fold_shade = Some(fold_shade);
        Ok(())
    }

    /// Bakes the 9-patch shadow. Only a DPI change invalidates it; resizing the
    /// window stretches the slices instead.
    fn rebuild_shadow(&mut self) -> Result<(), String> {
        self.shadow = Some(ShadowAtlas::bake(&self.surface, self.scale)?);
        Ok(())
    }

    /// The note's text as the user edited it. Used to hand the text back through
    /// the commit path when the device cannot be recovered.
    pub fn text(&self) -> &str {
        self.doc.text()
    }

    /// Rebuilds the GPU stack after a device loss, keeping the document, the
    /// caret and the scroll position.
    ///
    /// Without this the editor kept drawing into a dead device: every keystroke
    /// failed, the window froze on its last frame and the text could never be
    /// committed, because the flight image needs a live surface too.
    pub fn rebuild_surface(&mut self, hwnd: HWND) -> Result<(), String> {
        let surface = Surface::new(hwnd, self.width_px, self.height_px, self.dpi)?;
        let assets = Assets::new(&surface.rt, &surface.dwrite, self.color)?;
        self.surface = surface;
        self.assets = assets;
        // The cached layouts point at the dead target's brushes.
        self.layout_generation = self.layout_generation.wrapping_add(1);
        // Drop the old device's resources before rebuilding them.
        self.paper_brush = None;
        self.border_brush = None;
        self.clip = None;
        self.fold_edge = None;
        self.fold_shade = None;
        self.shadow = None;
        self.rebuild_scenery()?;
        self.rebuild_shadow()?;
        self.rebuild();
        self.dirty = true;
        logging::error("note editor surface rebuilt");
        Ok(())
    }

    pub fn resize(&mut self, width_px: u32, height_px: u32, dpi: u32) -> Result<(), String> {
        let width_px = width_px.max(1);
        let height_px = height_px.max(1);
        let dpi = dpi.max(96);
        let paper = paper_of_window(width_px, height_px, dpi);
        // A width change re-wraps every line; a height-only drag just shows more
        // or less of the same layout. A DPI change re-lays out for safety.
        let reflow = paper.content_width() != self.paper.content_width() || dpi != self.dpi;
        let rebake_shadow = dpi != self.dpi;
        self.width_px = width_px;
        self.height_px = height_px;
        self.dpi = dpi;
        self.scale = dpi as f32 / 96.0;
        self.paper = paper;
        self.view_height = paper.view_height();
        self.surface.resize(width_px, height_px, dpi)?;
        self.rebuild_scenery()?;
        if rebake_shadow {
            self.rebuild_shadow()?;
        }
        if reflow {
            // The cached layouts were wrapped to the old width; drop them all
            // rather than keep one bucket per drag step.
            self.layouts.clear();
            self.layout_generation = self.layout_generation.wrapping_add(1);
            self.parsed.ready = false;
            self.rebuild();
        } else {
            self.clamp_scroll();
        }
        self.dirty = true;
        Ok(())
    }

    pub fn handle(&mut self, command: Command, now: f64) -> Step {
        match command {
            Command::Resize {
                width_px,
                height_px,
                dpi,
            } => {
                if let Err(error) = self.resize(width_px, height_px, dpi) {
                    logging::error(format!("note editor resize: {error}"));
                }
            }
            Command::Hover(zone) => {
                if self.hover != zone {
                    self.hover = zone;
                    // The highlight lives on the window layer, but the frame is
                    // painted as a whole; one repaint per entry or exit.
                    self.dirty = true;
                }
            }
            Command::Focus(focused) => {
                self.focused = focused;
                self.caret_on = true;
                self.blink_at = now + BLINK_SECONDS;
                self.dirty = true;
            }
            Command::PointerDown { x, y } => {
                if !self.enabled {
                    return Step::Continue;
                }
                self.pointer_down = true;
                if let Some((display_byte, line)) = self.hit_test(x, y) {
                    let source = line.display_to_source(display_byte, true);
                    self.doc.set_caret(source, false);
                }
                self.composition = None;
                self.restart_blink(now);
                self.rebuild();
            }
            Command::PointerMove { x, y } => {
                if !self.enabled || !self.pointer_down {
                    return Step::Continue;
                }
                if let Some((display_byte, line)) = self.hit_test(x, y) {
                    let source = line.display_to_source(display_byte, true);
                    self.doc.set_caret(source, true);
                    self.rebuild();
                }
            }
            Command::PointerUp => {
                self.pointer_down = false;
            }
            Command::SelectWord => {
                if self.enabled {
                    let range = model::word_range(self.doc.text(), self.doc.caret());
                    if range.start < range.end {
                        self.doc.set_caret(range.start, false);
                        self.doc.set_caret(range.end, true);
                        self.restart_blink(now);
                        self.rebuild();
                    }
                }
            }
            Command::Char(character) => {
                if !self.enabled {
                    return Step::Continue;
                }
                self.doc.insert(&character.to_string(), now);
                self.composition = None;
                self.restart_blink(now);
                self.rebuild();
            }
            Command::Key {
                code,
                shift,
                control,
            } => {
                if !self.enabled {
                    return Step::Continue;
                }
                self.key(code, shift, control, now);
                if self.pending_commit {
                    self.pending_commit = false;
                    return self.commit_step();
                }
            }
            Command::Composition(composition) => {
                if !self.enabled {
                    return Step::Continue;
                }
                self.composition = if composition.is_empty() {
                    None
                } else {
                    Some(composition)
                };
                self.restart_blink(now);
                self.rebuild();
            }
            Command::CommitComposition(text) => {
                if !self.enabled {
                    return Step::Continue;
                }
                if !text.is_empty() {
                    self.doc.insert(&text, now);
                }
                self.composition = None;
                self.restart_blink(now);
                self.rebuild();
            }
            Command::Paste(text) => {
                if !self.enabled {
                    return Step::Continue;
                }
                self.doc.paste(&text, now);
                self.restart_blink(now);
                self.rebuild();
            }
            Command::Copy => {
                let text = self
                    .doc
                    .selected_text()
                    .unwrap_or_else(|| self.doc.text().to_string());
                return Step::Copy(text);
            }
            Command::Cut => {
                let Some(text) = self.doc.cut(now) else {
                    return Step::Continue;
                };
                self.restart_blink(now);
                self.rebuild();
                return Step::Copy(text);
            }
            Command::Scroll(delta) => {
                self.scroll = (self.scroll - delta).clamp(0.0, self.max_scroll());
                self.dirty = true;
            }
            Command::Commit => {
                if !self.enabled {
                    return Step::Continue;
                }
                self.enabled = false;
                self.composition = None;
                self.dirty = true;
                return self.commit_step();
            }
            Command::Resume => {
                self.enabled = true;
                self.focused = true;
                self.restart_blink(now);
                self.dirty = true;
            }
            Command::ErrorFlash => {
                self.enabled = true;
                self.focused = true;
                self.error_until = now + 0.12;
                self.restart_blink(now);
                self.dirty = true;
            }
            Command::Stop => return Step::Stop,
        }
        Step::Continue
    }

    fn commit_step(&mut self) -> Step {
        let text = self.doc.text().to_string();
        let image = match self.render_flight_image() {
            Ok(image) => Some(image),
            Err(error) => {
                logging::error(format!("note editor commit image: {error}"));
                None
            }
        };
        Step::Commit { text, image }
    }

    fn key(&mut self, code: KeyCode, shift: bool, control: bool, now: f64) {
        match code {
            KeyCode::Left => {
                let motion = if control {
                    Motion::WordLeft
                } else {
                    Motion::Left
                };
                self.doc.move_caret(motion, shift);
            }
            KeyCode::Right => {
                let motion = if control {
                    Motion::WordRight
                } else {
                    Motion::Right
                };
                self.doc.move_caret(motion, shift);
            }
            KeyCode::Home => {
                let motion = if control {
                    Motion::DocStart
                } else {
                    Motion::LineStart
                };
                self.doc.move_caret(motion, shift);
            }
            KeyCode::End => {
                let motion = if control {
                    Motion::DocEnd
                } else {
                    Motion::LineEnd
                };
                self.doc.move_caret(motion, shift);
            }
            KeyCode::Up => self.vertical(-1, shift),
            KeyCode::Down => self.vertical(1, shift),
            KeyCode::Backspace => self.backspace(now),
            KeyCode::Delete => self.doc.delete_forward(now),
            KeyCode::Enter => self.enter(now),
            KeyCode::Tab => self.doc.insert("    ", now),
            KeyCode::Escape => {
                self.enabled = false;
                self.composition = None;
                self.dirty = true;
                self.pending_commit = true;
            }
            KeyCode::S if control => {
                self.enabled = false;
                self.composition = None;
                self.dirty = true;
                self.pending_commit = true;
            }
            KeyCode::A if control => self.doc.select_all(),
            KeyCode::B if control => self.doc.toggle_marker("**", now),
            KeyCode::I if control => self.doc.toggle_marker("*", now),
            KeyCode::Z if control => {
                if shift {
                    self.doc.redo();
                } else {
                    self.doc.undo();
                }
            }
            KeyCode::Y if control => {
                self.doc.redo();
            }
            KeyCode::Backtick if control => self.doc.toggle_marker("`", now),
            _ => return,
        }
        self.restart_blink(now);
        self.rebuild();
    }

    fn restart_blink(&mut self, now: f64) {
        self.blink_at = now + BLINK_SECONDS;
        self.caret_on = true;
        self.dirty = true;
    }

    /// The Markdown block of the line the caret is on, from the current
    /// projection. The list commands use it to keep fenced code literal.
    fn caret_block(&self) -> Block {
        self.display
            .line_at(self.doc.caret())
            .map(|line| line.block)
            .unwrap_or(Block::Paragraph)
    }

    /// Enter continues a Markdown list: the next line repeats the bullet or the
    /// following number, so a list can be typed without writing every marker.
    /// On an empty item the marker is removed instead of adding a line, which
    /// both deletes the auto-inserted `- ` and ends the list on the second
    /// Enter (NOTE-EDITOR.md §5.4).
    fn enter(&mut self, now: f64) {
        let line = self.doc.caret_line();
        let action = display::list_enter(self.caret_block(), &self.doc.text()[line.clone()]);
        match action {
            Some(display::ListEnter::Continue(prefix)) => {
                self.doc.insert(&format!("\n{prefix}"), now);
            }
            Some(display::ListEnter::End { marker }) => {
                self.doc
                    .delete_range(line.start + marker.start..line.start + marker.end, now);
            }
            None => self.doc.insert("\n", now),
        }
    }

    /// Backspace removes a whole auto-inserted list marker when the item is
    /// empty; anywhere else it deletes one character.
    fn backspace(&mut self, now: f64) {
        if !self.doc.has_selection() {
            let line = self.doc.caret_line();
            let caret_in_line = self.doc.caret().saturating_sub(line.start);
            let marker = display::list_backspace_marker(
                self.caret_block(),
                &self.doc.text()[line.clone()],
                caret_in_line,
            );
            if let Some(marker) = marker {
                self.doc
                    .delete_range(line.start + marker.start..line.start + marker.end, now);
                return;
            }
        }
        self.doc.backspace(now);
    }

    fn vertical(&mut self, delta: i32, extend: bool) {
        let caret = self.doc.caret();
        let current = self.display.line_index(caret);
        let Some(laid) = self.lines.iter().find(|line| line.index == current) else {
            return;
        };
        let Some(layout) = laid.layout.as_ref() else {
            return;
        };
        let offset = self.display_offset(laid, caret);
        let mut x = 0.0;
        let mut y = 0.0;
        let mut metrics = DWRITE_HIT_TEST_METRICS::default();
        // SAFETY: the layout and out-params are live on this stack frame.
        unsafe {
            let _ = layout.HitTestTextPosition(
                utf16_offset(&laid.text, offset),
                false,
                &mut x,
                &mut y,
                &mut metrics,
            );
        }
        let target = current as i32 + delta;
        let Some(target) = self.lines.iter().find(|line| line.index as i32 == target) else {
            return;
        };
        let byte = if let Some(layout) = target.layout.as_ref() {
            let mut trailing = windows::core::BOOL::default();
            let mut inside = windows::core::BOOL::default();
            let mut hit = DWRITE_HIT_TEST_METRICS::default();
            // SAFETY: live layout and out-params.
            unsafe {
                let _ = layout.HitTestPoint(x, 1.0, &mut trailing, &mut inside, &mut hit);
            }
            let mut byte = byte_at_utf16(&target.text, hit.textPosition);
            if trailing.as_bool() {
                byte = model::next_char(&target.text, byte);
            }
            // The layout includes a composition run; the source does not.
            if let Some(composition) = &target.composition {
                if byte >= composition.end {
                    byte -= composition.len();
                } else if byte > composition.start {
                    byte = composition.start;
                }
            }
            byte
        } else {
            0
        };
        let source = self
            .display
            .lines
            .get(target.index)
            .map(|line| line.display_to_source(byte, true))
            .unwrap_or(caret);
        self.doc.set_caret(source, extend);
    }

    /// Maps client pixels to a display byte offset and the display line.
    fn hit_test(&self, x_px: i32, y_px: i32) -> Option<(usize, &DisplayLine)> {
        let x = x_px as f32 / self.scale - MARGIN_DIP - PAD_LEFT;
        let y = y_px as f32 / self.scale - MARGIN_DIP - PAD_TOP + self.scroll;
        let laid = self.lines.iter().min_by(|a, b| {
            distance(a, y)
                .partial_cmp(&distance(b, y))
                .unwrap_or(std::cmp::Ordering::Equal)
        })?;
        let line = self.display.lines.get(laid.index)?;
        let byte = if let Some(layout) = laid.layout.as_ref() {
            let local_x = (x - laid.indent).max(0.0);
            let local_y = (y - laid.text_top).clamp(0.0, laid.text_height.max(1.0) - 1.0);
            let mut trailing = windows::core::BOOL::default();
            let mut inside = windows::core::BOOL::default();
            let mut metrics = DWRITE_HIT_TEST_METRICS::default();
            // SAFETY: live layout and out-params.
            unsafe {
                let _ =
                    layout.HitTestPoint(local_x, local_y, &mut trailing, &mut inside, &mut metrics);
            }
            let mut byte = byte_at_utf16(&laid.text, metrics.textPosition);
            if trailing.as_bool() {
                byte = model::next_char(&laid.text, byte);
            }
            if let Some(composition) = &laid.composition {
                if byte >= composition.end {
                    byte -= composition.len();
                } else if byte > composition.start {
                    byte = composition.start;
                }
            }
            byte
        } else {
            0
        };
        Some((byte, line))
    }

    fn display_offset(&self, laid: &LaidLine, caret: usize) -> usize {
        let line = &self.display.lines[laid.index];
        let mut offset = caret.saturating_sub(line.source.start).min(laid.text.len());
        if let Some(composition) = &laid.composition {
            if caret >= line.source.start + composition.start {
                offset += composition.len();
            }
        }
        offset
    }

    fn max_scroll(&self) -> f32 {
        (self.content_height - self.view_height).max(0.0)
    }

    fn clamp_scroll(&mut self) {
        self.scroll = self.scroll.clamp(0.0, self.max_scroll());
    }

    fn rebuild(&mut self) {
        let caret = self.doc.caret();
        // `display` still belongs to the last full rebuild here.
        let active = self.display.line_index(caret);
        let content_width = self.paper.content_width();
        let caret_only = self.parsed.is_current(
            self.doc.text(),
            active,
            self.composition.as_deref(),
            caret,
            content_width,
            self.layout_generation,
        );
        if caret_only {
            // The document text, the IME composition and the active line are
            // unchanged, so the display map and every layout are still current.
            // Only the caret pixel and the selection rectangles moved, and
            // `paint` derives both from the live document: keep the caret on
            // screen and repaint instead of re-parsing the note.
            self.ensure_caret_visible();
            self.dirty = true;
            return;
        }
        self.display = DisplayDoc::parse(self.doc.text());
        let active = self.display.line_index(caret);
        let generation = self.layout_generation;
        // Remember what this full rebuild was built from; the next caret-only
        // move is then a byte comparison instead of a parse.
        self.parsed.text.clear();
        self.parsed.text.push_str(self.doc.text());
        self.parsed.active = active;
        self.parsed.composition.clone_from(&self.composition);
        self.parsed.caret = caret;
        self.parsed.width = content_width;
        self.parsed.generation = generation;
        self.parsed.ready = true;
        self.layout_stamp = self.layout_stamp.wrapping_add(1);
        let stamp = self.layout_stamp;
        self.lines.clear();
        let mut y = 0.0;
        for (index, line) in self.display.lines.iter().enumerate() {
            let is_active = index == active;
            if line.collapsed(is_active) {
                continue;
            }
            let indent = line.indent(is_active);
            let base_text = line.display_text(is_active);
            let mut text = base_text.clone();
            let mut composition = None;
            if is_active {
                if let Some(composing) = &self.composition {
                    let at = caret.saturating_sub(line.source.start).min(text.len());
                    text.insert_str(at, composing);
                    composition = Some(at..at + composing.len());
                }
            }
            let width = (content_width - indent).max(1.0);
            let metrics = line.metrics;
            let mut layout = None;
            let mut text_height = metrics.font_size * metrics.line_spacing;
            if !text.is_empty() {
                // Reuse the layout of an unchanged line. A hit is an `AddRef` on
                // the old `IDWriteTextLayout`; a miss pays one `CreateTextLayout`
                // (shaping, wrapping, font fallback) for the whole document.
                if let Some(bucket) = self.layouts.get_mut(&text) {
                    let reused = bucket.iter_mut().find(|entry| {
                        entry.generation == generation
                            && entry
                                .inputs
                                .matches(line, is_active, width, composition.as_ref())
                    });
                    if let Some(entry) = reused {
                        entry.stamp = stamp;
                        text_height = entry.text_height;
                        layout = Some(entry.layout.clone());
                    }
                }
                if layout.is_none() {
                    match self.assets.layout(
                        &self.surface.dwrite,
                        line,
                        is_active,
                        &base_text,
                        &text,
                        width,
                        composition.clone(),
                    ) {
                        Ok(laid) => {
                            let mut text_metrics = DWRITE_TEXT_METRICS::default();
                            // SAFETY: live layout and out-param.
                            unsafe {
                                let _ = laid.GetMetrics(&mut text_metrics);
                            }
                            text_height = text_height.max(text_metrics.height);
                            self.layouts
                                .entry(text.clone())
                                .or_default()
                                .push(CachedLayout {
                                    inputs: LayoutInputs::new(
                                        line,
                                        is_active,
                                        width,
                                        composition.as_ref(),
                                    ),
                                    text_height,
                                    generation,
                                    stamp,
                                    layout: laid.clone(),
                                });
                            layout = Some(laid);
                        }
                        Err(error) => logging::error(format!("note editor layout: {error}")),
                    }
                }
            }
            let box_height = metrics.space_before + text_height + metrics.space_after;
            self.lines.push(LaidLine {
                index,
                block: line.block,
                active: is_active,
                top: y,
                text_top: y + metrics.space_before,
                text_height,
                box_height,
                indent,
                font_size: metrics.font_size,
                text,
                layout,
                composition,
            });
            y += box_height;
        }
        // Any layout this rebuild did not ask for is stale (its line changed, or
        // it belongs to an older render target). Evicting here keeps the cache
        // the size of the document instead of letting it grow per keystroke.
        self.layouts.retain(|_, bucket| {
            bucket.retain(|entry| entry.stamp == stamp);
            !bucket.is_empty()
        });
        self.content_height = y;
        self.clamp_scroll();
        self.ensure_caret_visible();
        self.dirty = true;
    }

    fn ensure_caret_visible(&mut self) {
        let Some((_, y, height)) = self.caret_position() else {
            return;
        };
        if y < self.scroll {
            self.scroll = y;
        } else if y + height > self.scroll + self.view_height {
            self.scroll = (y + height - self.view_height).min(self.max_scroll());
        }
        self.clamp_scroll();
    }

    /// Caret position in paper-content DIP (x relative to the text origin, y
    /// relative to the content top).
    fn caret_position(&self) -> Option<(f32, f32, f32)> {
        let caret = self.doc.caret();
        let index = self.display.line_index(caret);
        let laid = self.lines.iter().find(|line| line.index == index)?;
        let Some(layout) = laid.layout.as_ref() else {
            // An empty document (or an empty line) has no layout, but the caret
            // still belongs at its start.
            return Some((laid.indent, laid.text_top, laid.text_height.max(1.0)));
        };
        let offset = self.display_offset(laid, caret);
        let mut x = 0.0;
        let mut y = 0.0;
        let mut metrics = DWRITE_HIT_TEST_METRICS::default();
        // SAFETY: live layout and out-params.
        unsafe {
            let _ = layout.HitTestTextPosition(
                utf16_offset(&laid.text, offset),
                false,
                &mut x,
                &mut y,
                &mut metrics,
            );
        }
        let height = if metrics.height > 0.0 {
            metrics.height
        } else {
            laid.text_height.max(1.0)
        };
        Some((laid.indent + x, laid.text_top + y, height))
    }

    /// The caret in client pixels, for the IME candidate window.
    pub fn caret_client_px(&self) -> Option<(i32, i32, i32)> {
        let (x, y, height) = self.caret_position()?;
        Some((
            ((MARGIN_DIP + PAD_LEFT + x) * self.scale).round() as i32,
            ((MARGIN_DIP + PAD_TOP + y - self.scroll) * self.scale).round() as i32,
            (height * self.scale).round().max(1.0) as i32,
        ))
    }

    /// Draws a frame if anything is dirty or the caret wants to blink.
    pub fn draw(&mut self, now: f64) -> Result<(), String> {
        self.now = now;
        if self.focused && self.enabled && now >= self.blink_at {
            self.caret_on = !self.caret_on;
            self.blink_at = now + BLINK_SECONDS;
            self.dirty = true;
        }
        if !self.dirty {
            return Ok(());
        }
        self.dirty = false;
        self.paint()?;
        self.frames += 1;
        Ok(())
    }

    /// How many frames the editor has presented since it opened.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// CPU time of the last painted frame, in microseconds, without `Present`.
    pub fn last_paint_us(&self) -> u32 {
        self.last_paint_us
    }

    /// How long until the next caret blink, or `None` when asleep.
    pub fn tick_in(&self, now: f64) -> Option<std::time::Duration> {
        if now < self.error_until {
            return Some(std::time::Duration::from_millis(40));
        }
        if self.focused && self.enabled {
            let seconds = (self.blink_at - now).max(0.0);
            Some(std::time::Duration::from_secs_f64(seconds.min(0.6)))
        } else {
            None
        }
    }

    fn paint(&mut self) -> Result<(), String> {
        let started = std::time::Instant::now();
        self.surface.begin_paper()?;
        let target = self.surface.rt.clone();
        self.paint_paper_on(&target);
        self.surface.end_paper()?;

        self.surface.begin_window()?;
        self.paint_window();
        self.surface.end()?;
        // Measured without the present: `Present(1, 0)` waits for vsync and
        // would report the display's cadence, not the drawing cost.
        self.last_paint_us = started.elapsed().as_micros().min(u128::from(u32::MAX)) as u32;

        if self.surface.present(true) == Present::DeviceLost {
            return Err("device lost in the note editor".to_string());
        }
        Ok(())
    }

    /// Renders the paper into a CPU-readable WIC bitmap for the flight to or
    /// from the line.
    ///
    /// This is the same paint code as the screen, not a scaled-up card
    /// thumbnail, so the text keeps the editor's size and stays sharp while the
    /// card travels. The editor's GPU paper bitmap is never read back.
    pub fn render_flight_image(&mut self) -> Result<ImageData, String> {
        let started = std::time::Instant::now();
        // The flight carries the paper at the editor's current size, capped so a
        // 250 % or larger display does not allocate an oversized image.
        let mut scale = self.scale.max(0.5);
        let long_dip = self.paper.width.max(self.paper.height);
        let long_px = long_dip * scale;
        if long_px > FLIGHT_MAX_PX {
            scale *= FLIGHT_MAX_PX / long_px;
        }
        let width = (self.paper.width * scale).round().max(1.0) as u32;
        let height = (self.paper.height * scale).round().max(1.0) as u32;
        let factory = crate::capture::wic::imaging_factory()?;
        // SAFETY: the factory is live, the size is at least one pixel and the
        // pixel format is a static WIC GUID.
        let wic = unsafe {
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
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
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
                .CreateWicBitmapRenderTarget(&wic, &properties)
        }
        .map_err(|error| format!("CreateWicBitmapRenderTarget: {error}"))?;

        let assets = Assets::new(&rt, &self.surface.dwrite, self.color)?;
        let palette = crate::notes::color(self.color);
        let paper_brush = paper_gradient(&rt, palette, self.paper)?;
        let (fold_edge, fold_shade) = fold_geometries(&self.surface.factory, self.paper)?;
        // The paint routines read the brushes from `self`; swap in the WIC
        // target's copies for this one render, then put the screen ones back.
        let screen_assets = std::mem::replace(&mut self.assets, assets);
        let screen_paper = self.paper_brush.replace(paper_brush);
        let screen_edge = self.fold_edge.replace(fold_edge);
        let screen_shade = self.fold_shade.replace(fold_shade);
        // Text layouts carry the link brush as a D2D drawing effect, and that
        // brush belongs to the screen target. Rebuild them against the WIC
        // target before painting, then restore the screen layouts afterwards.
        // The generation bump is what stops the cache from handing either pass
        // a layout built against the other target.
        self.layout_generation = self.layout_generation.wrapping_add(1);
        self.rebuild();

        let painted = {
            // SAFETY: the target is a fresh WIC render target on this thread.
            unsafe {
                rt.SetDpi(96.0, 96.0);
                rt.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
                rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
                rt.SetTransform(&Matrix3x2::scale(scale, scale));
                rt.BeginDraw();
            }
            self.paint_paper_on(&rt);
            // SAFETY: `BeginDraw` was called above on this thread.
            unsafe { rt.EndDraw(None, None) }.map_err(|error| format!("EndDraw: {error}"))
        };

        self.assets = screen_assets;
        self.paper_brush = screen_paper;
        self.fold_edge = screen_edge;
        self.fold_shade = screen_shade;
        // The WIC layouts must not survive the swap back to the screen target.
        self.layout_generation = self.layout_generation.wrapping_add(1);
        self.rebuild();
        painted?;

        // The target holds premultiplied pixels; the flight upload expects
        // straight BGRA.
        let converter = unsafe { factory.CreateFormatConverter() }
            .map_err(|error| format!("CreateFormatConverter: {error}"))?;
        // SAFETY: the converter is fresh and the WIC bitmap is alive.
        unsafe {
            converter.Initialize(
                &wic,
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
        logging::trace(format!(
            "note editor flight image {width}x{height} in {:.1} ms",
            started.elapsed().as_secs_f64() * 1000.0
        ));
        Ok(ImageData::new(width, height, bgra))
    }
}

impl Assets {
    #[allow(clippy::too_many_arguments)]
    fn layout(
        &self,
        dwrite: &IDWriteFactory,
        line: &DisplayLine,
        active: bool,
        base_text: &str,
        text: &str,
        width: f32,
        composition: Option<Range<usize>>,
    ) -> Result<IDWriteTextLayout, String> {
        let format = self.format(line);
        let wide: Vec<u16> = text.encode_utf16().collect();
        // SAFETY: `wide`, `format` and the factory are alive for the call.
        let layout = unsafe { dwrite.CreateTextLayout(&wide, format, width, 100_000.0) }
            .map_err(|error| format!("CreateTextLayout: {error}"))?;
        let metrics = line.metrics;
        // SAFETY: all calls below configure a live layout; ranges are clamped.
        unsafe {
            let _ = layout.SetWordWrapping(DWRITE_WORD_WRAPPING_WRAP);
            let _ = layout.SetLineSpacing(
                DWRITE_LINE_SPACING_METHOD_UNIFORM,
                metrics.font_size * metrics.line_spacing,
                metrics.font_size * 0.8,
            );
        }
        let inserted = composition
            .as_ref()
            .map(|range| text[range.clone()].encode_utf16().count() as u32)
            .unwrap_or(0);
        let mut offset = 0usize;
        for run in line.runs.iter() {
            let shown = active || !run.marker;
            if !shown {
                continue;
            }
            // `offset` indexes the base text; a composition inserted before
            // this run shifts its position in the laid-out text.
            let mut start = utf16_offset(base_text, offset);
            if let Some(composition) = &composition {
                if offset >= composition.end {
                    start += inserted;
                }
            }
            let length = run.text.encode_utf16().count() as u32;
            let range = DWRITE_TEXT_RANGE {
                startPosition: start,
                length,
            };
            apply_style(&layout, run, active, self, range)?;
            offset += run.text.len();
        }
        if let Some(composition) = &composition {
            let start = utf16_offset(text, composition.start);
            let length = text[composition.clone()].encode_utf16().count() as u32;
            let range = DWRITE_TEXT_RANGE {
                startPosition: start,
                length,
            };
            // SAFETY: live layout and range.
            unsafe {
                let _ = layout.SetUnderline(true, range);
                let _ = layout.SetDrawingEffect(&self.link, range);
            }
        }
        Ok(layout)
    }
}

fn apply_style(
    layout: &IDWriteTextLayout,
    run: &display::Run,
    active: bool,
    assets: &Assets,
    range: DWRITE_TEXT_RANGE,
) -> Result<(), String> {
    // SAFETY: the layout is alive and the range was computed from its text.
    unsafe {
        if run.marker {
            let _ = layout.SetDrawingEffect(&assets.marker, range);
            return Ok(());
        }
        if active {
            // The active line keeps its block style; only the markers dim.
            return Ok(());
        }
        match run.style {
            Style::Bold => {
                let _ = layout.SetFontWeight(DWRITE_FONT_WEIGHT_BOLD, range);
            }
            Style::Italic => {
                let _ = layout.SetFontStyle(DWRITE_FONT_STYLE_ITALIC, range);
            }
            Style::Strike => {
                let _ = layout.SetStrikethrough(true, range);
            }
            Style::Code => {
                let _ = layout.SetFontFamilyName(CODE_FAMILY, range);
                let _ = layout.SetFontSize(display::CODE_SIZE, range);
            }
            Style::Link => {
                let _ = layout.SetUnderline(true, range);
                let _ = layout.SetDrawingEffect(&assets.link, range);
            }
            Style::Body | Style::Marker => {}
        }
    }
    Ok(())
}

fn format(
    dwrite: &IDWriteFactory,
    family: PCWSTR,
    size: f32,
    weight: DWRITE_FONT_WEIGHT,
) -> Result<IDWriteTextFormat, String> {
    // SAFETY: static family and locale; the factory is alive.
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

fn number_format(dwrite: &IDWriteFactory) -> Result<IDWriteTextFormat, String> {
    let format = format(dwrite, BODY_FAMILY, 12.0, DWRITE_FONT_WEIGHT_NORMAL)?;
    // SAFETY: the format is alive and not shared yet.
    unsafe {
        let _ = format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_TRAILING);
        let _ = format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR);
    }
    Ok(format)
}

fn solid(
    context: &ID2D1RenderTarget,
    rgb: u32,
    alpha: f32,
) -> Result<ID2D1SolidColorBrush, String> {
    // SAFETY: the context is alive; the colour is by value.
    unsafe { context.CreateSolidColorBrush(&color_of(rgb, alpha), None) }
        .map_err(|error| format!("CreateSolidColorBrush: {error}"))
}

/// The system highlight colour as `0xRRGGBB` — `COLORREF` is `0xBBGGRR`.
/// Falls back to the 2.4 accent blue if the system reports none.
fn system_highlight() -> u32 {
    // SAFETY: reading a system colour has no preconditions.
    let colorref = unsafe { GetSysColor(COLOR_HIGHLIGHT) };
    let red = colorref & 0xFF;
    let green = (colorref >> 8) & 0xFF;
    let blue = (colorref >> 16) & 0xFF;
    let rgb = (red << 16) | (green << 8) | blue;
    if rgb == 0 { 0x4A_90_E2 } else { rgb }
}

fn gradient(
    context: &ID2D1RenderTarget,
    stops: &[D2D1_GRADIENT_STOP],
    start: (f32, f32),
    end: (f32, f32),
) -> Result<ID2D1LinearGradientBrush, String> {
    use windows::Win32::Graphics::Direct2D::{D2D1_EXTEND_MODE_CLAMP, D2D1_GAMMA_2_2};
    // SAFETY: the context is alive and the stop slice is live for the call.
    unsafe {
        let collection = context
            .CreateGradientStopCollection(stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP)
            .map_err(|error| format!("CreateGradientStopCollection: {error}"))?;
        let properties = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
            startPoint: Vector2 {
                X: start.0,
                Y: start.1,
            },
            endPoint: Vector2 { X: end.0, Y: end.1 },
        };
        context
            .CreateLinearGradientBrush(&properties, None, &collection)
            .map_err(|error| format!("CreateLinearGradientBrush: {error}"))
    }
}

fn color_of(rgb: u32, alpha: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((rgb >> 16) & 0xFF) as f32 / 255.0,
        g: ((rgb >> 8) & 0xFF) as f32 / 255.0,
        b: (rgb & 0xFF) as f32 / 255.0,
        a: alpha,
    }
}

fn mix(a: u32, b: u32, t: f32) -> u32 {
    let channel = |shift: u32| {
        let from = ((a >> shift) & 0xFF) as f32;
        let to = ((b >> shift) & 0xFF) as f32;
        (from + (to - from) * t).round() as u32
    };
    (channel(16) << 16) | (channel(8) << 8) | channel(0)
}

fn d2d_rect(rect: RectF) -> D2D_RECT_F {
    D2D_RECT_F {
        left: rect.left,
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
    }
}

/// The paper rectangle inside the window, DIP.
pub fn paper_rect(paper: PaperSize) -> RectF {
    RectF::from_xywh(MARGIN_DIP, MARGIN_DIP, paper.width, paper.height)
}

/// The paper a client area holds: the client minus the two shadow margins.
/// The units are pixels on one axis; the result is DIP.
pub fn paper_of_window(width_px: u32, height_px: u32, dpi: u32) -> PaperSize {
    let scale = dpi.max(96) as f32 / 96.0;
    PaperSize::new(
        (width_px.max(1) as f32 / scale - MARGIN_DIP * 2.0).max(1.0),
        (height_px.max(1) as f32 / scale - MARGIN_DIP * 2.0).max(1.0),
    )
}

/// A display byte offset as a UTF-16 position for DirectWrite.
fn utf16_offset(text: &str, byte: usize) -> u32 {
    let byte = byte.min(text.len());
    text[..byte].encode_utf16().count() as u32
}

/// The byte offset of the char at UTF-16 position `units`.
fn byte_at_utf16(text: &str, units: u32) -> usize {
    let mut count = 0u32;
    for (index, character) in text.char_indices() {
        if count >= units {
            return index;
        }
        count += character.len_utf16() as u32;
    }
    text.len()
}

/// Distance from a content y to a laid line, zero inside its box.
fn distance(line: &LaidLine, y: f32) -> f32 {
    if y < line.top {
        line.top - y
    } else if y > line.top + line.box_height {
        y - (line.top + line.box_height)
    } else {
        0.0
    }
}

/// The paper gradient shared by the on-screen paper bitmap and the WIC image
/// rendered for an editor flight.
fn paper_gradient(
    rt: &ID2D1RenderTarget,
    palette: crate::notes::NoteColor,
    paper: PaperSize,
) -> Result<ID2D1LinearGradientBrush, String> {
    let paper_top = mix(palette.paper, palette.edge, 0.45);
    let stops = [
        D2D1_GRADIENT_STOP {
            position: 0.0,
            color: color_of(palette.paper, 1.0),
        },
        D2D1_GRADIENT_STOP {
            position: 1.0,
            color: color_of(paper_top, 1.0),
        },
    ];
    gradient(rt, &stops, (0.0, 0.0), (0.0, paper.height))
}

/// The two dog-ear triangles, in paper-local DIP.
fn fold_geometries(
    factory: &ID2D1Factory,
    paper: PaperSize,
) -> Result<(ID2D1Geometry, ID2D1Geometry), String> {
    let fold = FOLD_DIP;
    let right = paper.width;
    let bottom = paper.height;
    let edge = triangle(
        factory,
        [
            (right - fold, bottom),
            (right, bottom - fold),
            (right - fold, bottom - fold),
        ],
    )?;
    let shade = triangle(
        factory,
        [
            (right - fold, bottom),
            (right, bottom - fold),
            (right, bottom),
        ],
    )?;
    Ok((edge, shade))
}

fn triangle(factory: &ID2D1Factory, points: [(f32, f32); 3]) -> Result<ID2D1Geometry, String> {
    // SAFETY: the factory is alive; the sink is opened and closed before use.
    unsafe {
        let path = factory
            .CreatePathGeometry()
            .map_err(|error| format!("CreatePathGeometry: {error}"))?;
        let sink = path
            .Open()
            .map_err(|error| format!("PathGeometry::Open: {error}"))?;
        sink.BeginFigure(
            Vector2 {
                X: points[0].0,
                Y: points[0].1,
            },
            D2D1_FIGURE_BEGIN_FILLED,
        );
        sink.AddLine(Vector2 {
            X: points[1].0,
            Y: points[1].1,
        });
        sink.AddLine(Vector2 {
            X: points[2].0,
            Y: points[2].1,
        });
        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
        sink.Close()
            .map_err(|error| format!("PathGeometry::Close: {error}"))?;
        path.cast::<ID2D1Geometry>()
            .map_err(|error| format!("PathGeometry -> ID2D1Geometry: {error}"))
    }
}

/// A rectangle in device pixels, used by the shadow 9-patch.
#[derive(Clone, Copy, Debug, PartialEq)]
struct PixelRect {
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
}

impl PixelRect {
    fn from_edges(left: u32, top: u32, right: u32, bottom: u32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    /// The same rectangle in DIP, for a D2D target at `scale` px/DIP.
    fn to_dip(self, scale: f32) -> D2D_RECT_F {
        D2D_RECT_F {
            left: self.left as f32 / scale,
            top: self.top as f32 / scale,
            right: self.right as f32 / scale,
            bottom: self.bottom as f32 / scale,
        }
    }
}

/// One 9-patch slice: where it goes (window pixels) and where it comes from
/// (atlas pixels).
#[derive(Clone, Copy, Debug, PartialEq)]
struct AtlasSlice {
    dest: PixelRect,
    src: PixelRect,
}

/// The 9-patch paper shadow: one small atlas baked per DPI. The four corners
/// are drawn one-to-one and the edges and middle stretch to the real window, so
/// a resize never re-bakes a full-window shadow (BEHAVIOR §10.1).
struct ShadowAtlas {
    bitmap: ID2D1Bitmap1,
    corner_px: u32,
    middle_px: u32,
}

/// Corner and middle strip of the atlas, device pixels. Pure so the DPI
/// re-bake policy is testable without a device.
fn shadow_atlas_pixels(scale: f32) -> (u32, u32) {
    let corner = (SHADOW_CORNER_DIP * scale).round().max(2.0) as u32;
    let middle = (SHADOW_MIDDLE_DIP * scale).round().max(1.0) as u32;
    (corner, middle)
}

impl ShadowAtlas {
    /// Bakes the atlas for a DPI scale. The reference window is square and just
    /// large enough that its four shadow corners never overlap.
    fn bake(surface: &Surface, scale: f32) -> Result<Self, String> {
        let (corner_px, middle_px) = shadow_atlas_pixels(scale);
        let atlas_px = corner_px * 2 + middle_px;
        let image = bake_shadow(atlas_px, atlas_px, scale);
        Ok(Self {
            bitmap: surface.upload(&image)?,
            corner_px,
            middle_px,
        })
    }
}

/// The nine slices for a window of `width_px` × `height_px`, in device pixels:
/// dest is the window rectangle, src the atlas rectangle.
///
/// Pure so the geometry is testable without a device.
fn shadow_slices(width_px: u32, height_px: u32, corner_px: u32, middle_px: u32) -> [AtlasSlice; 9] {
    let atlas = corner_px * 2 + middle_px;
    // At every size the editor allows the window is much wider than two
    // corners; clamp anyway so a tiny window cannot invert a slice.
    let corner_x = corner_px.min(width_px / 2);
    let corner_y = corner_px.min(height_px / 2);
    let dx = [0, corner_x, width_px - corner_x, width_px];
    let dy = [0, corner_y, height_px - corner_y, height_px];
    let sx = [0, corner_px, corner_px + middle_px, atlas];
    let sy = sx;
    let empty = AtlasSlice {
        dest: PixelRect::from_edges(0, 0, 0, 0),
        src: PixelRect::from_edges(0, 0, 0, 0),
    };
    let mut slices = [empty; 9];
    for row in 0..3 {
        for column in 0..3 {
            slices[row * 3 + column] = AtlasSlice {
                dest: PixelRect::from_edges(dx[column], dy[row], dx[column + 1], dy[row + 1]),
                src: PixelRect::from_edges(sx[column], sy[row], sx[column + 1], sy[row + 1]),
            };
        }
    }
    slices
}

/// Bakes the shadow for a reference window: a rounded-rect mask, three box blur
/// passes, 18 % black, offset 6 DIP down.
fn bake_shadow(width_px: u32, height_px: u32, scale: f32) -> ImageData {
    let width = width_px.max(1) as usize;
    let height = height_px.max(1) as usize;
    let blur = (SHADOW_BLUR_DIP * scale * 0.5).round().max(1.0) as i32;
    let offset = (SHADOW_OFFSET_DIP * scale).round() as i32;
    let margin = MARGIN_DIP * scale;
    let paper = RectF::from_xywh(
        margin,
        margin + offset as f32,
        (width_px as f32 - margin * 2.0).max(1.0),
        (height_px as f32 - margin * 2.0).max(1.0),
    );
    let radius = RADIUS_DIP * scale;
    let mut mask = vec![0u8; width * height];
    for y in 0..height {
        for x in 0..width {
            if inside_rounded(x as f32 + 0.5, y as f32 + 0.5, paper, radius) {
                mask[y * width + x] = 255;
            }
        }
    }
    let mut scratch = vec![0u8; width * height];
    for _ in 0..3 {
        blur_horizontal(&mask, &mut scratch, width, height, blur);
        blur_vertical(&scratch, &mut mask, width, height, blur);
    }
    let mut bgra = vec![0u8; width * height * 4];
    for (index, alpha) in mask.iter().enumerate() {
        let value = (*alpha as f32 * SHADOW_ALPHA).round() as u8;
        bgra[index * 4] = 0;
        bgra[index * 4 + 1] = 0;
        bgra[index * 4 + 2] = 0;
        bgra[index * 4 + 3] = value;
    }
    ImageData::new(width as u32, height as u32, bgra)
}

fn inside_rounded(x: f32, y: f32, rect: RectF, radius: f32) -> bool {
    if x < rect.left || x >= rect.right || y < rect.top || y >= rect.bottom {
        return false;
    }
    let radius = radius.min(rect.width() * 0.5).min(rect.height() * 0.5);
    let cx = if x < rect.left + radius {
        rect.left + radius
    } else if x > rect.right - radius {
        rect.right - radius
    } else {
        x
    };
    let cy = if y < rect.top + radius {
        rect.top + radius
    } else if y > rect.bottom - radius {
        rect.bottom - radius
    } else {
        y
    };
    let dx = x - cx;
    let dy = y - cy;
    dx * dx + dy * dy <= radius * radius
}

fn blur_horizontal(source: &[u8], target: &mut [u8], width: usize, height: usize, radius: i32) {
    let radius = radius.max(0) as usize;
    let divisor = (radius * 2 + 1) as u32;
    for y in 0..height {
        let row = y * width;
        let mut sum = 0u32;
        for x in 0..radius.min(width) {
            sum += source[row + x] as u32;
        }
        for x in 0..width {
            let right = x + radius;
            let left = x as i32 - radius as i32 - 1;
            if right < width {
                sum += source[row + right] as u32;
            }
            if left >= 0 {
                sum -= source[row + left as usize] as u32;
            }
            target[row + x] = (sum / divisor) as u8;
        }
    }
}

fn blur_vertical(source: &[u8], target: &mut [u8], width: usize, height: usize, radius: i32) {
    let radius = radius.max(0) as usize;
    let divisor = (radius * 2 + 1) as u32;
    for x in 0..width {
        let mut sum = 0u32;
        for y in 0..radius.min(height) {
            sum += source[y * width + x] as u32;
        }
        for y in 0..height {
            let bottom = y + radius;
            let top = y as i32 - radius as i32 - 1;
            if bottom < height {
                sum += source[bottom * width + x] as u32;
            }
            if top >= 0 {
                sum -= source[top as usize * width + x] as u32;
            }
            target[y * width + x] = (sum / divisor) as u8;
        }
    }
}

/// DirectComposition surface for the editor. The window is transparent, so the
/// swap chain is premultiplied; text goes to the opaque paper bitmap instead.
struct Surface {
    swap_chain: IDXGISwapChain1,
    swap_chain3: IDXGISwapChain3,
    context: ID2D1DeviceContext,
    /// The same context as the base render-target interface, for painting code
    /// that only needs `ID2D1RenderTarget` (and can therefore also run on a
    /// WIC target when the editor bitmap is rendered for a flight).
    rt: ID2D1RenderTarget,
    factory: ID2D1Factory,
    dwrite: IDWriteFactory,
    targets: [Option<ID2D1Bitmap1>; 2],
    target_props: D2D1_BITMAP_PROPERTIES1,
    paper: Option<ID2D1Bitmap1>,
    paper_props: D2D1_BITMAP_PROPERTIES1,
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
        // The paper is painted to an opaque bitmap, so ClearType is valid
        // there; the window's own surface is premultiplied, so text drawn
        // directly into it stays grayscale.
        let (d2d_factory, d2d_device, context) =
            create_d2d(&dxgi_device, dpi_f, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE)?;
        let factory: ID2D1Factory = d2d_factory
            .cast()
            .map_err(|error| format!("ID2D1Factory1 -> ID2D1Factory: {error}"))?;
        let rt: ID2D1RenderTarget = context
            .cast()
            .map_err(|error| format!("ID2D1DeviceContext -> ID2D1RenderTarget: {error}"))?;
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
        let paper_props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_IGNORE,
            },
            dpiX: dpi_f,
            dpiY: dpi_f,
            // A regular render target; only swap-chain buffers are CANNOT_DRAW.
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET,
            colorContext: ManuallyDrop::new(None),
        };
        let dwrite = create_dwrite()?;
        let mut surface = Self {
            swap_chain,
            swap_chain3,
            context,
            rt,
            factory,
            dwrite,
            targets: [None, None],
            target_props,
            paper: None,
            paper_props,
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
        surface.ensure_paper()?;
        Ok(surface)
    }

    fn attach_content(&mut self) -> Result<(), String> {
        let content: windows::core::IUnknown = self
            .swap_chain
            .cast()
            .map_err(|error| format!("swap chain -> IUnknown: {error}"))?;
        // SAFETY: the visual and DComp device are alive and owned by self.
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

    fn ensure_paper(&mut self) -> Result<(), String> {
        // The paper is whatever the client area holds once the two shadow
        // margins are removed; a resize reallocates only when that changes.
        let paper = paper_of_window(self.width_px, self.height_px, self.dpi.round() as u32);
        let scale = self.dpi / 96.0;
        let width = (paper.width * scale).round().max(1.0) as u32;
        let height = (paper.height * scale).round().max(1.0) as u32;
        if self
            .paper
            .as_ref()
            .map(|bitmap| bitmap_width(bitmap) == width && bitmap_height(bitmap) == height)
            .unwrap_or(false)
        {
            return Ok(());
        }
        // SAFETY: the context is ours; a new opaque bitmap is created with the
        // paper's physical pixel size.
        self.paper = Some(
            unsafe {
                self.context
                    .CreateBitmap(D2D_SIZE_U { width, height }, None, 0, &self.paper_props)
            }
            .map_err(|error| format!("CreateBitmap(paper): {error}"))?,
        );
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
        self.paper = None;
        // SAFETY: every D2D target was released above.
        unsafe {
            self.swap_chain.ResizeBuffers(
                2,
                width_px,
                height_px,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                DXGI_SWAP_CHAIN_FLAG(0),
            )
        }
        .map_err(|error| format!("ResizeBuffers: {error}"))?;
        self.width_px = width_px;
        self.height_px = height_px;
        self.dpi = dpi_f;
        self.target_props.dpiX = dpi_f;
        self.target_props.dpiY = dpi_f;
        self.paper_props.dpiX = dpi_f;
        self.paper_props.dpiY = dpi_f;
        // SAFETY: the context is ours; a DPI change is pushed down.
        unsafe {
            self.context.SetDpi(dpi_f, dpi_f);
        }
        self.ensure_paper()?;
        Ok(())
    }

    fn begin_paper(&mut self) -> Result<(), String> {
        self.ensure_paper()?;
        let Some(paper) = self.paper.as_ref() else {
            return Err("paper bitmap missing".to_string());
        };
        // SAFETY: the bitmap is a target for this context and stays alive.
        unsafe {
            self.context.SetTarget(paper);
            self.context.SetDpi(self.dpi, self.dpi);
            self.context.SetTransform(&Matrix3x2::identity());
            self.context
                .SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE);
            self.context.BeginDraw();
        }
        Ok(())
    }

    fn end_paper(&mut self) -> Result<(), String> {
        // SAFETY: `begin_paper` called `BeginDraw` on this thread.
        unsafe { self.context.EndDraw(None, None) }
            .map_err(|error| format!("EndDraw(paper): {error}"))
    }

    fn begin_window(&mut self) -> Result<(), String> {
        // SAFETY: the swap chain is ours and the returned index is checked.
        let index = unsafe { self.swap_chain3.GetCurrentBackBufferIndex() } as usize;
        if index >= self.targets.len() {
            return Err(format!("back buffer index {index}"));
        }
        if self.targets[index].is_none() {
            // SAFETY: the swap chain owns the buffer and the surface is alive
            // for the duration of the call.
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
            return Err("swap-chain target missing".to_string());
        };
        // SAFETY: the bitmap is a target for this context and stays alive.
        unsafe {
            self.context.SetTarget(bitmap);
            self.context.SetDpi(self.dpi, self.dpi);
            self.context.SetTransform(&Matrix3x2::identity());
            self.context
                .SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            self.context.BeginDraw();
        }
        Ok(())
    }

    fn end(&mut self) -> Result<(), String> {
        // SAFETY: `begin_window` called `BeginDraw`.
        unsafe { self.context.EndDraw(None, None) }.map_err(|error| format!("EndDraw: {error}"))
    }

    fn present(&self, sync: bool) -> Present {
        // SAFETY: the swap chain is ours; Present is on the render thread.
        let result = unsafe { self.swap_chain.Present(u32::from(sync), DXGI_PRESENT(0)) };
        if result.is_ok() {
            Present::Ok
        } else if result == DXGI_ERROR_WAS_STILL_DRAWING {
            Present::Retry
        } else if result == DXGI_ERROR_DEVICE_REMOVED || result == DXGI_ERROR_DEVICE_RESET {
            Present::DeviceLost
        } else {
            logging::error(format!("note editor Present failed: 0x{:08X}", result.0));
            Present::DeviceLost
        }
    }

    fn upload(&self, image: &ImageData) -> Result<ID2D1Bitmap1, String> {
        let properties = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: self.dpi,
            dpiY: self.dpi,
            bitmapOptions: windows::Win32::Graphics::Direct2D::D2D1_BITMAP_OPTIONS_NONE,
            colorContext: ManuallyDrop::new(None),
        };
        // SAFETY: `image` is a live, correctly sized BGRA buffer for the call.
        unsafe {
            self.context.CreateBitmap(
                D2D_SIZE_U {
                    width: image.width,
                    height: image.height,
                },
                Some(image.bgra.as_ptr() as *const c_void),
                image.width * 4,
                &properties,
            )
        }
        .map_err(|error| format!("CreateBitmap(upload): {error}"))
    }
}

fn bitmap_width(bitmap: &ID2D1Bitmap1) -> u32 {
    // SAFETY: a pure query on a live bitmap.
    unsafe { bitmap.GetPixelSize() }.width
}

fn bitmap_height(bitmap: &ID2D1Bitmap1) -> u32 {
    // SAFETY: a pure query on a live bitmap.
    unsafe { bitmap.GetPixelSize() }.height
}

#[cfg(test)]
mod tests {
    use super::paint::in_view;
    use super::*;
    use crate::note_editor::display::{DEFAULT_PAPER, LineMetrics, PAD_BOTTOM, PAPER_HEIGHT, Run};

    /// One hand-built display line, so a test can vary a single layout input.
    fn display_line(text: &str, block: Block, style: Style) -> DisplayLine {
        DisplayLine {
            source: 0..text.len(),
            block,
            runs: vec![Run {
                text: text.to_string(),
                source: 0..text.len(),
                style,
                marker: false,
            }],
            metrics: LineMetrics {
                font_size: display::BODY_SIZE,
                line_spacing: display::BODY_SPACING,
                bold: false,
                mono: false,
                indent: 0.0,
                space_before: 0.0,
                space_after: 0.0,
                ink: 1.0,
            },
        }
    }

    /// One laid line at a known content position, without a DirectWrite layout.
    fn laid_line(top: f32, box_height: f32) -> LaidLine {
        LaidLine {
            index: 0,
            block: Block::Paragraph,
            active: false,
            top,
            text_top: top,
            text_height: box_height,
            box_height,
            indent: 0.0,
            font_size: display::BODY_SIZE,
            text: "x".to_string(),
            layout: None,
            composition: None,
        }
    }

    #[test]
    fn a_layout_is_only_reused_when_every_input_matches() {
        let line = display_line("bold", Block::Paragraph, Style::Body);
        let base = LayoutInputs::new(&line, false, 400.0, None);
        assert!(base.matches(&line, false, 400.0, None));
        // The active line shows its Markdown markers and keeps the block style.
        assert!(!base.matches(&line, true, 400.0, None));
        // The wrap width comes from the indent.
        assert!(!base.matches(&line, false, 401.0, None));
        // A composition range is underlined in the link colour.
        let composing = 1..3;
        assert!(!base.matches(&line, false, 400.0, Some(&composing)));
        let with_composition = LayoutInputs::new(&line, true, 400.0, Some(&composing));
        assert!(with_composition.matches(&line, true, 400.0, Some(&composing)));
        // Same text, different Markdown style: DirectWrite applies other ranges.
        let italic = display_line("bold", Block::Paragraph, Style::Italic);
        assert!(!base.matches(&italic, false, 400.0, None));
        // Same text and style, other block: `Assets::format` picks another font.
        let heading = display_line("bold", Block::Heading(1), Style::Body);
        assert!(!base.matches(&heading, false, 400.0, None));
        // The line metrics set the font size, the spacing and the format.
        let mut monospace = display_line("bold", Block::Paragraph, Style::Body);
        monospace.metrics.mono = true;
        assert!(!base.matches(&monospace, false, 400.0, None));
        monospace.metrics.mono = false;
        monospace.metrics.font_size = 16.0;
        assert!(!base.matches(&monospace, false, 400.0, None));
        monospace.metrics.font_size = display::BODY_SIZE;
        monospace.metrics.line_spacing = 2.0;
        assert!(!base.matches(&monospace, false, 400.0, None));
    }

    #[test]
    fn only_lines_that_can_land_on_the_paper_are_painted() {
        let view_height = PAPER_HEIGHT - PAD_TOP - PAD_BOTTOM;
        let box_height = 26.0;
        // Inside the viewport.
        assert!(in_view(&laid_line(10.0, box_height), 0.0, view_height));
        // Scrolled up into the top padding: still on the paper bitmap.
        assert!(in_view(
            &laid_line(0.0, box_height),
            PAD_TOP - 1.0,
            view_height
        ));
        // Above the paper: the whole box is off the bitmap.
        assert!(!in_view(
            &laid_line(0.0, box_height),
            PAD_TOP + box_height + 1.0,
            view_height
        ));
        // Below the last visible line, still inside the bottom padding.
        assert!(in_view(
            &laid_line(view_height, box_height),
            0.0,
            view_height
        ));
        // Past the bottom padding: the box starts below the paper bitmap.
        assert!(!in_view(
            &laid_line(view_height + PAD_BOTTOM, box_height),
            0.0,
            view_height
        ));
    }

    #[test]
    fn the_window_is_paper_plus_a_shadow_margin() {
        assert_eq!(WINDOW_WIDTH_DIP, 500.0);
        assert_eq!(WINDOW_HEIGHT_DIP, 400.0);
        let paper = paper_rect(DEFAULT_PAPER);
        assert_eq!(paper.left, 20.0);
        assert_eq!(paper.top, 20.0);
        assert_eq!(paper.right, 480.0);
        assert_eq!(paper.bottom, 380.0);
    }

    #[test]
    fn a_window_holds_the_paper_it_was_sized_for() {
        // The client area minus the two shadow margins, in DIP.
        assert_eq!(paper_of_window(500, 400, 96), PaperSize::new(460.0, 360.0));
        assert_eq!(
            paper_of_window(1000, 800, 192),
            PaperSize::new(460.0, 360.0)
        );
        // A live drag lands on fractional DIP; the paint code handles it.
        assert_eq!(
            paper_of_window(1201, 801, 96),
            PaperSize::new(1161.0, 761.0)
        );
    }

    #[test]
    fn rounded_hit_matches_the_corners() {
        let rect = RectF::from_xywh(20.0, 20.0, 460.0, 360.0);
        assert!(inside_rounded(250.0, 200.0, rect, 22.0));
        assert!(!inside_rounded(21.0, 21.0, rect, 22.0));
        assert!(inside_rounded(42.0, 42.0, rect, 22.0));
    }

    #[test]
    fn utf16_offsets_map_both_ways_with_astral_characters() {
        let text = "a中😀b";
        assert_eq!(utf16_offset(text, 0), 0);
        assert_eq!(utf16_offset(text, 1), 1);
        assert_eq!(utf16_offset(text, 4), 2);
        assert_eq!(utf16_offset(text, 8), 4);
        assert_eq!(byte_at_utf16(text, 2), 4);
        assert_eq!(byte_at_utf16(text, 4), 8);
        assert_eq!(byte_at_utf16(text, 99), text.len());
    }

    #[test]
    fn the_shadow_has_transparent_edges_and_a_dark_centre() {
        let shadow = bake_shadow(200, 200, 1.0);
        assert!(shadow.is_valid());
        // Outside the paper: clear.
        assert_eq!(shadow.bgra[3], 0);
        // Over the paper centre: 18 % black.
        let centre = ((100 * 200 + 100) * 4) as usize;
        assert!(shadow.bgra[centre + 3] > 30 && shadow.bgra[centre + 3] < 55);
        // The visible left margin carries the blurred edge.
        let margin = ((100 * 200 + 2) * 4) as usize;
        assert!(shadow.bgra[margin + 3] > 0 && shadow.bgra[margin + 3] < 46);
    }

    #[test]
    fn the_shadow_atlas_rebakes_only_when_the_dpi_changes() {
        assert_eq!(shadow_atlas_pixels(1.0), (48, 8));
        assert_eq!(shadow_atlas_pixels(1.5), (72, 12));
        assert_eq!(shadow_atlas_pixels(2.0), (96, 16));
        assert_eq!(shadow_atlas_pixels(2.5), (120, 20));
    }

    #[test]
    fn the_nine_shadow_slices_tile_the_window() {
        // An 860×680 DIP window at 200 %.
        let (corner, middle) = shadow_atlas_pixels(2.0);
        let slices = shadow_slices(1720, 1360, corner, middle);
        // Corners keep their size; the middle takes whatever is left.
        assert_eq!(slices[0].dest, PixelRect::from_edges(0, 0, 96, 96));
        assert_eq!(slices[4].dest, PixelRect::from_edges(96, 96, 1624, 1264));
        assert_eq!(
            slices[8].dest,
            PixelRect::from_edges(1624, 1264, 1720, 1360)
        );
        // No seams, no overlap: every edge meets its neighbour exactly.
        for row in 0..3 {
            for column in 0..2 {
                let left = slices[row * 3 + column].dest;
                let right = slices[row * 3 + column + 1].dest;
                assert_eq!(left.right, right.left);
                assert_eq!(left.top, right.top);
                assert_eq!(left.bottom, right.bottom);
            }
        }
        for column in 0..3 {
            for row in 0..2 {
                let top = slices[row * 3 + column].dest;
                let bottom = slices[(row + 1) * 3 + column].dest;
                assert_eq!(top.bottom, bottom.top);
                assert_eq!(top.left, bottom.left);
                assert_eq!(top.right, bottom.right);
            }
        }
        // The covered area is exactly the window.
        let covered: u32 = slices
            .iter()
            .map(|slice| {
                (slice.dest.right - slice.dest.left) * (slice.dest.bottom - slice.dest.top)
            })
            .sum();
        assert_eq!(covered, 1720 * 1360);
        // Source slices are the corners plus a one-strip middle.
        assert_eq!(slices[0].src, PixelRect::from_edges(0, 0, 96, 96));
        assert_eq!(slices[8].src, PixelRect::from_edges(112, 112, 208, 208));
        assert_eq!(slices[4].src, PixelRect::from_edges(96, 96, 112, 112));
    }

    /// A two-line document parsed once with the caret at its start and no
    /// composition: the state a drag-selection begins from.
    fn parsed_document() -> (DisplayDoc, Parsed) {
        let text = "alpha\nbeta";
        let display = DisplayDoc::parse(text);
        let parsed = Parsed {
            text: text.to_string(),
            active: display.line_index(0),
            composition: None,
            caret: 0,
            width: DEFAULT_PAPER.content_width(),
            generation: 3,
            ready: true,
        };
        (display, parsed)
    }

    #[test]
    fn moving_the_caret_inside_the_active_line_skips_the_reparse() {
        let (display, parsed) = parsed_document();
        let text = "alpha\nbeta";
        // A click or a drag extends the selection inside the first line: the
        // text is unchanged and the caret still lands on the same active line,
        // so the display map and every layout stay valid.
        let width = DEFAULT_PAPER.content_width();
        for caret in [1, 3, 5] {
            assert!(parsed.is_current(text, display.line_index(caret), None, caret, width, 3));
        }
    }

    #[test]
    fn a_width_change_forces_the_reparse() {
        let (display, parsed) = parsed_document();
        let text = "alpha\nbeta";
        // The layouts were wrapped to the old width, so the drag has to lay the
        // note out again even though the text and the caret are unchanged.
        assert!(!parsed.is_current(text, display.line_index(0), None, 0, 500.0, 3));
    }

    #[test]
    fn a_text_change_forces_the_reparse() {
        let (display, parsed) = parsed_document();
        let caret = 2;
        assert!(!parsed.is_current(
            "alpha!\nbeta",
            display.line_index(caret),
            None,
            caret,
            DEFAULT_PAPER.content_width(),
            3
        ));
    }

    #[test]
    fn a_caret_line_change_forces_the_reparse() {
        let (display, parsed) = parsed_document();
        let text = "alpha\nbeta";
        // Taken from the display map so the test does not depend on whether the
        // newline belongs to the first or the second source line.
        let boundary = display.lines[1].source.start;
        let before = boundary - 1;
        assert_eq!(display.line_index(before), 0);
        assert!(parsed.is_current(
            text,
            display.line_index(before),
            None,
            before,
            DEFAULT_PAPER.content_width(),
            3
        ));
        // The caret crossed into the next source line, which now becomes the
        // active line showing its Markdown markers: both lines need layout.
        assert_eq!(display.line_index(boundary), 1);
        assert!(!parsed.is_current(
            text,
            display.line_index(boundary),
            None,
            boundary,
            DEFAULT_PAPER.content_width(),
            3
        ));
    }

    #[test]
    fn an_open_composition_tracks_the_caret() {
        let text = "alpha";
        let display = DisplayDoc::parse(text);
        let parsed = Parsed {
            text: text.to_string(),
            active: 0,
            composition: Some("ni".to_string()),
            caret: 3,
            width: DEFAULT_PAPER.content_width(),
            generation: 0,
            ready: true,
        };
        // The composition is laid out at the caret, so even without a text or
        // active-line change a caret move inside the line has to rebuild.
        assert!(parsed.is_current(
            text,
            display.line_index(3),
            Some("ni"),
            3,
            DEFAULT_PAPER.content_width(),
            0
        ));
        assert!(!parsed.is_current(
            text,
            display.line_index(1),
            Some("ni"),
            1,
            DEFAULT_PAPER.content_width(),
            0
        ));
    }

    #[test]
    fn a_new_render_target_or_an_unfinished_build_forces_the_reparse() {
        let (display, mut parsed) = parsed_document();
        let text = "alpha\nbeta";
        // A device loss or the flight-image render replaces the layout target.
        assert!(!parsed.is_current(
            text,
            display.line_index(0),
            None,
            0,
            DEFAULT_PAPER.content_width(),
            4
        ));
        parsed.generation = 4;
        assert!(parsed.is_current(
            text,
            display.line_index(0),
            None,
            0,
            DEFAULT_PAPER.content_width(),
            4
        ));
        // Until the first rebuild finished there is nothing to keep.
        parsed.ready = false;
        assert!(!parsed.is_current(
            text,
            display.line_index(0),
            None,
            0,
            DEFAULT_PAPER.content_width(),
            4
        ));
    }
}
