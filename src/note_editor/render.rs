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
    D2D1_BITMAP_PROPERTIES1, D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_DRAW_TEXT_OPTIONS_NONE,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_INTERPOLATION_MODE_LINEAR, D2D1_LAYER_OPTIONS1_NONE,
    D2D1_LAYER_PARAMETERS1, D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES, D2D1_ROUNDED_RECT,
    D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, D2D1CreateFactory,
    ID2D1Bitmap1, ID2D1Device, ID2D1DeviceContext, ID2D1Factory, ID2D1Factory1, ID2D1Geometry,
    ID2D1Image, ID2D1LinearGradientBrush, ID2D1SolidColorBrush,
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
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_ITALIC,
    DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT, DWRITE_FONT_WEIGHT_BOLD,
    DWRITE_FONT_WEIGHT_NORMAL, DWRITE_HIT_TEST_METRICS, DWRITE_LINE_SPACING_METHOD_UNIFORM,
    DWRITE_MEASURING_MODE_NATURAL, DWRITE_PARAGRAPH_ALIGNMENT_NEAR, DWRITE_TEXT_ALIGNMENT_TRAILING,
    DWRITE_TEXT_METRICS, DWRITE_TEXT_RANGE, DWRITE_WORD_WRAPPING_WRAP, DWriteCreateFactory,
    IDWriteFactory, IDWriteTextFormat, IDWriteTextLayout,
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
use windows_numerics::{Matrix3x2, Vector2};

use crate::i18n;
use crate::logging;
use crate::note_editor::display::{
    self, Block, CONTENT_WIDTH, DisplayDoc, DisplayLine, PAD_BOTTOM, PAD_LEFT, PAD_TOP,
    PAPER_HEIGHT, PAPER_WIDTH, Style,
};
use crate::note_editor::model::{self, Doc, Motion};
use crate::render::Present;
use crate::types::{ImageData, RectF};

/// The window is the paper plus a 20 DIP transparent margin for the shadow
/// (NOTE-EDITOR.md §5.1).
pub const MARGIN_DIP: f32 = 20.0;
pub const WINDOW_WIDTH_DIP: f32 = PAPER_WIDTH + MARGIN_DIP * 2.0;
pub const WINDOW_HEIGHT_DIP: f32 = PAPER_HEIGHT + MARGIN_DIP * 2.0;
/// Paper corner radius and glass border, DIP.
pub const RADIUS_DIP: f32 = 22.0;
pub const BORDER_DIP: f32 = 1.5;
/// Dog-ear size, DIP.
const FOLD_DIP: f32 = 18.0;
/// Shadow: 18 % black, 24 DIP blur, 6 DIP down.
const SHADOW_ALPHA: f32 = 0.18;
const SHADOW_BLUR_DIP: f32 = 24.0;
const SHADOW_OFFSET_DIP: f32 = 6.0;
/// Caret blink half-period, seconds.
const BLINK_SECONDS: f64 = 0.53;
/// Mouse wheel scroll, DIP per notch.
const WHEEL_STEP: f32 = 40.0;
/// The body and code font families.
const BODY_FAMILY: PCWSTR = w!("Microsoft YaHei UI");
const CODE_FAMILY: PCWSTR = w!("Consolas");
/// Link/selection accent on paper.
const ACCENT: u32 = 0x0E_63_9C;

/// What the render thread tells the UI thread after a command.
#[derive(Clone, Debug)]
pub enum Step {
    Continue,
    /// The document should be committed with this text.
    Commit(String),
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
    text: String,
    layout: Option<IDWriteTextLayout>,
    /// Composition range in `text`, display bytes.
    composition: Option<Range<usize>>,
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
}

impl Assets {
    fn new(
        context: &ID2D1DeviceContext,
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

/// The render thread's whole editor state.
pub struct EditorState {
    doc: Doc,
    display: DisplayDoc,
    color: usize,
    lines: Vec<LaidLine>,
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
    shadow: Option<ID2D1Bitmap1>,
    dpi: u32,
    width_px: u32,
    height_px: u32,
    scale: f32,
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
        let assets = Assets::new(&surface.context, &surface.dwrite, color)?;
        let mut doc = Doc::new(text);
        doc.set_caret(0, false);
        let display = DisplayDoc::parse(doc.text());
        let scale = dpi.max(96) as f32 / 96.0;
        let mut state = Self {
            doc,
            display,
            color,
            lines: Vec::new(),
            content_height: 0.0,
            scroll: 0.0,
            view_height: PAPER_HEIGHT - PAD_TOP - PAD_BOTTOM,
            dirty: true,
            enabled: true,
            focused: false,
            now: 0.0,
            error_until: 0.0,
            composition: None,
            pointer_down: false,
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
            dpi: dpi.max(96),
            width_px: width_px.max(1),
            height_px: height_px.max(1),
            scale,
        };
        state.rebuild_scenery()?;
        state.rebuild();
        Ok(state)
    }

    /// Recreates the brushes and geometries that depend on paper size/DPI.
    fn rebuild_scenery(&mut self) -> Result<(), String> {
        let palette = crate::notes::color(self.color);
        let paper = paper_rect(self.scale);
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
        self.paper_brush = Some(gradient(
            &self.surface.context,
            &stops,
            (0.0, 0.0),
            (0.0, PAPER_HEIGHT),
        )?);
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
            &self.surface.context,
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
        let fold = FOLD_DIP;
        let right = PAPER_WIDTH;
        let bottom = PAPER_HEIGHT;
        self.fold_edge = Some(triangle(
            &self.surface.factory,
            [
                (right - fold, bottom),
                (right, bottom - fold),
                (right - fold, bottom - fold),
            ],
        )?);
        self.fold_shade = Some(triangle(
            &self.surface.factory,
            [
                (right - fold, bottom),
                (right, bottom - fold),
                (right, bottom),
            ],
        )?);
        self.shadow = Some(self.surface.upload(&bake_shadow(
            self.width_px,
            self.height_px,
            self.dpi,
        ))?);
        Ok(())
    }

    pub fn resize(&mut self, width_px: u32, height_px: u32, dpi: u32) -> Result<(), String> {
        self.surface.resize(width_px, height_px, dpi)?;
        self.width_px = width_px.max(1);
        self.height_px = height_px.max(1);
        self.dpi = dpi.max(96);
        self.scale = self.dpi as f32 / 96.0;
        self.rebuild_scenery()?;
        self.rebuild();
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
                    return Step::Commit(self.doc.text().to_string());
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
                return Step::Commit(self.doc.text().to_string());
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
            KeyCode::Backspace => self.doc.backspace(now),
            KeyCode::Delete => self.doc.delete_forward(now),
            KeyCode::Enter => self.doc.insert("\n", now),
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
        self.display = DisplayDoc::parse(self.doc.text());
        let caret = self.doc.caret();
        let active = self.display.line_index(caret);
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
            let width = (CONTENT_WIDTH - indent).max(1.0);
            let metrics = line.metrics;
            let mut layout = None;
            let mut text_height = metrics.font_size * metrics.line_spacing;
            if !text.is_empty() {
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
                        layout = Some(laid);
                    }
                    Err(error) => logging::error(format!("note editor layout: {error}")),
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
                text,
                layout,
                composition,
            });
            y += box_height;
        }
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
        Ok(())
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
        self.surface.begin_paper()?;
        self.paint_paper();
        self.surface.end_paper()?;

        self.surface.begin_window()?;
        self.paint_window();
        self.surface.end()?;
        if self.surface.present(true) == Present::DeviceLost {
            return Err("device lost in the note editor".to_string());
        }
        Ok(())
    }

    fn paint_paper(&mut self) {
        let context = &self.surface.context;
        // The paper is opaque, so the whole bitmap can simply be filled.
        let palette = crate::notes::color(self.color);
        // SAFETY: the context is drawing to the paper target.
        unsafe {
            context.Clear(Some(&color_of(palette.paper, 1.0)));
            if let Some(brush) = self.paper_brush.as_ref() {
                let rounded = D2D1_ROUNDED_RECT {
                    rect: D2D_RECT_F {
                        left: 0.0,
                        top: 0.0,
                        right: PAPER_WIDTH,
                        bottom: PAPER_HEIGHT,
                    },
                    radiusX: RADIUS_DIP,
                    radiusY: RADIUS_DIP,
                };
                context.FillRoundedRectangle(&rounded, brush);
            }
        }
        // Dog-ear.
        // SAFETY: the brushes and geometries are alive.
        unsafe {
            if let Some(geometry) = self.fold_edge.as_ref() {
                context.FillGeometry(geometry, &self.assets.fold_edge, None);
            }
            if let Some(geometry) = self.fold_shade.as_ref() {
                context.FillGeometry(geometry, &self.assets.fold_shade, None);
            }
        }
        // Block decoration behind the text.
        let origin_x = PAD_LEFT;
        for line in &self.lines {
            let top = PAD_TOP + line.text_top - self.scroll;
            let height = line.text_height;
            if !line.active {
                match line.block {
                    Block::Code { fence: false } => {
                        // SAFETY: drawing on the paper target.
                        unsafe {
                            context.FillRectangle(
                                &D2D_RECT_F {
                                    left: origin_x + 2.0,
                                    top,
                                    right: origin_x + CONTENT_WIDTH,
                                    bottom: top + height,
                                },
                                &self.assets.code_background,
                            );
                            context.FillRectangle(
                                &D2D_RECT_F {
                                    left: origin_x,
                                    top,
                                    right: origin_x + 2.0,
                                    bottom: top + height,
                                },
                                &self.assets.quote_bar,
                            );
                        }
                    }
                    Block::Quote => {
                        // SAFETY: drawing on the paper target.
                        unsafe {
                            context.FillRectangle(
                                &D2D_RECT_F {
                                    left: origin_x,
                                    top,
                                    right: origin_x + 2.0,
                                    bottom: top + height,
                                },
                                &self.assets.quote_bar,
                            );
                        }
                    }
                    Block::Rule => {
                        // SAFETY: drawing on the paper target.
                        unsafe {
                            context.FillRectangle(
                                &D2D_RECT_F {
                                    left: origin_x,
                                    top: top + height * 0.5,
                                    right: origin_x + CONTENT_WIDTH,
                                    bottom: top + height * 0.5 + 1.0,
                                },
                                &self.assets.rule,
                            );
                        }
                    }
                    Block::List {
                        ordered,
                        number,
                        task,
                    } => self.paint_list_marker(
                        &line.block,
                        ordered,
                        number,
                        task,
                        origin_x,
                        top,
                        height,
                        line.indent,
                    ),
                    _ => {}
                }
            }
            // Inline code background.
            if !line.active {
                self.paint_inline_code(line, origin_x, top);
            }
        }
        // Selection goes behind the glyphs.
        self.paint_selection(origin_x);
        // Text.
        for line in &self.lines {
            let Some(layout) = line.layout.as_ref() else {
                continue;
            };
            let brush = if matches!(line.block, Block::Quote) {
                &self.assets.quote
            } else {
                &self.assets.ink
            };
            let origin = Vector2 {
                X: origin_x + line.indent,
                Y: PAD_TOP + line.text_top - self.scroll,
            };
            // SAFETY: the layout, brush and context are alive.
            unsafe {
                context.DrawTextLayout(origin, layout, brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
            }
        }
        self.paint_caret(origin_x);
        self.paint_placeholder(origin_x);
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_list_marker(
        &self,
        block: &Block,
        ordered: bool,
        number: u32,
        task: Option<bool>,
        origin_x: f32,
        top: f32,
        height: f32,
        indent: f32,
    ) {
        let context = &self.surface.context;
        let center_y = top + height * 0.5;
        // SAFETY: drawing on the paper target with live brushes.
        unsafe {
            match (ordered, task) {
                (_, Some(checked)) => {
                    let box_rect = D2D1_ROUNDED_RECT {
                        rect: D2D_RECT_F {
                            left: origin_x + 1.0,
                            top: center_y - 6.0,
                            right: origin_x + 13.0,
                            bottom: center_y + 6.0,
                        },
                        radiusX: 3.0,
                        radiusY: 3.0,
                    };
                    context.DrawRoundedRectangle(&box_rect, &self.assets.quote, 1.2, None);
                    if checked {
                        context.DrawLine(
                            Vector2 {
                                X: origin_x + 4.0,
                                Y: center_y,
                            },
                            Vector2 {
                                X: origin_x + 6.5,
                                Y: center_y + 2.5,
                            },
                            &self.assets.ink,
                            1.4,
                            None,
                        );
                        context.DrawLine(
                            Vector2 {
                                X: origin_x + 6.5,
                                Y: center_y + 2.5,
                            },
                            Vector2 {
                                X: origin_x + 11.0,
                                Y: center_y - 3.0,
                            },
                            &self.assets.ink,
                            1.4,
                            None,
                        );
                    }
                }
                (false, _) => {
                    let dot = windows::Win32::Graphics::Direct2D::D2D1_ELLIPSE {
                        point: Vector2 {
                            X: origin_x + 8.0,
                            Y: center_y,
                        },
                        radiusX: 2.0,
                        radiusY: 2.0,
                    };
                    context.FillEllipse(&dot, &self.assets.ink);
                }
                (true, None) => {
                    // A stack buffer keeps the number layout allocation-free
                    // even though the draw runs on every caret blink.
                    let mut digits = [0u16; 10];
                    let mut count = 0usize;
                    let mut value = number.max(1);
                    while value > 0 && count < digits.len() {
                        digits[count] = b'0' as u16 + (value % 10) as u16;
                        value /= 10;
                        count += 1;
                    }
                    let mut wide = [0u16; 11];
                    for index in 0..count {
                        wide[index] = digits[count - 1 - index];
                    }
                    wide[count] = b'.' as u16;
                    let len = count + 1;
                    let rect = D2D_RECT_F {
                        left: origin_x + indent - 22.0,
                        top,
                        right: origin_x + indent - 4.0,
                        bottom: top + height,
                    };
                    context.DrawText(
                        &wide[..len],
                        &self.assets.number,
                        &rect,
                        &self.assets.quote,
                        D2D1_DRAW_TEXT_OPTIONS_NONE,
                        DWRITE_MEASURING_MODE_NATURAL,
                    );
                }
            }
        }
        let _ = block;
    }

    fn paint_inline_code(&self, line: &LaidLine, origin_x: f32, top: f32) {
        let Some(layout) = line.layout.as_ref() else {
            return;
        };
        let display = &self.display.lines[line.index];
        let mut offset = 0usize;
        for run in display.runs.iter().filter(|run| !run.marker) {
            if run.style == Style::Code {
                let start = utf16_offset(&line.text, offset);
                let length = run.text.encode_utf16().count() as u32;
                if length > 0 {
                    let mut metrics = [DWRITE_HIT_TEST_METRICS::default(); 8];
                    let mut count = 0u32;
                    // SAFETY: live layout, buffer and out-param.
                    unsafe {
                        let _ = layout.HitTestTextRange(
                            start,
                            length,
                            0.0,
                            0.0,
                            Some(&mut metrics[..]),
                            &mut count,
                        );
                    }
                    // SAFETY: drawing on the paper target.
                    unsafe {
                        for metric in metrics.iter().take(count as usize) {
                            let rect = D2D1_ROUNDED_RECT {
                                rect: D2D_RECT_F {
                                    left: origin_x + line.indent + metric.left - 2.0,
                                    top: top + metric.top,
                                    right: origin_x
                                        + line.indent
                                        + metric.left
                                        + metric.width
                                        + 2.0,
                                    bottom: top + metric.top + metric.height,
                                },
                                radiusX: 3.0,
                                radiusY: 3.0,
                            };
                            self.surface
                                .context
                                .FillRoundedRectangle(&rect, &self.assets.code_background);
                        }
                    }
                }
            }
            offset += run.text.len();
        }
    }

    fn paint_selection(&self, origin_x: f32) {
        let Some(selection) = self.doc.selection() else {
            return;
        };
        for line in &self.lines {
            let Some(layout) = line.layout.as_ref() else {
                continue;
            };
            let display = &self.display.lines[line.index];
            for segment in display.segments(selection.clone(), line.active) {
                let mut start = segment.start;
                let mut end = segment.end;
                if let Some(composition) = &line.composition {
                    if start >= composition.end {
                        start -= composition.len();
                        end -= composition.len();
                    } else if start > composition.start {
                        start = composition.start;
                    }
                    if end > composition.start && end <= composition.end {
                        end = composition.start;
                    }
                }
                if start >= end {
                    continue;
                }
                let from = utf16_offset(&line.text, start);
                let length = (utf16_offset(&line.text, end) - from).max(1);
                let mut metrics = [DWRITE_HIT_TEST_METRICS::default(); 16];
                let mut count = 0u32;
                // SAFETY: live layout, buffer and out-param.
                unsafe {
                    let _ = layout.HitTestTextRange(
                        from,
                        length,
                        0.0,
                        0.0,
                        Some(&mut metrics[..]),
                        &mut count,
                    );
                }
                // SAFETY: drawing on the paper target.
                unsafe {
                    for metric in metrics.iter().take(count as usize) {
                        let rect = D2D_RECT_F {
                            left: origin_x + line.indent + metric.left,
                            top: PAD_TOP + line.text_top - self.scroll + metric.top,
                            right: origin_x + line.indent + metric.left + metric.width,
                            bottom: PAD_TOP + line.text_top - self.scroll
                                + metric.top
                                + metric.height,
                        };
                        self.surface
                            .context
                            .FillRectangle(&rect, &self.assets.selection);
                    }
                }
            }
        }
    }

    fn paint_caret(&self, origin_x: f32) {
        if !self.focused || !self.enabled || !self.caret_on {
            return;
        }
        let Some((x, y, height)) = self.caret_position() else {
            return;
        };
        let rect = D2D_RECT_F {
            left: origin_x + x,
            top: PAD_TOP + y - self.scroll,
            right: origin_x + x + 1.5,
            bottom: PAD_TOP + y - self.scroll + height,
        };
        // SAFETY: drawing on the paper target.
        unsafe {
            self.surface.context.FillRectangle(&rect, &self.assets.ink);
        }
    }

    fn paint_placeholder(&self, origin_x: f32) {
        if !self.doc.is_empty() || self.composition.is_some() {
            return;
        }
        let text = i18n::s().note_placeholder;
        let wide: Vec<u16> = text.encode_utf16().collect();
        let rect = D2D_RECT_F {
            left: origin_x,
            top: PAD_TOP,
            right: origin_x + CONTENT_WIDTH,
            bottom: PAD_TOP + display::BODY_SIZE * display::BODY_SPACING,
        };
        // SAFETY: drawing on the paper target with the body format.
        unsafe {
            self.surface.context.DrawText(
                &wide,
                &self.assets.body,
                &rect,
                &self.assets.marker,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }

    fn paint_window(&self) {
        let context = &self.surface.context;
        // SAFETY: the swap-chain target is set and transparent.
        unsafe {
            context.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }));
            if let Some(shadow) = self.shadow.as_ref() {
                let window = D2D_RECT_F {
                    left: 0.0,
                    top: 0.0,
                    right: self.width_px as f32 / self.scale,
                    bottom: self.height_px as f32 / self.scale,
                };
                context.DrawBitmap(
                    shadow,
                    Some(&window),
                    1.0,
                    D2D1_INTERPOLATION_MODE_LINEAR,
                    None,
                    None,
                );
            }
            let paper = paper_rect(self.scale);
            let Some(geometry) = self.clip.as_ref() else {
                return;
            };
            let layer = D2D1_LAYER_PARAMETERS1 {
                contentBounds: D2D_RECT_F {
                    left: 0.0,
                    top: 0.0,
                    right: self.width_px as f32 / self.scale,
                    bottom: self.height_px as f32 / self.scale,
                },
                geometricMask: ManuallyDrop::new(Some(geometry.clone())),
                maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                maskTransform: Matrix3x2::identity(),
                opacity: 1.0,
                opacityBrush: ManuallyDrop::new(None),
                layerOptions: D2D1_LAYER_OPTIONS1_NONE,
            };
            context.PushLayer(
                &layer,
                None::<&windows::Win32::Graphics::Direct2D::ID2D1Layer>,
            );
            if let Some(paper_bitmap) = self.surface.paper.as_ref() {
                let destination = d2d_rect(paper);
                context.DrawBitmap(
                    paper_bitmap,
                    Some(&destination),
                    1.0,
                    D2D1_INTERPOLATION_MODE_LINEAR,
                    None,
                    None,
                );
            }
            context.PopLayer();
            let rounded = D2D1_ROUNDED_RECT {
                rect: d2d_rect(paper),
                radiusX: RADIUS_DIP,
                radiusY: RADIUS_DIP,
            };
            let _ = geometry;
            if self.now < self.error_until {
                context.DrawRoundedRectangle(&rounded, &self.assets.error, BORDER_DIP, None);
            } else if let Some(brush) = self.border_brush.as_ref() {
                context.DrawRoundedRectangle(&rounded, brush, BORDER_DIP, None);
            }
        }
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
    context: &ID2D1DeviceContext,
    rgb: u32,
    alpha: f32,
) -> Result<ID2D1SolidColorBrush, String> {
    // SAFETY: the context is alive; the colour is by value.
    unsafe { context.CreateSolidColorBrush(&color_of(rgb, alpha), None) }
        .map_err(|error| format!("CreateSolidColorBrush: {error}"))
}

fn gradient(
    context: &ID2D1DeviceContext,
    stops: &[D2D1_GRADIENT_STOP],
    start: (f32, f32),
    end: (f32, f32),
) -> Result<ID2D1LinearGradientBrush, String> {
    use windows::Win32::Graphics::Direct2D::{
        D2D1_BUFFER_PRECISION_8BPC_UNORM, D2D1_COLOR_INTERPOLATION_MODE_PREMULTIPLIED,
        D2D1_COLOR_SPACE_SRGB, D2D1_EXTEND_MODE_CLAMP,
    };
    // SAFETY: the context is alive and the stop slice is live for the call.
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

/// The paper rectangle in window DIP.
pub fn paper_rect(scale: f32) -> RectF {
    let _ = scale;
    RectF::from_xywh(MARGIN_DIP, MARGIN_DIP, PAPER_WIDTH, PAPER_HEIGHT)
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

/// Bakes the paper shadow once per size/DPI: a rounded-rect mask, three box
/// blur passes, 18 % black, offset 6 DIP down.
fn bake_shadow(width_px: u32, height_px: u32, dpi: u32) -> ImageData {
    let width = width_px.max(1) as usize;
    let height = height_px.max(1) as usize;
    let scale = dpi.max(96) as f32 / 96.0;
    let blur = (SHADOW_BLUR_DIP * scale * 0.5).round().max(1.0) as i32;
    let offset = (SHADOW_OFFSET_DIP * scale).round() as i32;
    let paper = RectF::from_xywh(
        MARGIN_DIP * scale,
        MARGIN_DIP * scale + offset as f32,
        PAPER_WIDTH * scale,
        PAPER_HEIGHT * scale,
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
        let (d2d_factory, d2d_device, context) = create_d2d(&dxgi_device, dpi_f)?;
        let factory: ID2D1Factory = d2d_factory
            .cast()
            .map_err(|error| format!("ID2D1Factory1 -> ID2D1Factory: {error}"))?;
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
        let width = (PAPER_WIDTH * self.dpi / 96.0).round().max(1.0) as u32;
        let height = (PAPER_HEIGHT * self.dpi / 96.0).round().max(1.0) as u32;
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
    // SAFETY: the DXGI device is live and owned by this thread.
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
    // SAFETY: the out-params are owned here and the level array is live.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_is_paper_plus_a_shadow_margin() {
        assert_eq!(WINDOW_WIDTH_DIP, 500.0);
        assert_eq!(WINDOW_HEIGHT_DIP, 400.0);
        let paper = paper_rect(2.0);
        assert_eq!(paper.left, 20.0);
        assert_eq!(paper.top, 20.0);
        assert_eq!(paper.right, 480.0);
        assert_eq!(paper.bottom, 380.0);
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
        let shadow = bake_shadow(500, 400, 96);
        assert!(shadow.is_valid());
        // Outside the paper: clear.
        assert_eq!(shadow.bgra[3], 0);
        // Over the paper centre: 18 % black.
        let centre = ((200 * 500 + 250) * 4) as usize;
        assert!(shadow.bgra[centre + 3] > 30 && shadow.bgra[centre + 3] < 55);
    }
}
